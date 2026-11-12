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
/// of `signed` types (all with an empty list; none with `None`).
fn bucket(name: &str, root: &Path, signed: Option<Vec<String>>) -> Arc<Storage> {
    let mut c = StorageConfig::local(root);
    c.client_uploads = true;
    c.public_url = Some(Url::parse("https://media.tachyoncms.com").unwrap());
    if let Some(signed) = signed {
        c.signed_downloads = SignedDownloads::On {
            mime_types: signed,
            expires_in: Duration::from_secs(600),
        };
    }
    let store = LocalFileSystem::new_with_prefix(root).unwrap();
    Arc::new(Storage::with_store(name, Arc::new(store), None, c).with_signer(Arc::new(FakeSigner)))
}

fn options(d: &Dirs, workers: usize) -> Options {
    let local = Storage::open("default", &StorageConfig::local(&d.root)).unwrap();
    let storages = Storages(HashMap::from([
        ("default".into(), Arc::new(local)),
        (
            "bucket".into(),
            bucket("bucket", &d.bucket, Some(Vec::new())),
        ),
        (
            "pdfs".into(),
            bucket("pdfs", &d.pdfs, Some(vec!["application/pdf".into()])),
        ),
        // Its files stream through the route, so what it serves can be read back.
        ("plain".into(), bucket("plain", &d.bucket, None)),
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
    body: Vec<u8>,
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
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    Raw {
        status,
        headers,
        body,
    }
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
    // The client puts to a staging key of its own, never to the file's.
    let staging = signed_key(&url);
    // One staging area per storage, for one lifecycle rule to cover.
    assert!(staging.starts_with("_staging/"), "{staging}");
    assert_ne!(staging, *key);
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
    // The server wrote the file itself, from what it checked; the staging object is gone.
    assert!(d.bucket.join(&rows[0].2).exists());
    assert!(
        !d.bucket
            .join(signed_key(res.json["request"]["url"].as_str().unwrap()))
            .exists()
    );
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
    // couldn't leave anything behind, and what the client put is deleted already.
    let gone = async |instructions: &Value| {
        let name = instructions["file"]["filename"].as_str().unwrap();
        let (state, due): (String, bool) = sqlx::query_as(
            "SELECT f.state, EXISTS (SELECT 1 FROM jobs j WHERE j.kind = $2 AND j.state = 'queued'
                                     AND j.payload->>'file' = f.id::text AND j.run_at <= now())
             FROM files f WHERE f.filename = $1",
        )
        .bind(name)
        .bind(CLEANUP)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((state.as_str(), due), ("gone", true), "{name}");
        let staging = signed_key(instructions["request"]["url"].as_str().unwrap());
        assert!(!d.bucket.join(&staging).exists(), "{name}: staging deleted");
        staging
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
    // The oldest one's PUT URL runs out within 15 minutes.
    assert!((850..=900).contains(&wait), "{wait}");
    // Another collection counts on its own.
    assert_eq!(
        instructions(&app, "other", "x.png", "image/png", 10)
            .await
            .status,
        200
    );
    // One whose PUT URL has run out no longer counts.
    sqlx::query(
        "UPDATE jobs SET created_at = now() - interval '16 minutes'
         WHERE payload->'keys' ? (SELECT staging_key FROM files WHERE filename = '0.png')",
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
    // One given up still counts while its URL works: the client could still put to it.
    sqlx::query("UPDATE files SET state = 'gone' WHERE filename = '1.png'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        instructions(&app, "media", "y.png", "image/png", 10)
            .await
            .status,
        429
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn an_unclaimed_upload_is_cleaned_up(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let res = instructions(&app, "media", "a.png", "image/png", 10).await;
    put_to(&d.bucket, &res.json, &png(1, 1));
    let (id, _, _) = file_states(&pool, "media").await.remove(0);
    let key = signed_key(res.json["request"]["url"].as_str().unwrap());
    assert!(d.bucket.join(&key).exists());
    // The grace runs out (no clock to move: the reserve and its cleanup are moved back).
    sqlx::query("UPDATE files SET created_at = now() - interval '2 hours' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    // Both its cleanups: the reserve's, and its staging key's, due once the URL is spent.
    let moved = sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = $1 AND state = 'queued'")
        .bind(CLEANUP)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(moved.rows_affected(), 2);
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    until(
        "the reserve and the staging object are deleted",
        async || files::get(&pool, id).await.is_err() && !d.bucket.join(&key).exists(),
    )
    .await;
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
    // The object the client put, at its staging key, goes.
    assert!(
        !d.bucket
            .join(signed_key(res.json["request"]["url"].as_str().unwrap()))
            .exists()
    );

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

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn new_replacement_instructions_retire_the_last(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let image = png(1, 1);
    let size = image.len() as u64;
    let res = instructions(&app, "media", "a.png", "image/png", size).await;
    let file = put_to(&d.bucket, &res.json, &image);
    let doc = ok(&app, "POST", "/api/media", json!({"file": file})).await;
    let id = doc["id"].as_str().unwrap();
    let first = replacing(&app, "media", id, "a.png", size).await;
    put_to(&d.bucket, &first.json, &png(3, 3));
    let second = replacing(&app, "media", id, "a.png", size).await;
    assert_eq!(second.json["file"]["filename"], "a.png");
    // The first is given up, its staging key left for the sweep at its URL's expiry: only
    // the second can be claimed, and what was put for the first isn't it.
    let res = call(
        &app,
        "PATCH",
        &format!("/api/media/{id}"),
        json!({"file": first.json["file"]}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "file");
    let swept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND state = 'queued'
         AND payload->'keys' ? $2 AND run_at > now() + interval '15 minutes'",
    )
    .bind(CLEANUP)
    .bind(signed_key(first.json["request"]["url"].as_str().unwrap()))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(swept, 1);
    let second = replacing(&app, "media", id, "a.png", size).await;
    let file = put_to(&d.bucket, &second.json, &png(5, 5));
    let doc = ok(
        &app,
        "PATCH",
        &format!("/api/media/{id}"),
        json!({"file": file}),
    )
    .await;
    assert_eq!(
        (doc["filename"].clone(), doc["width"].clone()),
        (json!("a.png"), json!(5))
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn replacement_instructions_dont_rename_edits(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let res = upload(&app, "POST", "/api/media", Some(&png(4, 4)), "a.png", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let id = res.json["id"].as_str().unwrap().to_owned();
    let asked = replacing(&app, "media", &id, "a.png", 70).await;
    assert_eq!(asked.json["file"]["filename"], "a.png");
    // Never put: the document's own edits keep its name meanwhile.
    let doc = ok(
        &app,
        "PATCH",
        &format!("/api/media/{id}"),
        json!({"focalX": 10}),
    )
    .await;
    assert_eq!(doc["filename"], "a.png");
    let res = upload(
        &app,
        "PATCH",
        &format!("/api/media/{id}"),
        Some(&png(2, 2)),
        "a.png",
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["filename"], "a.png");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_client_cant_overwrite_the_checked_file(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "media",
        json!({"access": {"read": true},
               "upload": {"storage": "plain", "mimeTypes": ["image/*"], "filesizeLimit": 1000}}),
    )
    .await;
    let image = png(2, 2);
    let res = instructions(&app, "media", "a.png", "image/png", image.len() as u64).await;
    let file = put_to(&d.bucket, &res.json, &image);
    let doc = ok(&app, "POST", "/api/media", json!({"file": file})).await;
    let served = fetch(&app, "GET", "/api/media/file/a.png").await;
    assert_eq!(served.status, 200);
    let checked = served.body.clone();
    // The PUT URL still works for its 15 minutes; what it reaches isn't the file.
    let html = format!(
        "<!doctype html><script>alert(1)</script>{}",
        "x".repeat(5000)
    );
    put_to(&d.bucket, &res.json, html.as_bytes());
    let served = fetch(&app, "GET", "/api/media/file/a.png").await;
    assert_eq!(served.status, 200);
    assert_eq!(served.body, checked);
    assert_eq!(served.header("content-type"), "image/png");
    assert_eq!(served.body.len() as u64, doc["filesize"].as_u64().unwrap());
    let (_, _, key) = file_states(&pool, "media").await.remove(0);
    assert_ne!(
        key,
        signed_key(res.json["request"]["url"].as_str().unwrap())
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_stored_file_carries_its_sniffed_type(pool: PgPool) {
    use object_store::{Attribute, ObjectStore, ObjectStoreExt, PutOptions, memory::InMemory};
    let d = dirs();
    // A store that keeps content types, as a bucket does.
    let memory = Arc::new(InMemory::new());
    let mut c = StorageConfig::local(&d.bucket);
    c.adapter = tachyon_media::storage::Adapter::S3 {
        bucket: "b".into(),
        endpoint: None,
        region: None,
    };
    c.client_uploads = true;
    let typed =
        Storage::with_store("typed", memory.clone(), None, c).with_signer(Arc::new(FakeSigner));
    let mut o = options(&d, 0);
    let mut all = HashMap::new();
    for (k, v) in &o.uploads.storages.0 {
        all.insert(k.clone(), v.clone());
    }
    all.insert("typed".into(), Arc::new(typed));
    o.uploads.storages = Arc::new(Storages(all));
    let app = tachyon_api::router_with(pool.clone(), o).await.unwrap();
    let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    assert_eq!(
        send(&app, "POST", "/api/setup", Some(&body.to_string()))
            .await
            .status,
        201
    );
    let app = signed_in(app, ADMIN).await;
    collection(
        &app,
        "papers",
        json!({"upload": {"storage": "typed", "mimeTypes": ["application/pdf", "text/plain"]}}),
    )
    .await;
    // Claimed as text, put as text, but the bytes are a PDF.
    let pdf = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
    let res = instructions(&app, "papers", "a.txt", "text/plain", pdf.len() as u64).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let staging = Key::from(signed_key(res.json["request"]["url"].as_str().unwrap()));
    let mut put = PutOptions::default();
    put.attributes
        .insert(Attribute::ContentType, "text/plain".into());
    memory
        .put_opts(&staging, pdf.to_vec().into(), put)
        .await
        .unwrap();
    let doc = ok(
        &app,
        "POST",
        "/api/papers",
        json!({"file": res.json["file"]}),
    )
    .await;
    assert_eq!(doc["mimeType"], "application/pdf");
    let (_, _, key) = file_states(&pool, "papers").await.remove(0);
    let got = memory.get(&Key::from(key)).await.unwrap();
    assert_eq!(
        got.attributes
            .get(&Attribute::ContentType)
            .map(|v| v.as_ref()),
        Some("application/pdf")
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn giving_up_doesnt_escape_the_cap(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    for i in 0..100 {
        let res = instructions(&app, "media", "a.png", "image/png", 10).await;
        assert_eq!(res.status, 200, "{i}: {}", res.json);
        // Never put: the claim gives it up at once.
        let res = call(
            &app,
            "POST",
            "/api/media",
            json!({"file": res.json["file"]}),
        )
        .await;
        assert_eq!(res.status, 422, "{i}: {}", res.json);
    }
    assert_problem(
        &instructions(&app, "media", "a.png", "image/png", 10).await,
        429,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_cap_holds_under_concurrency(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let asks = (0..120).map(|i| {
        let app = app.clone();
        async move {
            instructions(&app, "media", &format!("{i}.png"), "image/png", 10)
                .await
                .status
        }
    });
    let statuses = futures_util::future::join_all(asks).await;
    let issued = statuses.iter().filter(|s| s.as_u16() == 200).count();
    let refused = statuses.iter().filter(|s| s.as_u16() == 429).count();
    assert_eq!((issued, refused), (100, 20), "{statuses:?}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_put_after_giving_up_is_swept(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let res = instructions(&app, "media", "a.png", "image/png", 10).await;
    let given = call(
        &app,
        "POST",
        "/api/media",
        json!({"file": res.json["file"]}),
    )
    .await;
    assert_problem(&given, 422);
    // The URL still works: the client puts anyway, after the give-up's own cleanup.
    let staging = signed_key(res.json["request"]["url"].as_str().unwrap());
    put_to(&d.bucket, &res.json, &png(1, 1));
    // The sweep due at the URL's expiry takes it.
    let moved =
        sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = $1 AND payload->'keys' ? $2")
            .bind(CLEANUP)
            .bind(&staging)
            .execute(&pool)
            .await
            .unwrap();
    assert_eq!(moved.rows_affected(), 1);
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    let root = d.bucket.clone();
    until("the late object is deleted", async || {
        !root.join(&staging).exists()
    })
    .await;
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn claims_are_matched_in_their_scope(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "media",
        json!({"upload": {"storage": "bucket", "filenameCompoundIndex": ["alt"]}}),
    )
    .await;
    let ask = |alt: &str, size: usize| json!({"filename": "a.png", "mimeType": "image/png", "filesize": size, "data": {"alt": alt}});
    // Two scopes, one name: one issuer's waiting names are kept apart across scopes, so
    // each create takes its own bytes.
    let (x, y) = (png(3, 3), png(7, 7));
    let for_x = call(
        &app,
        "POST",
        "/api/media/upload-instructions",
        ask("x", x.len()),
    )
    .await;
    let for_y = call(
        &app,
        "POST",
        "/api/media/upload-instructions",
        ask("y", y.len()),
    )
    .await;
    assert_eq!(for_x.json["file"]["filename"], "a.png");
    assert_eq!(for_y.json["file"]["filename"], "a-1.png");
    let fx = put_to(&d.bucket, &for_x.json, &x);
    let fy = put_to(&d.bucket, &for_y.json, &y);
    let doc = ok(&app, "POST", "/api/media", json!({"file": fy, "alt": "y"})).await;
    assert_eq!(doc["width"], 7);
    let doc = ok(&app, "POST", "/api/media", json!({"file": fx, "alt": "x"})).await;
    assert_eq!(doc["width"], 3);

    // A replacement waiting for one document doesn't stand in the way of a create in
    // another scope: the create's name is numbered apart, and it claims.
    let id = doc["id"].as_str().unwrap();
    let body = json!({"filename": "b.png", "mimeType": "image/png", "filesize": 70, "id": id});
    let replace = call(&app, "POST", "/api/media/upload-instructions", body).await;
    assert_eq!(replace.json["file"]["filename"], "b.png");
    let mut create = ask("z", x.len());
    create["filename"] = "b.png".into();
    let create = call(&app, "POST", "/api/media/upload-instructions", create).await;
    assert_eq!(create.json["file"]["filename"], "b-1.png");
    let file = put_to(&d.bucket, &create.json, &x);
    let res = call(
        &app,
        "POST",
        "/api/media",
        json!({"file": file, "alt": "z"}),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn rights_come_before_the_body(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let anon = router(&pool, &d).await;
    for (uri, body) in [
        ("/api/media/upload-instructions", "{\"nope\": 1}"),
        ("/api/media/upload-instructions", "not json"),
        ("/api/upload-instructions", "{\"nope\": 1}"),
        ("/api/upload-instructions", "not json"),
    ] {
        let res = send(&anon, "POST", uri, Some(body)).await;
        assert_problem(&res, 401);
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn refusals_after_the_claim_give_it_up(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    // Authors may not write a file's width: a field rule on what the file sets.
    collection(
        &app,
        "media",
        json!({"fields": [{"name": "alt", "type": "text"},
                          {"name": "width", "type": "number",
                           "access": {"create": {"role": ["admin"]}, "update": {"role": ["admin"]}}}]}),
    )
    .await;
    let amy = as_role(&pool, &d, "amy@example.com", "author").await;
    let image = png(2, 2);
    let state = async |res: &Res| -> String {
        sqlx::query_scalar("SELECT state FROM files WHERE staging_key = $1")
            .bind(signed_key(res.json["request"]["url"].as_str().unwrap()))
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let res = instructions(&amy, "media", "a.png", "image/png", image.len() as u64).await;
    let file = put_to(&d.bucket, &res.json, &image);
    let refused = call(&amy, "POST", "/api/media", json!({"file": file})).await;
    assert_problem(&refused, 403);
    assert_eq!(state(&res).await, "gone");

    // A 409: the collection's fields change while the update is between its read and write.
    let res = instructions(&app, "media", "b.png", "image/png", image.len() as u64).await;
    let file = put_to(&d.bucket, &res.json, &image);
    let doc = ok(&app, "POST", "/api/media", json!({"file": file})).await;
    let id: Uuid = doc["id"].as_str().unwrap().parse().unwrap();
    let res = replacing(&app, "media", &id.to_string(), "b.png", image.len() as u64).await;
    let file = put_to(&d.bucket, &res.json, &png(3, 3));
    let pause = tachyon_api::pause_next_update(id);
    let patch = {
        let app = app.clone();
        tokio::spawn(async move {
            call(
                &app,
                "PATCH",
                &format!("/api/media/{id}"),
                json!({"file": file}),
            )
            .await
        })
    };
    pause.reached().await;
    let schema = json!({"slug": "media", "upload": {"storage": "bucket"},
                        "fields": [{"name": "alt", "type": "text"}, {"name": "extra", "type": "text"}]});
    ok(
        &app,
        "PUT",
        "/api/admin/collections/media",
        json!({"schema": schema}),
    )
    .await;
    pause.resume();
    let patched = patch.await.unwrap();
    assert_problem(&patched, 409);
    assert_eq!(state(&res).await, "gone");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn signed_storages_keep_the_route_s_rules(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "kept",
        json!({"access": {"read": true}, "versions": {"maxVersions": 10}}),
    )
    .await;
    collection(
        &app,
        "hidden",
        json!({"access": {"read": true},
               "fields": [{"name": "filename", "type": "text",
                           "access": {"read": {"role": ["admin", "editor"]}}}]}),
    )
    .await;
    collection(&app, "art", json!({"access": {"read": true}})).await;
    let viewer = as_role(&pool, &d, "viewer@example.com", "viewer").await;
    let anon = router(&pool, &d).await;
    let not_found = |res: &Raw, what: &str| {
        assert_eq!(res.status, 404, "{what}");
        assert_eq!(res.header("location"), "", "{what}");
    };

    // A file only a version holds is served to nobody, signed or not.
    let res = upload(&app, "POST", "/api/kept", Some(&png(1, 1)), "old.png", None).await;
    let id = res.json["id"].as_str().unwrap().to_owned();
    let res = upload(
        &app,
        "PATCH",
        &format!("/api/kept/{id}"),
        Some(&png(2, 2)),
        "new.png",
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        fetch(&app, "GET", "/api/kept/file/new.png").await.status,
        302
    );
    not_found(
        &fetch(&app, "GET", "/api/kept/file/old.png").await,
        "version only",
    );

    // A hidden `filename` hides the file.
    upload(&app, "POST", "/api/hidden", Some(&png(1, 1)), "h.png", None).await;
    not_found(
        &fetch(&viewer, "GET", "/api/hidden/file/h.png").await,
        "hidden field",
    );
    assert_eq!(
        fetch(&app, "GET", "/api/hidden/file/h.png").await.status,
        302
    );

    // What the route sandboxes streams through it, sandboxed; the rest is signed.
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1"/></svg>"#;
    let res = upload(&app, "POST", "/api/art", Some(svg), "a.svg", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let res = fetch(&anon, "GET", "/api/art/file/a.svg").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("content-type"), "image/svg+xml");
    assert_eq!(res.header("content-security-policy"), "sandbox");
    upload(&app, "POST", "/api/art", Some(&png(1, 1)), "a.png", None).await;
    assert_eq!(fetch(&anon, "GET", "/api/art/file/a.png").await.status, 302);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_create_later_in_the_grace_still_claims(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let image = png(2, 2);
    let res = instructions(&app, "media", "a.png", "image/png", image.len() as u64).await;
    let file = put_to(&d.bucket, &res.json, &image);
    let staging = signed_key(res.json["request"]["url"].as_str().unwrap());
    // The staging key is swept once the claim's grace is over, not when the URL is.
    let late: bool = sqlx::query_scalar(
        "SELECT run_at > now() + interval '1 hour' FROM jobs WHERE payload->'keys' ? $1",
    )
    .bind(&staging)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(late);
    // Twenty minutes on (no clock: the file and its jobs are moved back), with a worker
    // running whatever is due.
    sqlx::query("UPDATE files SET created_at = created_at - interval '20 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE jobs SET created_at = created_at - interval '20 minutes',
                         run_at = run_at - interval '20 minutes' WHERE kind = $1",
    )
    .bind(CLEANUP)
    .execute(&pool)
    .await
    .unwrap();
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(d.bucket.join(&staging).exists());
    let doc = ok(&app, "POST", "/api/media", json!({"file": file})).await;
    assert_eq!(doc["width"], 2);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_swept_upload_is_like_any_other_unclaimable(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let made_up = call(
        &app,
        "POST",
        "/api/media",
        json!({"file": {"filename": "nope.png"}}),
    )
    .await;
    assert_problem(&made_up, 422);
    // Put, then swept before the create (or never put): the same answer.
    let res = instructions(&app, "media", "a.png", "image/png", 70).await;
    put_to(&d.bucket, &res.json, &png(1, 1));
    std::fs::remove_file(
        d.bucket
            .join(signed_key(res.json["request"]["url"].as_str().unwrap())),
    )
    .unwrap();
    let swept = call(
        &app,
        "POST",
        "/api/media",
        json!({"file": res.json["file"]}),
    )
    .await;
    assert_eq!(refusal(&swept), refusal(&made_up));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_compound_create_claims_without_hints(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "media",
        json!({"upload": {"storage": "bucket", "filenameCompoundIndex": ["alt"]}}),
    )
    .await;
    // Asked for as a client written for the reference shape asks: no `data`.
    let image = png(3, 3);
    let res = instructions(&app, "media", "a.png", "image/png", image.len() as u64).await;
    let file = put_to(&d.bucket, &res.json, &image);
    let doc = ok(
        &app,
        "POST",
        "/api/media",
        json!({"file": file, "alt": "x"}),
    )
    .await;
    assert_eq!(
        (doc["filename"].clone(), doc["width"].clone()),
        (json!("a.png"), json!(3))
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn stray_staging_objects_are_swept(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    // Objects in the staging area that no file knows: one long finished, one just put.
    let old = d.bucket.join(format!("_staging/{}", Uuid::new_v4()));
    let fresh = d.bucket.join(format!("_staging/{}", Uuid::new_v4()));
    // And old ones that aren't a staging key's shape, which it leaves alone.
    let nested = d
        .bucket
        .join(format!("_staging/media/{}/a.png", Uuid::now_v7()));
    let named = d.bucket.join("_staging/not-a-uuid");
    for (path, bytes) in [
        (&old, "slow"),
        (&fresh, "new"),
        (&nested, "png"),
        (&named, "x"),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    for path in [&old, &nested, &named] {
        age(path);
    }
    sweep_now(&pool).await;
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    until("the stray object is deleted", async || !old.exists()).await;
    assert!(fresh.exists());
    assert!(nested.exists() && named.exists());
    drop(app);
}

/// `path` written three hours ago.
fn age(path: &Path) {
    let three_hours = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(three_hours)
        .unwrap();
}

/// The hourly cleanup sweep, due now.
async fn sweep_now(pool: &PgPool) {
    let moved = sqlx::query(
        "UPDATE jobs SET run_at = now() WHERE kind = 'file_cleanup_sweep' AND state = 'queued'",
    )
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(moved.rows_affected(), 1);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_sweep_run_stops_at_its_cap(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let dir = d.bucket.join("_staging");
    std::fs::create_dir_all(&dir).unwrap();
    // Two of the test's storages, `bucket` and `plain`, share this directory: one run
    // sweeps it twice, 1,000 each.
    for _ in 0..2005 {
        let path = dir.join(Uuid::new_v4().to_string());
        std::fs::write(&path, b"x").unwrap();
        age(&path);
    }
    let left = || std::fs::read_dir(&dir).unwrap().count();
    sweep_now(&pool).await;
    let worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    // A run takes 1,000 a storage; the next hour's takes the rest.
    until("a run is done", async || left() == 5).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(left(), 5);
    sweep_now(&pool).await;
    until("the next run is done", async || left() == 0).await;
    drop((worker, app));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_static_dir_cant_be_the_staging_area(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    for dir in ["_staging", "_staging/media"] {
        let schema = json!({"slug": "media", "upload": {"storage": "plain", "staticDir": dir},
                            "fields": [{"name": "alt", "type": "text"}]});
        let res = call(&app, "POST", "/api/admin/collections", schema).await;
        assert_problem(&res, 422);
        assert_eq!(res.json["errors"][0]["path"], "upload.staticDir", "{dir}");
    }
    // Beside it is fine.
    let schema = json!({"slug": "media", "upload": {"storage": "plain", "staticDir": "media/_staging"},
                        "fields": [{"name": "alt", "type": "text"}]});
    ok(&app, "POST", "/api/admin/collections", schema).await;
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn replacements_in_two_scopes_keep_one_claimable(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "media",
        json!({"upload": {"storage": "bucket", "filenameCompoundIndex": ["alt"]}}),
    )
    .await;
    let alt = json!({"alt": "x"});
    let res = upload(
        &app,
        "POST",
        "/api/media",
        Some(&png(1, 1)),
        "a.png",
        Some(&alt),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let id = res.json["id"].as_str().unwrap().to_owned();
    let ask = |alt: &str, size: usize| {
        json!({"filename": "a.png", "mimeType": "image/png", "filesize": size,
               "id": id, "data": {"alt": alt}})
    };
    let (x, y) = (png(3, 3), png(7, 7));
    let for_x = call(
        &app,
        "POST",
        "/api/media/upload-instructions",
        ask("x", x.len()),
    )
    .await;
    put_to(&d.bucket, &for_x.json, &x);
    let for_y = call(
        &app,
        "POST",
        "/api/media/upload-instructions",
        ask("y", y.len()),
    )
    .await;
    let file = put_to(&d.bucket, &for_y.json, &y);
    let body = json!({"file": file, "alt": "y"});
    let doc = ok(&app, "PATCH", &format!("/api/media/{id}"), body).await;
    assert_eq!(doc["width"], 7);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_claimed_upload_isnt_swept_from_under_its_claim(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "media", json!({})).await;
    let res = instructions(&app, "media", "a.png", "image/png", 70).await;
    put_to(&d.bucket, &res.json, &png(1, 1));
    let staging = signed_key(res.json["request"]["url"].as_str().unwrap());
    // A claim is under way: taken, not yet live.
    let (c, user): (Uuid, Uuid) =
        sqlx::query_as("SELECT collection_id, issued_to FROM files WHERE staging_key = $1")
            .bind(&staging)
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let claimed = files::claim_pending(&mut tx, c, "a.png", user, None, None)
        .await
        .unwrap();
    assert!(claimed.is_some());
    tx.commit().await.unwrap();
    // Its key's sweep comes due meanwhile, and leaves it to the claim.
    let moved =
        sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = $1 AND payload->'keys' ? $2")
            .bind(CLEANUP)
            .bind(&staging)
            .execute(&pool)
            .await
            .unwrap();
    assert_eq!(moved.rows_affected(), 1);
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    until("the sweep is done", async || {
        sqlx::query_scalar::<_, String>("SELECT state FROM jobs WHERE payload->'keys' ? $1")
            .bind(&staging)
            .fetch_one(&pool)
            .await
            .unwrap()
            == "done"
    })
    .await;
    assert!(d.bucket.join(&staging).exists());
}

/// A bucket whose staging objects vanish when read: `head` finds them, the download doesn't.
#[derive(Debug)]
struct Vanishing(LocalFileSystem);

impl std::fmt::Display for Vanishing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Vanishing({})", self.0)
    }
}

type Listing =
    futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>;
type Keys = futures_util::stream::BoxStream<'static, object_store::Result<Key>>;

#[async_trait::async_trait]
impl object_store::ObjectStore for Vanishing {
    async fn put_opts(
        &self,
        location: &Key,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.0.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Key,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.0.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Key,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if !options.head && location.as_ref().starts_with("_staging/") {
            return Err(object_store::Error::NotFound {
                path: location.to_string(),
                source: "swept".into(),
            });
        }
        self.0.get_opts(location, options).await
    }

    fn delete_stream(&self, locations: Keys) -> Keys {
        self.0.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Key>) -> Listing {
        self.0.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Key>,
    ) -> object_store::Result<object_store::ListResult> {
        self.0.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Key,
        to: &Key,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.0.copy_opts(from, to, options).await
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn an_upload_gone_mid_claim_is_unclaimable(pool: PgPool) {
    let d = dirs();
    let mut c = StorageConfig::local(&d.bucket);
    c.client_uploads = true;
    let store = Vanishing(LocalFileSystem::new_with_prefix(&d.bucket).unwrap());
    let vanishing = Storage::with_store("vanishing", Arc::new(store), None, c)
        .with_signer(Arc::new(FakeSigner));
    let mut o = options(&d, 0);
    let mut all: HashMap<String, Arc<Storage>> = o.uploads.storages.0.clone();
    all.insert("vanishing".into(), Arc::new(vanishing));
    o.uploads.storages = Arc::new(Storages(all));
    let app = tachyon_api::router_with(pool.clone(), o).await.unwrap();
    let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
    assert_eq!(res.status, 201);
    let app = signed_in(app, ADMIN).await;
    collection(&app, "media", json!({"upload": {"storage": "vanishing"}})).await;
    let made_up = json!({"file": {"filename": "nope.png"}});
    let made_up = call(&app, "POST", "/api/media", made_up).await;
    let res = instructions(&app, "media", "a.png", "image/png", 70).await;
    let file = put_to(&d.bucket, &res.json, &png(1, 1));
    let gone = call(&app, "POST", "/api/media", json!({"file": file})).await;
    assert_eq!(refusal(&gone), refusal(&made_up));
}
