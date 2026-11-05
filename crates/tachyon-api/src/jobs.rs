//! Workers that run queued jobs, such as scheduled publishes.
//!
//! A short handler that changes the database calls `jobs::complete_in` first, then does
//! its work, in one transaction, rolling back when `complete_in` returns false: another
//! worker took the job over, and its run is the one that counts. It must never call
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
/// off. Clones share one set.
#[derive(Debug, Clone, Default)]
pub struct RunningJobs(Arc<Mutex<BTreeSet<Uuid>>>);

impl RunningJobs {
    pub fn ids(&self) -> Vec<Uuid> {
        self.lock().iter().copied().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<Uuid>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes `id` until the returned guard drops, even when the run's future is dropped.
    fn start(&self, id: Uuid) -> impl Drop {
        struct Started(RunningJobs, Uuid);
        impl Drop for Started {
            fn drop(&mut self) {
                self.0.lock().remove(&self.1);
            }
        }
        self.lock().insert(id);
        Started(self.clone(), id)
    }
}

/// Why a run failed. A `Retry` runs again later, until the job is out of attempts.
#[derive(Debug)]
pub(crate) enum JobError {
    #[expect(dead_code, reason = "no handler retries yet")]
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
async fn run(state: &AppState, running: &RunningJobs, job: Job) {
    let outcome = {
        let _running = running.start(job.id);
        let (state, job) = (state.clone(), job.clone());
        tokio::spawn(async move { dispatch(&state, &job).await }).await
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

async fn dispatch(_state: &AppState, job: &Job) -> Result<(), JobError> {
    match job.kind.as_str() {
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

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_dropped_run_forgets_its_job(pool: PgPool) {
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
        assert!(
            running.ids().is_empty(),
            "the dropped run left its job behind"
        );
    }
}
