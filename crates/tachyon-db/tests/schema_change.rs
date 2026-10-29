use std::time::Duration;

use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_core::diff::Renames;
use tachyon_db::collections;
use tachyon_db::documents::{self, Prepared, Relation};
use tachyon_db::schema_change::{ChangeError, apply, preview};
use uuid::Uuid;

fn schema(v: Value) -> CollectionSchema {
    serde_json::from_value(v).unwrap()
}

fn posts(fields: Value) -> CollectionSchema {
    schema(json!({"slug": "posts", "fields": fields}))
}

struct Seed {
    posts: Uuid,
    post: Uuid,
    ann: Uuid,
}

async fn seed(pool: &PgPool) -> Seed {
    let authors = collections::create(
        pool,
        &schema(json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]})),
    )
    .await
    .unwrap()
    .id;
    let posts_id = collections::create(
        pool,
        &posts(json!([
            {"name": "title", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "summary", "type": "textarea"}
        ])),
    )
    .await
    .unwrap()
    .id;
    let ann = documents::insert(
        pool,
        authors,
        1,
        None,
        &Prepared::plain(json!({"name": "Ann"}).as_object().unwrap().clone()),
    )
    .await
    .unwrap()
    .id;
    let data = json!({"title": "Hello", "author": ann.to_string(), "summary": "S"})
        .as_object()
        .unwrap()
        .clone();
    let relations = vec![Relation {
        path: "author".into(),
        at: "author".into(),
        collection_id: authors,
        id: ann,
    }];
    let post = documents::insert(pool, posts_id, 1, None, &Prepared { data, relations })
        .await
        .unwrap()
        .id;
    Seed {
        posts: posts_id,
        post,
        ann,
    }
}

fn renamed() -> Renames {
    [
        ("title".to_owned(), "headline".to_owned()),
        ("author".to_owned(), "writer".to_owned()),
    ]
    .into_iter()
    .collect()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn preview_reports_without_touching_anything(pool: PgPool) {
    let s = seed(&pool).await;
    let new = posts(json!([
        {"name": "headline", "type": "text"},
        {"name": "writer", "type": "relationship", "relationTo": "authors"},
        {"name": "summary", "type": "textarea"},
        {"name": "rating", "type": "number", "required": true}
    ]));
    let p = preview(&pool, "posts", &new, &renamed()).await.unwrap();
    assert_eq!((p.documents, p.invalid), (1, 1));
    assert_eq!(p.examples[0].id, s.post);
    assert_eq!(p.examples[0].errors[0].path, "rating");
    assert!(p.needs_confirmation);
    assert_eq!(p.diff.changes.len(), 3);

    let doc = documents::get(&pool, s.posts, s.post).await.unwrap();
    assert_eq!(doc.data.0["title"], "Hello");
    assert_eq!(
        collections::get_by_slug(&pool, "posts")
            .await
            .unwrap()
            .schema_version,
        1
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn apply_carries_data_and_links_across(pool: PgPool) {
    let s = seed(&pool).await;
    let new = posts(json!([
        {"name": "headline", "type": "text"},
        {"name": "writer", "type": "relationship", "relationTo": "authors"},
        {"name": "summary", "type": "textarea"}
    ]));
    let (c, p) = apply(&pool, "posts", &new, &renamed(), false)
        .await
        .unwrap();
    assert!(!p.needs_confirmation);
    assert_eq!(c.schema_version, 2);
    assert_eq!(c.schema.0, new);

    let doc = documents::get(&pool, s.posts, s.post).await.unwrap();
    assert_eq!(
        Value::Object(doc.data.0),
        json!({"headline": "Hello", "writer": s.ann.to_string(), "summary": "S"})
    );
    let refs = documents::references(&pool, s.ann).await.unwrap();
    assert_eq!(refs[0].path, "writer");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn destructive_changes_wait_for_confirmation(pool: PgPool) {
    let s = seed(&pool).await;
    let new = posts(json!([
        {"name": "title", "type": "text"},
        {"name": "author", "type": "relationship", "relationTo": "authors"}
    ]));
    let Err(ChangeError::NeedsConfirmation(p)) =
        apply(&pool, "posts", &new, &Renames::new(), false).await
    else {
        panic!("removing a field should need confirmation")
    };
    assert!(p.diff.is_destructive());
    assert_eq!(
        documents::get(&pool, s.posts, s.post).await.unwrap().data.0["summary"],
        "S"
    );

    apply(&pool, "posts", &new, &Renames::new(), true)
        .await
        .unwrap();
    assert!(
        documents::get(&pool, s.posts, s.post)
            .await
            .unwrap()
            .data
            .0
            .get("summary")
            .is_none()
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unique_over_duplicates_is_refused_even_when_confirmed(pool: PgPool) {
    let s = seed(&pool).await;
    let again = json!({"title": "Hello"}).as_object().unwrap().clone();
    documents::insert(&pool, s.posts, 1, None, &Prepared::plain(again))
        .await
        .unwrap();
    let new = posts(json!([
        {"name": "title", "type": "text", "unique": true},
        {"name": "author", "type": "relationship", "relationTo": "authors"},
        {"name": "summary", "type": "textarea"}
    ]));
    let Err(ChangeError::Duplicates(p)) = apply(&pool, "posts", &new, &Renames::new(), true).await
    else {
        panic!("duplicates should block a unique field")
    };
    assert_eq!(p.duplicates, ["title"]);
    assert_eq!(
        collections::get_by_slug(&pool, "posts")
            .await
            .unwrap()
            .schema_version,
        1
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn swapped_renames_keep_both_values(pool: PgPool) {
    let s = seed(&pool).await;
    let same = posts(json!([
        {"name": "title", "type": "text"},
        {"name": "author", "type": "relationship", "relationTo": "authors"},
        {"name": "summary", "type": "textarea"}
    ]));
    let swap: Renames = [
        ("title".to_owned(), "summary".to_owned()),
        ("summary".to_owned(), "title".to_owned()),
    ]
    .into_iter()
    .collect();
    apply(&pool, "posts", &same, &swap, true).await.unwrap();
    let doc = documents::get(&pool, s.posts, s.post).await.unwrap();
    assert_eq!(
        (doc.data.0["title"].as_str(), doc.data.0["summary"].as_str()),
        (Some("S"), Some("Hello"))
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn equal_numbers_count_as_duplicates(pool: PgPool) {
    let items = collections::create(
        &pool,
        &schema(json!({"slug": "items", "fields": [{"name": "n", "type": "number"}]})),
    )
    .await
    .unwrap()
    .id;
    for n in [json!(1), json!(1.0)] {
        let data = json!({"n": n}).as_object().unwrap().clone();
        documents::insert(&pool, items, 1, None, &Prepared::plain(data))
            .await
            .unwrap();
    }
    let unique = schema(
        json!({"slug": "items", "fields": [{"name": "n", "type": "number", "unique": true}]}),
    );
    let Err(ChangeError::Duplicates(p)) =
        apply(&pool, "items", &unique, &Renames::new(), true).await
    else {
        panic!("1 and 1.0 are the same number")
    };
    assert_eq!(p.duplicates, ["n"]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn retargeting_a_link_clears_it_after_confirmation(pool: PgPool) {
    let s = seed(&pool).await;
    collections::create(
        &pool,
        &schema(json!({"slug": "tags", "fields": [{"name": "name", "type": "text"}]})),
    )
    .await
    .unwrap();
    let new = posts(json!([
        {"name": "title", "type": "text"},
        {"name": "author", "type": "relationship", "relationTo": "tags"},
        {"name": "summary", "type": "textarea"}
    ]));
    let p = preview(&pool, "posts", &new, &Renames::new())
        .await
        .unwrap();
    assert_eq!(p.cleared, 1, "the preview must say which links go");
    assert!(p.needs_confirmation);
    apply(&pool, "posts", &new, &Renames::new(), true)
        .await
        .unwrap();
    let doc = documents::get(&pool, s.posts, s.post).await.unwrap();
    assert!(doc.data.0.get("author").is_none());
    assert!(
        documents::references(&pool, s.ann)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn previews_never_wait_for_writers(pool: PgPool) {
    let s = seed(&pool).await;
    // A save in progress holds the collection row the way check_schema does.
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM collections WHERE id = $1 FOR KEY SHARE")
        .bind(s.posts)
        .execute(&mut *writer)
        .await
        .unwrap();
    let same = posts(json!([
        {"name": "title", "type": "text"},
        {"name": "author", "type": "relationship", "relationTo": "authors"},
        {"name": "summary", "type": "textarea"}
    ]));
    tokio::time::timeout(
        Duration::from_secs(3),
        preview(&pool, "posts", &same, &Renames::new()),
    )
    .await
    .expect("preview waited for a lock")
    .unwrap();
    writer.rollback().await.unwrap();
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn one_target_to_several_wraps_existing_links(pool: PgPool) {
    let s = seed(&pool).await;
    let new = posts(json!([
        {"name": "title", "type": "text"},
        {"name": "author", "type": "relationship", "relationTo": ["authors", "posts"]},
        {"name": "summary", "type": "textarea"}
    ]));
    let (_, p) = apply(&pool, "posts", &new, &Renames::new(), false)
        .await
        .unwrap();
    assert!(!p.needs_confirmation, "wrapping loses nothing");
    let doc = documents::get(&pool, s.posts, s.post).await.unwrap();
    assert_eq!(
        doc.data.0["author"],
        json!({"relationTo": "authors", "value": s.ann.to_string()})
    );
    assert_eq!(
        documents::references(&pool, s.ann).await.unwrap()[0].path,
        "author",
        "the link stays protected"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn turning_a_link_into_a_list_keeps_it_protected(pool: PgPool) {
    let s = seed(&pool).await;
    let new = posts(json!([
        {"name": "title", "type": "text"},
        {"name": "author", "type": "relationship", "relationTo": "authors", "hasMany": true},
        {"name": "summary", "type": "textarea"}
    ]));
    let (_, p) = apply(&pool, "posts", &new, &Renames::new(), false)
        .await
        .unwrap();
    assert!(!p.needs_confirmation && p.invalid == 0, "{p:?}");
    let doc = documents::get(&pool, s.posts, s.post).await.unwrap();
    assert_eq!(doc.data.0["author"], json!([s.ann.to_string()]));
    assert_eq!(
        documents::references(&pool, s.ann).await.unwrap()[0].path,
        "author.0"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn renaming_onto_a_removed_field_moves_the_data(pool: PgPool) {
    let s = seed(&pool).await;
    let new = posts(json!([
        {"name": "summary", "type": "text"},
        {"name": "author", "type": "relationship", "relationTo": "authors"}
    ]));
    let r: Renames = [("title".to_owned(), "summary".to_owned())]
        .into_iter()
        .collect();
    let p = preview(&pool, "posts", &new, &r).await.unwrap();
    assert!(p.needs_confirmation, "the old summary's data goes");
    apply(&pool, "posts", &new, &r, true).await.unwrap();
    let doc = documents::get(&pool, s.posts, s.post).await.unwrap();
    assert_eq!(doc.data.0["summary"], "Hello");
    assert!(!doc.data.0.contains_key("title"));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn previews_list_index_changes(pool: PgPool) {
    seed(&pool).await;
    let indexed = posts(json!([
        {"name": "title", "type": "text", "unique": true, "filterable": true},
        {"name": "author", "type": "relationship", "relationTo": "authors"},
        {"name": "summary", "type": "textarea"}
    ]));
    let p = preview(&pool, "posts", &indexed, &Renames::new())
        .await
        .unwrap();
    assert_eq!(
        p.indexes.create,
        ["title (unique)", "title (filter)", "title (filter desc)"]
    );
    assert!(p.indexes.drop.is_empty());

    let (c, _) = apply(&pool, "posts", &indexed, &Renames::new(), false)
        .await
        .unwrap();
    tachyon_db::indexes::reconcile_indexes(&pool, c.id)
        .await
        .unwrap();
    let plain = posts(json!([
        {"name": "title", "type": "text", "filterable": true},
        {"name": "author", "type": "relationship", "relationTo": "authors"},
        {"name": "summary", "type": "textarea"}
    ]));
    let p = preview(&pool, "posts", &plain, &Renames::new())
        .await
        .unwrap();
    assert!(p.indexes.create.is_empty());
    assert_eq!(p.indexes.drop, ["title (unique)"]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn changing_default_populate_is_applied(pool: PgPool) {
    seed(&pool).await;
    let mut authors = schema(json!({"slug": "authors", "defaultPopulate": {"name": true},
                                     "fields": [{"name": "name", "type": "text"}]}));
    let (c, p) = apply(&pool, "authors", &authors, &Renames::new(), false)
        .await
        .unwrap();
    assert!(!p.diff.is_empty());
    assert_eq!(c.schema.0.default_populate, authors.default_populate.take());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn removing_a_link_field_drops_its_links(pool: PgPool) {
    let s = seed(&pool).await;
    let new = posts(json!([
        {"name": "title", "type": "text"},
        {"name": "summary", "type": "textarea"}
    ]));
    apply(&pool, "posts", &new, &Renames::new(), true)
        .await
        .unwrap();
    assert!(
        documents::references(&pool, s.ann)
            .await
            .unwrap()
            .is_empty()
    );
    // Nothing is left to block deleting what it linked to.
    let authors = collections::get_by_slug(&pool, "authors").await.unwrap();
    documents::delete(&pool, authors.id, s.ann).await.unwrap();
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_change_rechecks_every_schema_under_the_lock(pool: PgPool) {
    // As if every check before the save read a stale cache: `apply` must still refuse a
    // shared block change that leaves a join pointing at a field that's gone.
    let schema = |v: serde_json::Value| -> CollectionSchema { serde_json::from_value(v).unwrap() };
    let byline = |fields: serde_json::Value| {
        schema(json!({"slug": "byline", "kind": "block", "fields": fields}))
    };
    collections::create(&pool, &schema(json!({"slug": "authors", "fields": []})))
        .await
        .unwrap();
    collections::create(
        &pool,
        &byline(json!([{"name": "who", "type": "relationship", "relationTo": "authors"}])),
    )
    .await
    .unwrap();
    collections::create(
        &pool,
        &schema(json!({"slug": "stories", "fields": [
            {"name": "layout", "type": "blocks", "blocks": [{"blockRef": "byline"}]}
        ]})),
    )
    .await
    .unwrap();
    let authors = schema(json!({"slug": "authors", "fields": [
        {"name": "stories", "type": "join", "collection": "stories", "on": "layout.who"}
    ]}));
    apply(&pool, "authors", &authors, &Renames::new(), false)
        .await
        .unwrap();

    let gone = byline(json!([{"name": "name", "type": "text"}]));
    let Err(ChangeError::Invalid(e)) = apply(&pool, "byline", &gone, &Renames::new(), true).await
    else {
        panic!("the change went in");
    };
    assert_eq!(
        e.to_string(),
        "this change would break authors: field `stories`: stories has no field `layout.who`"
    );
}

fn block(slug: &str, next: Option<&str>) -> CollectionSchema {
    let fields = match next {
        Some(n) => json!([{"name": "l", "type": "blocks", "blocks": [{"blockRef": n}]}]),
        None => json!([{"name": "t", "type": "text"}]),
    };
    serde_json::from_value(json!({"slug": slug, "kind": "block", "fields": fields})).unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn creates_get_every_check_under_the_lock(pool: PgPool) {
    use tachyon_db::schema_change::create;
    // b7 … b0, each through a save: b0's chain is eight deep, as deep as one goes.
    for i in (0..8).rev() {
        let next = (i < 7).then(|| format!("b{}", i + 1));
        create(&pool, &block(&format!("b{i}"), next.as_deref()))
            .await
            .unwrap();
    }
    // A ninth on top is refused, though every name it uses exists: whatever a cache said.
    let Err(ChangeError::Invalid(e)) = create(&pool, &block("x", Some("b0"))).await else {
        panic!("x went in");
    };
    assert!(
        e.to_string()
            .contains("shared blocks can nest at most 8 deep: x → b0"),
        "{e}"
    );
    // Nothing was stored, so other saves still work.
    let mut b7 = block("b7", None);
    b7.fields
        .extend(block("b7", None).fields.into_iter().map(|mut f| {
            f.name = "u".into();
            f
        }));
    apply(&pool, "b7", &b7, &Renames::new(), false)
        .await
        .unwrap();

    // A rule reading a block field that a save just dropped: refused on create too.
    let schema = |v: Value| -> CollectionSchema { serde_json::from_value(v).unwrap() };
    let byline =
        |fields: Value| schema(json!({"slug": "byline", "kind": "block", "fields": fields}));
    create(&pool, &byline(json!([{"name": "heading", "type": "text"}])))
        .await
        .unwrap();
    create(
        &pool,
        &schema(json!({"slug": "stories", "fields": [
            {"name": "layout", "type": "blocks", "blocks": [{"blockRef": "byline"}]}
        ]})),
    )
    .await
    .unwrap();
    let notes = schema(json!({"slug": "notes",
        "access": {"read": {"story.layout.heading": {"equals": "x"}}},
        "fields": [{"name": "story", "type": "relationship", "relationTo": "stories"}]}));
    // Checked against the block as it was, as a stale cache would: it fits.
    let fits = |s: &CollectionSchema| {
        let all = [byline(json!([{"name": "heading", "type": "text"}]))];
        s.expand(|slug: &str| all.iter().find(|b| b.slug == slug))
            .is_ok()
    };
    assert!(fits(&notes));
    apply(
        &pool,
        "byline",
        &byline(json!([{"name": "t", "type": "text"}])),
        &Renames::new(),
        true,
    )
    .await
    .unwrap();
    let Err(ChangeError::Invalid(e)) = create(&pool, &notes).await else {
        panic!("notes went in");
    };
    assert!(e.to_string().starts_with("access.read: "), "{e}");
    assert!(collections::get_by_slug(&pool, "notes").await.is_err());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_schema_reached_by_two_saved_blocks_is_listed_under_both(pool: PgPool) {
    use tachyon_db::schema_change::{Save, create, preview_all};
    let cta = block("cta", None);
    let section = block("section", Some("cta"));
    create(&pool, &cta).await.unwrap();
    create(&pool, &section).await.unwrap();
    let landing: CollectionSchema = serde_json::from_value(json!({"slug": "landing", "fields": [
        {"name": "layout", "type": "blocks", "blocks": [{"blockRef": "section"}]}
    ]}))
    .unwrap();
    create(&pool, &landing).await.unwrap();

    let mut cta2 = block("cta", None);
    cta2.fields[0].name = "u".into();
    let mut section2 = block("section", Some("cta"));
    section2.fields.push(block("x", None).fields.remove(0));
    let none = Renames::new();
    let saves = [
        Save {
            slug: "section",
            schema: &section2,
            renames: &none,
            new: false,
        },
        Save {
            slug: "cta",
            schema: &cta2,
            renames: &none,
            new: false,
        },
    ];
    let out = preview_all(&pool, &saves).await.unwrap();
    for o in &out {
        let landing = &o.preview.collections["landing"];
        assert_eq!(landing.diff.changes.len(), 3, "{:?}", landing.diff);
    }
}
