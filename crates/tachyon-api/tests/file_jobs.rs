//! The jobs that delete stored files, run by a real worker against local storage, and
//! those that make every file's sizes again when its collection's schema changes.

mod common;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use common::{ADMIN, PASSWORD, Res, png, send, signed_in, upload};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use http_body_util::BodyExt;
use object_store::local::LocalFileSystem;
use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tachyon_api::{JobsConfig, Options, UploadsConfig};
use tachyon_core::CollectionSchema;
use tachyon_db::collections::{self, Collection};
use tachyon_db::documents::{self, FileChange, Prepared};
use tachyon_db::files::{self, CLEANUP, CLEANUP_ATTEMPTS, FileRow, NewFile};
use tachyon_db::jobs::{self, NewJob};
use tachyon_db::{DbError, DocCopy};
use tachyon_media::fixtures;
use tachyon_media::storage::{Storage, StorageConfig, Storages, object_key};
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

/// Local storage whose deletes fail while `fails` is above 0, counting down.
#[derive(Debug)]
struct Flaky {
    inner: LocalFileSystem,
    fails: AtomicUsize,
}

impl fmt::Display for Flaky {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Flaky({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Flaky {
    async fn put_opts(
        &self,
        location: &Key,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Key,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Key,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Key>>,
    ) -> BoxStream<'static, object_store::Result<Key>> {
        let failing = self
            .fails
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if failing {
            return futures_util::stream::once(async {
                Err(object_store::Error::Generic {
                    store: "flaky",
                    source: "storage is down".into(),
                })
            })
            .boxed();
        }
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Key>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Key>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Key,
        to: &Key,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// A fresh directory, removed on drop.
struct Temp(tempfile::TempDir);

impl std::ops::Deref for Temp {
    type Target = Path;
    fn deref(&self) -> &Path {
        self.0.path()
    }
}

fn temp_root() -> Temp {
    Temp(
        tempfile::Builder::new()
            .prefix("tachyon-file-jobs-")
            .tempdir()
            .unwrap(),
    )
}

/// `default` storage in a fresh directory, its deletes failing `fails` times.
fn storage(fails: usize) -> (Arc<Storage>, Temp) {
    named("default", fails)
}

/// Storage `name` in a fresh directory, its deletes failing `fails` times.
fn named(name: &str, fails: usize) -> (Arc<Storage>, Temp) {
    let root = temp_root();
    let store = Flaky {
        inner: LocalFileSystem::new_with_prefix(&*root).unwrap(),
        fails: AtomicUsize::new(fails),
    };
    let storage = Storage::with_store(name, Arc::new(store), None, StorageConfig::local(&*root));
    (Arc::new(storage), root)
}

/// The API with one job worker, files going to `storage`.
async fn serve(pool: &PgPool, storage: Arc<Storage>) -> axum::Router {
    serve_all(pool, vec![storage]).await
}

/// The API with one job worker and `storages`.
async fn serve_all(pool: &PgPool, storages: Vec<Arc<Storage>>) -> axum::Router {
    let storages = storages
        .into_iter()
        .map(|s| (s.name().to_owned(), s))
        .collect();
    let options = Options {
        jobs: JobsConfig {
            workers: 1,
            ..JobsConfig::default()
        },
        uploads: UploadsConfig {
            storages: Arc::new(Storages(storages)),
            ..UploadsConfig::default()
        },
        ..Options::default()
    };
    tachyon_api::router_with(pool.clone(), options)
        .await
        .unwrap()
}

/// `images`, an upload collection without versions.
async fn seed(pool: &PgPool) -> Collection {
    seed_with(pool, json!({})).await
}

/// `images`, with `extra` options.
async fn seed_with(pool: &PgPool, extra: serde_json::Value) -> Collection {
    let mut schema = json!({
        "slug": "images", "upload": true,
        "fields": [{"name": "alt", "type": "text"}]
    });
    schema
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let schema: CollectionSchema = serde_json::from_value(schema).unwrap();
    collections::create(pool, &schema).await.unwrap()
}

/// A pending file named `name`, with its original and one size put to `storage`.
async fn stored(pool: &PgPool, c: &Collection, storage: &Storage, name: &str) -> FileRow {
    let f = NewFile {
        collection_id: c.id,
        storage: storage.name(),
        filename: name,
        scope: "",
        mime_type: "image/png",
        filesize: 3,
        width: Some(4),
        height: Some(3),
        focal: (50.0, 50.0),
        blurhash: None,
        issued_to: None,
        document: None,
    };
    let file = files::reserve(pool, &f, |id, name| {
        object_key(None, "images", id, name).map_err(|e| e.to_string())
    })
    .await
    .unwrap();
    let size = format!("small-{name}");
    sqlx::query("UPDATE files SET names = names || $2 WHERE id = $1")
        .bind(file.id)
        .bind(&size)
        .execute(pool)
        .await
        .unwrap();
    let dir = temp_root();
    let source = dir.join("source");
    std::fs::write(&source, b"png").unwrap();
    for key in [file.key.clone(), sibling(&file.key, &size)] {
        storage.put_file(&key, &source, "image/png").await.unwrap();
    }
    files::get(pool, file.id).await.unwrap()
}

/// `name` beside `key`.
fn sibling(key: &str, name: &str) -> String {
    format!("{}/{name}", key.rsplit_once('/').unwrap().0)
}

/// Every object of `file`, as paths under `root`.
fn objects(root: &Path, file: &FileRow) -> Vec<PathBuf> {
    file.names
        .iter()
        .map(|n| root.join(sibling(&file.key, n)))
        .collect()
}

fn with_file(file: FileChange) -> Prepared {
    Prepared {
        file,
        ..Prepared::plain(Map::new())
    }
}

/// A new document holding `file`.
async fn holding(pool: &PgPool, c: &Collection, file: Uuid) -> Uuid {
    let mut tx = pool.begin().await.unwrap();
    let doc = documents::insert_in(
        &mut tx,
        c.id,
        c.schema_version,
        None,
        DocCopy::Published,
        &with_file(FileChange::Set(file)),
        None,
    )
    .await
    .unwrap()
    .id;
    tx.commit().await.unwrap();
    doc
}

async fn set_file(pool: &PgPool, c: &Collection, doc: Uuid, file: Uuid) {
    let mut tx = pool.begin().await.unwrap();
    documents::update_in::<DbError>(
        &mut tx,
        c.id,
        c.schema_version,
        doc,
        DocCopy::Published,
        None,
        None,
        |_| Ok(with_file(FileChange::Set(file))),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// The newest `file_cleanup` of `file`: its state and attempts.
async fn cleanup(pool: &PgPool, file: Uuid) -> (Uuid, String, i32) {
    sqlx::query_as(
        "SELECT id, state, attempts FROM jobs WHERE kind = $1 AND payload->>'file' = $2
         ORDER BY id DESC LIMIT 1",
    )
    .bind(CLEANUP)
    .bind(file.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Polls `check` until it holds, for up to 90 s: a debug build takes its time over AVIF.
async fn until<F: AsyncFn() -> bool>(what: &str, check: F) {
    for _ in 0..1800 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting until {what}");
}

async fn row_exists(pool: &PgPool, file: Uuid) -> bool {
    !matches!(files::get(pool, file).await, Err(DbError::NotFound))
}

/// Makes `job` due now, skipping a retry's back-off.
async fn due_now(pool: &PgPool, job: Uuid) {
    sqlx::query("UPDATE jobs SET run_at = now() WHERE id = $1 AND state = 'queued'")
        .bind(job)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn cleanup_deletes_released_files(pool: PgPool) {
    let (storage, root) = storage(0);
    let c = seed(&pool).await;
    let a = stored(&pool, &c, &storage, "a.png").await;
    let b = stored(&pool, &c, &storage, "b.png").await;
    let doc = holding(&pool, &c, a.id).await;
    set_file(&pool, &c, doc, b.id).await;
    assert_eq!(files::get(&pool, a.id).await.unwrap().state, "gone");
    let _app = serve(&pool, storage).await;
    until("A's row is deleted", async || {
        !row_exists(&pool, a.id).await
    })
    .await;
    for path in objects(&root, &a) {
        assert!(!path.exists(), "{} deleted", path.display());
    }
    assert_eq!(cleanup(&pool, a.id).await.1, "done");
    assert_eq!(files::get(&pool, b.id).await.unwrap().state, "live");
    for path in objects(&root, &b) {
        assert!(path.exists(), "{} kept", path.display());
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn cleanup_retries_storage_errors(pool: PgPool) {
    let (storage, root) = storage(2);
    let c = seed(&pool).await;
    let a = stored(&pool, &c, &storage, "a.png").await;
    let b = stored(&pool, &c, &storage, "b.png").await;
    let doc = holding(&pool, &c, a.id).await;
    set_file(&pool, &c, doc, b.id).await;
    let _app = serve(&pool, storage).await;
    let job = cleanup(&pool, a.id).await.0;
    for failed in 1..=2 {
        until("the cleanup failed and waits to retry", async || {
            let (_, state, attempts) = cleanup(&pool, a.id).await;
            state == "queued" && attempts == failed
        })
        .await;
        assert!(row_exists(&pool, a.id).await, "kept while storage is down");
        due_now(&pool, job).await;
    }
    until("the cleanup is done", async || {
        cleanup(&pool, a.id).await.1 == "done"
    })
    .await;
    assert_eq!(cleanup(&pool, a.id).await, (job, "done".into(), 3));
    assert!(!row_exists(&pool, a.id).await);
    for path in objects(&root, &a) {
        assert!(!path.exists(), "{} deleted", path.display());
    }
    let max: i32 = sqlx::query_scalar("SELECT max_attempts FROM jobs WHERE id = $1")
        .bind(job)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(max, CLEANUP_ATTEMPTS);

    // A live file's cleanup deletes nothing and isn't retried.
    let mut conn = pool.acquire().await.unwrap();
    let live = jobs::enqueue(
        &mut conn,
        &NewJob {
            kind: CLEANUP,
            document_id: None,
            collection_id: None,
            payload: json!({ "file": b.id }),
            run_at: OffsetDateTime::now_utc(),
            dedupe_key: Some(format!("{CLEANUP}:{}", b.id)),
            created_by: None,
            key_id: None,
            max_attempts: Some(CLEANUP_ATTEMPTS),
        },
    )
    .await
    .unwrap();
    until("the live file's cleanup failed", async || {
        cleanup(&pool, b.id).await.1 == "failed"
    })
    .await;
    assert_eq!(cleanup(&pool, b.id).await, (live, "failed".into(), 1));
    assert_eq!(files::get(&pool, b.id).await.unwrap().state, "live");
    for path in objects(&root, &b) {
        assert!(path.exists(), "{} kept", path.display());
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn failed_cleanups_are_requeued(pool: PgPool) {
    // Storage stays down, so the requeued cleanup can only fail and wait again.
    let (storage, _root) = storage(usize::MAX);
    let c = seed(&pool).await;
    let a = stored(&pool, &c, &storage, "a.png").await;
    let b = stored(&pool, &c, &storage, "b.png").await;
    let doc = holding(&pool, &c, a.id).await;
    set_file(&pool, &c, doc, b.id).await;
    let job = cleanup(&pool, a.id).await.0;
    // As if it had run out of attempts while storage was down.
    sqlx::query(
        "UPDATE jobs SET state = 'failed', attempts = max_attempts, finished_at = now(),
                last_error = 'storage is down' WHERE id = $1",
    )
    .bind(job)
    .execute(&pool)
    .await
    .unwrap();
    let _app = serve(&pool, storage).await;
    let sweep: (Uuid, OffsetDateTime) = sqlx::query_as(
        "SELECT id, run_at FROM jobs WHERE kind = 'file_cleanup_sweep' AND state = 'queued'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let hour = time::Duration::HOUR;
    assert!(
        sweep.1 > OffsetDateTime::now_utc() + hour - time::Duration::minutes(1),
        "startup queues the first sweep an hour on"
    );
    // Another server starting, with no workers of its own.
    let _again = tachyon_api::router(pool.clone()).await.unwrap();
    let sweeps: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = 'file_cleanup_sweep' AND state IN ('queued', 'running')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sweeps, 1, "a second server start doesn't add another");

    sqlx::query("UPDATE jobs SET run_at = now() WHERE id = $1")
        .bind(sweep.0)
        .execute(&pool)
        .await
        .unwrap();
    until("the sweep ran", async || {
        jobs::status(&pool, sweep.0).await.unwrap().unwrap().state == "done"
    })
    .await;
    // The worker may have run it once since, failing again; it's queued either way.
    until("the cleanup is queued with fresh attempts", async || {
        let (id, state, attempts) = cleanup(&pool, a.id).await;
        id == job && state == "queued" && attempts <= 1
    })
    .await;
    let next: OffsetDateTime = sqlx::query_scalar(
        "SELECT run_at FROM jobs WHERE kind = 'file_cleanup_sweep' AND state = 'queued'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let now = OffsetDateTime::now_utc();
    assert!(
        next > now + hour - time::Duration::minutes(1) && next <= now + hour,
        "the next sweep is an hour on: {next}"
    );
}

/// Moves `file`'s reservation two hours back, past its grace.
async fn backdate(pool: &PgPool, file: Uuid) {
    sqlx::query("UPDATE files SET created_at = now() - interval '2 hours' WHERE id = $1")
        .bind(file)
        .execute(pool)
        .await
        .unwrap();
}

async fn job_state(pool: &PgPool, job: Uuid) -> (String, i32) {
    let s = jobs::status(pool, job).await.unwrap().unwrap();
    (s.state, s.attempts)
}

/// Marks `job` failed for good, as if it had run out of attempts.
async fn fail_for_good(pool: &PgPool, job: Uuid) {
    sqlx::query(
        "UPDATE jobs SET state = 'failed', attempts = max_attempts, finished_at = now(),
                locked_until = NULL, last_error = 'storage is down' WHERE id = $1",
    )
    .bind(job)
    .execute(pool)
    .await
    .unwrap();
}

/// Queues a cleanup with `payload`, due now.
async fn queue_cleanup(pool: &PgPool, payload: serde_json::Value, key: Option<String>) -> Uuid {
    let mut conn = pool.acquire().await.unwrap();
    jobs::enqueue(
        &mut conn,
        &NewJob {
            kind: CLEANUP,
            document_id: None,
            collection_id: None,
            payload,
            run_at: OffsetDateTime::now_utc(),
            dedupe_key: key,
            created_by: None,
            key_id: None,
            max_attempts: Some(CLEANUP_ATTEMPTS),
        },
    )
    .await
    .unwrap()
}

/// The queued sweep's ID.
async fn queued_sweep(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("SELECT id FROM jobs WHERE kind = 'file_cleanup_sweep' AND state = 'queued'")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Runs the queued sweep now and waits for it to finish one way or another.
async fn sweep_now(pool: &PgPool) -> Uuid {
    let sweep = queued_sweep(pool).await;
    due_now(pool, sweep).await;
    until("the sweep ran", async || {
        let (state, attempts) = job_state(pool, sweep).await;
        state == "done" || (state == "queued" && attempts > 0)
    })
    .await;
    sweep
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_version_keeps_a_file_until_its_pruned(pool: PgPool) {
    let (storage, root) = storage(0);
    let c = seed_with(&pool, json!({"versions": {"maxVersions": 1}})).await;
    let a = stored(&pool, &c, &storage, "a.png").await;
    let b = stored(&pool, &c, &storage, "b.png").await;
    let doc = holding(&pool, &c, a.id).await;
    let record = async || {
        let mut conn = pool.acquire().await.unwrap();
        tachyon_db::versions::record_version(
            &mut conn,
            doc,
            &c,
            DocCopy::Published,
            &Map::new(),
            None,
        )
        .await
        .unwrap();
    };
    record().await;
    set_file(&pool, &c, doc, b.id).await;
    let _app = serve(&pool, storage).await;
    // Long enough for the worker to have run anything due.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        files::get(&pool, a.id).await.unwrap().state,
        "live",
        "its version holds it"
    );
    let jobs_for_a: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND payload->>'file' = $2 AND state <> 'cancelled'",
    )
    .bind(CLEANUP)
    .bind(a.id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs_for_a, 0, "no cleanup while a version holds it");
    for path in objects(&root, &a) {
        assert!(path.exists(), "{} kept", path.display());
    }
    // The next version prunes the one holding A.
    record().await;
    until("A's row is deleted", async || {
        !row_exists(&pool, a.id).await
    })
    .await;
    for path in objects(&root, &a) {
        assert!(!path.exists(), "{} deleted", path.display());
    }
    for path in objects(&root, &b) {
        assert!(path.exists(), "{} kept", path.display());
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_pending_file_within_its_grace_is_kept(pool: PgPool) {
    let (storage, root) = storage(0);
    let c = seed(&pool).await;
    let p = stored(&pool, &c, &storage, "p.png").await;
    let (job, _, _) = cleanup(&pool, p.id).await;
    due_now(&pool, job).await;
    let _app = serve(&pool, storage).await;
    until("the cleanup waits to retry", async || {
        job_state(&pool, job).await == ("queued".into(), 1)
    })
    .await;
    assert_eq!(files::get(&pool, p.id).await.unwrap().state, "pending");
    for path in objects(&root, &p) {
        assert!(path.exists(), "{} kept", path.display());
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_pending_file_past_its_grace_is_deleted(pool: PgPool) {
    let (storage, root) = storage(0);
    let c = seed(&pool).await;
    let p = stored(&pool, &c, &storage, "p.png").await;
    backdate(&pool, p.id).await;
    let (job, _, _) = cleanup(&pool, p.id).await;
    due_now(&pool, job).await;
    let _app = serve(&pool, storage).await;
    until("the cleanup is done", async || {
        job_state(&pool, job).await.0 == "done"
    })
    .await;
    assert!(!row_exists(&pool, p.id).await, "its row is deleted");
    for path in objects(&root, &p) {
        assert!(!path.exists(), "{} deleted", path.display());
    }
    // Nothing can take it now.
    let mut tx = pool.begin().await.unwrap();
    assert!(files::go_live_in(&mut tx, c.id, p.id).await.is_err());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_pending_file_taken_before_its_cleanup_is_kept(pool: PgPool) {
    let (storage, root) = storage(0);
    let c = seed(&pool).await;
    let p = stored(&pool, &c, &storage, "p.png").await;
    backdate(&pool, p.id).await;
    let (job, _, _) = cleanup(&pool, p.id).await;
    holding(&pool, &c, p.id).await;
    let _app = serve(&pool, storage).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(job_state(&pool, job).await, ("cancelled".into(), 0));
    assert_eq!(files::get(&pool, p.id).await.unwrap().state, "live");
    for path in objects(&root, &p) {
        assert!(path.exists(), "{} kept", path.display());
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn keys_cleanups_delete_only_the_named_keys(pool: PgPool) {
    let (default, root) = storage(0);
    let (other, other_root) = named("other", 0);
    let c = seed(&pool).await;
    // A live file whose size a stale cleanup still names.
    let b = stored(&pool, &c, &default, "b.png").await;
    holding(&pool, &c, b.id).await;
    let dir = temp_root();
    let source = dir.join("source");
    std::fs::write(&source, b"x").unwrap();
    let (stale, kept) = ("images/x/stale.png", "images/x/kept.png");
    for s in [&default, &other] {
        for key in [stale, kept] {
            s.put_file(key, &source, "image/png").await.unwrap();
        }
    }
    let size = sibling(&b.key, &b.names[1]);
    let job = queue_cleanup(
        &pool,
        json!({"keys": [stale, size], "storage": "default"}),
        None,
    )
    .await;
    let unnamed = queue_cleanup(&pool, json!({"keys": [kept]}), None).await;
    let _app = serve_all(&pool, vec![default, other]).await;
    until("the cleanup is done", async || {
        job_state(&pool, job).await.0 == "done"
    })
    .await;
    assert!(!root.join(stale).exists(), "the named key is deleted");
    assert!(root.join(kept).exists(), "another key isn't");
    assert!(
        other_root.join(stale).exists(),
        "nor the same key on another storage"
    );
    assert!(
        root.join(&size).exists(),
        "nor a key a live file still serves"
    );
    until("the unnamed cleanup failed", async || {
        job_state(&pool, unnamed).await == ("failed".into(), 1)
    })
    .await;
    assert!(
        root.join(kept).exists(),
        "a cleanup naming no storage deletes nothing"
    );
    let error: String = sqlx::query_scalar("SELECT last_error FROM jobs WHERE id = $1")
        .bind(unnamed)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(error, "the payload names no storage");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_missing_files_cleanup_is_done(pool: PgPool) {
    let (storage, _root) = storage(0);
    let gone = Uuid::now_v7();
    let job = queue_cleanup(
        &pool,
        json!({ "file": gone }),
        Some(format!("{CLEANUP}:{gone}")),
    )
    .await;
    let _app = serve(&pool, storage).await;
    until("the cleanup is done", async || {
        job_state(&pool, job).await == ("done".into(), 1)
    })
    .await;
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_sweep_requeues_only_what_still_waits(pool: PgPool) {
    // Storage stays down, so whatever is queued again can only fail and wait again.
    let (storage, _root) = storage(usize::MAX);
    let (old, _old_root) = named("old", 0);
    let c = seed(&pool).await;
    // An expired reservation whose cleanup failed.
    let p = stored(&pool, &c, &storage, "p.png").await;
    backdate(&pool, p.id).await;
    let p_job = cleanup(&pool, p.id).await.0;
    fail_for_good(&pool, p_job).await;
    // Gone files: `a` failed twice, `c` failed and has another cleanup queued, `d` sits on a
    // storage the server no longer has.
    let mut gone = Vec::new();
    for (name, on) in [("a.png", &storage), ("c.png", &storage), ("d.png", &old)] {
        let f = stored(&pool, &c, on, name).await;
        let keep = stored(&pool, &c, &storage, &format!("keep-{name}")).await;
        let doc = holding(&pool, &c, f.id).await;
        set_file(&pool, &c, doc, keep.id).await;
        let first = cleanup(&pool, f.id).await.0;
        fail_for_good(&pool, first).await;
        gone.push((f, first));
    }
    let [(a, a_older), (cf, c_failed), (d, d_job)] = gone.try_into().unwrap();
    let a_newer = queue_cleanup(
        &pool,
        json!({ "file": a.id }),
        Some(format!("{CLEANUP}:{}", a.id)),
    )
    .await;
    fail_for_good(&pool, a_newer).await;
    let c_queued = queue_cleanup(
        &pool,
        json!({ "file": cf.id }),
        Some(format!("{CLEANUP}:{}", cf.id)),
    )
    .await;
    // Stale sizes: one on a configured storage, one not.
    let keys = queue_cleanup(
        &pool,
        json!({"keys": ["images/x/s.png"], "storage": "default"}),
        None,
    )
    .await;
    fail_for_good(&pool, keys).await;
    // Refused for its payload: reviving it would only see it refused again.
    let refused = queue_cleanup(
        &pool,
        json!({"keys": "images/x/s.png", "storage": "default"}),
        None,
    )
    .await;
    let keys_old = queue_cleanup(
        &pool,
        json!({"keys": ["images/x/s.png"], "storage": "old"}),
        None,
    )
    .await;
    fail_for_good(&pool, keys_old).await;

    let _app = serve(&pool, storage).await;
    until("the refused cleanup failed", async || {
        job_state(&pool, refused).await == ("failed".into(), 1)
    })
    .await;
    let finished = async || -> Option<OffsetDateTime> {
        sqlx::query_scalar("SELECT finished_at FROM jobs WHERE id = $1")
            .bind(refused)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let refused_at = finished().await;
    sweep_now(&pool).await;
    for (what, job) in [
        ("the expired reservation's", p_job),
        ("the newest of a's", a_newer),
        ("the stale sizes'", keys),
    ] {
        until(&format!("{what} cleanup is queued again"), async || {
            let (state, attempts) = job_state(&pool, job).await;
            state == "queued" && attempts <= 1
        })
        .await;
    }
    for (what, job) in [
        ("a's older cleanup", a_older),
        ("c's failed cleanup, as another is queued", c_failed),
        ("d's cleanup, on a storage not configured", d_job),
        ("stale sizes on a storage not configured", keys_old),
    ] {
        assert_eq!(
            job_state(&pool, job).await,
            ("failed".into(), CLEANUP_ATTEMPTS),
            "{what} stays failed, never run again"
        );
    }
    assert_eq!(
        job_state(&pool, refused).await,
        ("failed".into(), 1),
        "a refused payload isn't revived"
    );
    assert_eq!(finished().await, refused_at, "nor run again");
    assert_ne!(job_state(&pool, c_queued).await.0, "failed");
    assert_eq!(files::get(&pool, d.id).await.unwrap().state, "gone");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_failing_sweep_still_has_a_next_run(pool: PgPool) {
    let (storage, _root) = storage(usize::MAX);
    let c = seed(&pool).await;
    let a = stored(&pool, &c, &storage, "a.png").await;
    let b = stored(&pool, &c, &storage, "b.png").await;
    let doc = holding(&pool, &c, a.id).await;
    set_file(&pool, &c, doc, b.id).await;
    let failed = cleanup(&pool, a.id).await.0;
    fail_for_good(&pool, failed).await;
    // The database refuses the sweep's work: reviving a failed cleanup.
    sqlx::query(
        "CREATE FUNCTION refuse() RETURNS trigger LANGUAGE plpgsql AS
         $$ BEGIN RAISE EXCEPTION 'the database is failing'; END $$",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER no_revive BEFORE UPDATE ON jobs FOR EACH ROW
         WHEN (OLD.kind = 'file_cleanup' AND OLD.state = 'failed' AND NEW.state = 'queued')
         EXECUTE FUNCTION refuse()",
    )
    .execute(&pool)
    .await
    .unwrap();
    let _app = serve(&pool, storage).await;
    let first = sweep_now(&pool).await;
    assert_eq!(
        job_state(&pool, failed).await.0,
        "failed",
        "the work failed"
    );
    let next = queued_sweep(&pool).await;
    assert_ne!(next, first, "but the next sweep is queued");

    // The database refuses even queuing the next one: the sweep itself waits and retries.
    sqlx::query(
        "CREATE TRIGGER no_sweep BEFORE INSERT ON jobs FOR EACH ROW
         WHEN (NEW.kind = 'file_cleanup_sweep') EXECUTE FUNCTION refuse()",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(sweep_now(&pool).await, next);
    let status = jobs::status(&pool, next).await.unwrap().unwrap();
    assert_eq!((status.state.as_str(), status.attempts), ("queued", 1));
    assert_eq!(
        status.max_attempts,
        i32::MAX,
        "it never runs out of attempts"
    );

    // Once the database is back, the same sweep runs and revives the cleanup.
    sqlx::raw_sql("DROP TRIGGER no_sweep ON jobs; DROP TRIGGER no_revive ON jobs")
        .execute(&pool)
        .await
        .unwrap();
    due_now(&pool, next).await;
    until("the sweep is done", async || {
        job_state(&pool, next).await.0 == "done"
    })
    .await;
    until("the cleanup is queued again", async || {
        let (state, attempts) = job_state(&pool, failed).await;
        state == "queued" && attempts <= 1
    })
    .await;
    assert_ne!(queued_sweep(&pool).await, next);
}

// Regenerating sizes when a schema changes, and removing uploads.

/// A test's spool and store, removed with everything in them when the test is done.
struct Dirs {
    spool: PathBuf,
    store: PathBuf,
    _base: tempfile::TempDir,
}

fn dirs() -> Dirs {
    let base = temp_root().0;
    let d = Dirs {
        spool: base.path().join("spool"),
        store: base.path().join("store"),
        _base: base,
    };
    std::fs::create_dir_all(&d.spool).unwrap();
    std::fs::create_dir_all(&d.store).unwrap();
    d
}

fn api_options(d: &Dirs, workers: usize, inline_rows: Option<i64>) -> Options {
    let storages = Storages::open(&std::collections::BTreeMap::from([(
        "default".into(),
        StorageConfig::local(&d.store),
    )]))
    .unwrap();
    Options {
        jobs: JobsConfig {
            workers,
            ..JobsConfig::default()
        },
        uploads: UploadsConfig {
            temp_dir: d.spool.clone(),
            storages: Arc::new(storages),
            ..UploadsConfig::default()
        },
        schema_inline_rows: inline_rows,
        ..Options::default()
    }
}

/// The API over `d` with no workers, signed in as its first Admin.
async fn api(pool: &PgPool, d: &Dirs) -> Router {
    api_with(pool, d, None).await
}

async fn api_with(pool: &PgPool, d: &Dirs, inline_rows: Option<i64>) -> Router {
    let app = tachyon_api::router_with(pool.clone(), api_options(d, 0, inline_rows))
        .await
        .unwrap();
    let setup = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&setup.to_string())).await;
    if res.status != 201 {
        assert_eq!(res.status, 403, "setup: {}", res.json);
    }
    signed_in(app, ADMIN).await
}

/// A job worker over a pool of its own; closing that pool stops it.
async fn worker(pool: &PgPool, d: &Dirs) -> PgPool {
    let own = PgPoolOptions::new()
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    // Its workers run on their own; the router isn't needed.
    let _ = tachyon_api::router_with(own.clone(), api_options(d, 1, None))
        .await
        .unwrap();
    own
}

/// Whether any job is due or running, other than the hourly sweep.
async fn busy(pool: &PgPool) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM jobs WHERE kind <> 'file_cleanup_sweep'
           AND (state = 'running' OR (state = 'queued' AND run_at <= now())))",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let res = send(app, method, uri, body.map(|b| b.to_string()).as_deref()).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

fn media(upload: Value, extra: Value) -> Value {
    let mut schema = json!({
        "slug": "media", "upload": upload,
        "fields": [{"name": "alt", "type": "text"}]
    });
    schema
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    schema
}

async fn change(app: &Router, slug: &str, schema: Value, confirm: bool) -> Res {
    let body = json!({"schema": schema, "confirm": confirm});
    send(
        app,
        "PUT",
        &format!("/api/admin/collections/{slug}"),
        Some(&body.to_string()),
    )
    .await
}

/// The status `url` (a full URL) is served with, and its bytes.
async fn fetch(app: &Router, url: &Value) -> (u16, Vec<u8>) {
    let url = url::Url::parse(url.as_str().unwrap_or_else(|| panic!("no url: {url}"))).unwrap();
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::get(url.path())
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, body)
}

/// The file document `id` holds in its published copy, or else its draft.
async fn held(pool: &PgPool, id: &Value) -> Uuid {
    sqlx::query_scalar("SELECT coalesce(published_file, draft_file) FROM documents WHERE id = $1")
        .bind(id.as_str().unwrap().parse::<Uuid>().unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn changing_image_sizes_regenerates(pool: PgPool) {
    let d = dirs();
    let app = api(&pool, &d).await;
    let thumb = |w: u32, h: u32| json!({"name": "thumbnail", "width": w, "height": h});
    let versions = json!({"versions": {"maxVersions": 10}});
    let schema = media(json!({"imageSizes": [thumb(400, 300)]}), versions.clone());
    ok(&app, "POST", "/api/admin/collections", Some(schema)).await;
    let mut ids = Vec::new();
    for n in 0..3 {
        let res = upload(
            &app,
            "POST",
            "/api/media",
            Some(&png(800, 600)),
            &format!("{n}.png"),
            None,
        )
        .await;
        assert_eq!(res.status, 201, "{}", res.json);
        ids.push(res.json["id"].clone());
    }
    let first = ids[0].as_str().unwrap().to_owned();
    // A second version of the first, holding the same file.
    ok(
        &app,
        "PATCH",
        &format!("/api/media/{first}"),
        Some(json!({"alt": "two"})),
    )
    .await;
    let w = worker(&pool, &d).await;
    until("the first sizes are made", async || !busy(&pool).await).await;
    w.close().await;
    let mut old = Vec::new();
    for id in &ids {
        let file = files::get(&pool, held(&pool, id).await).await.unwrap();
        assert_eq!(file.sizes["thumbnail"]["width"], 400, "{:?}", file.sizes);
        for name in file.names.iter().filter(|n| n.contains("400x300")) {
            old.push(d.store.join(sibling(&file.key, name)));
        }
    }
    assert_eq!(old.len(), 9, "a png, avif and webp each: {old:?}");
    assert!(old.iter().all(|p| p.exists()));

    // The thumbnail shrinks and a card is added.
    let card = json!({"name": "card", "width": 768});
    let schema = media(json!({"imageSizes": [thumb(300, 200), card]}), versions);
    let res = change(&app, "media", schema, false).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM jobs WHERE kind = 'regenerate_sizes' AND state = 'queued'"
        )
        .await,
        1
    );
    // Until the jobs run, reads show the old sizes.
    for id in &ids {
        let doc = ok(
            &app,
            "GET",
            &format!("/api/media/{}", id.as_str().unwrap()),
            None,
        )
        .await;
        assert_eq!(doc["sizes"]["thumbnail"]["width"], 400, "{doc}");
        assert!(doc["sizes"]["card"].is_null(), "{doc}");
    }

    let w = worker(&pool, &d).await;
    until(
        "every size is made again and the old ones deleted",
        async || !busy(&pool).await && old.iter().all(|p| !p.exists()),
    )
    .await;
    w.close().await;
    for id in &ids {
        let doc = ok(
            &app,
            "GET",
            &format!("/api/media/{}", id.as_str().unwrap()),
            None,
        )
        .await;
        let sizes = &doc["sizes"];
        assert_eq!(
            (&sizes["thumbnail"]["width"], &sizes["thumbnail"]["height"]),
            (&json!(300), &json!(200)),
            "{sizes}"
        );
        assert_eq!(sizes["card"]["width"], 768, "{sizes}");
        for size in ["thumbnail", "card"] {
            assert_eq!(fetch(&app, &sizes[size]["url"]).await.0, 200, "{sizes}");
        }
        let file = files::get(&pool, held(&pool, id).await).await.unwrap();
        assert!(
            !file.names.iter().any(|n| n.contains("400x300")),
            "{:?}",
            file.names
        );
    }
    // Every version holding a file says what it has now.
    let stale = count(
        &pool,
        "SELECT count(*) FROM versions WHERE data->'sizes'->'thumbnail'->>'width' <> '300'
           OR data->'sizes'->'card' IS NULL OR data->'sizes'->'card' = 'null'",
    )
    .await;
    assert_eq!(stale, 0);
    // Restoring the first version serves working sizes.
    let list = ok(&app, "GET", &format!("/api/media/{first}/versions"), None).await;
    let docs = list["docs"].as_array().unwrap();
    let oldest = docs.last().unwrap()["id"].as_str().unwrap();
    let restored = ok(
        &app,
        "POST",
        &format!("/api/media/{first}/versions/{oldest}/restore"),
        None,
    )
    .await;
    let restored = match restored.get("doc") {
        Some(doc) => doc.clone(),
        None => restored,
    };
    for size in ["thumbnail", "card"] {
        let url = &restored["sizes"][size]["url"];
        assert_eq!(fetch(&app, url).await.0, 200, "{restored}");
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn regeneration_takes_the_focal_point_from_the_document(pool: PgPool) {
    let d = dirs();
    let app = api(&pool, &d).await;
    let square = json!({"imageSizes": [{"name": "square", "width": 200, "height": 200}]});
    let drafts = json!({"versions": {"drafts": true, "maxVersions": 10}});
    let mut off = square.clone();
    off["focalPoint"] = json!(false);
    let schema = media(off, drafts.clone());
    ok(&app, "POST", "/api/admin/collections", Some(schema)).await;
    let halves = image::RgbImage::from_fn(1000, 500, |x, _| {
        if x < 500 {
            image::Rgb([255, 0, 0])
        } else {
            image::Rgb([0, 0, 255])
        }
    });
    let image = fixtures::encoded(
        &image::DynamicImage::ImageRgb8(halves),
        image::ImageFormat::Png,
    );
    let res = upload(
        &app,
        "POST",
        "/api/media",
        Some(&image),
        "a.png",
        Some(&json!({"focalX": 90})),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let id = res.json["id"].as_str().unwrap().to_owned();
    ok(&app, "POST", &format!("/api/media/{id}/publish"), None).await;
    // Without a focal point, a focal write keeps the file: both copies hold it, each with
    // its own focalX, the published copy's on the blue side, the draft's on the red.
    ok(
        &app,
        "PATCH",
        &format!("/api/media/{id}"),
        Some(json!({"focalX": 10})),
    )
    .await;
    let (published, draft): (Option<Uuid>, Option<Uuid>) =
        sqlx::query_as("SELECT published_file, draft_file FROM documents WHERE id = $1")
            .bind(id.parse::<Uuid>().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(published, draft, "one file for both copies");

    let res = change(&app, "media", media(square, drafts), false).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let w = worker(&pool, &d).await;
    until("the sizes are made again", async || {
        !busy(&pool).await
            && count(
                &pool,
                "SELECT count(*) FROM jobs WHERE kind = 'regenerate_sizes' AND state = 'done'",
            )
            .await
                == 1
    })
    .await;
    w.close().await;
    // The draft's focal point wins: it's the newer edit, and what publishing takes.
    let draft = ok(&app, "GET", &format!("/api/media/{id}?draft=true"), None).await;
    assert_eq!(draft["focalX"], 10, "{draft}");
    let (status, body) = fetch(&app, &draft["sizes"]["square"]["url"]).await;
    assert_eq!(status, 200, "{draft}");
    let made = image::load_from_memory(&body).unwrap().to_rgb8();
    let [r, _, b] = made.get_pixel(100, 100).0;
    assert!(r > 200 && b < 50, "the centre is red: {r} {b}");
    let published = ok(&app, "GET", &format!("/api/media/{id}"), None).await;
    assert_eq!(published["focalX"], 90, "{published}");
    let entry = |doc: &Value| {
        let e = &doc["sizes"]["square"];
        (e["filename"].clone(), e["filesize"].clone())
    };
    assert_eq!(
        entry(&published),
        entry(&draft),
        "one file, one set of sizes"
    );
}

/// `n` live files of `c`, each held by a document of its own, nothing stored.
async fn many(pool: &PgPool, c: &Collection, n: usize) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for i in 0..n {
        let name = format!("{i}.png");
        let f = NewFile {
            collection_id: c.id,
            storage: "default",
            filename: &name,
            scope: "",
            mime_type: "image/png",
            filesize: 3,
            width: Some(4),
            height: Some(3),
            focal: (50.0, 50.0),
            blurhash: None,
            issued_to: None,
            document: None,
        };
        let file = files::reserve(pool, &f, |id, name| {
            object_key(None, "images", id, name).map_err(|e| e.to_string())
        })
        .await
        .unwrap();
        holding(pool, c, file.id).await;
        ids.push(file.id);
    }
    ids.sort();
    ids
}

/// `c`'s schema with a `thumb` size `width` wide.
fn sized(c: &Collection, width: u32) -> CollectionSchema {
    let mut schema = c.schema.0.clone();
    let mut upload = schema.upload.clone().unwrap();
    upload.image_sizes =
        vec![serde_json::from_value(json!({"name": "thumb", "width": width})).unwrap()];
    schema.upload = Some(upload);
    schema
}

async fn apply(pool: &PgPool, schema: &CollectionSchema) {
    tachyon_db::schema_change::apply(
        pool,
        "images",
        schema,
        &tachyon_core::diff::Renames::new(),
        true,
    )
    .await
    .unwrap();
}

/// The walks of `c`: each batch's `after` and state, oldest first.
async fn walks(pool: &PgPool, c: &Collection) -> Vec<(Option<Uuid>, String)> {
    sqlx::query_as(
        "SELECT (payload->>'after')::uuid, state FROM jobs
         WHERE kind = 'regenerate_sizes' AND collection_id = $1 ORDER BY id",
    )
    .bind(c.id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The files of `c` given a size job to make every size again since `since`.
async fn regenerated(pool: &PgPool, since: OffsetDateTime) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT DISTINCT (payload->>'file')::uuid FROM jobs
         WHERE kind = 'image_sizes' AND payload->>'regenerate' = 'true' AND created_at >= $1
         ORDER BY 1",
    )
    .bind(since)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn regeneration_walks_every_batch(pool: PgPool) {
    let c = seed_with(
        &pool,
        json!({"upload": {"imageSizes": [{"name": "thumb", "width": 10}]}}),
    )
    .await;
    let ids = many(&pool, &c, 250).await;
    let since = OffsetDateTime::now_utc();
    apply(&pool, &sized(&c, 20)).await;
    let (store, _root) = storage(0);
    let _app = serve(&pool, store).await;
    until("the walk is done", async || {
        walks(&pool, &c)
            .await
            .iter()
            .filter(|(_, s)| s == "done")
            .count()
            == 3
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let w = walks(&pool, &c).await;
    assert_eq!(
        w,
        [
            (None, "done".into()),
            (Some(ids[99]), "done".into()),
            (Some(ids[199]), "done".into()),
        ],
        "three batches, each from where the last stopped, and no fourth"
    );
    let keyed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = 'regenerate_sizes'
         AND dedupe_key = 'regenerate_sizes:' || $1::text",
    )
    .bind(c.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(keyed, 3, "every batch can be replaced by a newer walk");
    assert_eq!(regenerated(&pool, since).await, ids);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_change_mid_walk_starts_it_again(pool: PgPool) {
    let c = seed_with(
        &pool,
        json!({"upload": {"imageSizes": [{"name": "thumb", "width": 10}]}}),
    )
    .await;
    let ids = many(&pool, &c, 150).await;
    apply(&pool, &sized(&c, 20)).await;
    // As if its first batch had run: the walk goes on from the 100th file.
    sqlx::query(
        "UPDATE jobs SET payload = jsonb_build_object('after', $2::text)
         WHERE kind = 'regenerate_sizes' AND collection_id = $1",
    )
    .bind(c.id)
    .bind(ids[99].to_string())
    .execute(&pool)
    .await
    .unwrap();
    let since = OffsetDateTime::now_utc();
    let c = collections::get_by_slug(&pool, "images").await.unwrap();
    apply(&pool, &sized(&c, 30)).await;
    assert_eq!(
        walks(&pool, &c).await,
        [(Some(ids[99]), "cancelled".into()), (None, "queued".into()),],
        "the newer walk replaces the older, from the start"
    );
    let (store, _root) = storage(0);
    let _app = serve(&pool, store).await;
    until("the new walk is done", async || {
        walks(&pool, &c)
            .await
            .iter()
            .filter(|(_, s)| s == "done")
            .count()
            == 2
    })
    .await;
    assert_eq!(
        regenerated(&pool, since).await,
        ids,
        "every file, the first 100 too"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn other_upload_options_dont_regenerate(pool: PgPool) {
    let c = seed_with(
        &pool,
        json!({"upload": {"imageSizes": [{"name": "thumb", "width": 10}]}}),
    )
    .await;
    many(&pool, &c, 1).await;
    let mut schema = c.schema.0.clone();
    let mut upload = schema.upload.clone().unwrap();
    upload.mime_types = vec!["image/*".into()];
    upload.static_dir = Some("pictures".into());
    schema.upload = Some(upload);
    apply(&pool, &schema).await;
    assert_eq!(walks(&pool, &c).await, []);
    // Without sizes before or after, an option sizes are made with remakes nothing.
    let mut schema = collections::get_by_slug(&pool, "images")
        .await
        .unwrap()
        .schema
        .0;
    let mut upload = schema.upload.clone().unwrap();
    upload.image_sizes = Vec::new();
    schema.upload = Some(upload.clone());
    apply(&pool, &schema).await;
    let dropped = walks(&pool, &c).await.len();
    assert_eq!(dropped, 1, "dropping the sizes walks, to delete them");
    upload.with_metadata = true;
    schema.upload = Some(upload);
    apply(&pool, &schema).await;
    assert_eq!(walks(&pool, &c).await.len(), dropped);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn removing_upload_needs_confirm(pool: PgPool) {
    let d = dirs();
    let app = api(&pool, &d).await;
    let versions = json!({"versions": {"maxVersions": 10}});
    let schema = media(json!(true), versions);
    ok(&app, "POST", "/api/admin/collections", Some(schema)).await;
    let posts = json!({"slug": "posts", "fields": [
        {"name": "title", "type": "text"},
        {"name": "cover", "type": "upload", "relationTo": "media"}]});
    ok(&app, "POST", "/api/admin/collections", Some(posts)).await;
    let a = upload(&app, "POST", "/api/media", Some(&png(4, 3)), "a.png", None).await;
    assert_eq!(a.status, 201, "{}", a.json);
    let b = upload(&app, "POST", "/api/media", Some(&png(4, 3)), "b.png", None).await;
    assert_eq!(b.status, 201, "{}", b.json);
    let (a, b) = (a.json["id"].clone(), b.json["id"].clone());
    let (a_id, b_id) = (a.as_str().unwrap(), b.as_str().unwrap());
    let first_b = held(&pool, &b).await;
    // B's first file is now held only by a version of B.
    let res = upload(
        &app,
        "PATCH",
        &format!("/api/media/{b_id}"),
        Some(&png(5, 3)),
        "c.png",
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    // A is in the trash.
    ok(&app, "DELETE", &format!("/api/media/{a_id}"), None).await;
    let held_files: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM files ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(held_files.len(), 3);
    assert!(held_files.contains(&first_b));
    // An upload under way.
    let c = collections::get_by_slug(&pool, "media").await.unwrap();
    let pending = files::reserve(
        &pool,
        &NewFile {
            collection_id: c.id,
            storage: "default",
            filename: "d.png",
            scope: "",
            mime_type: "image/png",
            filesize: 3,
            width: Some(4),
            height: Some(3),
            focal: (50.0, 50.0),
            blurhash: None,
            issued_to: None,
            document: None,
        },
        |id, name| object_key(None, "media", id, name).map_err(|e| e.to_string()),
    )
    .await
    .unwrap();

    // Upload fields point at media: no confirm passes that.
    let off = json!({"slug": "media", "versions": {"maxVersions": 10},
                     "fields": [{"name": "alt", "type": "text"}]});
    let referrer = "media can't stop holding files while these upload fields point at it: posts.cover; change them first";
    for confirm in [false, true] {
        let res = change(&app, "media", off.clone(), confirm).await;
        assert_eq!(res.status, 409, "{}", res.json);
        assert_eq!(res.json["detail"], referrer, "{}", res.json);
    }
    let posts = json!({"slug": "posts", "fields": [{"name": "title", "type": "text"}]});
    let res = change(&app, "posts", posts, true).await;
    assert_eq!(res.status, 200, "{}", res.json);

    // Without confirm: a 409 counting the documents holding files, trashed and
    // version-only ones too, and nothing changes.
    let res = change(&app, "media", off.clone(), false).await;
    assert_eq!(res.status, 409, "{}", res.json);
    assert_eq!(
        res.json["detail"],
        "2 documents hold files; they'll be removed. This change removes or retypes fields, \
         or leaves documents that won't validate. Send it again with \"confirm\": true to go ahead.",
        "{}",
        res.json
    );
    assert_eq!(res.json["preview"]["holdingFiles"], 2, "{}", res.json);
    let stored = ok(&app, "GET", "/api/admin/collections/media", None).await;
    assert_eq!(stored["schema"]["upload"], true, "{stored}");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM files WHERE state = 'live'").await,
        3
    );

    // With it: every file reference goes, and every file is released.
    let res = change(&app, "media", off, true).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM documents
             WHERE published_file IS NOT NULL OR draft_file IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM versions WHERE file_id IS NOT NULL"
        )
        .await,
        0
    );
    assert!(count(&pool, "SELECT count(*) FROM versions").await >= 3);
    for id in held_files.iter().chain([&pending.id]) {
        let row = files::get(&pool, *id).await.unwrap();
        assert_eq!(row.state, "gone", "{id}");
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = 'file_cleanup' AND state = 'queued'
             AND payload->>'file' = $1",
        )
        .bind(id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "one cleanup for {id}");
    }
    // The pending file's cleanup keeps its grace: an upload still storing it may land.
    let (pending_job, _, _) = cleanup(&pool, pending.id).await;
    let at = jobs::status(&pool, pending_job)
        .await
        .unwrap()
        .unwrap()
        .run_at;
    assert!(at > OffsetDateTime::now_utc() + time::Duration::minutes(50));
    let doc = ok(&app, "GET", &format!("/api/media/{b_id}"), None).await;
    assert!(doc.get("filename").is_none(), "{doc}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn removing_upload_from_a_big_collection_runs_as_a_job(pool: PgPool) {
    let d = dirs();
    let app = api_with(&pool, &d, Some(0)).await;
    ok(
        &app,
        "POST",
        "/api/admin/collections",
        Some(media(json!(true), json!({}))),
    )
    .await;
    let res = upload(&app, "POST", "/api/media", Some(&png(4, 3)), "a.png", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let file = files::get(&pool, held(&pool, &res.json["id"]).await)
        .await
        .unwrap();
    let object = d.store.join(&file.key);
    assert!(object.exists());
    let off = json!({"slug": "media", "fields": [{"name": "alt", "type": "text"}]});
    let res = change(&app, "media", off, true).await;
    assert_eq!(res.status, 202, "{}", res.json);
    let w = worker(&pool, &d).await;
    until("the file is cleaned up", async || {
        !row_exists(&pool, file.id).await && !object.exists()
    })
    .await;
    w.close().await;
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM documents WHERE published_file IS NOT NULL"
        )
        .await,
        0
    );
}
