use serde_json::json;
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_db::{DbError, collections};

fn schema(slug: &str) -> CollectionSchema {
    serde_json::from_value(
        json!({"slug": slug, "fields": [{"name": "title", "type": "text", "required": true}]}),
    )
    .unwrap()
}

#[sqlx::test]
async fn create_then_get_by_slug(pool: PgPool) {
    let created = collections::create(&pool, &schema("posts")).await.unwrap();
    let found = collections::get_by_slug(&pool, "posts").await.unwrap();
    assert_eq!(found.id, created.id);
    assert_eq!(found.schema.0, schema("posts"));
    assert_eq!(found.schema_version, 1);
}

#[sqlx::test]
async fn duplicate_slug_is_a_conflict(pool: PgPool) {
    collections::create(&pool, &schema("posts")).await.unwrap();
    let err = collections::create(&pool, &schema("posts"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DbError::Conflict(ref what) if what == "collection `posts`"),
        "{err:?}"
    );
}

#[sqlx::test]
async fn unknown_slug_is_not_found(pool: PgPool) {
    let err = collections::get_by_slug(&pool, "nope").await.unwrap_err();
    assert!(matches!(err, DbError::NotFound), "{err:?}");
}

#[sqlx::test]
async fn list_is_sorted_by_slug(pool: PgPool) {
    collections::create(&pool, &schema("posts")).await.unwrap();
    collections::create(&pool, &schema("authors"))
        .await
        .unwrap();
    let slugs: Vec<_> = collections::list(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.schema.0.slug)
        .collect();
    assert_eq!(slugs, ["authors", "posts"]);
}
