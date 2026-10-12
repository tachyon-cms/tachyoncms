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

async fn set_schema(pool: &PgPool, id: uuid::Uuid, schema: &CollectionSchema) {
    sqlx::query("UPDATE collections SET schema = $2 WHERE id = $1")
        .bind(id)
        .bind(sqlx::types::Json(schema))
        .execute(pool)
        .await
        .unwrap();
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
    reconcile_indexes(&pool, c.id).await.unwrap();

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
    reconcile_indexes(&pool, c.id).await.unwrap();
    // Either direction's index serves a range filter; the planner picks.
    let names = [IndexKind::Filter, IndexKind::FilterDesc]
        .map(|kind| index_name(c.id, "price", kind, ValueKind::Number));

    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .unwrap();
    let sql = format!(
        "EXPLAIN SELECT id FROM documents WHERE collection_id = '{}' AND deleted_at IS NULL \
         AND {} > 0",
        c.id,
        tachyon_db::query::field_expr(&["price".to_owned()], ValueKind::Number)
    );
    let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    assert!(
        plan.iter()
            .any(|line| names.iter().any(|n| line.contains(n))),
        "{plan:#?}"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn reconcile_drops_stale_indexes_and_rebuilds_broken_ones(pool: PgPool) {
    let c = collections::create(&pool, &filterable_price())
        .await
        .unwrap();
    reconcile_indexes(&pool, c.id).await.unwrap();
    let number_ix = index_name(c.id, "price", IndexKind::Filter, ValueKind::Number);
    assert_eq!(index_state(&pool, &number_ix).await, Some(true));

    // Retyped: the old expression index goes, a new one takes its place.
    let as_text = schema(json!({"name": "price", "type": "text", "filterable": true}));
    set_schema(&pool, c.id, &as_text).await;
    reconcile_indexes(&pool, c.id).await.unwrap();
    let text_ix = index_name(c.id, "price", IndexKind::Filter, ValueKind::Text);
    assert_eq!(index_state(&pool, &number_ix).await, None);
    assert_eq!(index_state(&pool, &text_ix).await, Some(true));

    // No longer filterable: dropped.
    let plain = schema(json!({"name": "price", "type": "text"}));
    set_schema(&pool, c.id, &plain).await;
    reconcile_indexes(&pool, c.id).await.unwrap();
    assert_eq!(index_state(&pool, &text_ix).await, None);

    // Left invalid (as a failed CONCURRENTLY build would): rebuilt.
    set_schema(&pool, c.id, &as_text).await;
    reconcile_indexes(&pool, c.id).await.unwrap();
    sqlx::query("UPDATE pg_index SET indisvalid = false WHERE indexrelid = $1::regclass")
        .bind(&text_ix)
        .execute(&pool)
        .await
        .unwrap();
    set_schema(&pool, c.id, &as_text).await;
    reconcile_indexes(&pool, c.id).await.unwrap();
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

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn reconcile_leaves_indexes_it_did_not_create_alone(pool: PgPool) {
    let c = collections::create(&pool, &filterable_price())
        .await
        .unwrap();
    let sql = format!(
        "CREATE INDEX custom_ix ON documents (id) WHERE collection_id = '{}'",
        c.id
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .execute(&pool)
        .await
        .unwrap();
    reconcile_indexes(&pool, c.id).await.unwrap();
    assert_eq!(index_state(&pool, "custom_ix").await, Some(true));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unique_fields_inside_layout_are_indexed(pool: PgPool) {
    let s: CollectionSchema = serde_json::from_value(json!({"slug": "pages", "fields": [
        {"type": "row", "fields": [{"name": "title", "type": "text"}, {"name": "slug", "type": "slug", "from": "title"}]}
    ]}))
    .unwrap();
    let c = collections::create(&pool, &s).await.unwrap();
    reconcile_indexes(&pool, c.id).await.unwrap();
    let name = index_name(c.id, "slug", IndexKind::Unique, ValueKind::Text);
    assert_eq!(index_state(&pool, &name).await, Some(true));
}

async fn plan(pool: &PgPool, sql: String) -> Vec<String> {
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
        .fetch_all(&mut *conn)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn sorts_both_ways_can_use_an_index(pool: PgPool) {
    let c = collections::create(&pool, &filterable_price())
        .await
        .unwrap();
    reconcile_indexes(&pool, c.id).await.unwrap();
    let price = tachyon_db::query::field_expr(&["price".to_owned()], ValueKind::Number);
    for (direction, kind) in [("ASC", IndexKind::Filter), ("DESC", IndexKind::FilterDesc)] {
        let name = index_name(c.id, "price", kind, ValueKind::Number);
        let lines = plan(
            &pool,
            format!(
                "SELECT id FROM documents WHERE collection_id = '{}' AND deleted_at IS NULL \
                 ORDER BY {price} {direction} NULLS LAST, id DESC LIMIT 10",
                c.id
            ),
        )
        .await;
        assert!(
            lines.iter().any(|line| line.contains(&name))
                && !lines.iter().any(|l| l.contains("Sort")),
            "{direction}: {lines:#?}"
        );
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unique_filterable_fields_get_sort_indexes_too(pool: PgPool) {
    let c = collections::create(
        &pool,
        &schema(json!({"name": "price", "type": "number", "unique": true, "filterable": true})),
    )
    .await
    .unwrap();
    reconcile_indexes(&pool, c.id).await.unwrap();
    for kind in [IndexKind::Unique, IndexKind::Filter, IndexKind::FilterDesc] {
        let name = index_name(c.id, "price", kind, ValueKind::Number);
        assert_eq!(index_state(&pool, &name).await, Some(true), "{kind:?}");
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_cancelled_reconcile_releases_its_lock(pool: PgPool) {
    let c = collections::create(&pool, &filterable_price())
        .await
        .unwrap();
    // An open writer makes CREATE INDEX CONCURRENTLY wait, so the reconcile is mid-build
    // (holding its advisory lock) when the caller gives up on it.
    let mut writer = pool.begin().await.unwrap();
    documents::insert(&pool, c.id, 1, None, &doc("x"))
        .await
        .unwrap();
    sqlx::query("UPDATE documents SET updated_at = now()")
        .execute(&mut *writer)
        .await
        .unwrap();
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        reconcile_indexes(&pool, c.id),
    )
    .await;
    assert!(cancelled.is_err(), "the build should still be waiting");
    writer.commit().await.unwrap();

    let key = i64::from_le_bytes(c.id.as_bytes()[..8].try_into().unwrap());
    let mut free = false;
    for _ in 0..100 {
        free = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap();
        if free {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(free, "the lock outlived the cancelled reconcile");
}
