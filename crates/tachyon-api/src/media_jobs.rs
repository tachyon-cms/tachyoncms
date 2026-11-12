//! Jobs that look after stored files: making an image's sizes, deleting the files nothing
//! holds, and an hourly sweep that tries the deletes storage refused for longer than their
//! retries.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool};
use tachyon_core::uploads::{ImageFormat, ImageSize, rename_sizes, suffixed_size_filename};
use tachyon_db::DbError;
use tachyon_db::collections::Collection;
use tachyon_db::files::{self, Renamed, SizeLock};
use tachyon_db::jobs::{self, Job, NewJob};
use tachyon_media::image::{self as picture, ImageError, Made, Original, SizeOut};
use tachyon_media::storage::Storage;
use time::OffsetDateTime;
use tokio::sync::{Semaphore, SemaphorePermit};
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

/// How often a size's write goes round again because a restore renamed the file under
/// it, before the job retries.
const STORE_TRIES: usize = 5;

/// How long after its last size the job's verify run looks for each size's objects: past
/// the longest a `{"keys"}` cleanup's delete can still be landing (`DELETE_TIMEOUT`).
const VERIFY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// Size jobs decoding at once, taken before a `DECODES` permit, so requests always keep at
/// least one of those.
static JOB_DECODES: Semaphore = Semaphore::const_new(1);

/// Makes each of the file's sizes its row doesn't have yet from its stored original,
/// decoded once, all of them before any is stored. Each size's objects are stored with
/// nothing locked (a cleanup queued first in case nothing records them), then it's written
/// everywhere the file is described (`fill_size_in`) in a transaction of its own, the last
/// one completing the job and queuing a verify run (`{"file", "verify": true}`) that makes
/// again any size whose objects have gone. A size that can't be made is written as skipped
/// and logged. A run that lost its lease stops before its next size. A file deleted or
/// released meanwhile fails it for good; one a user's write or a schema change holds defers
/// it; storage and database errors retry it.
pub(crate) async fn run_sizes(state: &AppState, job: &Job) -> Result<(), JobError> {
    let id = file_of(job)?;
    let verify = job.payload.get("verify") == Some(&Value::Bool(true));
    let file = match files::get(&state.pool, id).await {
        Ok(f) if f.state == "live" => f,
        // Nothing left to verify.
        Ok(_) | Err(DbError::NotFound) if verify => return complete(state, job, None).await,
        Ok(_) | Err(DbError::NotFound) => return Err(released()),
        Err(e) => return Err(retry(e)),
    };
    let leased = keep_leased(state, job, LEASE).await?;
    // Already taken over: the run that took it is the one that counts.
    if leased.lost() {
        return Ok(());
    }
    let c = state.schemas.get_by_id(file.collection_id);
    let upload = c.as_ref().and_then(|c| c.schema.upload.clone());
    let format = picture::size_format(&file.mime_type);
    let (Some(c), Some(u), Some(format)) = (c, upload, format) else {
        drop(leased);
        return complete(state, job, None).await;
    };
    let storage = storage(state, &file.storage)?;
    let mut todo: Vec<&ImageSize> = Vec::new();
    for s in &u.image_sizes {
        let entry = file.sizes.get(&s.name).filter(|e| !e.is_null());
        let wanted = match entry {
            // Those an earlier run made are kept.
            Some(_) if !verify => false,
            None => !verify,
            Some(e) => missing(storage, &file.key, e).await?,
        };
        if wanted {
            todo.push(s);
        }
    }
    if todo.is_empty() {
        drop(leased);
        return complete(state, job, None).await;
    }
    let cap = picture::cap(&u, state.media.max_pixels);
    let original = {
        // No bigger than the row says it is: what was stored is what was checked.
        let size = u64::try_from(file.filesize).unwrap_or_default();
        let fetched = crate::uploads::download(
            storage,
            &file.key,
            &state.media.temp_dir,
            &file.filename,
            size,
        )
        .await
        .map_err(retry)?
        .ok_or_else(|| {
            JobError::Permanent(format!(
                "the stored original of `{}` is bigger than the {size} bytes it was",
                file.id
            ))
        })?
        .file;
        open(fetched.path(), cap, u.with_metadata).await?
    };
    let focal = u.focal_point.then_some((file.focal_x, file.focal_y));
    // Every size is made, into files, before any is stored: the decoded original and its
    // permits are given back before the job waits on storage or the database.
    let mut made = Vec::with_capacity(todo.len());
    for size in &todo {
        let m = match &original {
            Ok(o) => make(o.clone(), &file.filename, size, focal, format, cap, state).await?,
            Err(e) => Made::Failed(e.clone()),
        };
        made.push(match m {
            Made::Size(m) => Some(m),
            Made::Skipped => None,
            Made::Failed(error) => {
                tracing::error!(file = %id, size = %size.name, %error, "a size can't be made from this image; it's left empty");
                None
            }
        });
    }
    drop(original);
    let mut write = Write {
        state,
        storage,
        c: &c,
        job,
        sizes: &u.image_sizes,
        filename: file.filename.clone(),
        key: file.key.clone(),
        known: known(&file.sizes.0),
        lease: Some(leased),
        // A first run always verifies what it, or a run before it, stored.
        verify_after: !verify,
    };
    for (i, (size, made)) in todo.iter().zip(&made).enumerate() {
        if write.lease.as_ref().is_some_and(Leased::lost) {
            return Ok(());
        }
        if !write
            .size(id, &size.name, made.as_ref(), i + 1 == todo.len())
            .await?
        {
            return Ok(());
        }
    }
    Ok(())
}

/// Whether any object size entry `e` names, in the directory of `key`, is gone.
async fn missing(storage: &Storage, key: &str, e: &Value) -> Result<bool, JobError> {
    let mut names: Vec<&str> = e
        .get("filename")
        .and_then(Value::as_str)
        .into_iter()
        .collect();
    names.extend(
        e.get("formats")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(Map::values)
            .filter_map(|f| f.get("filename")?.as_str()),
    );
    for name in names {
        match storage.head(&beside(key, name)).await {
            Ok(_) => {}
            Err(e) if e.is_not_found() => return Ok(true),
            Err(e) => return Err(retry(e)),
        }
    }
    Ok(false)
}

fn file_of(job: &Job) -> Result<Uuid, JobError> {
    job.payload
        .get("file")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| JobError::Permanent("the payload names no file".into()))
}

/// An original decoded for its sizes, holding a `DECODES` permit for as long as any of
/// its sizes is being made from it, a blocking task's included. The job drops it once
/// they're all made, before it stores any.
struct Opened {
    original: Original,
    _decoding: Decoding,
    _job: SemaphorePermit<'static>,
}

/// The image at `path` decoded on a blocking thread, or why it can't be (each of its sizes
/// is then left empty). A file that can't be read retries.
async fn open(
    path: &Path,
    cap: u64,
    keep_metadata: bool,
) -> Result<Result<Arc<Opened>, ImageError>, JobError> {
    let job = JOB_DECODES
        .acquire()
        .await
        .expect("JOB_DECODES is never closed");
    let permit = Decoding::permit().await;
    let path = path.to_owned();
    let opened = tokio::task::spawn_blocking(move || {
        let decoding = Decoding::running(permit);
        Original::open(&path, cap, keep_metadata).map(|original| {
            Arc::new(Opened {
                original,
                _decoding: decoding,
                _job: job,
            })
        })
    })
    .await
    .map_err(|e| JobError::Retry(format!("decoding the original failed: {e}")))?;
    match opened {
        Err(ImageError::Internal(e)) => Err(JobError::Retry(e)),
        other => Ok(other),
    }
}

/// `size` made from `original` on a blocking thread. What it makes is deleted when the
/// result drops, there too should this be dropped meanwhile. Only files that couldn't be
/// written retry.
async fn make(
    original: Arc<Opened>,
    filename: &str,
    size: &ImageSize,
    focal: Option<(f32, f32)>,
    format: ImageFormat,
    cap: u64,
    state: &AppState,
) -> Result<Made, JobError> {
    let (filename, size) = (filename.to_owned(), size.clone());
    let out_dir = state.media.temp_dir.clone();
    tokio::task::spawn_blocking(move || {
        original
            .original
            .make(&filename, &size, focal, format, cap, &out_dir)
    })
    .await
    .map_err(|e| JobError::Retry(format!("making a size failed: {e}")))?
    .map_err(|e| JobError::Retry(e.to_string()))
}

/// Each stored object of `sizes` (a file's) by name: the size it belongs to and its entry.
fn known(sizes: &Map<String, Value>) -> HashMap<String, (String, Value)> {
    let mut out = HashMap::new();
    for (size, entry) in sizes {
        let mut add = |e: &Value| {
            if let Some(name) = e.get("filename").and_then(Value::as_str) {
                out.insert(name.to_owned(), (size.clone(), e.clone()));
            }
        };
        add(entry);
        for f in entry
            .get("formats")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(Map::values)
        {
            add(f);
        }
    }
    out
}

/// What a run writes its sizes with.
struct Write<'a> {
    state: &'a AppState,
    storage: &'a Storage,
    c: &'a Collection,
    job: &'a Job,
    /// The collection's `imageSizes`, to tell whether two sizes make the same image.
    sizes: &'a [ImageSize],
    /// The file's name, as it was last seen locked.
    filename: String,
    key: String,
    known: HashMap<String, (String, Value)>,
    /// Dropped before the last size completes the job.
    lease: Option<Leased>,
    /// Whether the last size queues a verify run: always for a first run, and for a verify
    /// run once it has stored anything.
    verify_after: bool,
}

impl Write<'_> {
    /// Whether sizes `a` and `b` make the same image from one original: alike in all but
    /// their names.
    fn same(&self, a: &str, b: &str) -> bool {
        let config = |n: &str| {
            self.sizes.iter().find(|s| s.name == n).map(|s| ImageSize {
                name: String::new(),
                ..s.clone()
            })
        };
        config(a).is_some() && config(a) == config(b)
    }

    /// Stores size `name` of file `id` as `made` (`None`: skipped or failed) and writes it
    /// in, completing the job when it's the `last`. False when the run lost the job.
    async fn size(
        &mut self,
        id: Uuid,
        name: &str,
        made: Option<&SizeOut>,
        last: bool,
    ) -> Result<bool, JobError> {
        let pool = &self.state.pool;
        // Made again (a verify run), its own objects don't count as another size's.
        self.known.retain(|_, (owner, _)| owner != name);
        for _ in 0..STORE_TRIES {
            let (entry, objects) = match made {
                // The reference's shape for a size it skips.
                None => (
                    json!({"width": null, "height": null, "mimeType": null, "filesize": null,
                           "filename": null}),
                    Vec::new(),
                ),
                Some(m) => self.shared(name, named(m, name, &self.filename)),
            };
            let keys: Vec<String> = objects
                .iter()
                .map(|(n, _, _)| beside(&self.key, n))
                .collect();
            if !keys.is_empty() {
                // Committed before anything is stored: should no write below record them,
                // this removes the objects, a crash's included.
                let grace = time::Duration::try_from(files::PENDING_GRACE).expect("an hour fits");
                let mut conn = pool.acquire().await.map_err(retry)?;
                let at = OffsetDateTime::now_utc() + grace;
                files::queue_key_cleanup(&mut conn, self.storage.name(), &keys, at)
                    .await
                    .map_err(retry)?;
                drop(conn);
                for (key, (_, path, mime)) in keys.iter().zip(&objects) {
                    self.storage
                        .put_file(key, path, mime)
                        .await
                        .map_err(retry)?;
                }
                self.verify_after = true;
            }
            let mut tx = pool.begin().await.map_err(retry)?;
            let file = match files::lock_for_size_in(&mut tx, self.c.id, id)
                .await
                .map_err(retry)?
            {
                SizeLock::Released => return Err(released()),
                SizeLock::Moved => {
                    return Err(JobError::Defer(
                        "a document took the file as its size was written".into(),
                    ));
                }
                SizeLock::Busy => {
                    return Err(JobError::Defer(
                        "a write held the file as its size was written".into(),
                    ));
                }
                SizeLock::Live { schema_version, .. }
                    if schema_version != self.c.schema_version =>
                {
                    return Err(JobError::Defer(
                        "the collection's schema changed; its sizes are made again".into(),
                    ));
                }
                SizeLock::Live { file, .. } => file,
            };
            // Renamed by a restore since: stored again under the new names.
            if file.filename != self.filename {
                self.filename = file.filename;
                continue;
            }
            let names: Vec<String> = objects.iter().map(|(n, _, _)| n.clone()).collect();
            files::fill_size_in(&mut tx, id, name, &entry, &names)
                .await
                .map_err(retry)?;
            if last {
                drop(self.lease.take());
                if !jobs::complete_in(&mut tx, self.job).await.map_err(retry)? {
                    return Ok(false);
                }
                // A `{"keys"}` cleanup that decided before this commit may yet take an
                // object stored above; the verify run, due once its delete can't still be
                // landing, makes again what's gone.
                if self.verify_after {
                    let after = time::Duration::try_from(VERIFY_AFTER).expect("a minute fits");
                    let verify = NewJob {
                        kind: IMAGE_SIZES,
                        document_id: None,
                        collection_id: Some(self.c.id),
                        payload: json!({ "file": id, "verify": true }),
                        run_at: OffsetDateTime::now_utc() + after,
                        // Its own key: sharing the size job's, either would cancel the other.
                        dedupe_key: Some(format!("{IMAGE_SIZES}_verify:{id}")),
                        created_by: None,
                        key_id: None,
                        max_attempts: None,
                    };
                    // One already running may have looked before these objects were
                    // stored, so it isn't cancelled (it would never finish) nor relied on:
                    // this job waits for it, then queues its own.
                    if !jobs::replace_queued(&mut tx, &verify)
                        .await
                        .map_err(retry)?
                    {
                        return Err(JobError::Defer(
                            "the file's verify run was running as its last size was written".into(),
                        ));
                    }
                }
            }
            tx.commit().await.map_err(retry)?;
            self.known
                .extend(known(&Map::from_iter([(name.to_owned(), entry)])));
            return Ok(true);
        }
        Err(JobError::Retry(
            "the file kept being renamed while its size was written".into(),
        ))
    }

    /// `named`'s entry and objects for size `size`. An object another size of the file
    /// already stores under the same name is left out and its entry taken from that size's
    /// when the two make the same image; otherwise each of this size's names gets its
    /// name as a suffix, so neither overwrites the other.
    fn shared(
        &self,
        size: &str,
        (mut entry, mut objects): (Value, Vec<(String, PathBuf, String)>),
    ) -> (Value, Vec<(String, PathBuf, String)>) {
        let clash = |n: &str| self.known.get(n).filter(|(owner, _)| owner != size);
        if objects
            .iter()
            .any(|(n, _, _)| clash(n).is_some_and(|(owner, _)| !self.same(owner, size)))
        {
            let mut rename = |e: &mut Value| {
                if let Some(n) = e.get("filename").and_then(Value::as_str) {
                    e["filename"] = suffixed_size_filename(n, size).into();
                }
            };
            rename(&mut entry);
            if let Some(Value::Object(formats)) = entry.get_mut("formats") {
                formats.values_mut().for_each(&mut rename);
            }
            for (n, _, _) in &mut objects {
                *n = suffixed_size_filename(n, size);
            }
            return (entry, objects);
        }
        let take = |e: &mut Value| {
            if let Some((_, k)) = e.get("filename").and_then(Value::as_str).and_then(clash) {
                *e = k.clone();
            }
        };
        take(&mut entry);
        if let Some(Value::Object(formats)) = entry.get_mut("formats") {
            formats.values_mut().for_each(take);
        }
        objects.retain(|(n, _, _)| clash(n).is_none());
        (entry, objects)
    }
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

/// How often a restore goes round again because the names it would give its files
/// changed, so their sizes' objects weren't copied yet.
pub(crate) const RESTORE_TRIES: usize = 4;
/// How often it goes round again with the same names, because a size job filled more of
/// their sizes meanwhile.
pub(crate) const SIZE_ROUNDS: usize = 20;

/// The size objects a restore has copied to new names, by storage and key.
#[derive(Default)]
pub(crate) struct Copied(HashSet<(String, String)>);

impl Copied {
    fn pairs(r: &Renamed) -> impl Iterator<Item = (String, String)> + '_ {
        r.sizes
            .iter()
            .map(|(from, to)| (beside(&r.key, from), beside(&r.key, to)))
    }

    /// Whether every size `renamed` renames has its object at the new name.
    pub(crate) fn covers(&self, renamed: &[Renamed]) -> bool {
        renamed
            .iter()
            .all(|r| Self::pairs(r).all(|(_, to)| self.0.contains(&(r.storage.clone(), to))))
    }

    /// Copies each size object `renamed` moves that isn't copied yet to its new name, with
    /// nothing locked: a cleanup of the new names is committed first, due after the grace,
    /// so a restore that never commits leaves nothing behind; one that does keeps them.
    pub(crate) async fn copy(
        &mut self,
        state: &AppState,
        renamed: &[Renamed],
    ) -> Result<(), ApiError> {
        let grace = time::Duration::try_from(files::PENDING_GRACE).expect("an hour fits");
        for r in renamed {
            let todo: Vec<(String, String)> = Self::pairs(r)
                .filter(|(_, to)| !self.0.contains(&(r.storage.clone(), to.clone())))
                .collect();
            if todo.is_empty() {
                continue;
            }
            let storage = state.media.storages.get(&r.storage).ok_or_else(|| {
                ApiError::Internal(format!("no storage named `{}` is configured", r.storage))
            })?;
            let keys: Vec<String> = todo.iter().map(|(_, to)| to.clone()).collect();
            let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
            files::queue_key_cleanup(
                &mut conn,
                &r.storage,
                &keys,
                OffsetDateTime::now_utc() + grace,
            )
            .await?;
            drop(conn);
            for (from, to) in todo {
                match storage.copy(&from, &to).await {
                    Ok(()) => {}
                    // A size whose object was never stored has nothing to move.
                    Err(e) if e.is_not_found() => {}
                    Err(e) => return Err(ApiError::Internal(format!("can't move a size: {e}"))),
                }
                self.0.insert((r.storage.clone(), to));
            }
        }
        Ok(())
    }
}

/// Queues, in the restore's `tx`, the cleanup of the old names' objects of the sizes
/// `renamed` moved; it runs once the restore commits, and keeps any still served.
pub(crate) async fn forget_old_in(
    tx: &mut PgConnection,
    renamed: &[Renamed],
) -> Result<(), ApiError> {
    for r in renamed.iter().filter(|r| !r.sizes.is_empty()) {
        let old: Vec<String> = Copied::pairs(r).map(|(from, _)| from).collect();
        files::queue_key_cleanup(tx, &r.storage, &old, OffsetDateTime::now_utc()).await?;
    }
    Ok(())
}

/// The longest a `{"keys"}` cleanup waits on one delete while it holds the file's row.
const DELETE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
            // Bounded, so a stalled store can't hold the file's row from the writes that
            // wait on it.
            tokio::time::timeout(DELETE_TIMEOUT, storage.delete(key))
                .await
                .map_err(|_| JobError::Retry(format!("deleting `{key}` took too long")))?
                .map_err(retry)?;
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
