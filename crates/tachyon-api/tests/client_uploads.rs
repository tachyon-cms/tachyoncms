//! Files a client puts straight into a bucket (`/api/upload-instructions`, then a create or
//! update naming the file), and files served from a bucket through short-lived links.
//!
//! The bucket is a local store with a fake signer: a "signed" URL names the method, the key
//! and what it was signed with, and a client's `PUT` to it is a write into the store's root.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use common::{ADMIN, PASSWORD, Res, add_user, assert_problem, png, send, signed_in, upload};
use http_body_util::BodyExt;
use object_store::local::LocalFileSystem;
use object_store::path::Path as Key;
use object_store::signer::{Method, SignedUrlOptions, Signer};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_api::{JobsConfig, Options, UploadsConfig};
use tachyon_db::files::{self, CLEANUP};
use tachyon_media::storage::{SignedDownloads, Storage, StorageConfig, Storages};
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

/// Signs nothing: its URLs say what they were made for.
#[derive(Debug)]
struct FakeSigner;

#[async_trait::async_trait]
impl Signer for FakeSigner {
    async fn signed_url(
        &self,
        method: Method,
        path: &Key,
        expires_in: Duration,
    ) -> object_store::Result<Url> {
        self.signed_url_opts(method, path, expires_in, &SignedUrlOptions::default())
            .await
    }

    async fn signed_url_opts(
        &self,
        method: Method,
        path: &Key,
        expires_in: Duration,
        options: &SignedUrlOptions,
    ) -> object_store::Result<Url> {
        let mut url = Url::parse("https://bucket.test/").unwrap();
        url.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .extend(path.as_ref().split('/'));
        url.query_pairs_mut()
            .append_pair("method", method.as_str())
            .append_pair("expires", &expires_in.as_secs().to_string());
        if let Some(t) = options.signed_headers.get("content-type") {
            url.query_pairs_mut()
                .append_pair("content-type", t.to_str().unwrap());
        }
        Ok(url)
    }
}

/// A test's spool directory, its local `default` storage and its "bucket".
struct Dirs {
    temp: PathBuf,
    root: PathBuf,
    bucket: PathBuf,
    pdfs: PathBuf,
    _base: tempfile::TempDir,
}

fn dirs() -> Dirs {
    let base = tempfile::Builder::new()
        .prefix("tachyon-client-")
        .tempdir()
        .unwrap();
    let d = Dirs {
        temp: base.path().join("spool"),
        root: base.path().join("store"),
        bucket: base.path().join("bucket"),
        pdfs: base.path().join("pdfs"),
        _base: base,
    };
    for dir in [&d.temp, &d.root, &d.bucket, &d.pdfs] {
        std::fs::create_dir_all(dir).unwrap();
    }
    d
}

/// A bucket over the local directory `root`, taking client uploads and signing downloads
/// of `signed` types (all with an empty list).
fn bucket(name: &str, root: &Path, signed: Vec<String>) -> Arc<Storage> {
    let mut c = StorageConfig::local(root);
    c.client_uploads = true;
    c.public_url = Some(Url::parse("https://media.tachyoncms.com").unwrap());
    c.signed_downloads = SignedDownloads::On {
        mime_types: signed,
        expires_in: Duration::from_secs(600),
    };
    let store = LocalFileSystem::new_with_prefix(root).unwrap();
    Arc::new(Storage::with_store(name, Arc::new(store), None, c).with_signer(Arc::new(FakeSigner)))
}

fn options(d: &Dirs, workers: usize) -> Options {
    let local = Storage::open("default", &StorageConfig::local(&d.root)).unwrap();
    let storages = Storages(HashMap::from([
        ("default".into(), Arc::new(local)),
        ("bucket".into(), bucket("bucket", &d.bucket, Vec::new())),
        (
            "pdfs".into(),
            bucket("pdfs", &d.pdfs, vec!["application/pdf".into()]),
        ),
    ]));
    Options {
        jobs: JobsConfig {
            workers,
            ..JobsConfig::default()
        },
        uploads: UploadsConfig {
            temp_dir: d.temp.clone(),
            storages: Arc::new(storages),
            ..UploadsConfig::default()
        },
        ..Options::default()
    }
}

async fn router(pool: &PgPool, d: &Dirs) -> Router {
    tachyon_api::router_with(pool.clone(), options(d, 0))
        .await
        .unwrap()
}

async fn admin(pool: &PgPool, d: &Dirs) -> Router {
    let app = router(pool, d).await;
    let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "setup: {}", res.json);
    signed_in(app, ADMIN).await
}

async fn as_role(pool: &PgPool, d: &Dirs, email: &str, role: &str) -> Router {
    add_user(pool, email, role).await;
    signed_in(router(pool, d).await, email).await
}

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> Res {
    send(app, method, uri, Some(&body.to_string())).await
}

async fn ok(app: &Router, method: &str, uri: &str, body: Value) -> Value {
    let res = call(app, method, uri, body).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

/// Creates `slug`, an upload collection on the bucket with an `alt` field, `extra` merged
/// over it (`upload` included).
async fn collection(app: &Router, slug: &str, extra: Value) {
    let mut schema = json!({
        "slug": slug, "upload": {"storage": "bucket"},
        "fields": [{"name": "alt", "type": "text"}]
    });
    schema
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    ok(app, "POST", "/api/admin/collections", schema).await;
}

/// Instructions for `filename` in `slug`, claimed as `mime` and `size` bytes.
async fn instructions(app: &Router, slug: &str, filename: &str, mime: &str, size: u64) -> Res {
    let body = json!({"filename": filename, "mimeType": mime, "filesize": size});
    call(
        app,
        "POST",
        &format!("/api/{slug}/upload-instructions"),
        body,
    )
    .await
}

/// What a client does with instructions: `PUT`s `bytes` to their URL, here a write into
/// the bucket's directory `root`. Returns the file as the create is to name it.
fn put_to(root: &Path, instructions: &Value, bytes: &[u8]) -> Value {
    let url = Url::parse(instructions["request"]["url"].as_str().unwrap()).unwrap();
    let key: Vec<String> = url.path_segments().unwrap().map(percent_decode).collect();
    let path = root.join(key.join("/"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    instructions["file"].clone()
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes())
        .next()
        .unwrap()
        .1
        .into_owned()
}

/// The key a URL from the fake signer was signed for.
fn signed_key(url: &str) -> String {
    let url = Url::parse(url).unwrap();
    let segments: Vec<String> = url.path_segments().unwrap().map(percent_decode).collect();
    segments.join("/")
}

fn query(url: &str, name: &str) -> Option<String> {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// Every file `slug` has, by state.
async fn file_states(pool: &PgPool, slug: &str) -> Vec<(Uuid, String, String)> {
    sqlx::query_as(
        "SELECT f.id, f.state, f.key FROM files f JOIN collections c ON c.id = f.collection_id
         WHERE c.slug = $1 ORDER BY f.id",
    )
    .bind(slug)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn until<F: AsyncFn() -> bool>(what: &str, check: F) {
    for _ in 0..400 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting until {what}");
}

/// A refusal's body, without what differs between any two.
fn refusal(res: &Res) -> Value {
    let mut body = res.json.clone();
    body.as_object_mut().unwrap().remove("requestId");
    body
}

struct Raw {
    status: u16,
    headers: axum::http::HeaderMap,
}

impl Raw {
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .map_or("", |v| v.to_str().unwrap_or_default())
    }
}

async fn fetch(app: &Router, method: &str, uri: &str) -> Raw {
    let req = Request::builder().method(method).uri(uri);
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let _ = res.into_body().collect().await.unwrap();
    Raw { status, headers }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn instructions_shape_and_rights(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "media",
        json!({"upload": {"storage": "bucket", "mimeTypes": ["image/*"], "filesizeLimit": 1000}}),
    )
    .await;
    collection(&app, "local", json!({"upload": true})).await;
    let viewer = as_role(&pool, &d, "viewer@example.com", "viewer").await;
    let anon = router(&pool, &d).await;

    let res = instructions(&app, "media", "a b.png", "image/png", 10).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let rows = file_states(&pool, "media").await;
    assert_eq!(rows.len(), 1);
    let (id, state, key) = &rows[0];
    assert_eq!(state, "pending");
    assert_eq!(key, &format!("media/{id}/a b.png"));
    let url = res.json["request"]["url"].as_str().unwrap().to_owned();
    assert_eq!(
        res.json,
        json!({
            "type": "http",
            "request": {"url": url, "method": "PUT", "headers": {"content-type": "image/png"}},
            "file": {"filename": "a b.png", "mimeType": "image/png", "filesize": 10}
        })
    );
    assert_eq!(signed_key(&url), *key);
    assert_eq!(query(&url, "method").as_deref(), Some("PUT"));
    assert_eq!(query(&url, "expires").as_deref(), Some("900"));
    assert_eq!(query(&url, "content-type").as_deref(), Some("image/png"));
    let issued: (Option<Uuid>, Option<Uuid>) =
        sqlx::query_as("SELECT issued_to, issued_key FROM files WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let admin_id: Uuid = sqlx::query_scalar("SELECT user_id FROM accounts WHERE email = $1")
        .bind(ADMIN)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(issued, (Some(admin_id), None));

    // The same through the route naming its collection in the body; the name is numbered.
    let body = json!({"collectionSlug": "media", "filename": "a b.png",
                      "mimeType": "image/png", "filesize": 10});
    let res = call(&app, "POST", "/api/upload-instructions", body).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["file"]["filename"], "a b-1.png");

    // What the client claims is checked as an upload would be.
    let res = instructions(&app, "media", "big.png", "image/png", 1001).await;
    assert_problem(&res, 413);
    assert!(res.json["detail"].as_str().unwrap().contains("1000"));
    assert_problem(
        &instructions(&app, "media", "a.html", "text/html", 10).await,
        415,
    );
    assert_problem(
        &instructions(&app, "media", "a.png", "image/png\r\nx: y", 10).await,
        415,
    );

    // Who may ask, and where.
    assert_problem(
        &instructions(&viewer, "media", "a.png", "image/png", 10).await,
        403,
    );
    assert_problem(
        &instructions(&anon, "media", "a.png", "image/png", 10).await,
        401,
    );
    let res = instructions(&app, "local", "a.png", "image/png", 10).await;
    assert_problem(&res, 400);
    assert_problem(
        &instructions(&app, "nope", "a.png", "image/png", 10).await,
        404,
    );
    // Nothing was reserved for any refusal.
    assert_eq!(file_states(&pool, "media").await.len(), 2);
    assert!(file_states(&pool, "local").await.is_empty());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn client_upload_creates_a_document(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let image = png(3, 2);
    let res = instructions(&app, "media", "a.png", "image/png", image.len() as u64).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let file = put_to(&d.bucket, &res.json, &image);
    let doc = ok(
        &app,
        "POST",
        "/api/media",
        json!({"file": file, "alt": "x"}),
    )
    .await;
    assert_eq!(doc["filename"], "a.png");
    assert_eq!(doc["mimeType"], "image/png");
    assert_eq!(
        (doc["width"].clone(), doc["height"].clone()),
        (json!(3), json!(2))
    );
    assert_eq!(doc["alt"], "x");
    let rows = file_states(&pool, "media").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "live");
    assert!(d.bucket.join(&rows[0].2).exists());
    // No cleanup is left waiting for it.
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND state = 'queued' AND payload->>'file' = $2",
    )
    .bind(CLEANUP)
    .bind(rows[0].0.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(queued, 0);
    // Claimed once, it can't be claimed again.
    let again = call(&app, "POST", "/api/media", json!({"file": file})).await;
    assert_problem(&again, 422);
    assert_eq!(again.json["errors"][0]["path"], "file");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn client_upload_is_verified_after_put(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "media",
        json!({"upload": {"storage": "bucket", "mimeTypes": ["image/*"], "filesizeLimit": 100},
               "fields": [{"name": "alt", "type": "text", "required": true}]}),
    )
    .await;
    // Given up before the answer: its row is gone and its cleanup is due, so a crash now
    // couldn't leave the object behind.
    let gone = async |instructions: &Value| {
        let key = signed_key(instructions["request"]["url"].as_str().unwrap());
        let (state, due): (String, bool) = sqlx::query_as(
            "SELECT f.state, EXISTS (SELECT 1 FROM jobs j WHERE j.kind = $2 AND j.state = 'queued'
                                     AND j.payload->>'file' = f.id::text AND j.run_at <= now())
             FROM files f WHERE f.key = $1",
        )
        .bind(&key)
        .bind(CLEANUP)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((state.as_str(), due), ("gone", true), "{key}");
        key
    };
    let mut keys = Vec::new();

    // HTML said to be a PNG: sniffed, refused, and nothing kept.
    let html = b"<!doctype html><script>alert(1)</script>";
    let given = instructions(&app, "media", "a.png", "image/png", html.len() as u64).await;
    let file = put_to(&d.bucket, &given.json, html);
    let res = call(
        &app,
        "POST",
        "/api/media",
        json!({"file": file, "alt": "x"}),
    )
    .await;
    assert_problem(&res, 415);
    assert!(res.json["detail"].as_str().unwrap().contains("text/html"));
    keys.push(gone(&given.json).await);

    // Bigger in the bucket than claimed, and than the limit: the bucket's size counts.
    let given = instructions(&app, "media", "b.png", "image/png", 10).await;
    let file = put_to(&d.bucket, &given.json, &png(20, 20));
    let res = call(
        &app,
        "POST",
        "/api/media",
        json!({"file": file, "alt": "x"}),
    )
    .await;
    assert_problem(&res, 413);
    keys.push(gone(&given.json).await);

    // A payload that fails validation gives the file up as well.
    let image = png(1, 1);
    let given = instructions(&app, "media", "c.png", "image/png", image.len() as u64).await;
    let file = put_to(&d.bucket, &given.json, &image);
    let res = call(&app, "POST", "/api/media", json!({"file": file})).await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "alt");
    keys.push(gone(&given.json).await);

    // Nothing was put, so there's nothing to take.
    let given = instructions(&app, "media", "d.png", "image/png", 10).await;
    let res = call(
        &app,
        "POST",
        "/api/media",
        json!({"file": given.json["file"], "alt": "x"}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "file");
    keys.push(gone(&given.json).await);

    // The cleanups run, and take every object with them.
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    let root = d.bucket.clone();
    until("every object is deleted", async || {
        keys.iter().all(|k| !root.join(k).exists())
    })
    .await;
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn client_upload_cant_be_claimed_by_another_user(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    collection(&app, "other", json!({})).await;
    let amy = as_role(&pool, &d, "amy@example.com", "author").await;
    let image = png(1, 1);
    let res = instructions(&app, "media", "a.png", "image/png", image.len() as u64).await;
    let file = put_to(&d.bucket, &res.json, &image);

    let stranger = call(&amy, "POST", "/api/media", json!({"file": file})).await;
    assert_problem(&stranger, 422);
    let mut made_up = file.clone();
    made_up["filename"] = "made-up.png".into();
    let invented = call(&app, "POST", "/api/media", json!({"file": made_up})).await;
    assert_eq!(refusal(&stranger), refusal(&invented));
    assert_eq!(stranger.json["errors"][0]["path"], "file");
    // Another collection's, and one past its grace, are the same refusal.
    let elsewhere = call(&app, "POST", "/api/other", json!({"file": file})).await;
    assert_eq!(refusal(&elsewhere), refusal(&invented));
    sqlx::query("UPDATE files SET created_at = now() - interval '2 hours'")
        .execute(&pool)
        .await
        .unwrap();
    let late = call(&app, "POST", "/api/media", json!({"file": file})).await;
    assert_eq!(refusal(&late), refusal(&invented));
    sqlx::query("UPDATE files SET created_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    // The refusals left it for its owner.
    assert_eq!(file_states(&pool, "media").await[0].1, "pending");
    let doc = ok(&app, "POST", "/api/media", json!({"file": file})).await;
    assert_eq!(doc["filename"], "a.png");

    // An API key is its own claimant: the same user's session can't take its file.
    let key = ok(
        &app,
        "POST",
        "/api/auth/keys",
        json!({"name": "uploader", "scopes": ["*:write", "*:read"]}),
    )
    .await;
    let keyed = common::with_key(router(&pool, &d).await, key["key"].as_str().unwrap());
    let res = instructions(&keyed, "media", "k.png", "image/png", image.len() as u64).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let file = put_to(&d.bucket, &res.json, &image);
    let session = call(&app, "POST", "/api/media", json!({"file": file})).await;
    assert_eq!(refusal(&session), refusal(&invented));
    let res = call(&keyed, "POST", "/api/media", json!({"file": file})).await;
    assert_eq!(res.status, 201, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_claim_is_taken_once(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let image = png(1, 1);
    let res = instructions(&app, "media", "a.png", "image/png", image.len() as u64).await;
    let file = put_to(&d.bucket, &res.json, &image);
    let body = json!({"file": file});
    let (a, b) = tokio::join!(
        call(&app, "POST", "/api/media", body.clone()),
        call(&app, "POST", "/api/media", body.clone()),
    );
    let mut statuses = [a.status.as_u16(), b.status.as_u16()];
    statuses.sort();
    assert_eq!(statuses, [201, 422], "{} {}", a.json, b.json);
    let lost = if a.status == 422 { &a } else { &b };
    assert_eq!(lost.json["errors"][0]["path"], "file");
    let made: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM documents d JOIN collections c ON c.id = d.collection_id
         WHERE c.slug = 'media'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(made, 1);
    assert_eq!(file_states(&pool, "media").await[0].1, "live");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn outstanding_instructions_are_capped(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    collection(&app, "other", json!({})).await;
    for i in 0..100 {
        let res = instructions(&app, "media", &format!("{i}.png"), "image/png", 10).await;
        assert_eq!(res.status, 200, "{i}: {}", res.json);
    }
    let res = instructions(&app, "media", "x.png", "image/png", 10).await;
    assert_problem(&res, 429);
    let wait: u64 = res.headers["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((3500..=3600).contains(&wait), "{wait}");
    // Another collection counts on its own.
    assert_eq!(
        instructions(&app, "other", "x.png", "image/png", 10)
            .await
            .status,
        200
    );
    // One past its grace no longer counts, nor one given up.
    sqlx::query(
        "UPDATE files SET created_at = now() - interval '2 hours'
         WHERE filename = '0.png'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        instructions(&app, "media", "x.png", "image/png", 10)
            .await
            .status,
        200
    );
    assert_eq!(
        instructions(&app, "media", "y.png", "image/png", 10)
            .await
            .status,
        429
    );
    sqlx::query("UPDATE files SET state = 'gone' WHERE filename = '1.png'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        instructions(&app, "media", "y.png", "image/png", 10)
            .await
            .status,
        200
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn an_unclaimed_upload_is_cleaned_up(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let res = instructions(&app, "media", "a.png", "image/png", 10).await;
    put_to(&d.bucket, &res.json, &png(1, 1));
    let (id, _, key) = file_states(&pool, "media").await.remove(0);
    assert!(d.bucket.join(&key).exists());
    // The grace runs out (no clock to move: the reserve and its cleanup are moved back).
    sqlx::query("UPDATE files SET created_at = now() - interval '2 hours' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = $1 AND payload->>'file' = $2")
        .bind(CLEANUP)
        .bind(id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    until("the reserve is deleted", async || {
        files::get(&pool, id).await.is_err()
    })
    .await;
    assert!(!d.bucket.join(&key).exists());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn client_uploads_replace_and_save_drafts(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "media",
        json!({"versions": {"drafts": true, "maxVersions": 10},
               "upload": {"storage": "bucket", "formatOptions": {"format": "webp"}}}),
    )
    .await;
    let image = png(4, 4);
    let res = instructions(&app, "media", "a.png", "image/png", image.len() as u64).await;
    let file = put_to(&d.bucket, &res.json, &image);
    // A draft save, its original made a WebP as it's stored.
    let doc = ok(&app, "POST", "/api/media", json!({"file": file})).await;
    assert_eq!(doc["filename"], "a.webp");
    assert_eq!(doc["mimeType"], "image/webp");
    let id = doc["id"].as_str().unwrap().to_owned();
    let rows = file_states(&pool, "media").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "live");
    assert!(rows[0].2.ends_with("/a.webp"), "{}", rows[0].2);
    assert!(d.bucket.join(&rows[0].2).exists());
    // The object the client put, under its old name, goes.
    let first = rows[0].2.replace("a.webp", "a.png");
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    let root = d.bucket.clone();
    until("the client's object is deleted", async || {
        !root.join(&first).exists()
    })
    .await;

    // A replacement, to the draft, the same way.
    let image = png(2, 2);
    let res = instructions(&app, "media", "b.png", "image/png", image.len() as u64).await;
    let file = put_to(&d.bucket, &res.json, &image);
    let patched = ok(
        &app,
        "PATCH",
        &format!("/api/media/{id}"),
        json!({"file": file}),
    )
    .await;
    assert_eq!(patched["filename"], "b.webp");
    assert_eq!(patched["width"], 2);
    let rows = file_states(&pool, "media").await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.1 == "live"), "{rows:?}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn signed_downloads_redirect_after_access(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "mine",
        json!({"access": {"read": {"owner": "$user.id"}, "create": true},
               "upload": {"storage": "bucket",
                          "imageSizes": [{"name": "thumb", "width": 2}]},
               "fields": [{"name": "owner", "type": "relationship", "relationTo": "users"}]}),
    )
    .await;
    collection(
        &app,
        "open",
        json!({"access": {"read": true}, "upload": {"storage": "bucket"}}),
    )
    .await;
    collection(
        &app,
        "papers",
        json!({"access": {"read": true}, "upload": {"storage": "pdfs"}}),
    )
    .await;
    collection(
        &app,
        "drafted",
        json!({"versions": {"drafts": true}, "upload": {"storage": "bucket"}}),
    )
    .await;
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    let amy_id = add_user(&pool, "amy@example.com", "author").await;
    let amy = signed_in(router(&pool, &d).await, "amy@example.com").await;
    let bob = as_role(&pool, &d, "bob@example.com", "author").await;
    let anon = router(&pool, &d).await;
    let res = upload(
        &amy,
        "POST",
        "/api/mine?wait=true",
        Some(&png(4, 4)),
        "m.png",
        Some(&json!({"owner": amy_id})),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let thumb = res.json["sizes"]["thumb"]["filename"]
        .as_str()
        .expect("the size was made")
        .to_owned();
    let key: String = sqlx::query_scalar("SELECT key FROM files WHERE filename = 'm.png'")
        .fetch_one(&pool)
        .await
        .unwrap();

    for method in ["GET", "HEAD"] {
        let res = fetch(&amy, method, "/api/mine/file/m.png").await;
        assert_eq!(res.status, 302, "{method}");
        assert_eq!(res.header("cache-control"), "private, no-store");
        let location = res.header("location");
        assert_eq!(signed_key(location), key);
        assert_eq!(query(location, "method").as_deref(), Some("GET"));
        assert_eq!(query(location, "expires").as_deref(), Some("600"));
        // Another Author gets what a missing file gets, and no link.
        let res = fetch(&bob, method, "/api/mine/file/m.png").await;
        assert_eq!(res.status, 404, "{method}");
        assert_eq!(res.header("location"), "");
    }
    // A size is signed beside its original.
    let res = fetch(&amy, "GET", &format!("/api/mine/file/{thumb}")).await;
    assert_eq!(res.status, 302);
    let (dir, _) = key.rsplit_once('/').unwrap();
    assert_eq!(signed_key(res.header("location")), format!("{dir}/{thumb}"));
    assert_eq!(
        fetch(&bob, "GET", &format!("/api/mine/file/{thumb}"))
            .await
            .status,
        404
    );

    // Readable by anyone, a signed bucket's files still go through the route.
    let res = upload(&app, "POST", "/api/open", Some(&png(1, 1)), "o.png", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["url"], "http://localhost:3000/api/open/file/o.png");
    let res = fetch(&anon, "GET", "/api/open/file/o.png").await;
    assert_eq!(res.status, 302);
    assert_eq!(res.header("cache-control"), "private, no-store");

    // Only the types listed are signed; the rest stream.
    let res = upload(&app, "POST", "/api/papers", Some(&png(1, 1)), "p.png", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(
        fetch(&anon, "GET", "/api/papers/file/p.png").await.status,
        200
    );

    // A draft's file: only for who may read it, when they ask.
    let res = upload(
        &app,
        "POST",
        "/api/drafted",
        Some(&png(1, 1)),
        "d.png",
        None,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let res = fetch(&anon, "GET", "/api/drafted/file/d.png?draft=true").await;
    assert_eq!(res.status, 404);
    assert_eq!(res.header("location"), "");
    let res = fetch(&app, "GET", "/api/drafted/file/d.png?draft=true").await;
    assert_eq!(res.status, 302);
    assert_eq!(res.header("cache-control"), "private, no-store");
}

/// Instructions for replacing document `id`'s file with `filename`.
async fn replacing(app: &Router, slug: &str, id: &str, filename: &str, size: u64) -> Res {
    let body = json!({"filename": filename, "mimeType": "image/png", "filesize": size, "id": id});
    call(
        app,
        "POST",
        &format!("/api/{slug}/upload-instructions"),
        body,
    )
    .await
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_replacement_keeps_its_name(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({"versions": {"maxVersions": 10}})).await;
    let image = png(1, 1);
    let size = image.len() as u64;
    let mut docs = Vec::new();
    for name in ["a.png", "b.png"] {
        let res = instructions(&app, "media", name, "image/png", size).await;
        let file = put_to(&d.bucket, &res.json, &image);
        let doc = ok(&app, "POST", "/api/media", json!({"file": file})).await;
        docs.push(doc["id"].as_str().unwrap().to_owned());
    }
    let (a, b) = (&docs[0], &docs[1]);

    // Without naming the document, its own a.png is a clash; naming it, it isn't.
    let res = instructions(&app, "media", "a.png", "image/png", size).await;
    assert_eq!(res.json["file"]["filename"], "a-1.png");
    let image = png(2, 2);
    let res = replacing(&app, "media", a, "a.png", image.len() as u64).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["file"]["filename"], "a.png");
    let file = put_to(&d.bucket, &res.json, &image);
    // While that one waits, it holds the name, even for its own document.
    let res = replacing(&app, "media", a, "a.png", size).await;
    assert_eq!(res.json["file"]["filename"], "a-2.png");

    // Only a PATCH of that document takes it.
    let made_up = call(
        &app,
        "PATCH",
        &format!("/api/media/{a}"),
        json!({"file": {"filename": "nope.png"}}),
    )
    .await;
    assert_problem(&made_up, 422);
    let other = call(
        &app,
        "PATCH",
        &format!("/api/media/{b}"),
        json!({"file": file}),
    )
    .await;
    assert_eq!(refusal(&other), refusal(&made_up));
    let create = call(&app, "POST", "/api/media", json!({"file": file})).await;
    assert_eq!(refusal(&create), refusal(&made_up));
    let doc = ok(
        &app,
        "PATCH",
        &format!("/api/media/{a}"),
        json!({"file": file}),
    )
    .await;
    assert_eq!(doc["filename"], "a.png");
    assert_eq!(doc["width"], 2);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn replacing_needs_the_rights_a_patch_does(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let res = instructions(&app, "media", "a.png", "image/png", 70).await;
    let file = put_to(&d.bucket, &res.json, &png(1, 1));
    let doc = ok(&app, "POST", "/api/media", json!({"file": file})).await;
    let id = doc["id"].as_str().unwrap();
    let viewer = as_role(&pool, &d, "viewer@example.com", "viewer").await;
    let anon = router(&pool, &d).await;
    let missing = Uuid::now_v7().to_string();
    for (who, app, id) in [
        ("viewer", &viewer, id),
        ("signed out", &anon, id),
        ("admin, no such document", &app, missing.as_str()),
        ("admin, not an id", &app, "x"),
    ] {
        let patch = call(
            app,
            "PATCH",
            &format!("/api/media/{id}"),
            json!({"alt": "x"}),
        )
        .await;
        let asked = replacing(app, "media", id, "a.png", 70).await;
        assert_eq!(asked.status, patch.status, "{who}: {}", asked.json);
        assert!(!asked.status.is_success(), "{who}");
    }
    // None of them reserved anything.
    assert_eq!(file_states(&pool, "media").await.len(), 1);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn instructions_name_files_in_their_scope(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "media",
        json!({"upload": {"storage": "bucket", "filenameCompoundIndex": ["alt"]}}),
    )
    .await;
    let image = png(1, 1);
    let size = image.len() as u64;
    let ask = |alt: &str| {
        json!({"filename": "a.png", "mimeType": "image/png", "filesize": size,
               "data": {"alt": alt}})
    };
    let res = call(&app, "POST", "/api/media/upload-instructions", ask("x")).await;
    assert_eq!(res.json["file"]["filename"], "a.png", "{}", res.json);
    let file = put_to(&d.bucket, &res.json, &image);
    ok(
        &app,
        "POST",
        "/api/media",
        json!({"file": file, "alt": "x"}),
    )
    .await;
    // Another scope has the name free; the same one numbers it, and the create keeps that.
    for (alt, name) in [("y", "a.png"), ("x", "a-1.png")] {
        let res = call(&app, "POST", "/api/media/upload-instructions", ask(alt)).await;
        assert_eq!(res.json["file"]["filename"], name, "{alt}");
        let file = put_to(&d.bucket, &res.json, &image);
        let doc = ok(
            &app,
            "POST",
            "/api/media",
            json!({"file": file, "alt": alt}),
        )
        .await;
        assert_eq!(doc["filename"], name, "{alt}");
    }
}
