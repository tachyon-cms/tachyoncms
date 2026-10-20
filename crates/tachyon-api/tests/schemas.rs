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

    // A change the listener never hears about: written without NOTIFY. Its startup reload
    // can still land after the insert and pick it up, so try fresh slugs until one stays
    // unseen; once that reload has run, nothing else reloads.
    let mut unseen = None;
    for n in 0..50 {
        let slug = format!("posts{n}");
        let schema = serde_json::json!({"slug": slug, "fields": []});
        sqlx::query(
            "INSERT INTO collections (id, tenant_id, slug, kind, schema) VALUES ($1, $2, $3, 'collection', $4)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(tachyon_db::DEFAULT_TENANT)
        .bind(&slug)
        .bind(sqlx::types::Json(schema))
        .execute(&pool)
        .await
        .unwrap();
        if send(&app, "GET", &format!("/api/{slug}"), None)
            .await
            .status
            == 404
        {
            unseen = Some(slug);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let slug = unseen.expect("the startup reload never settled");

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
        if send(&app, "GET", &format!("/api/{slug}"), None)
            .await
            .status
            == 200
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the listener never reloaded after reconnecting");
}
