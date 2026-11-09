//! Jobs that look after stored files: deleting the ones nothing holds, and an hourly sweep
//! that tries the deletes storage refused for longer than their retries.

use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_db::DbError;
use tachyon_db::files;
use tachyon_db::jobs::{self, Job, NewJob};
use tachyon_media::storage::Storage;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;
use crate::jobs::JobError;

/// Re-queues cleanups that ran out of attempts; runs hourly.
pub(crate) const CLEANUP_SWEEP: &str = "file_cleanup_sweep";
const SWEEP_EVERY: time::Duration = time::Duration::HOUR;

fn sweep(run_at: OffsetDateTime) -> NewJob<'static> {
    NewJob {
        kind: CLEANUP_SWEEP,
        document_id: None,
        collection_id: None,
        payload: json!({}),
        run_at,
        dedupe_key: Some(CLEANUP_SWEEP.into()),
        created_by: None,
        key_id: None,
        // It's what keeps failed cleanups from being lost, so it never gives up.
        max_attempts: Some(i32::MAX),
    }
}

/// Queues the sweep an hour from now, unless one is already queued or running, and warns
/// about files left on a storage that's no longer configured.
pub(crate) async fn seed_sweep(pool: &PgPool, storages: &[String]) -> Result<(), DbError> {
    let stranded = files::on_unknown_storage(pool, storages).await?;
    if stranded > 0 {
        tracing::warn!(
            files = stranded,
            "files waiting to be deleted are on a storage that's no longer configured; configure it again to delete them"
        );
    }
    let first = sweep(OffsetDateTime::now_utc() + SWEEP_EVERY);
    match jobs::add(&mut *pool.acquire().await?, &first).await {
        Ok(_) => Ok(()),
        Err(DbError::UniqueViolation(c)) if c == "jobs_one_pending" => Ok(()),
        Err(e) => Err(e),
    }
}

fn retry(e: impl std::fmt::Display) -> JobError {
    JobError::Retry(e.to_string())
}

fn storage<'a>(state: &'a AppState, name: &str) -> Result<&'a Arc<Storage>, JobError> {
    state
        .media
        .storages
        .get(name)
        .ok_or_else(|| JobError::Permanent(format!("no storage named `{name}` is configured")))
}

/// Deletes a file's objects, then its row (`{"file": id}`), or only the objects named
/// (`{"keys": [..], "storage": name}`). A live file is left alone; a pending one only once
/// its grace is over. Storage errors are retried.
pub(crate) async fn run_cleanup(state: &AppState, job: &Job) -> Result<(), JobError> {
    if let Some(keys) = job.payload.get("keys") {
        let keys: Vec<String> = serde_json::from_value(keys.clone())
            .map_err(|e| JobError::Permanent(format!("bad keys: {e}")))?;
        // Guessing would delete from the wrong store.
        let name = job.payload["storage"]
            .as_str()
            .ok_or_else(|| JobError::Permanent("the payload names no storage".into()))?;
        let storage = storage(state, name)?;
        for key in &keys {
            // shortcut: checked, then deleted, so a size remade under this name in between is
            // lost; make the remake lock the file row before it writes if that ever matters.
            if files::key_in_use(&state.pool, name, key)
                .await
                .map_err(retry)?
            {
                continue;
            }
            storage.delete(key).await.map_err(retry)?;
        }
        return complete(state, job, None).await;
    }
    let id: Uuid = job
        .payload
        .get("file")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| JobError::Permanent("the payload names no file".into()))?;
    let file = match files::get(&state.pool, id).await {
        Ok(file) => file,
        // Already cleaned up, by an earlier run whose completion was lost.
        Err(DbError::NotFound) => return complete(state, job, None).await,
        Err(e) => return Err(retry(e)),
    };
    match file.state.as_str() {
        "gone" => {}
        "pending" => {
            if !files::expire(&state.pool, id).await.map_err(retry)? {
                return Err(JobError::Retry(format!(
                    "the file `{id}` is still within its grace or was taken meanwhile"
                )));
            }
        }
        _ => {
            return Err(JobError::Permanent(format!(
                "the file `{id}` is in use, so it's kept"
            )));
        }
    }
    let storage = storage(state, &file.storage)?;
    // Sizes sit beside the original, under the file's own prefix. The key keeps the name
    // the file had when stored, which a rename since may have changed in `names`.
    let dir = file.key.rsplit_once('/').map_or("", |(dir, _)| dir);
    storage.delete(&file.key).await.map_err(retry)?;
    for name in &file.names {
        storage
            .delete(&format!("{dir}/{name}"))
            .await
            .map_err(retry)?;
    }
    complete(state, job, Some(id)).await
}

/// Completes `job`, deleting gone file `forget`'s row in the same transaction.
async fn complete(state: &AppState, job: &Job, forget: Option<Uuid>) -> Result<(), JobError> {
    let mut tx = state.pool.begin().await.map_err(retry)?;
    if let Some(id) = forget
        && !files::forget_in(&mut tx, id).await.map_err(retry)?
    {
        // Gone is final and nothing can point at a gone file, so this would mean a file
        // something holds just lost its objects.
        tracing::error!(file = %id, "a cleaned-up file was still referenced; its objects are deleted");
    }
    if jobs::complete_in(&mut tx, job).await.map_err(retry)? {
        tx.commit().await.map_err(retry)?;
    }
    Ok(())
}

/// Queues the next sweep, then each failed cleanup still worth trying. The next run is
/// queued first, in its own transaction, so it's there whatever happens to this one's work,
/// which is safe to repeat.
pub(crate) async fn run_cleanup_sweep(state: &AppState, job: &Job) -> Result<(), JobError> {
    let mut tx = state.pool.begin().await.map_err(retry)?;
    if !jobs::complete_in(&mut tx, job).await.map_err(retry)? {
        return Ok(());
    }
    jobs::add(&mut tx, &sweep(OffsetDateTime::now_utc() + SWEEP_EVERY))
        .await
        .map_err(retry)?;
    tx.commit().await.map_err(retry)?;
    let names: Vec<String> = state.media.storages.0.keys().cloned().collect();
    let mut tx = state.pool.begin().await.map_err(retry)?;
    let n = files::requeue_cleanups_in(&mut tx, &names)
        .await
        .map_err(retry)?;
    tx.commit().await.map_err(retry)?;
    if n > 0 {
        tracing::info!(cleanups = n, "queued failed file cleanups again");
    }
    Ok(())
}
