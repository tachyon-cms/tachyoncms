use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_core::query::ValueKind;
use tachyon_db::documents::{self, Prepared};
use tachyon_db::indexes::{IndexKind, index_name, reconcile_indexes};
use tachyon_db::{DbError, collections};

fn schema(price: Value) -> CollectionSchema {
    serde_json::from_value(json!({"slug": "posts", "fields": [
        {"name": "title", "type": "text"},
        {"name": "slug", "type": "slug", "from": "title"},
        price
    ]}))
    .unwrap()
}

fn filterable_price() -> CollectionSchema {
    schema(json!({"name": "price", "type": "number", "filterable": true}))
}

fn doc(slug: &str) -> Prepared {
    Prepared::plain(
        json!({"title": "t", "slug": slug, "price": 1})
            .as_object()
            .unwrap()
            .clone(),
    )
}

async fn index_state(pool: &PgPool, name: &str) -> Option<bool> {
    sqlx::query_scalar("SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = $1")
        .bind(name)
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unique_fields_reject_duplicates_among_live_documents(pool: PgPool) {
    let c = collections::create(&pool, &filterable_price())
        .await
        .unwrap();
    reconcile_indexes(&pool, c.id, &c.schema.0).await.unwrap();

    let first = documents::insert(&pool, c.id, 1, None, &doc("hello"))
        .await
        .unwrap();
    let err = documents::insert(&pool, c.id, 1, None, &doc("hello"))
        .await
        .unwrap_err();
    let expected = index_name(c.id, "slug", IndexKind::Unique, ValueKind::Text);
    assert!(
        matches!(err, DbError::UniqueViolation(ref name) if *name == expected),
        "{err:?}"
    );

    documents::delete(&pool, c.id, first.id).await.unwrap();
    documents::insert(&pool, c.id, 1, None, &doc("hello"))
        .await
        .unwrap();
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn filter_queries_can_use_the_index(pool: PgPool) {
    let c = collections::create(&pool, &filterable_price())
        .await
        .unwrap();
    reconcile_indexes(&pool, c.id, &c.schema.0).await.unwrap();
    let name = index_name(c.id, "price", IndexKind::Filter, ValueKind::Number);

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

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn reconcile_drops_stale_indexes_and_rebuilds_broken_ones(pool: PgPool) {
    let c = collections::create(&pool, &filterable_price())
        .await
        .unwrap();
    reconcile_indexes(&pool, c.id, &c.schema.0).await.unwrap();
    let number_ix = index_name(c.id, "price", IndexKind::Filter, ValueKind::Number);
    assert_eq!(index_state(&pool, &number_ix).await, Some(true));

    // Retyped: the old expression index goes, a new one takes its place.
    let as_text = schema(json!({"name": "price", "type": "text", "filterable": true}));
    reconcile_indexes(&pool, c.id, &as_text).await.unwrap();
    let text_ix = index_name(c.id, "price", IndexKind::Filter, ValueKind::Text);
    assert_eq!(index_state(&pool, &number_ix).await, None);
    assert_eq!(index_state(&pool, &text_ix).await, Some(true));

    // No longer filterable: dropped.
    let plain = schema(json!({"name": "price", "type": "text"}));
    reconcile_indexes(&pool, c.id, &plain).await.unwrap();
    assert_eq!(index_state(&pool, &text_ix).await, None);

    // Left invalid (as a failed CONCURRENTLY build would): rebuilt.
    reconcile_indexes(&pool, c.id, &as_text).await.unwrap();
    sqlx::query("UPDATE pg_index SET indisvalid = false WHERE indexrelid = $1::regclass")
        .bind(&text_ix)
        .execute(&pool)
        .await
        .unwrap();
    reconcile_indexes(&pool, c.id, &as_text).await.unwrap();
    assert_eq!(index_state(&pool, &text_ix).await, Some(true));
}

#[test]
fn index_names_are_short_and_stable() {
    let id = uuid::Uuid::nil();
    let long = "a_very_long_field_name_that_goes_on_and_on_and_on_for_a_while";
    let name = index_name(id, long, IndexKind::Unique, ValueKind::Text);
    assert!(name.len() <= 63, "{name}");
    assert_eq!(
        name,
        index_name(id, long, IndexKind::Unique, ValueKind::Text)
    );
    assert_ne!(
        name,
        index_name(id, "other", IndexKind::Unique, ValueKind::Text)
    );
    assert_ne!(
        name,
        index_name(id, long, IndexKind::Unique, ValueKind::Number)
    );
}
