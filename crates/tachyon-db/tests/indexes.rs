use serde_json::json;
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_db::documents::{self, Prepared};
use tachyon_db::indexes::{IndexKind, create_field_indexes, index_name};
use tachyon_db::{DbError, collections};

fn schema() -> CollectionSchema {
    serde_json::from_value(json!({"slug": "posts", "fields": [
        {"name": "title", "type": "text"},
        {"name": "slug", "type": "slug", "from": "title"},
        {"name": "price", "type": "number", "filterable": true}
    ]}))
    .unwrap()
}

fn doc(slug: &str) -> Prepared {
    Prepared::plain(
        json!({"title": "t", "slug": slug, "price": 1})
            .as_object()
            .unwrap()
            .clone(),
    )
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unique_fields_reject_duplicates_among_live_documents(pool: PgPool) {
    let c = collections::create(&pool, &schema()).await.unwrap();
    create_field_indexes(&pool, c.id, &c.schema.0)
        .await
        .unwrap();

    let first = documents::insert(&pool, c.id, 1, None, &doc("hello"))
        .await
        .unwrap();
    let err = documents::insert(&pool, c.id, 1, None, &doc("hello"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation(ref name) if *name == index_name(c.id, "slug", IndexKind::Unique)),
        "{err:?}"
    );

    // Deleted documents release their values.
    documents::delete(&pool, c.id, first.id).await.unwrap();
    documents::insert(&pool, c.id, 1, None, &doc("hello"))
        .await
        .unwrap();
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn filter_queries_can_use_the_index(pool: PgPool) {
    let c = collections::create(&pool, &schema()).await.unwrap();
    create_field_indexes(&pool, c.id, &c.schema.0)
        .await
        .unwrap();
    let name = index_name(c.id, "price", IndexKind::Filter);

    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .unwrap();
    let sql = format!(
        "EXPLAIN SELECT id FROM documents WHERE collection_id = '{}' AND deleted_at IS NULL \
         AND ((published #>> '{{price}}'))::numeric > 0",
        c.id
    );
    let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    assert!(plan.iter().any(|line| line.contains(&name)), "{plan:#?}");
}

#[test]
fn index_names_are_short_and_stable() {
    let id = uuid::Uuid::nil();
    let long = "a_very_long_field_name_that_goes_on_and_on_and_on_for_a_while";
    let name = index_name(id, long, IndexKind::Unique);
    assert!(name.len() <= 63, "{name}");
    assert_eq!(name, index_name(id, long, IndexKind::Unique));
    assert_ne!(name, index_name(id, "other", IndexKind::Unique));
}
