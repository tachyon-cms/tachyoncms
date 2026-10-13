mod common;

use std::time::Duration;

use common::send;
use sqlx::PgPool;

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_second_server_sees_new_collections_without_a_restart(pool: PgPool) {
    let a = common::admin_app(pool.clone()).await;
    let b = tachyon_api::router(pool).await.unwrap();
    let body = r#"{"slug": "posts", "fields": [{"name": "title", "type": "text"}]}"#;
    assert_eq!(
        send(&a, "POST", "/api/admin/collections", Some(body))
            .await
            .status,
        201
    );

    // The creating server knows immediately; the other learns through NOTIFY.
    assert_eq!(send(&a, "GET", "/api/posts", None).await.status, 200);
    for _ in 0..50 {
        if send(&b, "GET", "/api/posts", None).await.status == 200 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("server b never saw the new collection");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_listener_reloads_after_reconnecting(pool: PgPool) {
    let app = common::admin_app(pool.clone()).await;

    // A change the listener never hears about: written without NOTIFY.
    let schema = serde_json::json!({"slug": "posts", "fields": []});
    sqlx::query(
        "INSERT INTO collections (id, tenant_id, slug, kind, schema) VALUES ($1, $2, 'posts', 'collection', $3)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tachyon_db::DEFAULT_TENANT)
    .bind(sqlx::types::Json(schema))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(send(&app, "GET", "/api/posts", None).await.status, 404);

    // Drop the listener's connection; on reconnect it must reload and see the change.
    let pid: i32 = loop {
        let found = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity WHERE datname = current_database() AND query LIKE 'LISTEN%'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        if let Some(pid) = found {
            break pid;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .execute(&pool)
        .await
        .unwrap();

    for _ in 0..100 {
        if send(&app, "GET", "/api/posts", None).await.status == 200 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the listener never reloaded after reconnecting");
}
