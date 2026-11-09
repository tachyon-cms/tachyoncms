//! The jobs that delete stored files, run by a real worker against local storage.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use object_store::local::LocalFileSystem;
use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use serde_json::{Map, json};
use sqlx::PgPool;
use tachyon_api::{JobsConfig, Options, UploadsConfig};
use tachyon_core::CollectionSchema;
use tachyon_db::collections::{self, Collection};
use tachyon_db::documents::{self, FileChange, Prepared};
use tachyon_db::files::{self, CLEANUP, CLEANUP_ATTEMPTS, FileRow, NewFile};
use tachyon_db::jobs::{self, NewJob};
use tachyon_db::{DbError, DocCopy};
use tachyon_media::storage::{Storage, StorageConfig, Storages, object_key};
use time::OffsetDateTime;
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

fn temp_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tachyon-file-jobs-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `default` storage in a fresh directory, its deletes failing `fails` times.
fn storage(fails: usize) -> (Arc<Storage>, PathBuf) {
    named("default", fails)
}

/// Storage `name` in a fresh directory, its deletes failing `fails` times.
fn named(name: &str, fails: usize) -> (Arc<Storage>, PathBuf) {
    let root = temp_root();
    let store = Flaky {
        inner: LocalFileSystem::new_with_prefix(&root).unwrap(),
        fails: AtomicUsize::new(fails),
    };
    let storage = Storage::with_store(name, Arc::new(store), None, StorageConfig::local(&root));
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
    let source = temp_root().join("source");
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

/// Polls `check` until it holds, for up to 20 s.
async fn until<F: AsyncFn() -> bool>(what: &str, check: F) {
    for _ in 0..400 {
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
    let source = temp_root().join("source");
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
    let (old, _) = named("old", 0);
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
