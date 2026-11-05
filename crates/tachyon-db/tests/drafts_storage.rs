use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_core::diff::Renames;
use tachyon_db::documents::{self, Prepared, Relation};
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
    let refs = documents::references(&pool, s.ann).await.unwrap();
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
