mod common;

use std::time::Duration;

use common::send;
use sqlx::PgPool;

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_second_server_sees_new_collections_without_a_restart(pool: PgPool) {
    let a = tachyon_api::router(pool.clone()).await.unwrap();
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
