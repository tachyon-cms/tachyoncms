//! Work to run later, such as a scheduled publish, taken by workers on any server.
//!
//! A worker claims a due job, which leases it until `locked_until` and bumps `attempts`.
//! A lease that runs out lets another worker claim the job again, so every write that ends
//! a run is fenced on the attempt it claimed: the run whose lease was taken over can't
//! complete, fail or extend the job, and its transaction rolls back.
//!
//! A run is one of two shapes, never both:
//! - Short and transactional, all in one transaction: first lock the domain rows the work
//!   will change (a document before its jobs, the order every write takes them), then
//!   `complete_in`, then the work. The job's row lock keeps any other worker off the job
//!   until the transaction ends. It must never `heartbeat`, which runs on another
//!   connection and would wait on that lock forever.
//! - Long: `heartbeat` at least every third of the lease, stopping when it returns false,
//!   and `complete_in` only at the end.
//!
//! Finish times use `clock_timestamp()`, the moment of the write, not the start of the
//! transaction, which for a short run is when its work began.

use std::time::Duration;

use serde_json::Value;
use sqlx::types::Json;
use sqlx::{PgConnection, PgExecutor, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{DEFAULT_TENANT, DbError, map_unique};

/// How long a finished job (done, cancelled or failed) is kept, so its outcome can be looked
/// into, before `claim` sweeps it away (Ruling F-3). A finished job holds no version.
pub const KEEP_FINISHED_DAYS: i32 = 30;

/// A job to add. `dedupe_key` names what it's for, such as `publish:<document id>`; a new
/// one replaces the pending job of the same key.
pub struct NewJob<'a> {
    pub kind: &'a str,
    pub document_id: Option<Uuid>,
    /// Required with `document_id`: the document's own collection.
    pub collection_id: Option<Uuid>,
    pub payload: Value,
    pub run_at: OffsetDateTime,
    pub dedupe_key: Option<String>,
    pub created_by: Option<Uuid>,
    pub key_id: Option<Uuid>,
}

/// A queued or running job. `attempts` counts this run, once claimed.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: Uuid,
    pub kind: String,
    pub document_id: Option<Uuid>,
    pub collection_id: Option<Uuid>,
    pub payload: Value,
    pub attempts: i32,
    pub max_attempts: i32,
    pub created_by: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub run_at: OffsetDateTime,
}

/// Adds `job`, first cancelling any pending job with its `dedupe_key`, on `conn` so a
/// caller's transaction takes both or neither. A running job cancelled this way can't
/// complete, since completing is fenced on `running`. When other callers add a job of the
/// same key at the same moment, the last one wins: a caller whose insert clashes tries
/// again, which cancels the one that got in first.
pub async fn enqueue(conn: &mut PgConnection, job: &NewJob<'_>) -> Result<Uuid, DbError> {
    // Each clash means another caller committed in between; this many is far past real use.
    const TRIES: usize = 5;
    let mut tried = 0;
    loop {
        tried += 1;
        // A savepoint (or a transaction, outside one), so a clash leaves the caller's open.
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        match enqueue_once(&mut tx, job).await {
            Err(DbError::UniqueViolation(c)) if c == "jobs_one_pending" && tried < TRIES => {
                tx.rollback().await?;
            }
            Ok(id) => {
                tx.commit().await?;
                return Ok(id);
            }
            Err(e) => return Err(e),
        }
    }
}

async fn enqueue_once(conn: &mut PgConnection, job: &NewJob<'_>) -> Result<Uuid, DbError> {
    if let Some(key) = &job.dedupe_key {
        cancel(conn, key).await?;
    }
    add(conn, job).await
}

/// Adds `job`, leaving any pending job with its `dedupe_key` in place: then it's
/// `DbError::UniqueViolation("jobs_one_pending")`, and `pending_key` names the one there.
pub async fn add(conn: &mut PgConnection, job: &NewJob<'_>) -> Result<Uuid, DbError> {
    let id = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO jobs (id, tenant_id, kind, document_id, collection_id, payload, run_at,
                             state, dedupe_key, created_by, key_id)
           VALUES ($1, $2, $3, $4, $5, $6, $7, 'queued', $8, $9, $10)"#,
        id,
        DEFAULT_TENANT,
        job.kind,
        job.document_id,
        job.collection_id,
        Json(&job.payload) as _,
        job.run_at,
        job.dedupe_key,
        job.created_by,
        job.key_id,
    )
    .execute(&mut *conn)
    .await
    .map_err(map_unique)?;
    Ok(id)
}

/// Cancels the pending (queued or running) job with `dedupe_key`. False when there was none.
pub async fn cancel(conn: &mut PgConnection, dedupe_key: &str) -> Result<bool, DbError> {
    let done = sqlx::query!(
        r#"UPDATE jobs SET state = 'cancelled', locked_until = NULL, finished_at = clock_timestamp()
           WHERE tenant_id = $1 AND dedupe_key = $2 AND state IN ('queued', 'running')"#,
        DEFAULT_TENANT,
        dedupe_key,
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Leases one job for `lease`: a queued one that's due, or a running one whose lease ran
/// out. Rows another worker holds are skipped, not waited for. A job that lost its lease
/// on its last attempt fails here instead, so one that kills its worker every time stops
/// being retried; one whose run still holds its row is left to that run.
/// Finished jobs older than `KEEP_FINISHED_DAYS` are deleted here too, a batch at a time.
pub async fn claim(pool: &PgPool, lease: Duration) -> Result<Option<Job>, DbError> {
    sqlx::query!(
        r#"UPDATE jobs SET state = 'failed', locked_until = NULL, finished_at = clock_timestamp(),
               last_error = 'its lease ran out on the last attempt'
           WHERE id IN (
               SELECT id FROM jobs
               WHERE tenant_id = $1 AND state = 'running' AND locked_until < now()
                 AND attempts >= max_attempts
               FOR UPDATE SKIP LOCKED)"#,
        DEFAULT_TENANT,
    )
    .execute(pool)
    .await?;
    sqlx::query!(
        r#"DELETE FROM jobs
           WHERE id IN (
               SELECT id FROM jobs
               WHERE tenant_id = $1 AND state IN ('done', 'cancelled', 'failed')
                 AND finished_at < now() - make_interval(days => $2)
               LIMIT 100
               FOR UPDATE SKIP LOCKED)"#,
        DEFAULT_TENANT,
        KEEP_FINISHED_DAYS,
    )
    .execute(pool)
    .await?;
    let job = sqlx::query_as!(
        Job,
        r#"UPDATE jobs SET state = 'running', attempts = attempts + 1,
               locked_until = now() + make_interval(secs => $2)
           WHERE id = (
               SELECT id FROM jobs
               WHERE tenant_id = $1
                 AND ((state = 'queued' AND run_at <= now())
                      OR (state = 'running' AND locked_until < now()
                          AND attempts < max_attempts))
               ORDER BY run_at, id
               LIMIT 1
               FOR UPDATE SKIP LOCKED)
           RETURNING id, kind, document_id, collection_id, payload, attempts, max_attempts,
                     created_by, key_id, run_at"#,
        DEFAULT_TENANT,
        lease.as_secs_f64(),
    )
    .fetch_optional(pool)
    .await?;
    Ok(job)
}

/// Extends `job`'s lease to `lease` from now, on `conn`: a pool, or a connection a long run
/// holds for it so a busy pool can't starve it. False when the lease was lost: another worker
/// took the job over, or it was cancelled. The caller should stop. Only for long runs: a
/// run that called `complete_in` holds the row, and this would wait on it forever.
pub async fn heartbeat<'e>(
    conn: impl PgExecutor<'e>,
    job: &Job,
    lease: Duration,
) -> Result<bool, DbError> {
    let done = sqlx::query!(
        r#"UPDATE jobs SET locked_until = now() + make_interval(secs => $3)
           WHERE id = $1 AND attempts = $2 AND state = 'running'"#,
        job.id,
        job.attempts,
        lease.as_secs_f64(),
    )
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Marks `job` done in the caller's transaction, which holds the job's effects. False when
/// this run no longer holds the job; the caller must roll back, which is what makes a job's
/// effects commit once even when two workers ran it. A short run calls it after locking
/// the domain rows it will change and before its work: it locks the job's row, so no other
/// worker can take the job over until the transaction ends, and taking the domain rows
/// first keeps it from deadlocking with a write that locks a document, then its jobs. Such
/// a run must never `heartbeat`. A long run calls it at the end instead.
pub async fn complete_in(tx: &mut PgConnection, job: &Job) -> Result<bool, DbError> {
    complete_with(tx, job, None).await
}

/// `complete_in`, keeping `result` for whoever waits on the job.
pub async fn complete_with(
    tx: &mut PgConnection,
    job: &Job,
    result: Option<&Value>,
) -> Result<bool, DbError> {
    let done = sqlx::query!(
        r#"UPDATE jobs SET state = 'done', locked_until = NULL, finished_at = clock_timestamp(),
               result = $3
           WHERE id = $1 AND attempts = $2 AND state = 'running'"#,
        job.id,
        job.attempts,
        result.map(Json) as _,
    )
    .execute(&mut *tx)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Records that `job`'s run failed with `error`. With `retry`, and attempts left, it's
/// queued again after 10 s × attempts²; otherwise it fails for good. Does nothing when this
/// run no longer holds the job.
pub async fn fail(pool: &PgPool, job: &Job, error: &str, retry: bool) -> Result<(), DbError> {
    fail_with(pool, job, error, retry, None).await
}

/// `fail`, keeping `result` for whoever waits on the job once it fails for good.
pub async fn fail_with(
    pool: &PgPool,
    job: &Job,
    error: &str,
    retry: bool,
    result: Option<&Value>,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE jobs SET
               state = CASE WHEN $4 AND attempts < max_attempts THEN 'queued' ELSE 'failed' END,
               run_at = CASE WHEN $4 AND attempts < max_attempts
                             THEN now() + make_interval(secs => 10 * attempts * attempts)
                             ELSE run_at END,
               finished_at = CASE WHEN $4 AND attempts < max_attempts THEN NULL ELSE clock_timestamp() END,
               locked_until = NULL,
               last_error = $3,
               result = CASE WHEN $4 AND attempts < max_attempts THEN NULL ELSE $5::jsonb END
           WHERE id = $1 AND attempts = $2 AND state = 'running'"#,
        job.id,
        job.attempts,
        error,
        retry,
        result.map(Json) as _,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// `document_id`'s queued and running jobs, soonest first.
pub async fn pending(pool: &PgPool, document_id: Uuid) -> Result<Vec<Job>, DbError> {
    let jobs = sqlx::query_as!(
        Job,
        r#"SELECT id, kind, document_id, collection_id, payload, attempts, max_attempts,
                  created_by, key_id, run_at
           FROM jobs
           WHERE tenant_id = $1 AND document_id = $2 AND state IN ('queued', 'running')
           ORDER BY run_at, id"#,
        DEFAULT_TENANT,
        document_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// How a job stands, for whoever queued it. `result` is null until it finishes.
#[derive(Debug, Clone)]
pub struct Status {
    pub id: Uuid,
    pub kind: String,
    pub state: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub run_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
    pub result: Option<Value>,
}

/// Job `id`, whatever its state. `None` when there's no such job.
pub async fn status(pool: &PgPool, id: Uuid) -> Result<Option<Status>, DbError> {
    let status = sqlx::query_as!(
        Status,
        r#"SELECT id, kind, state, attempts, max_attempts, run_at, finished_at, result
           FROM jobs WHERE tenant_id = $1 AND id = $2"#,
        DEFAULT_TENANT,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status)
}

/// The pending (queued or running) job with `dedupe_key`, if any.
pub async fn pending_key(pool: &PgPool, dedupe_key: &str) -> Result<Option<Uuid>, DbError> {
    let id = sqlx::query_scalar!(
        r#"SELECT id FROM jobs
           WHERE tenant_id = $1 AND dedupe_key = $2 AND state IN ('queued', 'running')"#,
        DEFAULT_TENANT,
        dedupe_key,
    )
    .fetch_optional(pool)
    .await?;
    Ok(id)
}
