//! Jobs that look after stored files: making an image's sizes, deleting the files nothing
//! holds, and an hourly sweep that tries the deletes storage refused for longer than their
//! retries.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use tachyon_core::uploads::{ImageFormat, ImageSize, rename_sizes};
use tachyon_db::DbError;
use tachyon_db::collections::Collection;
use tachyon_db::files::{self, Renamed};
use tachyon_db::jobs::{self, Job, NewJob};
use tachyon_media::image::{self as picture, ImageError, SizeOut};
use tachyon_media::storage::Storage;
use tempfile::NamedTempFile;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::jobs::{JobError, LEASE, Leased, keep_leased};
use crate::uploads::Decoding;
use crate::{ApiError, AppState};

/// Makes a file's `imageSizes`. Its payload is `{"file": id}`, its dedupe key
/// `image_sizes:<file id>`.
pub(crate) const IMAGE_SIZES: &str = "image_sizes";

/// Queues the sizes of file `file`, which a document of `c` is taking in `tx`. The job's
/// ID, or `None` when there's nothing to make: `c` has no `imageSizes`, or the file isn't
/// an image they can be made from (an SVG, say).
pub(crate) async fn queue_sizes(
    tx: &mut PgConnection,
    c: &Collection,
    file: Uuid,
) -> Result<Option<Uuid>, DbError> {
    if c.schema
        .upload
        .as_ref()
        .is_none_or(|u| u.image_sizes.is_empty())
    {
        return Ok(None);
    }
    let f = files::get(&mut *tx, file).await?;
    if picture::size_format(&f.mime_type).is_none() {
        return Ok(None);
    }
    let job = NewJob {
        kind: IMAGE_SIZES,
        document_id: None,
        collection_id: Some(c.id),
        payload: json!({ "file": file }),
        run_at: OffsetDateTime::now_utc(),
        dedupe_key: Some(format!("{IMAGE_SIZES}:{file}")),
        created_by: None,
        key_id: None,
        max_attempts: None,
    };
    jobs::enqueue(tx, &job).await.map(Some)
}

fn released() -> JobError {
    JobError::Permanent("file released".into())
}

/// The key of `name` in the directory of `key`, where a file's sizes sit.
fn beside(key: &str, name: &str) -> String {
    match key.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.to_owned(),
    }
}

/// Makes each of the file's sizes its row doesn't have yet, from its stored original, one
/// at a time: each is stored and written everywhere the file is described (`fill_size_in`)
/// in a transaction of its own, the last one completing the job. A run that lost its lease
/// stops before its next size. A file deleted or released meanwhile fails it for good.
pub(crate) async fn run_sizes(state: &AppState, job: &Job) -> Result<(), JobError> {
    let id = file_of(job)?;
    let file = match files::get(&state.pool, id).await {
        Ok(f) if f.state == "live" => f,
        Ok(_) | Err(DbError::NotFound) => return Err(released()),
        Err(e) => return Err(retry(e)),
    };
    let leased = keep_leased(state, job, LEASE).await?;
    // Already taken over: the run that took it is the one that counts.
    if leased.lost() {
        return Ok(());
    }
    let upload = state
        .schemas
        .get_by_id(file.collection_id)
        .and_then(|c| c.schema.upload.clone());
    let format = picture::size_format(&file.mime_type);
    let (Some(u), Some(format)) = (upload, format) else {
        drop(leased);
        return complete(state, job, None).await;
    };
    // Those an earlier run made are kept.
    let todo: Vec<&ImageSize> = u
        .image_sizes
        .iter()
        .filter(|s| file.sizes.get(&s.name).is_none_or(Value::is_null))
        .collect();
    if todo.is_empty() {
        drop(leased);
        return complete(state, job, None).await;
    }
    let storage = storage(state, &file.storage)?;
    let original = fetch(storage, &file.key, &state.media.temp_dir).await?;
    let cap = picture::cap(&u, state.media.max_pixels);
    let focal = u.focal_point.then_some((file.focal_x, file.focal_y));
    let mut leased = Some(leased);
    for (i, size) in todo.iter().enumerate() {
        if leased.as_ref().is_some_and(Leased::lost) {
            return Ok(());
        }
        let made = make(
            state,
            original.path(),
            &file.filename,
            size,
            focal,
            format,
            cap,
        )
        .await?;
        let last = i + 1 == todo.len();
        let tx = state.pool.begin().await.map_err(retry)?;
        let lease = if last { leased.take() } else { None };
        if !store_size(
            state,
            storage,
            tx,
            job,
            id,
            &size.name,
            made.as_ref(),
            lease,
            last,
        )
        .await?
        {
            return Ok(());
        }
    }
    Ok(())
}

fn file_of(job: &Job) -> Result<Uuid, JobError> {
    job.payload
        .get("file")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| JobError::Permanent("the payload names no file".into()))
}

/// The object at `key`, copied into a new file in `dir` that's deleted when it drops.
async fn fetch(storage: &Storage, key: &str, dir: &Path) -> Result<NamedTempFile, JobError> {
    let got = storage.get_range(key, None).await.map_err(retry)?;
    let file = NamedTempFile::new_in(dir).map_err(retry)?;
    let mut out = tokio::fs::File::from_std(file.reopen().map_err(retry)?);
    let mut chunks = got.into_stream();
    while let Some(chunk) = chunks.next().await {
        out.write_all(&chunk.map_err(retry)?).await.map_err(retry)?;
    }
    out.flush().await.map_err(retry)?;
    Ok(file)
}

/// `size` made from `original` on a blocking thread, holding a `DECODES` permit. What it
/// makes is deleted when the result drops, there too should this be dropped meanwhile.
async fn make(
    state: &AppState,
    original: &Path,
    filename: &str,
    size: &ImageSize,
    focal: Option<(f32, f32)>,
    format: ImageFormat,
    cap: u64,
) -> Result<Option<SizeOut>, JobError> {
    let permit = Decoding::permit().await;
    let (original, filename, size): (PathBuf, String, ImageSize) =
        (original.to_owned(), filename.to_owned(), size.clone());
    let out_dir = state.media.temp_dir.clone();
    tokio::task::spawn_blocking(move || {
        let _decoding = Decoding::running(permit);
        picture::make_size(&original, &filename, &size, focal, format, cap, &out_dir)
    })
    .await
    .map_err(|e| JobError::Retry(format!("making a size failed: {e}")))?
    .map_err(|e| match e {
        ImageError::Internal(e) => JobError::Retry(e),
        e => JobError::Permanent(e.to_string()),
    })
}

/// Stores size `name` of file `id` as `made` (`None`: skipped) and writes it in, in `tx`
/// with the file locked, completing `job` when it's the `last` (`lease` is dropped first).
/// False when the run lost the job, its work undone. Objects stored by a write that didn't
/// commit are queued for cleanup, which keeps any a later run has stored since.
#[allow(clippy::too_many_arguments)]
async fn store_size(
    state: &AppState,
    storage: &Storage,
    mut tx: Transaction<'static, Postgres>,
    job: &Job,
    id: Uuid,
    name: &str,
    made: Option<&SizeOut>,
    lease: Option<Leased>,
    last: bool,
) -> Result<bool, JobError> {
    let Some(file) = files::lock_live_in(&mut tx, id).await.map_err(retry)? else {
        return Err(released());
    };
    let (entry, objects) = match made {
        // The reference's shape for a size it skips.
        None => (
            json!({"width": null, "height": null, "mimeType": null, "filesize": null,
                   "filename": null}),
            Vec::new(),
        ),
        Some(m) => named(m, name, &file.filename),
    };
    let mut stored = Vec::new();
    let wrote = async {
        for (key, path, mime) in &objects {
            let key = beside(&file.key, key);
            stored.push(key.clone());
            storage.put_file(&key, path, mime).await.map_err(retry)?;
        }
        let names: Vec<String> = objects.iter().map(|(n, _, _)| n.clone()).collect();
        files::fill_size_in(&mut tx, id, name, &entry, &names)
            .await
            .map_err(retry)?;
        if last {
            drop(lease);
            if !jobs::complete_in(&mut tx, job).await.map_err(retry)? {
                return Ok(false);
            }
        }
        tx.commit().await.map_err(retry)?;
        Ok(true)
    }
    .await;
    if !matches!(wrote, Ok(true)) && !stored.is_empty() {
        let queued = async {
            let mut conn = state.pool.acquire().await?;
            files::queue_key_cleanup(&mut conn, storage.name(), &stored).await
        };
        if let Err(error) = queued.await {
            tracing::error!(%error, file = %id, keys = ?stored, "couldn't queue the cleanup of a size that wasn't written; its objects are left");
        }
    }
    wrote
}

/// `made`'s entry for size `name`, its copies under `formats`, named after `filename`, the
/// name the file has now, and the objects to store: (name, path, type).
fn named(made: &SizeOut, name: &str, filename: &str) -> (Value, Vec<(String, PathBuf, String)>) {
    let mut entry = made.entry.clone();
    if !made.formats.is_empty() {
        let formats: Map<String, Value> = made
            .formats
            .iter()
            .map(|(k, f)| ((*k).to_owned(), Value::Object(f.clone())))
            .collect();
        entry.insert("formats".into(), formats.into());
    }
    // A restore may have renamed the file since the size was made.
    let mut one = Map::from_iter([(name.to_owned(), Value::Object(entry))]);
    let moved = rename_sizes(&mut one, filename);
    let entry = one.remove(name).unwrap_or_default();
    let mime_of = |n: &str| {
        let of = |e: &Value| {
            (e.get("filename")?.as_str()? == n)
                .then(|| e.get("mimeType")?.as_str().map(str::to_owned))
                .flatten()
        };
        of(&entry)
            .or_else(|| entry.get("formats")?.as_object()?.values().find_map(of))
            .unwrap_or_else(|| "application/octet-stream".into())
    };
    let objects = made
        .files
        .iter()
        .map(|(n, path)| {
            let n = moved
                .iter()
                .find(|(old, _)| old == n)
                .map_or_else(|| n.clone(), |(_, new)| new.clone());
            let mime = mime_of(&n);
            (n, path.clone(), mime)
        })
        .collect();
    (entry, objects)
}

/// After the restore that moved `renamed`'s sizes failed to commit: their new copies are
/// queued for cleanup (each is kept should a file serve it after all).
pub(crate) async fn forget_moved(state: &AppState, renamed: &[Renamed]) {
    for r in renamed.iter().filter(|r| !r.sizes.is_empty()) {
        let keys: Vec<String> = r.sizes.iter().map(|(_, new)| beside(&r.key, new)).collect();
        let queued = async {
            let mut conn = state.pool.acquire().await?;
            files::queue_key_cleanup(&mut conn, &r.storage, &keys).await
        };
        if let Err(error) = queued.await {
            tracing::error!(%error, ?keys, "couldn't queue the cleanup of sizes copied for a restore that failed");
        }
    }
}

/// Moves the sizes of each file `renamed` lists to their new names, copying them in `tx`'s
/// life, before it commits: the old objects are queued for cleanup in `tx`, and copies a
/// failure leaves are queued for cleanup outside it.
pub(crate) async fn move_sizes_in(
    state: &AppState,
    tx: &mut PgConnection,
    renamed: &[Renamed],
) -> Result<(), ApiError> {
    for r in renamed.iter().filter(|r| !r.sizes.is_empty()) {
        let storage = state.media.storages.get(&r.storage).ok_or_else(|| {
            ApiError::Internal(format!("no storage named `{}` is configured", r.storage))
        })?;
        let mut copied = Vec::new();
        let mut old = Vec::new();
        for (from, to) in &r.sizes {
            let (from, to) = (beside(&r.key, from), beside(&r.key, to));
            if let Err(e) = storage.copy(&from, &to).await {
                // A size whose object was never stored has nothing to move.
                if e.is_not_found() {
                    continue;
                }
                let queued = async {
                    let mut conn = state.pool.acquire().await?;
                    files::queue_key_cleanup(&mut conn, &r.storage, &copied).await
                };
                if !copied.is_empty()
                    && let Err(error) = queued.await
                {
                    tracing::error!(%error, keys = ?copied, "couldn't queue the cleanup of sizes copied for a restore that failed");
                }
                return Err(ApiError::Internal(format!("can't move a size: {e}")));
            }
            copied.push(to);
            old.push(from);
        }
        if !old.is_empty() {
            files::queue_key_cleanup(tx, &r.storage, &old).await?;
        }
    }
    Ok(())
}

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
        // A retry: the storage may be configured again, and the sweep revives it then.
        .ok_or_else(|| JobError::Retry(format!("no storage named `{name}` is configured")))
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
            // The file is locked from the check until the delete is done, and a size is
            // only stored under that lock, so one can't be stored here in between.
            let mut tx = state.pool.begin().await.map_err(retry)?;
            if files::key_in_use_in(&mut tx, name, key)
                .await
                .map_err(retry)?
            {
                continue;
            }
            storage.delete(key).await.map_err(retry)?;
            tx.commit().await.map_err(retry)?;
        }
        return complete(state, job, None).await;
    }
    let id = file_of(job)?;
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
    // This run is done whatever happens now, and the next one is queued: a failure is only
    // logged, and the next run tries again.
    let requeued = async {
        let mut tx = state.pool.begin().await?;
        let n = files::requeue_cleanups_in(&mut tx, &names).await?;
        tx.commit().await?;
        Ok::<_, DbError>(n)
    };
    let n = match requeued.await {
        Ok(n) => n,
        Err(error) => {
            tracing::error!(%error, "couldn't queue failed file cleanups again; the next sweep tries in an hour");
            return Ok(());
        }
    };
    if n > 0 {
        tracing::info!(cleanups = n, "queued failed file cleanups again");
    }
    Ok(())
}
