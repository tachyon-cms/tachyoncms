//! Workers that run queued jobs, such as scheduled publishes.
//!
//! A short handler that changes the database calls `jobs::complete_in` before its work, in
//! one transaction, rolling back when `complete_in` returns false: another worker took the
//! job over, and its run is the one that counts. Rows the work locks are locked first, in
//! the order every write takes them (a document before its jobs), so a write waiting on
//! the document can't deadlock with the job's row. It must never call
//! `jobs::heartbeat`, which would wait forever on the row `complete_in` locked.
//! A long handler calls `jobs::heartbeat` at least every third of `LEASE`, stops when that
//! returns false, and calls `jobs::complete_in` only at the end. `keep_leased` does the
//! heartbeats, on a connection of its own; drop what it returns before `complete_in`.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tachyon_db::jobs::{self, Job};
use uuid::Uuid;

use crate::AppState;

/// How long a claimed job is a worker's before another may take it over.
pub(crate) const LEASE: Duration = Duration::from_secs(60);
/// How long an idle worker waits before looking for due jobs again.
const POLL: Duration = Duration::from_secs(1);

/// How many workers this server runs. The default runs none; the server binary asks for
/// two.
#[derive(Debug, Clone, Default)]
pub struct JobsConfig {
    /// 0 runs none, leaving the jobs to other servers.
    pub workers: usize,
    /// Where the workers note what they're running. Keep a clone to read it.
    pub running: RunningJobs,
}

/// The jobs some workers are in the middle of, such as to name the ones a shutdown cut
/// off. Clones share one set. Each run is noted by its job and attempt, so a run that
/// ends after this server took its job over doesn't hide the run that took it.
#[derive(Debug, Clone, Default)]
pub struct RunningJobs(Arc<Mutex<BTreeSet<(Uuid, i32)>>>);

impl RunningJobs {
    pub fn ids(&self) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = self.lock().iter().map(|(id, _)| *id).collect();
        ids.dedup();
        ids
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<(Uuid, i32)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes `job`'s run until the returned guard drops.
    fn start(&self, job: &Job) -> impl Drop + Send + 'static {
        struct Started(RunningJobs, (Uuid, i32));
        impl Drop for Started {
            fn drop(&mut self) {
                self.0.lock().remove(&self.1);
            }
        }
        let run = (job.id, job.attempts);
        self.lock().insert(run);
        Started(self.clone(), run)
    }
}

/// Why a run failed. A `Retry` runs again later, until the job is out of attempts.
#[derive(Debug)]
pub(crate) enum JobError {
    Retry(String),
    Permanent(String),
    /// Permanent, with the answer to keep for whoever waits on the job.
    Failed {
        error: String,
        result: Value,
    },
}

/// A long handler's lease, kept until this drops (see `keep_leased`).
pub(crate) struct Leased {
    beating: tokio::task::JoinHandle<()>,
    lost: Arc<AtomicBool>,
}

impl Leased {
    /// A heartbeat found the lease gone: another worker took the job over, or it was
    /// cancelled. The handler should stop, as its run can no longer complete.
    pub(crate) fn lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }
}

impl Drop for Leased {
    fn drop(&mut self) {
        self.beating.abort();
    }
}

/// Keeps `job`'s lease for a long handler: a heartbeat now, then every third of `lease`
/// until the returned guard drops or one finds the lease gone. All of them run on one
/// connection taken first, so a busy pool can't starve them past the lease. Without a
/// connection to spare it's a `Retry`, before any work.
pub(crate) async fn keep_leased(
    state: &AppState,
    job: &Job,
    lease: Duration,
) -> Result<Leased, JobError> {
    let mut conn =
        state.pool.acquire().await.map_err(|e| {
            JobError::Retry(format!("no connection to keep the job's lease on: {e}"))
        })?;
    let lost = Arc::new(AtomicBool::new(false));
    // The first one here, so a run that already lost the job stops before its work.
    match jobs::heartbeat(&mut *conn, job, lease).await {
        Ok(held) => lost.store(!held, Ordering::Relaxed),
        Err(e) => return Err(JobError::Retry(e.to_string())),
    }
    let (flag, job, state) = (lost.clone(), job.clone(), state.clone());
    let beating = tokio::spawn(async move {
        let every = lease / 3;
        let mut beats = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        beats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while !flag.load(Ordering::Relaxed) {
            beats.tick().await;
            match jobs::heartbeat(&mut *conn, &job, lease).await {
                Ok(held) => flag.store(!held, Ordering::Relaxed),
                // The next one may get through before the lease runs out, on a fresh
                // connection if this one has died.
                Err(error) => {
                    tracing::warn!(job = %job.id, %error, "couldn't extend a job's lease");
                    if let Ok(fresh) = state.pool.acquire().await {
                        conn.close_on_drop();
                        conn = fresh;
                    }
                }
            }
        }
    });
    Ok(Leased { beating, lost })
}

/// Starts `config.workers` workers, each its own task, until the pool closes.
pub fn spawn_workers(state: AppState, config: &JobsConfig) {
    for _ in 0..config.workers {
        tokio::spawn(work(state.clone(), config.running.clone()));
    }
}

async fn work(state: AppState, running: RunningJobs) {
    let pool = &state.pool;
    while !pool.is_closed() {
        match jobs::claim(pool, LEASE).await {
            Ok(Some(job)) => run(&state, &running, job).await,
            Ok(None) => idle(&state).await,
            Err(error) => {
                if pool.is_closed() {
                    break;
                }
                tracing::warn!(%error, "couldn't claim a job");
                idle(&state).await;
            }
        }
    }
}

async fn idle(state: &AppState) {
    tokio::select! {
        () = tokio::time::sleep(POLL) => {}
        () = state.pool.close_event() => {}
    }
}

/// Runs `job` in a task of its own, so a handler that panics fails its run, not the worker.
/// The task holds the note in `running`: dropping this future leaves the handler running,
/// and it stays listed until it ends.
async fn run(state: &AppState, running: &RunningJobs, job: Job) {
    let started = running.start(&job);
    let outcome = {
        let (state, job) = (state.clone(), job.clone());
        tokio::spawn(async move {
            let _running = started;
            dispatch(&state, &job).await
        })
        .await
    };
    let (error, retry, result) = match outcome {
        Ok(Ok(())) => return,
        Ok(Err(JobError::Retry(e))) => (e, true, None),
        Ok(Err(JobError::Permanent(e))) => (e, false, None),
        Ok(Err(JobError::Failed { error, result })) => (error, false, Some(result)),
        Err(_) => ("the job's handler panicked".to_owned(), true, None),
    };
    tracing::warn!(job = %job.id, kind = %job.kind, attempt = job.attempts, %error, retry, "job failed");
    if let Err(e) = jobs::fail_with(&state.pool, &job, &error, retry, result.as_ref()).await {
        // Its lease runs out, and the job is claimed again.
        tracing::warn!(job = %job.id, error = %e, "couldn't record a job's failure");
    }
}

async fn dispatch(state: &AppState, job: &Job) -> Result<(), JobError> {
    match job.kind.as_str() {
        "publish" | "unpublish" => crate::schedule::run(state, job).await,
        crate::collections::SCHEMA_CHANGE | crate::collections::SCHEMA_PREVIEW => {
            crate::collections::run_job(state, job).await
        }
        #[cfg(test)]
        "test-panic" => panic!("a handler panicked"),
        #[cfg(test)]
        "test-hang" => std::future::pending().await,
        _ => Err(JobError::Permanent("unknown job kind".into())),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use tachyon_db::jobs::NewJob;
    use time::OffsetDateTime;

    use super::*;
    use crate::Options;

    async fn add(pool: &PgPool, kind: &str, run_at: OffsetDateTime) -> Uuid {
        let job = NewJob {
            kind,
            document_id: None,
            collection_id: None,
            payload: json!({}),
            run_at,
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
    async fn a_panicking_handler_is_retried_and_the_worker_goes_on(pool: PgPool) {
        let now = OffsetDateTime::now_utc();
        let panics = add(&pool, "test-panic", now - time::Duration::minutes(1)).await;
        let next = add(&pool, "nothing-handles-this", now).await;
        let running = RunningJobs::default();
        let options = Options {
            jobs: JobsConfig {
                workers: 1,
                running: running.clone(),
            },
            ..Options::default()
        };
        let _app = crate::router_with(pool.clone(), options).await.unwrap();
        for _ in 0..100 {
            if state(&pool, next).await.0 == "failed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            state(&pool, next).await.0,
            "failed",
            "the one worker ran the next job"
        );
        assert_eq!(
            state(&pool, panics).await,
            (
                "queued".to_owned(),
                1,
                Some("the job's handler panicked".to_owned())
            ),
            "queued again for a retry"
        );
        assert!(!running.ids().contains(&panics));
    }

    #[test]
    fn a_run_taken_over_in_this_process_leaves_the_new_one_listed() {
        let running = RunningJobs::default();
        let mut job = Job {
            id: Uuid::now_v7(),
            kind: "test".into(),
            document_id: None,
            collection_id: None,
            payload: json!({}),
            attempts: 1,
            max_attempts: 5,
            created_by: None,
            key_id: None,
            run_at: OffsetDateTime::now_utc(),
        };
        let first = running.start(&job);
        job.attempts = 2;
        let second = running.start(&job);
        assert_eq!(running.ids(), [job.id]);
        drop(first);
        assert_eq!(running.ids(), [job.id], "the takeover is still running");
        drop(second);
        assert!(running.ids().is_empty());
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_long_run_keeps_its_lease_until_it_lets_go(pool: PgPool) {
        let lease = Duration::from_millis(600);
        add(&pool, "test", OffsetDateTime::now_utc()).await;
        let state = AppState {
            pool: pool.clone(),
            schemas: std::sync::Arc::new(crate::schemas::Schemas::load(&pool).await.unwrap()),
            auth: std::sync::Arc::new(crate::auth::AuthState::new(Default::default()).unwrap()),
            inline_rows: crate::collections::INLINE_ROWS,
        };
        let job = jobs::claim(&pool, lease).await.unwrap().unwrap();
        let held = keep_leased(&state, &job, lease).await.unwrap();
        tokio::time::sleep(lease * 3).await;
        assert!(
            jobs::claim(&pool, lease).await.unwrap().is_none(),
            "still leased three leases on"
        );
        drop(held);
        tokio::time::sleep(lease * 2).await;
        let taken = jobs::claim(&pool, lease).await.unwrap().unwrap();
        assert_eq!(
            (taken.id, taken.attempts),
            (job.id, 2),
            "taken over once let go"
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_lease_is_kept_while_the_pool_is_full(
        opts: PgPoolOptions,
        connect: PgConnectOptions,
    ) {
        let lease = Duration::from_millis(600);
        let pool = opts
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(connect.clone())
            .await
            .unwrap();
        // Another server's workers, which the full pool doesn't hold up.
        let other = PgPool::connect_with(connect).await.unwrap();
        add(&pool, "test", OffsetDateTime::now_utc()).await;
        let state = AppState {
            pool: pool.clone(),
            schemas: std::sync::Arc::new(crate::schemas::Schemas::load(&pool).await.unwrap()),
            auth: std::sync::Arc::new(crate::auth::AuthState::new(Default::default()).unwrap()),
            inline_rows: crate::collections::INLINE_ROWS,
        };
        let job = jobs::claim(&pool, lease).await.unwrap().unwrap();
        let _held = keep_leased(&state, &job, lease).await.unwrap();
        let mut busy = Vec::new();
        while let Ok(Ok(conn)) =
            tokio::time::timeout(Duration::from_millis(200), pool.acquire()).await
        {
            busy.push(conn);
        }
        assert_eq!(busy.len(), 1, "the heartbeats hold the other connection");
        tokio::time::sleep(lease * 3).await;
        assert!(
            jobs::claim(&other, lease).await.unwrap().is_none(),
            "still leased with the pool full"
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_lease_is_kept_when_its_connection_dies(
        opts: PgPoolOptions,
        connect: PgConnectOptions,
    ) {
        let lease = Duration::from_millis(600);
        let pool = opts
            .max_connections(2)
            .connect_with(connect.clone())
            .await
            .unwrap();
        let other = PgPool::connect_with(connect).await.unwrap();
        add(&pool, "test", OffsetDateTime::now_utc()).await;
        let state = AppState {
            pool: pool.clone(),
            schemas: std::sync::Arc::new(crate::schemas::Schemas::load(&pool).await.unwrap()),
            auth: std::sync::Arc::new(crate::auth::AuthState::new(Default::default()).unwrap()),
            inline_rows: crate::collections::INLINE_ROWS,
        };
        let job = jobs::claim(&pool, lease).await.unwrap().unwrap();
        let _held = keep_leased(&state, &job, lease).await.unwrap();
        // Ends every connection but this one, the heartbeats' included.
        let mut killer = other.acquire().await.unwrap();
        sqlx::query(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
             WHERE datname = current_database() AND pid <> pg_backend_pid()",
        )
        .execute(&mut *killer)
        .await
        .unwrap();
        drop(killer);
        tokio::time::sleep(lease * 3).await;
        assert!(
            jobs::claim(&other, lease).await.unwrap().is_none(),
            "still leased after the heartbeats' connection died"
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_dropped_run_leaves_its_handler_listed(pool: PgPool) {
        let id = add(&pool, "test-hang", OffsetDateTime::now_utc()).await;
        let state = AppState {
            pool: pool.clone(),
            schemas: std::sync::Arc::new(crate::schemas::Schemas::load(&pool).await.unwrap()),
            auth: std::sync::Arc::new(crate::auth::AuthState::new(Default::default()).unwrap()),
            inline_rows: crate::collections::INLINE_ROWS,
        };
        let job = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
        let running = RunningJobs::default();
        let task = {
            let running = running.clone();
            tokio::spawn(async move { run(&state, &running, job).await })
        };
        for _ in 0..100 {
            if running.ids().contains(&id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(running.ids(), [id]);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(running.ids(), [id], "its handler is still running");
    }
}
