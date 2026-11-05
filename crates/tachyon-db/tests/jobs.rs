use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_db::documents::{self, Prepared};
use tachyon_db::jobs::{self, Job, NewJob};
use tachyon_db::{DbError, collections};
use time::OffsetDateTime;
use uuid::Uuid;

const LEASE: Duration = Duration::from_secs(60);

fn job(run_at: OffsetDateTime, key: Option<&str>) -> NewJob<'static> {
    NewJob {
        kind: "test",
        document_id: None,
        collection_id: None,
        payload: json!({}),
        run_at,
        dedupe_key: key.map(str::to_owned),
        created_by: None,
        key_id: None,
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

async fn add(pool: &PgPool, new: &NewJob<'_>) -> Uuid {
    jobs::enqueue(&mut pool.acquire().await.unwrap(), new)
        .await
        .unwrap()
}

async fn state(pool: &PgPool, id: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT state, last_error FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Ends `job`'s lease now, as if its worker had stalled past it.
async fn expire(pool: &PgPool, id: Uuid) {
    sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 second' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

/// Makes a retried job due now, instead of after its backoff.
async fn due_now(pool: &PgPool, id: Uuid) {
    sqlx::query("UPDATE jobs SET run_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn enqueue_replaces_pending_by_key(pool: PgPool) {
    let schema: CollectionSchema =
        serde_json::from_value(json!({"slug": "posts", "fields": []})).unwrap();
    let posts = collections::create(&pool, &schema).await.unwrap().id;
    let prepared = Prepared {
        data: serde_json::Map::new(),
        relations: Vec::new(),
    };
    let doc = documents::insert(&pool, posts, 1, None, &prepared)
        .await
        .unwrap()
        .id;
    let about = |run_at, key| NewJob {
        document_id: Some(doc),
        collection_id: Some(posts),
        ..job(run_at, Some(key))
    };

    let later = now() + time::Duration::hours(1);
    let first = add(&pool, &about(later, "publish:a")).await;
    let other = add(&pool, &about(later, "unpublish:a")).await;
    let second = add(&pool, &about(later, "publish:a")).await;
    assert_eq!(state(&pool, first).await.0, "cancelled");
    let pending: Vec<Uuid> = jobs::pending(&pool, doc)
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(pending, [other, second], "one pending job per key");

    // Keyless jobs never replace each other.
    let a = add(&pool, &job(later, None)).await;
    let b = add(&pool, &job(later, None)).await;
    assert_eq!(state(&pool, a).await.0, "queued");
    assert_eq!(state(&pool, b).await.0, "queued");

    // Replacing a running job cancels it, so its run can't complete.
    let running = add(&pool, &job(now(), Some("k"))).await;
    let claimed = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
    assert_eq!(claimed.id, running);
    let replacement = add(&pool, &job(later, Some("k"))).await;
    let mut tx = pool.begin().await.unwrap();
    assert!(!jobs::complete_in(&mut tx, &claimed).await.unwrap());
    tx.rollback().await.unwrap();
    assert_eq!(state(&pool, running).await.0, "cancelled");

    let mut conn = pool.acquire().await.unwrap();
    assert!(jobs::cancel(&mut conn, "k").await.unwrap());
    assert!(!jobs::cancel(&mut conn, "k").await.unwrap(), "nothing left");
    assert_eq!(state(&pool, replacement).await.0, "cancelled");

    // The enqueue and its cancel share the caller's transaction.
    let kept = add(&pool, &job(later, Some("t"))).await;
    let mut tx = pool.begin().await.unwrap();
    jobs::enqueue(&mut tx, &job(later, Some("t")))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(state(&pool, kept).await.0, "queued");

    // A job about a document must name the document's own collection.
    let wrong = NewJob {
        collection_id: Some(Uuid::now_v7()),
        ..about(later, "x")
    };
    let refused = jobs::enqueue(&mut conn, &wrong).await.unwrap_err();
    assert!(matches!(refused, DbError::Sqlx(_)), "{refused:?}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn claim_skips_locked_and_future(pool: PgPool) {
    add(&pool, &job(now() + time::Duration::minutes(5), None)).await;
    assert!(
        jobs::claim(&pool, LEASE).await.unwrap().is_none(),
        "a job in the future waits"
    );

    // A row another worker is claiming is skipped, not waited for.
    let due = add(&pool, &job(now() - time::Duration::seconds(1), None)).await;
    let mut other = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(due)
        .execute(&mut *other)
        .await
        .unwrap();
    let skipped = tokio::time::timeout(Duration::from_secs(5), jobs::claim(&pool, LEASE))
        .await
        .expect("claim waited on a locked row")
        .unwrap();
    assert!(skipped.is_none());
    other.rollback().await.unwrap();

    let claimed = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
    assert_eq!((claimed.id, claimed.attempts), (due, 1));
    assert_eq!(state(&pool, due).await.0, "running");
    assert!(
        jobs::claim(&pool, LEASE).await.unwrap().is_none(),
        "a leased job isn't claimed again"
    );

    // Concurrent claims never share a job.
    let one = add(&pool, &job(now(), None)).await;
    let two = add(&pool, &job(now(), None)).await;
    let (a, b) = tokio::join!(jobs::claim(&pool, LEASE), jobs::claim(&pool, LEASE));
    let mut got = [a.unwrap().unwrap().id, b.unwrap().unwrap().id];
    got.sort();
    assert_eq!(got, [one, two]);

    let last = add(&pool, &job(now(), None)).await;
    let claims = claims_at_once(&pool, 4).await;
    let ids: Vec<Uuid> = claims.into_iter().flatten().map(|j| j.id).collect();
    assert_eq!(ids, [last], "one claim wins, the others get nothing");
}

async fn claims_at_once(pool: &PgPool, n: usize) -> Vec<Option<Job>> {
    let mut tasks = Vec::new();
    for _ in 0..n {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move { jobs::claim(&pool, LEASE).await }));
    }
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap().unwrap());
    }
    out
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn job_runs_once_across_workers(pool: PgPool) {
    sqlx::query("CREATE TABLE effects (job uuid NOT NULL, by text NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let id = add(&pool, &job(now(), Some("publish:x"))).await;

    // Worker A claims, then stalls past its lease; worker B takes the job over.
    let a = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
    assert!(jobs::heartbeat(&pool, &a, LEASE).await.unwrap());
    expire(&pool, id).await;
    let b = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
    assert_eq!((b.id, a.attempts, b.attempts), (id, 1, 2));
    assert!(
        !jobs::heartbeat(&pool, &a, LEASE).await.unwrap(),
        "A's lease is gone"
    );

    // Both run the job's effect in a transaction that completes the job.
    let run = |worker: Job, by: &'static str| {
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("INSERT INTO effects VALUES ($1, $2)")
                .bind(worker.id)
                .bind(by)
                .execute(&mut *tx)
                .await
                .unwrap();
            if jobs::complete_in(&mut tx, &worker).await.unwrap() {
                tx.commit().await.unwrap();
                true
            } else {
                tx.rollback().await.unwrap();
                false
            }
        }
    };
    assert!(run(b, "B").await);
    assert!(!run(a.clone(), "A").await, "A can't commit after B");
    // A's failure report doesn't touch B's finished job either.
    jobs::fail(&pool, &a, "late", true).await.unwrap();

    let effects: Vec<(String,)> = sqlx::query_as("SELECT by FROM effects")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(effects, [("B".to_owned(),)], "one effect is recorded");
    assert_eq!(state(&pool, id).await, ("done".to_owned(), None));
    assert!(jobs::claim(&pool, LEASE).await.unwrap().is_none());

    // A stale run that completes first still loses to the takeover.
    let id = add(&pool, &job(now(), None)).await;
    let a = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
    expire(&pool, id).await;
    let b = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
    assert!(!run(a, "A").await);
    assert!(run(b, "B").await);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn failures_retry_then_fail(pool: PgPool) {
    let id = add(&pool, &job(now(), None)).await;
    let mut delays = Vec::new();
    for attempt in 1..=5 {
        let j = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
        assert_eq!(j.attempts, attempt);
        jobs::fail(&pool, &j, &format!("try {attempt}"), true)
            .await
            .unwrap();
        let (state, error) = state(&pool, id).await;
        assert_eq!(error.as_deref(), Some(format!("try {attempt}").as_str()));
        if attempt < 5 {
            assert_eq!(state, "queued");
            let delay: f64 = sqlx::query_scalar(
                "SELECT extract(epoch FROM run_at - now())::float8 FROM jobs WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
            delays.push(delay.round());
            assert!(
                jobs::claim(&pool, LEASE).await.unwrap().is_none(),
                "it waits out the backoff"
            );
            due_now(&pool, id).await;
        } else {
            assert_eq!(state, "failed", "out of attempts");
        }
    }
    assert_eq!(delays, [10.0, 40.0, 90.0, 160.0], "10 s × attempts²");
    assert!(jobs::claim(&pool, LEASE).await.unwrap().is_none());

    // A permanent failure doesn't retry.
    let id = add(&pool, &job(now(), None)).await;
    let j = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
    jobs::fail(&pool, &j, "user deleted", false).await.unwrap();
    assert_eq!(
        state(&pool, id).await,
        ("failed".to_owned(), Some("user deleted".to_owned()))
    );
    let finished: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT finished_at FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(finished.is_some());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_job_that_keeps_losing_its_lease_fails(pool: PgPool) {
    let id = add(&pool, &job(now(), None)).await;
    for attempt in 1..=5 {
        let j = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
        assert_eq!(j.attempts, attempt);
        expire(&pool, id).await;
    }
    assert!(jobs::claim(&pool, LEASE).await.unwrap().is_none());
    let (state, error) = state(&pool, id).await;
    assert_eq!(state, "failed");
    assert_eq!(
        error.as_deref(),
        Some("its lease ran out on the last attempt")
    );
    let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 5);
}
