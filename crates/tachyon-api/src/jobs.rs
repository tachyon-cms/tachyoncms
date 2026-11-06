//! Workers that run queued jobs, such as scheduled publishes.
//!
//! A short handler that changes the database calls `jobs::complete_in` before its work, in
//! one transaction, rolling back when `complete_in` returns false: another worker took the
//! job over, and its run is the one that counts. Rows the work locks are locked first, in
//! the order every write takes them (a document before its jobs), so a write waiting on
//! the document can't deadlock with the job's row. It must never call
//! `jobs::heartbeat`, which would wait forever on the row `complete_in` locked.
//! A long handler calls `jobs::heartbeat` at least every third of `LEASE`, stops when that
//! returns false, and calls `jobs::complete_in` only at the end.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

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
    let (error, retry) = match outcome {
        Ok(Ok(())) => return,
        Ok(Err(JobError::Retry(e))) => (e, true),
        Ok(Err(JobError::Permanent(e))) => (e, false),
        Err(_) => ("the job's handler panicked".to_owned(), true),
    };
    tracing::warn!(job = %job.id, kind = %job.kind, attempt = job.attempts, %error, retry, "job failed");
    if let Err(e) = jobs::fail(&state.pool, &job, &error, retry).await {
        // Its lease runs out, and the job is claimed again.
        tracing::warn!(job = %job.id, error = %e, "couldn't record a job's failure");
    }
}

async fn dispatch(state: &AppState, job: &Job) -> Result<(), JobError> {
    match job.kind.as_str() {
        "publish" | "unpublish" => crate::schedule::run(state, job).await,
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
    async fn a_dropped_run_leaves_its_handler_listed(pool: PgPool) {
        let id = add(&pool, "test-hang", OffsetDateTime::now_utc()).await;
        let state = AppState {
            pool: pool.clone(),
            schemas: std::sync::Arc::new(crate::schemas::Schemas::load(&pool).await.unwrap()),
            auth: std::sync::Arc::new(crate::auth::AuthState::new(Default::default()).unwrap()),
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
