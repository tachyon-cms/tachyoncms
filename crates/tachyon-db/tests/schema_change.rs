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
