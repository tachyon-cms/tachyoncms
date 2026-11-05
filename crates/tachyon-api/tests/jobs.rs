use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use tachyon_api::{JobsConfig, Options};
use tachyon_db::jobs::{self, NewJob};
use time::OffsetDateTime;
use uuid::Uuid;

async fn app(pool: &PgPool, workers: usize) -> axum::Router {
    let options = Options {
        jobs: JobsConfig { workers },
        ..Options::default()
    };
    tachyon_api::router_with(pool.clone(), options)
        .await
        .unwrap()
}

/// A due job of a kind no handler knows, which a worker fails for good on its first run.
async fn enqueue(pool: &PgPool) -> Uuid {
    let job = NewJob {
        kind: "nothing-handles-this",
        document_id: None,
        collection_id: None,
        payload: json!({}),
        run_at: OffsetDateTime::now_utc(),
        dedupe_key: None,
        created_by: None,
        key_id: None,
    };
    jobs::enqueue(&mut pool.acquire().await.unwrap(), &job)
        .await
        .unwrap()
}

async fn state(pool: &PgPool, id: Uuid) -> (String, i32, Option<String>) {
    sqlx::query_as("SELECT state, attempts, last_error FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn jobs_survive_restart(pool: PgPool) {
    let id = enqueue(&pool).await;
    drop(app(&pool, 0).await);

    let _app = app(&pool, 2).await;
    let mut seen = state(&pool, id).await;
    for _ in 0..100 {
        if seen.0 != "queued" && seen.0 != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        seen = state(&pool, id).await;
    }
    assert_eq!(
        seen,
        ("failed".to_owned(), 1, Some("unknown job kind".to_owned())),
        "it ran once, and an unknown kind isn't retried"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn workers_zero_disables(pool: PgPool) {
    let id = enqueue(&pool).await;
    let _app = app(&pool, 0).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(state(&pool, id).await, ("queued".to_owned(), 0, None));
}
