//! Workers that run queued jobs, such as scheduled publishes.
//!
//! A handler that changes the database does its work and `jobs::complete_in` in one
//! transaction, rolling back when that returns false: another worker took the job over,
//! and its run is the one that counts. A long handler calls `jobs::heartbeat` at least
//! every third of `LEASE`, and stops when that returns false.

use std::time::Duration;

use tachyon_db::jobs::{self, Job};

use crate::AppState;

/// How long a claimed job is a worker's before another may take it over.
pub(crate) const LEASE: Duration = Duration::from_secs(60);
/// How long an idle worker waits before looking for due jobs again.
const POLL: Duration = Duration::from_secs(1);

/// How many workers this server runs.
#[derive(Debug, Clone)]
pub struct JobsConfig {
    /// 0 runs none, leaving the jobs to other servers.
    pub workers: usize,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self { workers: 2 }
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
        tokio::spawn(work(state.clone()));
    }
}

async fn work(state: AppState) {
    let pool = &state.pool;
    while !pool.is_closed() {
        match jobs::claim(pool, LEASE).await {
            Ok(Some(job)) => run(&state, job).await,
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
async fn run(state: &AppState, job: Job) {
    let outcome = {
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
    #[expect(clippy::match_single_binding, reason = "one arm per kind, none yet")]
    match job.kind.as_str() {
        _ => Err(JobError::Permanent("unknown job kind".into())),
    }
}
