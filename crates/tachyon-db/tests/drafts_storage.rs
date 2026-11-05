use std::collections::HashMap;

use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_core::diff::Renames;
use tachyon_core::query::{Filter, Target, parse_filter_with, parse_query};
use tachyon_db::documents::{self, Prepared, Relation};
use tachyon_db::query::{self, JoinQuery, ListQuery, Readable};
use tachyon_db::schema_change::{ChangeError, apply};
use tachyon_db::{DEFAULT_TENANT, DbError, DocCopy, collections};
use uuid::Uuid;

fn posts(versions: Value, fields: Value) -> CollectionSchema {
    serde_json::from_value(json!({"slug": "posts", "versions": versions, "fields": fields}))
        .unwrap()
}

fn fields(relation_to: Value) -> Value {
    json!([
        {"name": "title", "type": "text"},
        {"name": "author", "type": "relationship", "relationTo": relation_to},
        {"name": "summary", "type": "textarea"}
    ])
}

struct Seed {
    authors: Uuid,
    posts: Uuid,
    ann: Uuid,
    bob: Uuid,
}

/// Two authors, and `posts` with `versions`, holding nothing yet.
async fn seed(pool: &PgPool, versions: Value) -> Seed {
    let authors: CollectionSchema = serde_json::from_value(
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
    )
    .unwrap();
    let authors = collections::create(pool, &authors).await.unwrap().id;
    collections::create(
        pool,
        &serde_json::from_value(json!({"slug": "editors", "fields": []})).unwrap(),
    )
    .await
    .unwrap();
    let posts = collections::create(pool, &posts(versions, fields(json!("authors"))))
        .await
        .unwrap()
        .id;
    let author = |name: &str| Prepared::plain(json!({"name": name}).as_object().unwrap().clone());
    let ann = documents::insert(pool, authors, 1, None, &author("Ann"))
        .await
        .unwrap()
        .id;
    let bob = documents::insert(pool, authors, 1, None, &author("Bob"))
        .await
        .unwrap()
        .id;
    Seed {
        authors,
        posts,
        ann,
        bob,
    }
}

/// A post by `author`, published.
async fn post(pool: &PgPool, s: &Seed, title: &str, author: Uuid) -> Uuid {
    let data = json!({"title": title, "author": author.to_string(), "summary": "S"})
        .as_object()
        .unwrap()
        .clone();
    let relations = vec![Relation {
        path: "author".into(),
        at: "author".into(),
        collection_id: s.authors,
        id: author,
    }];
    documents::insert(pool, s.posts, 1, None, &Prepared { data, relations })
        .await
        .unwrap()
        .id
}

async fn copies(pool: &PgPool, id: Uuid) -> (Option<Value>, Option<Value>) {
    sqlx::query_as("SELECT published, draft FROM documents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn set_draft(pool: &PgPool, id: Uuid, draft: Option<Value>) {
    sqlx::query("UPDATE documents SET draft = $2 WHERE id = $1")
        .bind(id)
        .bind(draft)
        .execute(pool)
        .await
        .unwrap();
}

/// Every link row from `id`, as (copy, path, target).
async fn links(pool: &PgPool, id: Uuid) -> Vec<(String, String, Uuid)> {
    sqlx::query_as(
        "SELECT copy, field_path, to_doc FROM relations WHERE from_doc = $1 ORDER BY copy, field_path",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn add_version(pool: &PgPool, s: &Seed, doc: Uuid, data: Value) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO versions (id, tenant_id, collection_id, document_id, kind, data)
         VALUES ($1, $2, $3, $4, 'draft', $5)",
    )
    .bind(id)
    .bind(DEFAULT_TENANT)
    .bind(s.posts)
    .bind(doc)
    .bind(data)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn versions_of(pool: &PgPool, doc: Uuid) -> Vec<(Uuid, Value)> {
    sqlx::query_as("SELECT id, data FROM versions WHERE document_id = $1 ORDER BY id")
        .bind(doc)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn relations_are_kept_per_copy(pool: PgPool) {
    let s = seed(&pool, json!(null)).await;
    let a = post(&pool, &s, "A", s.ann).await;
    // Turning drafts on syncs the draft copy's links beside the published ones.
    apply(
        &pool,
        "posts",
        &posts(json!({"drafts": true}), fields(json!("authors"))),
        &Renames::new(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        links(&pool, a).await,
        vec![
            ("draft".into(), "author".into(), s.ann),
            ("published".into(), "author".into(), s.ann)
        ]
    );
    let refs = documents::references(&pool, s.ann, &[DocCopy::Published, DocCopy::Draft])
        .await
        .unwrap();
    let seen: Vec<(Uuid, &str, DocCopy)> = refs
        .iter()
        .map(|r| (r.id, r.path.as_str(), r.copy))
        .collect();
    assert_eq!(
        seen,
        vec![
            (a, "author", DocCopy::Draft),
            (a, "author", DocCopy::Published)
        ]
    );
    assert_eq!(serde_json::to_value(&refs[0]).unwrap()["copy"], "draft");

    // A link only the draft holds still protects its target.
    sqlx::query("DELETE FROM relations WHERE from_doc = $1 AND copy = 'published'")
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        documents::delete(&pool, s.authors, s.ann).await,
        Err(DbError::Referenced(1))
    ));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn schema_change_rewrites_every_copy(pool: PgPool) {
    let s = seed(&pool, json!({"drafts": true})).await;
    let live = post(&pool, &s, "Live", s.ann).await;
    let trashed = post(&pool, &s, "Trashed", s.ann).await;
    // The live draft has moved on to another author; its link rows haven't caught up, so
    // only the re-sync below can make them right.
    set_draft(
        &pool,
        live,
        Some(json!({"title": "Draft", "author": s.bob.to_string(), "summary": "D"})),
    )
    .await;
    set_draft(
        &pool,
        trashed,
        Some(json!({"title": "Old draft", "author": s.ann.to_string(), "summary": "D"})),
    )
    .await;
    for n in 1..=3 {
        add_version(
            &pool,
            &s,
            live,
            json!({"title": format!("v{n}"), "author": s.ann.to_string(), "summary": "V"}),
        )
        .await;
    }
    documents::delete(&pool, s.posts, trashed).await.unwrap();

    let renamed: Renames = [("title".to_owned(), "headline".to_owned())]
        .into_iter()
        .collect();
    let new = posts(
        json!({"drafts": true}),
        json!([
            {"name": "headline", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "authors"}
        ]),
    );
    apply(&pool, "posts", &new, &renamed, true).await.unwrap();

    let (published, draft) = copies(&pool, live).await;
    assert_eq!(
        published.unwrap(),
        json!({"headline": "Live", "author": s.ann.to_string()})
    );
    assert_eq!(
        draft.unwrap(),
        json!({"headline": "Draft", "author": s.bob.to_string()})
    );
    let (published, draft) = copies(&pool, trashed).await;
    assert_eq!(
        published.unwrap(),
        json!({"headline": "Trashed", "author": s.ann.to_string()})
    );
    assert_eq!(
        draft.unwrap(),
        json!({"headline": "Old draft", "author": s.ann.to_string()})
    );
    let versions: Vec<Value> = versions_of(&pool, live)
        .await
        .into_iter()
        .map(|(_, d)| d)
        .collect();
    assert_eq!(
        versions,
        (1..=3)
            .map(|n| json!({"headline": format!("v{n}"), "author": s.ann.to_string()}))
            .collect::<Vec<_>>()
    );
    // Each copy's links follow its own data.
    assert_eq!(
        links(&pool, live).await,
        vec![
            ("draft".into(), "author".into(), s.bob),
            ("published".into(), "author".into(), s.ann)
        ]
    );

    // Authors no longer allowed: both copies lose the link, data and rows alike.
    let retargeted = posts(
        json!({"drafts": true}),
        json!([
            {"name": "headline", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "editors"}
        ]),
    );
    apply(&pool, "posts", &retargeted, &Renames::new(), true)
        .await
        .unwrap();
    let (published, draft) = copies(&pool, live).await;
    assert_eq!(published.unwrap(), json!({"headline": "Live"}));
    assert_eq!(draft.unwrap(), json!({"headline": "Draft"}));
    assert_eq!(links(&pool, live).await, vec![]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn turning_drafts_on_seeds_drafts(pool: PgPool) {
    let s = seed(&pool, json!(null)).await;
    let live = post(&pool, &s, "Live", s.ann).await;
    let trashed = post(&pool, &s, "Trashed", s.bob).await;
    documents::delete(&pool, s.posts, trashed).await.unwrap();
    apply(
        &pool,
        "posts",
        &posts(json!({"drafts": true}), fields(json!("authors"))),
        &Renames::new(),
        false,
    )
    .await
    .unwrap();
    for (doc, author) in [(live, s.ann), (trashed, s.bob)] {
        let (published, draft) = copies(&pool, doc).await;
        assert_eq!(draft, published);
        assert!(draft.is_some());
        assert_eq!(
            links(&pool, doc).await,
            vec![
                ("draft".into(), "author".into(), author),
                ("published".into(), "author".into(), author)
            ]
        );
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn turning_drafts_off_is_refused_with_pending_drafts(pool: PgPool) {
    let s = seed(&pool, json!({"drafts": true})).await;
    let mut docs = Vec::new();
    for title in ["Same", "Changed", "Never published", "Trashed"] {
        let id = post(&pool, &s, title, s.ann).await;
        let (published, _) = copies(&pool, id).await;
        set_draft(&pool, id, published).await;
        docs.push(id);
    }
    let (same, changed, unpublished, trashed) = (docs[0], docs[1], docs[2], docs[3]);
    let edited = json!({"title": "Edited", "author": s.ann.to_string(), "summary": "S"});
    set_draft(&pool, changed, Some(edited.clone())).await;
    set_draft(&pool, trashed, Some(edited.clone())).await;
    sqlx::query("UPDATE documents SET published = NULL WHERE id = $1")
        .bind(unpublished)
        .execute(&pool)
        .await
        .unwrap();
    documents::delete(&pool, s.posts, trashed).await.unwrap();
    sqlx::query(
        "INSERT INTO relations (tenant_id, from_doc, copy, field_path, to_doc)
         SELECT tenant_id, from_doc, 'draft', field_path, to_doc FROM relations WHERE from_doc = $1",
    )
    .bind(same)
    .execute(&pool)
    .await
    .unwrap();

    let off = posts(json!({"drafts": false}), fields(json!("authors")));
    // `confirm` makes no difference: there's no way to turn drafts off without losing work.
    let refused = apply(&pool, "posts", &off, &Renames::new(), true).await;
    match refused {
        Err(ChangeError::PendingDrafts(message)) => assert_eq!(
            message,
            "3 documents have unpublished drafts (1 in trash); publish or delete them first"
        ),
        other => panic!("{other:?}"),
    }
    assert!(copies(&pool, same).await.1.is_some());

    // Once everything is published, drafts go, and their links with them.
    sqlx::query("UPDATE documents SET published = draft WHERE collection_id = $1")
        .bind(s.posts)
        .execute(&pool)
        .await
        .unwrap();
    apply(&pool, "posts", &off, &Renames::new(), false)
        .await
        .unwrap();
    for doc in docs {
        assert_eq!(copies(&pool, doc).await.1, None);
    }
    assert_eq!(
        links(&pool, same).await,
        vec![("published".into(), "author".into(), s.ann)]
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn pruning_versions_needs_confirm(pool: PgPool) {
    let s = seed(&pool, json!({"maxVersions": 5})).await;
    let a = post(&pool, &s, "A", s.ann).await;
    let b = post(&pool, &s, "B", s.ann).await;
    let mut held = None;
    for n in 0..4 {
        let v = add_version(&pool, &s, a, json!({"title": format!("a{n}")})).await;
        if n == 0 {
            held = Some(v);
        }
    }
    add_version(&pool, &s, b, json!({"title": "b0"})).await;
    // A queued publish holds a's oldest version.
    sqlx::query(
        "INSERT INTO jobs (id, tenant_id, kind, collection_id, document_id, payload, run_at, state)
         VALUES ($1, $2, 'publish', $3, $4, $5, now() + interval '1 day', 'queued')",
    )
    .bind(Uuid::now_v7())
    .bind(DEFAULT_TENANT)
    .bind(s.posts)
    .bind(a)
    .bind(json!({"version": held.unwrap()}))
    .execute(&pool)
    .await
    .unwrap();

    // Raising the limit, or keeping every version, prunes nothing.
    for max in [10, 0] {
        let (_, p) = apply(
            &pool,
            "posts",
            &posts(json!({"maxVersions": max}), fields(json!("authors"))),
            &Renames::new(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(p.pruned, 0);
    }

    let two = posts(json!({"maxVersions": 2}), fields(json!("authors")));
    match apply(&pool, "posts", &two, &Renames::new(), false).await {
        Err(ChangeError::NeedsConfirmation(p)) => assert_eq!(p.pruned, 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(versions_of(&pool, a).await.len(), 4);
    apply(&pool, "posts", &two, &Renames::new(), true)
        .await
        .unwrap();
    let kept: Vec<Value> = versions_of(&pool, a)
        .await
        .into_iter()
        .map(|(_, d)| d["title"].clone())
        .collect();
    // The newest two, and the one the job holds.
    assert_eq!(kept, vec![json!("a0"), json!("a2"), json!("a3")]);

    // Turning versions off deletes every one but the held.
    let none = posts(json!(null), fields(json!("authors")));
    match apply(&pool, "posts", &none, &Renames::new(), false).await {
        Err(ChangeError::NeedsConfirmation(p)) => assert_eq!(p.pruned, 3),
        other => panic!("{other:?}"),
    }
    apply(&pool, "posts", &none, &Renames::new(), true)
        .await
        .unwrap();
    assert_eq!(versions_of(&pool, a).await.len(), 1);
    assert_eq!(versions_of(&pool, b).await.len(), 0);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn versions_and_jobs_stay_in_their_documents_collection(pool: PgPool) {
    let s = seed(&pool, json!({"drafts": true})).await;
    let a = post(&pool, &s, "A", s.ann).await;
    let version = sqlx::query(
        "INSERT INTO versions (id, tenant_id, collection_id, document_id, kind, data)
         VALUES ($1, $2, $3, $4, 'draft', '{}')",
    )
    .bind(Uuid::now_v7())
    .bind(DEFAULT_TENANT)
    .bind(s.authors)
    .bind(a)
    .execute(&pool)
    .await;
    let job = sqlx::query(
        "INSERT INTO jobs (id, tenant_id, kind, collection_id, document_id, run_at, state)
         VALUES ($1, $2, 'publish', $3, $4, now(), 'queued')",
    )
    .bind(Uuid::now_v7())
    .bind(DEFAULT_TENANT)
    .bind(s.authors)
    .bind(a)
    .execute(&pool)
    .await;
    for refused in [version, job] {
        let e = refused.unwrap_err();
        assert!(
            e.as_database_error()
                .is_some_and(|e| e.is_foreign_key_violation()),
            "{e}"
        );
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_job_about_a_document_names_its_collection(pool: PgPool) {
    let s = seed(&pool, json!(null)).await;
    let a = post(&pool, &s, "A", s.ann).await;
    let refused = sqlx::query(
        "INSERT INTO jobs (id, tenant_id, kind, document_id, run_at, state)
         VALUES ($1, $2, 'publish', $3, now(), 'queued')",
    )
    .bind(Uuid::now_v7())
    .bind(DEFAULT_TENANT)
    .bind(a)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        refused
            .as_database_error()
            .is_some_and(|e| e.is_check_violation()),
        "{refused}"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn pruning_preview_only_reads(pool: PgPool) {
    let s = seed(&pool, json!({"maxVersions": 5})).await;
    let a = post(&pool, &s, "A", s.ann).await;
    for n in 0..3 {
        add_version(&pool, &s, a, json!({"title": format!("a{n}")})).await;
    }
    // A replica, or any read-only transaction, can still preview.
    let options = (*pool.connect_options())
        .clone()
        .options([("default_transaction_read_only", "on")]);
    let replica = sqlx::postgres::PgPoolOptions::new()
        .connect_with(options)
        .await
        .unwrap();
    let two = posts(json!({"maxVersions": 2}), fields(json!("authors")));
    let p = tachyon_db::schema_change::preview(&replica, "posts", &two, &Renames::new())
        .await
        .unwrap();
    assert_eq!(p.pruned, 1);
    assert_eq!(versions_of(&pool, a).await.len(), 3);
}

fn readable(copy: DocCopy, owner: Option<Uuid>) -> Readable {
    Readable {
        filter: None,
        copy,
        owner,
    }
}

fn list_query(filter: Option<&Filter>, copy: DocCopy, owner: Option<Uuid>) -> ListQuery<'_> {
    ListQuery {
        filter,
        sort: None,
        limit: 100,
        offset: 0,
        after: None,
        count: true,
        copy,
        owner,
    }
}

/// Each title, sorted, and the count the page reports.
async fn listed(pool: &PgPool, c: Uuid, q: &ListQuery<'_>, key: &str) -> (Vec<String>, i64) {
    let page = query::list(pool, c, q).await.unwrap();
    let mut seen: Vec<String> = page
        .docs
        .iter()
        .map(|d| d.data.0[key].as_str().unwrap().to_owned())
        .collect();
    seen.sort();
    (seen, page.total.unwrap())
}

fn titles_of(found: &[(Uuid, documents::Document)]) -> Vec<String> {
    let mut t: Vec<String> = found
        .iter()
        .map(|(_, d)| d.data.0["title"].as_str().unwrap().to_owned())
        .collect();
    t.sort();
    t
}

/// `notes`, each linking to one post.
async fn notes(pool: &PgPool, s: &Seed, links: &[(&str, Uuid)]) -> (Uuid, CollectionSchema) {
    let schema: CollectionSchema = serde_json::from_value(json!({"slug": "notes", "fields": [
        {"name": "label", "type": "text"},
        {"name": "post", "type": "relationship", "relationTo": "posts"}
    ]}))
    .unwrap();
    let c = collections::create(pool, &schema).await.unwrap().id;
    for (label, to) in links {
        let data = json!({"label": label, "post": to.to_string()})
            .as_object()
            .unwrap()
            .clone();
        let relations = vec![Relation {
            path: "post".into(),
            at: "post".into(),
            collection_id: s.posts,
            id: *to,
        }];
        documents::insert(pool, c, 1, None, &Prepared { data, relations })
            .await
            .unwrap();
    }
    (c, schema)
}

/// `qs` on `fields`, with every relationship it crosses guarded by `guards`.
fn guarded(
    fields: &[tachyon_core::Field],
    qs: &str,
    lookup: &dyn Fn(&str) -> Option<CollectionSchema>,
    guards: &[(&str, Readable)],
) -> Filter {
    fn target(t: Target, guards: &[(&str, Readable)]) -> Target {
        match t {
            Target::Via {
                link,
                many,
                polymorphic,
                collections,
                inner,
                ..
            } => Target::Via {
                link,
                many,
                polymorphic,
                collections,
                inner: Box::new(target(*inner, guards)),
                guards: guards
                    .iter()
                    .map(|(slug, r)| ((*slug).to_owned(), r.clone()))
                    .collect(),
            },
            other => other,
        }
    }
    let f = parse_filter_with(fields, &parse_query(qs).unwrap()["where"], lookup).unwrap();
    match f {
        Filter::Cond(mut c) => {
            c.target = target(c.target, guards);
            Filter::Cond(c)
        }
        other => panic!("one condition expected, got {other:?}"),
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn published_reads_skip_unpublished(pool: PgPool) {
    let s = seed(&pool, json!({"drafts": true})).await;
    let live = post(&pool, &s, "Live", s.ann).await;
    let hidden = post(&pool, &s, "Hidden", s.ann).await;
    sqlx::query("UPDATE documents SET draft = published WHERE collection_id = $1")
        .bind(s.posts)
        .execute(&pool)
        .await
        .unwrap();
    // Never published: its work lives in the draft only.
    sqlx::query(
        "UPDATE documents SET published = NULL, published_at = NULL, status = 'draft' WHERE id = $1",
    )
    .bind(hidden)
    .execute(&pool)
    .await
    .unwrap();

    let published = list_query(None, DocCopy::Published, None);
    assert_eq!(
        listed(&pool, s.posts, &published, "title").await,
        (vec!["Live".into()], 1)
    );
    let draft = list_query(None, DocCopy::Draft, None);
    assert_eq!(
        listed(&pool, s.posts, &draft, "title").await,
        (vec!["Hidden".into(), "Live".into()], 2)
    );

    assert!(matches!(
        documents::get(&pool, s.posts, hidden, DocCopy::Published).await,
        Err(DbError::NotFound)
    ));
    let got = documents::get(&pool, s.posts, hidden, DocCopy::Draft)
        .await
        .unwrap();
    assert_eq!(got.data.0["title"], "Hidden");
    assert_eq!(got.published_at, None);
    assert!(
        serde_json::to_value(&got)
            .unwrap()
            .get("publishedAt")
            .is_none()
    );
    let live_doc = documents::get(&pool, s.posts, live, DocCopy::Published)
        .await
        .unwrap();
    assert!(live_doc.published_at.is_some());
    assert!(serde_json::to_value(&live_doc).unwrap()["publishedAt"].is_string());
    let many = documents::get_many(&pool, &[live, hidden]).await.unwrap();
    assert_eq!(titles_of(&many), ["Live"]);

    let ids = [live, hidden];
    let read = HashMap::from([(s.posts, readable(DocCopy::Published, None))]);
    let found = query::get_many_readable(&pool, &ids, &read).await.unwrap();
    assert_eq!(titles_of(&found), ["Live"]);
    let read = HashMap::from([(s.posts, readable(DocCopy::Draft, None))]);
    let found = query::get_many_readable(&pool, &ids, &read).await.unwrap();
    assert_eq!(titles_of(&found), ["Hidden", "Live"]);

    // Through a link: guarded or not, an unpublished post never matches a published read.
    let (notes, schema) = notes(&pool, &s, &[("to-live", live), ("to-hidden", hidden)]).await;
    let posts_schema = posts(json!({"drafts": true}), fields(json!("authors")));
    let lookup = |slug: &str| (slug == "posts").then(|| posts_schema.clone());
    let qs = "where[post.title][exists]=true";
    for (guards, want) in [
        (vec![], vec!["to-live"]),
        (
            vec![("posts", readable(DocCopy::Published, None))],
            vec!["to-live"],
        ),
        (
            vec![("posts", readable(DocCopy::Draft, None))],
            vec!["to-hidden", "to-live"],
        ),
    ] {
        let f = guarded(&schema.fields, qs, &lookup, &guards);
        let q = list_query(Some(&f), DocCopy::Published, None);
        assert_eq!(
            listed(&pool, notes, &q, "label").await.0,
            want,
            "{guards:?}"
        );
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn draft_reads_follow_draft_links(pool: PgPool) {
    let s = seed(&pool, json!({"drafts": true})).await;
    // The published copy links to Bob; the draft has moved on to Ann.
    let a = post(&pool, &s, "A", s.bob).await;
    set_draft(
        &pool,
        a,
        Some(json!({"title": "A", "author": s.ann.to_string(), "summary": "S"})),
    )
    .await;
    sqlx::query(
        "INSERT INTO relations (tenant_id, from_doc, copy, field_path, to_doc)
         VALUES ($1, $2, 'draft', 'author', $3)",
    )
    .bind(DEFAULT_TENANT)
    .bind(a)
    .bind(s.ann)
    .execute(&pool)
    .await
    .unwrap();
    let authors = HashMap::from([(s.authors, readable(DocCopy::Published, None))]);
    let posts_schema = posts(json!({"drafts": true}), fields(json!("authors")));
    let authors_schema: CollectionSchema = serde_json::from_value(
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
    )
    .unwrap();
    let lookup = |slug: &str| (slug == "authors").then(|| authors_schema.clone());

    for (copy, linked, other) in [
        (DocCopy::Draft, "Ann", "Bob"),
        (DocCopy::Published, "Bob", "Ann"),
    ] {
        // Populate: the copy read names the link, and the link is what gets fetched.
        let doc = documents::get(&pool, s.posts, a, copy).await.unwrap();
        let to = Uuid::parse_str(doc.data.0["author"].as_str().unwrap()).unwrap();
        let found = query::get_many_readable(&pool, &[to], &authors)
            .await
            .unwrap();
        assert_eq!(found[0].1.data.0["name"], linked, "{copy:?}");

        // A `where` through the link follows the same copy, guarded or not.
        let guards = [("authors", readable(DocCopy::Published, None))];
        for guards in [&guards[..], &[]] {
            for (name, want) in [(linked, vec!["A".to_owned()]), (other, vec![])] {
                let qs = format!("where[author.name][equals]={name}");
                let f = guarded(&posts_schema.fields, &qs, &lookup, guards);
                let q = list_query(Some(&f), copy, None);
                assert_eq!(listed(&pool, s.posts, &q, "title").await.0, want);
            }
        }

        // Joins follow the copy's links too.
        let q = JoinQuery {
            collection_id: s.posts,
            pattern: "^author$",
            filter: None,
            sort: None,
            limit: 10,
            offset: 0,
            count: true,
            copy,
            owner: None,
        };
        let (ann, bob) = if linked == "Ann" { (1, 0) } else { (0, 1) };
        let pages = query::joined(&pool, &[s.ann, s.bob], &q).await.unwrap();
        assert_eq!(pages[&s.ann].docs.len(), ann, "{copy:?}");
        assert_eq!(pages[&s.ann].total, Some(ann as i64));
        assert_eq!(pages[&s.bob].docs.len(), bob, "{copy:?}");
        assert_eq!(pages[&s.bob].total, Some(bob as i64));
        if let Some(d) = pages[&s.ann].docs.first().or(pages[&s.bob].docs.first()) {
            assert_eq!(d.data.0["author"], doc.data.0["author"]);
        }
    }

    // References list both copies, or only those asked for.
    let both = [DocCopy::Published, DocCopy::Draft];
    let seen = |refs: Vec<documents::Reference>| -> Vec<(Uuid, DocCopy)> {
        refs.iter().map(|r| (r.id, r.copy)).collect()
    };
    assert_eq!(
        seen(documents::references(&pool, s.ann, &both).await.unwrap()),
        vec![(a, DocCopy::Draft)]
    );
    assert_eq!(
        seen(documents::references(&pool, s.bob, &both).await.unwrap()),
        vec![(a, DocCopy::Published)]
    );
    assert!(
        documents::references(&pool, s.ann, &[DocCopy::Published])
            .await
            .unwrap()
            .is_empty()
    );
    // Delete protection still counts the draft's link.
    assert!(matches!(
        documents::delete(&pool, s.authors, s.ann).await,
        Err(DbError::Referenced(1))
    ));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn owner_narrows_reads(pool: PgPool) {
    let s = seed(&pool, json!({"drafts": true})).await;
    let (u, v) = (Uuid::now_v7(), Uuid::now_v7());
    let mut mine = Vec::new();
    for (title, by) in [("by-u", u), ("by-v", v)] {
        let data = json!({"title": title, "author": s.ann.to_string(), "summary": "S"})
            .as_object()
            .unwrap()
            .clone();
        let relations = vec![Relation {
            path: "author".into(),
            at: "author".into(),
            collection_id: s.authors,
            id: s.ann,
        }];
        let doc = documents::insert_as(
            &pool,
            s.posts,
            1,
            None,
            &Prepared { data, relations },
            Some(by),
        )
        .await
        .unwrap();
        mine.push(doc.id);
    }
    sqlx::query("UPDATE documents SET draft = published WHERE collection_id = $1")
        .bind(s.posts)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO relations (tenant_id, from_doc, copy, field_path, to_doc)
         SELECT tenant_id, from_doc, 'draft', field_path, to_doc FROM relations
         WHERE from_doc = ANY($1)",
    )
    .bind(&mine)
    .execute(&pool)
    .await
    .unwrap();

    for copy in [DocCopy::Published, DocCopy::Draft] {
        let q = list_query(None, copy, Some(u));
        assert_eq!(
            listed(&pool, s.posts, &q, "title").await,
            (vec!["by-u".into()], 1)
        );
        let read = HashMap::from([(s.posts, readable(copy, Some(u)))]);
        let found = query::get_many_readable(&pool, &mine, &read).await.unwrap();
        assert_eq!(titles_of(&found), ["by-u"]);

        let q = JoinQuery {
            collection_id: s.posts,
            pattern: "^author$",
            filter: None,
            sort: None,
            limit: 10,
            offset: 0,
            count: true,
            copy,
            owner: Some(u),
        };
        let pages = query::joined(&pool, &[s.ann], &q).await.unwrap();
        assert_eq!(pages[&s.ann].docs.len(), 1);
        assert_eq!(pages[&s.ann].docs[0].data.0["title"], "by-u");
        assert_eq!(pages[&s.ann].total, Some(1));
    }

    // Through a link, the guard's owner narrows what can match.
    let (notes, schema) = notes(&pool, &s, &[("to-u", mine[0]), ("to-v", mine[1])]).await;
    let posts_schema = posts(json!({"drafts": true}), fields(json!("authors")));
    let lookup = |slug: &str| (slug == "posts").then(|| posts_schema.clone());
    for copy in [DocCopy::Published, DocCopy::Draft] {
        let guards = [("posts", readable(copy, Some(u)))];
        let f = guarded(
            &schema.fields,
            "where[post.title][exists]=true",
            &lookup,
            &guards,
        );
        let q = list_query(Some(&f), DocCopy::Published, None);
        assert_eq!(listed(&pool, notes, &q, "label").await.0, ["to-u"]);
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn only_drafts_collections_report_a_publish_time(pool: PgPool) {
    let s = seed(&pool, json!({"drafts": true})).await;
    // Authors have no drafts: a field of theirs may be named publishedAt.
    let ann = documents::get(&pool, s.authors, s.ann, DocCopy::Published)
        .await
        .unwrap();
    assert_eq!(ann.published_at, None);
    let mut doc = ann.clone();
    doc.data.0.insert("publishedAt".into(), json!("their own"));
    let json = serde_json::to_string(&doc).unwrap();
    assert_eq!(json.matches("publishedAt").count(), 1, "{json}");
    let page = query::list(
        &pool,
        s.authors,
        &list_query(None, DocCopy::Published, None),
    )
    .await
    .unwrap();
    assert!(page.docs.iter().all(|d| d.published_at.is_none()));

    // A post with drafts that has been published says when.
    let live = post(&pool, &s, "Live", s.ann).await;
    set_draft(&pool, live, Some(json!({"title": "Live"}))).await;
    let read = documents::get(&pool, s.posts, live, DocCopy::Published)
        .await
        .unwrap();
    assert!(
        read.published_at
            .is_some_and(|at| at <= time::OffsetDateTime::now_utc()),
        "{:?}",
        read.published_at
    );
    let json = serde_json::to_value(&read).unwrap();
    assert!(json["publishedAt"].is_string(), "{json}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn draft_reads_skip_a_missing_draft(pool: PgPool) {
    let s = seed(&pool, json!({"drafts": true})).await;
    let with = post(&pool, &s, "With", s.ann).await;
    let without = post(&pool, &s, "Without", s.ann).await;
    set_draft(&pool, with, Some(json!({"title": "With"}))).await;

    let draft = list_query(None, DocCopy::Draft, None);
    assert_eq!(
        listed(&pool, s.posts, &draft, "title").await,
        (vec!["With".into()], 1)
    );
    assert!(matches!(
        documents::get(&pool, s.posts, without, DocCopy::Draft).await,
        Err(DbError::NotFound)
    ));
    let read = HashMap::from([(s.posts, readable(DocCopy::Draft, None))]);
    let found = query::get_many_readable(&pool, &[with, without], &read)
        .await
        .unwrap();
    assert_eq!(titles_of(&found), ["With"]);
    let (notes, schema) = notes(&pool, &s, &[("to-with", with), ("to-without", without)]).await;
    let posts_schema = posts(json!({"drafts": true}), fields(json!("authors")));
    let lookup = |slug: &str| (slug == "posts").then(|| posts_schema.clone());
    let guards = [("posts", readable(DocCopy::Draft, None))];
    for (id, want) in [(with, vec!["to-with"]), (without, vec![])] {
        let qs = format!("where[post.id][equals]={id}");
        let f = guarded(&schema.fields, &qs, &lookup, &guards);
        let q = list_query(Some(&f), DocCopy::Published, None);
        assert_eq!(listed(&pool, notes, &q, "label").await.0, want);
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn draft_writes_leave_the_published_copy_alone(pool: PgPool) {
    let s = seed(&pool, json!({"drafts": true})).await;
    let by_ann = |title: &str, author: Uuid| Prepared {
        data: json!({"title": title, "author": author.to_string()})
            .as_object()
            .unwrap()
            .clone(),
        relations: vec![Relation {
            path: "author".into(),
            at: "author".into(),
            collection_id: s.authors,
            id: author,
        }],
    };
    let mut tx = pool.begin().await.unwrap();
    let doc = documents::insert_in(
        &mut tx,
        s.posts,
        1,
        None,
        DocCopy::Draft,
        &by_ann("First", s.ann),
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(doc.status, "draft");
    assert_eq!(doc.data.0["title"], "First", "the copy written comes back");
    assert_eq!(doc.published_at, None);
    let (published, draft) = copies(&pool, doc.id).await;
    assert_eq!(published, None);
    assert_eq!(draft.unwrap()["title"], "First");
    assert_eq!(
        links(&pool, doc.id).await,
        [("draft".into(), "author".into(), s.ann)]
    );

    // An update reads and writes the draft, and its guard judges the draft.
    let title_is = |t: &str| {
        let schema = posts(json!({"drafts": true}), fields(json!("authors")));
        let lookup = |_: &str| None;
        parse_filter_with(
            &schema.fields,
            &parse_query(&format!("where[title][equals]={t}")).unwrap()["where"],
            &lookup,
        )
        .unwrap()
    };
    let mut tx = pool.begin().await.unwrap();
    let refused = documents::update_in(
        &mut tx,
        s.posts,
        1,
        doc.id,
        DocCopy::Draft,
        None,
        Some(&title_is("Other")),
        |_| Ok::<_, DbError>(by_ann("Never", s.bob)),
    )
    .await;
    assert!(matches!(refused, Err(DbError::Forbidden)));
    drop(tx);
    let mut tx = pool.begin().await.unwrap();
    let updated = documents::update_in(
        &mut tx,
        s.posts,
        1,
        doc.id,
        DocCopy::Draft,
        None,
        Some(&title_is("First")),
        |current| {
            assert_eq!(current["title"], "First");
            Ok::<_, DbError>(by_ann("Second", s.bob))
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(updated.data.0["title"], "Second");
    assert_eq!(copies(&pool, doc.id).await.0, None, "still unpublished");
    assert_eq!(
        links(&pool, doc.id).await,
        [("draft".into(), "author".into(), s.bob)]
    );

    // A published document's draft is written beside what's live.
    let live = post(&pool, &s, "Live", s.ann).await;
    set_draft(&pool, live, Some(json!({"title": "Live"}))).await;
    let mut tx = pool.begin().await.unwrap();
    documents::update_in(
        &mut tx,
        s.posts,
        1,
        live,
        DocCopy::Draft,
        None,
        None,
        |_| Ok::<_, DbError>(by_ann("Edited", s.bob)),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (published, draft) = copies(&pool, live).await;
    assert_eq!(published.unwrap()["title"], "Live");
    assert_eq!(draft.unwrap()["title"], "Edited");
    assert_eq!(
        links(&pool, live).await,
        [
            ("draft".into(), "author".into(), s.bob),
            ("published".into(), "author".into(), s.ann)
        ]
    );

    // A delete's guard judges the draft too.
    let refused = documents::delete_as(
        &pool,
        s.posts,
        live,
        None,
        Some(&title_is("Live")),
        DocCopy::Draft,
    )
    .await;
    assert!(matches!(refused, Err(DbError::Forbidden)));
    documents::delete_as(
        &pool,
        s.posts,
        live,
        None,
        Some(&title_is("Edited")),
        DocCopy::Draft,
    )
    .await
    .unwrap();
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn recorded_versions_are_pruned_but_never_a_held_one(pool: PgPool) {
    let s = seed(&pool, json!({"drafts": true, "maxVersions": 2})).await;
    let posts = collections::get_by_slug(&pool, "posts").await.unwrap();
    let a = post(&pool, &s, "A", s.ann).await;
    let by = Some(Uuid::now_v7());
    let record = |n: i64| {
        let (pool, posts) = (pool.clone(), posts.clone());
        async move {
            let mut conn = pool.acquire().await.unwrap();
            let data = json!({"title": n}).as_object().unwrap().clone();
            tachyon_db::versions::record_version(&mut conn, a, &posts, DocCopy::Draft, &data, by)
                .await
                .unwrap()
        }
    };
    let first = record(1).await;
    let (collection, kind, created_by): (Uuid, String, Option<Uuid>) =
        sqlx::query_as("SELECT collection_id, kind, created_by FROM versions WHERE id = $1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (collection, kind.as_str(), created_by),
        (s.posts, "draft", by)
    );
    record(2).await;
    record(3).await;
    let kept: Vec<Value> = versions_of(&pool, a)
        .await
        .into_iter()
        .map(|v| v.1)
        .collect();
    assert_eq!(
        kept,
        [json!({"title": 2}), json!({"title": 3})],
        "the newest 2"
    );

    // A version a pending job holds outlives its turn.
    let held = versions_of(&pool, a).await[0].0;
    sqlx::query(
        "INSERT INTO jobs (id, tenant_id, kind, collection_id, document_id, payload, run_at, state)
         VALUES ($1, $2, 'publish', $3, $4, $5, now(), 'queued')",
    )
    .bind(Uuid::now_v7())
    .bind(DEFAULT_TENANT)
    .bind(s.posts)
    .bind(a)
    .bind(json!({"version": held.to_string()}))
    .execute(&pool)
    .await
    .unwrap();
    record(4).await;
    let kept: Vec<Value> = versions_of(&pool, a)
        .await
        .into_iter()
        .map(|v| v.1)
        .collect();
    assert_eq!(
        kept,
        [
            json!({"title": 2}),
            json!({"title": 3}),
            json!({"title": 4})
        ],
        "ranked as any other, and kept"
    );

    // `maxVersions: 0` keeps every one.
    let all = seed_unlimited(&pool).await;
    let unlimited = collections::get_by_slug(&pool, "diary").await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    for _ in 0..3 {
        tachyon_db::versions::record_version(
            &mut conn,
            all,
            &unlimited,
            DocCopy::Published,
            &Default::default(),
            None,
        )
        .await
        .unwrap();
    }
    assert_eq!(versions_of(&pool, all).await.len(), 3);
}

/// A `diary` that keeps every version, holding one entry.
async fn seed_unlimited(pool: &PgPool) -> Uuid {
    let diary: CollectionSchema = serde_json::from_value(
        json!({"slug": "diary", "versions": {"maxVersions": 0}, "fields": []}),
    )
    .unwrap();
    let diary = collections::create(pool, &diary).await.unwrap().id;
    documents::insert(pool, diary, 1, None, &Prepared::plain(Default::default()))
        .await
        .unwrap()
        .id
}
