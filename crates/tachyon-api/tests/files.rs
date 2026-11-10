//! Serving stored files from `/api/{collection}/file/{filename}`, and the `url`s that lead
//! there.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use common::{ADMIN, PASSWORD, add_user, assert_problem, png, send, signed_in, upload};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_api::{Options, UploadsConfig};
use tachyon_db::files::{self, CLEANUP};
use tachyon_media::storage::{StorageConfig, Storages};
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

/// Where a test's spool and stores are: `default`, and `r2`, a local store standing in for a
/// public bucket at `https://media.tachyoncms.com`.
struct Dirs {
    temp: PathBuf,
    root: PathBuf,
    r2: PathBuf,
}

fn dirs() -> Dirs {
    let base = std::env::temp_dir().join(format!("tachyon-files-{}", Uuid::now_v7()));
    let d = Dirs {
        temp: base.join("spool"),
        root: base.join("store"),
        r2: base.join("r2"),
    };
    for dir in [&d.temp, &d.root, &d.r2] {
        std::fs::create_dir_all(dir).unwrap();
    }
    d
}

fn options(d: &Dirs) -> Options {
    let mut r2 = StorageConfig::local(&d.r2);
    r2.public_url = Some(Url::parse("https://media.tachyoncms.com").unwrap());
    let storages = Storages::open(&BTreeMap::from([
        ("default".into(), StorageConfig::local(&d.root)),
        ("r2".into(), r2),
    ]))
    .unwrap();
    Options {
        uploads: UploadsConfig {
            temp_dir: d.temp.clone(),
            storages: Arc::new(storages),
            ..UploadsConfig::default()
        },
        ..Options::default()
    }
}

async fn router(pool: &PgPool, d: &Dirs) -> Router {
    tachyon_api::router_with(pool.clone(), options(d))
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

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let res = send(app, method, uri, body.map(|b| b.to_string()).as_deref()).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

/// Creates `slug`, an upload collection with an `alt` field, `extra` merged in.
async fn collection(app: &Router, slug: &str, extra: Value) {
    let mut schema = json!({
        "slug": slug, "upload": true,
        "fields": [{"name": "alt", "type": "text"}]
    });
    schema
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    ok(app, "POST", "/api/admin/collections", Some(schema)).await;
}

/// Uploads `bytes` as `name` to `slug`, and returns the document.
async fn put(app: &Router, slug: &str, bytes: &[u8], name: &str, payload: Option<Value>) -> Value {
    let res = upload(
        app,
        "POST",
        &format!("/api/{slug}"),
        Some(bytes),
        name,
        payload.as_ref(),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    res.json
}

/// A response with its body as bytes.
struct Raw {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Raw {
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .map_or("", |v| v.to_str().unwrap_or_default())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

async fn fetch_with(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)]) -> Raw {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    Raw {
        status,
        headers,
        body,
    }
}

async fn fetch(app: &Router, uri: &str) -> Raw {
    fetch_with(app, "GET", uri, &[]).await
}

/// The path of `url`, with its query, for the router.
fn path_of(url: &Value) -> String {
    let url = Url::parse(url.as_str().unwrap_or_else(|| panic!("no url: {url}"))).unwrap();
    match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    }
}

/// The file the published or draft copy of document `id` holds.
async fn held(pool: &PgPool, id: &Value, column: &str) -> Option<Uuid> {
    let sql = match column {
        "published_file" => "SELECT published_file FROM documents WHERE id = $1",
        _ => "SELECT draft_file FROM documents WHERE id = $1",
    };
    sqlx::query_scalar(sql)
        .bind(id.as_str().unwrap().parse::<Uuid>().unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Gives file `file` a size `size`, as the sizes job will once there is one: its object
/// beside the original in `root`, its entry (with a WebP copy) in `files.sizes` and every
/// document copy that holds it, and its names in `files.names`. Returns the entry.
async fn seed_size(pool: &PgPool, root: &std::path::Path, file: Uuid, size: &str) -> Value {
    let row = files::get(pool, file).await.unwrap();
    let stem = row.filename.rsplit_once('.').unwrap().0.to_owned();
    let (name, webp) = (format!("{stem}-10x5.png"), format!("{stem}-10x5.webp"));
    let dir = row.key.rsplit_once('/').unwrap().0;
    std::fs::write(root.join(dir).join(&name), png(10, 5)).unwrap();
    std::fs::write(root.join(dir).join(&webp), b"RIFF\0\0\0\0WEBPVP8 ").unwrap();
    let entry = json!({
        "filename": name, "mimeType": "image/png", "width": 10, "height": 5,
        "filesize": png(10, 5).len(),
        "formats": {"webp": {"filename": webp, "mimeType": "image/webp", "width": 10,
                             "height": 5, "filesize": 16}}
    });
    sqlx::query(
        "UPDATE files SET sizes = jsonb_set(sizes, ARRAY[$2], $3),
                          names = names || ARRAY[$4, $5] WHERE id = $1",
    )
    .bind(file)
    .bind(size)
    .bind(&entry)
    .bind(&name)
    .bind(&webp)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE documents SET
             published = CASE WHEN published_file = $1
                              THEN jsonb_set(published, ARRAY['sizes', $2], $3) ELSE published END,
             draft = CASE WHEN draft_file = $1
                          THEN jsonb_set(draft, ARRAY['sizes', $2], $3) ELSE draft END
         WHERE published_file = $1 OR draft_file = $1",
    )
    .bind(file)
    .bind(size)
    .bind(&entry)
    .execute(pool)
    .await
    .unwrap();
    entry
}

fn stored(root: &std::path::Path, pool_key: &str) -> Vec<u8> {
    std::fs::read(root.join(pool_key)).unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn serves_the_original_and_sizes(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"upload": {"imageSizes": [{"name": "thumbnail", "width": 10}]}}),
    )
    .await;
    let doc = put(&app, "images", &png(40, 30), "cat.png", None).await;
    let file = held(&pool, &doc["id"], "published_file").await.unwrap();
    let row = files::get(&pool, file).await.unwrap();
    let bytes = stored(&d.root, &row.key);
    let anon = router(&pool, &d).await;

    let res = fetch(&anon, "/api/images/file/cat.png").await;
    assert_eq!(res.status, 200, "{}", res.json());
    assert_eq!(res.body, bytes);
    assert_eq!(res.header("content-type"), "image/png");
    assert_eq!(res.header("x-content-type-options"), "nosniff");
    assert_eq!(res.header("content-length"), bytes.len().to_string());
    assert_eq!(res.header("etag"), format!("\"{file}-cat.png\""));
    assert_eq!(
        res.header("cache-control"),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        res.header("content-disposition"),
        "inline; filename=\"cat.png\"; filename*=UTF-8''cat.png"
    );
    assert_eq!(
        res.header("content-security-policy"),
        "",
        "an image runs nothing"
    );
    let modified = res.header("last-modified").to_owned();
    assert_eq!(
        httpdate::parse_http_date(&modified).unwrap(),
        httpdate::parse_http_date(&httpdate::fmt_http_date(row.created_at.into())).unwrap()
    );

    let part = fetch_with(
        &anon,
        "GET",
        "/api/images/file/cat.png",
        &[("range", "bytes=0-9")],
    )
    .await;
    assert_eq!(part.status, 206);
    assert_eq!(part.body, bytes[..10]);
    assert_eq!(
        part.header("content-range"),
        format!("bytes 0-9/{}", bytes.len())
    );
    let tail = fetch_with(
        &anon,
        "GET",
        "/api/images/file/cat.png",
        &[("range", "bytes=-4")],
    )
    .await;
    assert_eq!(
        (tail.status, tail.body.as_slice()),
        (StatusCode::PARTIAL_CONTENT, &bytes[bytes.len() - 4..])
    );
    // Several ranges get the whole file; one past the end can't be served.
    let many = fetch_with(
        &anon,
        "GET",
        "/api/images/file/cat.png",
        &[("range", "bytes=0-1,4-5")],
    )
    .await;
    assert_eq!(
        (many.status, many.body.len()),
        (StatusCode::OK, bytes.len())
    );
    let past = fetch_with(
        &anon,
        "GET",
        "/api/images/file/cat.png",
        &[("range", "bytes=99999-")],
    )
    .await;
    assert_eq!(past.status, 416);
    assert_eq!(past.header("content-type"), "application/problem+json");
    assert_eq!(
        past.header("content-range"),
        format!("bytes */{}", bytes.len())
    );

    // A `Range` under `If-Range` holds only while the copy it names is this one.
    let etag = res.header("etag").to_owned();
    for (if_range, status) in [
        (etag.as_str(), 206),
        (modified.as_str(), 206),
        ("\"other\"", 200),
        ("Mon, 01 Jan 2001 00:00:00 GMT", 200),
    ] {
        let res = fetch_with(
            &anon,
            "GET",
            "/api/images/file/cat.png",
            &[("range", "bytes=0-9"), ("if-range", if_range)],
        )
        .await;
        assert_eq!(res.status.as_u16(), status, "{if_range}");
    }
    for (k, v) in [
        ("if-none-match", etag.as_str()),
        ("if-none-match", "\"other\", W/\"x\", *"),
        ("if-modified-since", modified.as_str()),
    ] {
        let again = fetch_with(&anon, "GET", "/api/images/file/cat.png", &[(k, v)]).await;
        assert_eq!(again.status, 304, "{k}: {v}");
        assert!(again.body.is_empty());
        assert_eq!(again.header("etag"), etag);
    }
    let changed = fetch_with(
        &anon,
        "GET",
        "/api/images/file/cat.png",
        &[("if-none-match", "\"other\"")],
    )
    .await;
    assert_eq!(changed.status, 200);
    let older = fetch_with(
        &anon,
        "GET",
        "/api/images/file/cat.png",
        &[("if-modified-since", "Mon, 01 Jan 2001 00:00:00 GMT")],
    )
    .await;
    assert_eq!(older.status, 200);

    // A `HEAD` answers as the `GET` does, without the bytes, a `Range` or not.
    for range in [None, Some(("range", "bytes=0-9"))] {
        let head = fetch_with(&anon, "HEAD", "/api/images/file/cat.png", range.as_slice()).await;
        assert_eq!(head.status, 200);
        assert!(head.body.is_empty());
        assert_eq!(head.header("content-length"), bytes.len().to_string());
        assert_eq!(head.header("etag"), etag);
        assert_eq!(head.header("content-type"), "image/png");
    }

    // A size and its other format, under their own names.
    let entry = seed_size(&pool, &d.root, file, "thumbnail").await;
    let small = fetch(&anon, "/api/images/file/cat-10x5.png").await;
    assert_eq!(small.status, 200, "{}", small.json());
    assert_eq!(small.body, png(10, 5));
    assert_eq!(small.header("content-type"), "image/png");
    assert_eq!(small.header("etag"), format!("\"{file}-cat-10x5.png\""));
    let webp = fetch(&anon, "/api/images/file/cat-10x5.webp").await;
    assert_eq!(webp.status, 200);
    assert_eq!(webp.header("content-type"), "image/webp");
    let got = ok(
        &anon,
        "GET",
        &format!("/api/images/{}", doc["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(got["sizes"]["thumbnail"]["width"], entry["width"]);
    assert_eq!(
        got["sizes"]["thumbnail"]["url"],
        "http://localhost:3000/api/images/file/cat-10x5.png"
    );
    assert_eq!(
        got["sizes"]["thumbnail"]["formats"]["webp"]["url"],
        "http://localhost:3000/api/images/file/cat-10x5.webp"
    );

    // Another document's original named as this size is: the original wins, and the size
    // is found only within its own file.
    let twin = put(&app, "images", &png(2, 2), "cat-10x5.png", None).await;
    assert_eq!(twin["filename"], "cat-10x5.png");
    let twin_file = held(&pool, &twin["id"], "published_file").await.unwrap();
    let twin_bytes = stored(&d.root, &files::get(&pool, twin_file).await.unwrap().key);
    let res = fetch(&anon, "/api/images/file/cat-10x5.png").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, twin_bytes);
    assert_eq!(res.header("etag"), format!("\"{twin_file}-cat-10x5.png\""));

    for missing in ["/api/images/file/nope.png", "/api/nowhere/file/cat.png"] {
        assert_eq!(fetch(&anon, missing).await.status, 404, "{missing}");
    }
    // A collection without uploads has no files.
    ok(
        &app,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "posts", "fields": [{"name": "title", "type": "text"}]})),
    )
    .await;
    assert_eq!(fetch(&anon, "/api/posts/file/cat.png").await.status, 404);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn private_files_stay_private(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    // Every collection first: a router reads schemas when it starts.
    collection(&app, "drafted", json!({"versions": {"drafts": true}})).await;
    collection(
        &app,
        "mine",
        json!({"access": {"read": {"owner": "$user.id"}, "create": true},
               "fields": [{"name": "owner", "type": "relationship", "relationTo": "users"}]}),
    )
    .await;
    collection(
        &app,
        "hidden",
        json!({"fields": [{"name": "filename", "type": "text",
                           "access": {"read": {"role": ["admin", "editor"]}}}]}),
    )
    .await;
    collection(&app, "kept", json!({"versions": {"maxVersions": 10}})).await;
    collection(
        &app,
        "members",
        json!({"access": {"read": {"signedIn": true}}}),
    )
    .await;
    let anon = router(&pool, &d).await;
    let editor = as_role(&pool, &d, "editor@example.com", "editor").await;
    let viewer = as_role(&pool, &d, "viewer@example.com", "viewer").await;
    let not_found = |res: &Raw, what: &str| {
        assert_eq!(res.status, 404, "{what}");
        assert_eq!(
            res.header("content-type"),
            "application/problem+json",
            "{what}"
        );
        assert!(res.json().get("detail").is_none(), "{what}: says nothing");
    };

    // A draft's file: nobody gets it without asking for drafts, and only who may read them.
    put(&app, "drafted", &png(1, 1), "d.png", None).await;
    not_found(&fetch(&anon, "/api/drafted/file/d.png").await, "anonymous");
    not_found(
        &fetch(&editor, "/api/drafted/file/d.png").await,
        "editor, published",
    );
    assert_problem_raw(
        &fetch(&anon, "/api/drafted/file/d.png?draft=true").await,
        401,
    );
    let res = fetch(&editor, "/api/drafted/file/d.png?draft=true").await;
    assert_eq!(res.status, 200, "{}", res.json());
    assert_eq!(res.header("cache-control"), "private, no-store");

    // Own-only reads: another Author's file is missing.
    let amy_id = add_user(&pool, "amy@example.com", "author").await;
    let amy = signed_in(router(&pool, &d).await, "amy@example.com").await;
    let bob = as_role(&pool, &d, "bob@example.com", "author").await;
    put(
        &amy,
        "mine",
        &png(1, 1),
        "m.png",
        Some(json!({"owner": amy_id})),
    )
    .await;
    let res = fetch(&amy, "/api/mine/file/m.png").await;
    assert_eq!(res.status, 200, "{}", res.json());
    assert_eq!(res.header("cache-control"), "private, no-store");
    not_found(&fetch(&bob, "/api/mine/file/m.png").await, "another author");

    // A field rule hiding `filename` hides the file, even published.
    put(&app, "hidden", &png(1, 1), "h.png", None).await;
    not_found(&fetch(&viewer, "/api/hidden/file/h.png").await, "viewer");
    let res = fetch(&editor, "/api/hidden/file/h.png").await;
    assert_eq!(res.status, 200);
    assert_eq!(
        res.header("cache-control"),
        "private, no-store",
        "not for everyone"
    );

    // A file only a version holds, and a trashed document's, are served to nobody.
    let doc = put(&app, "kept", &png(1, 1), "old.png", None).await;
    let res = upload(
        &app,
        "PATCH",
        &format!("/api/kept/{}", doc["id"].as_str().unwrap()),
        Some(&png(2, 2)),
        "new.png",
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(fetch(&app, "/api/kept/file/new.png").await.status, 200);
    not_found(&fetch(&app, "/api/kept/file/old.png").await, "version only");
    ok(
        &app,
        "DELETE",
        &format!("/api/kept/{}", doc["id"].as_str().unwrap()),
        None,
    )
    .await;
    not_found(&fetch(&app, "/api/kept/file/new.png").await, "trashed");

    // Signed out where only the signed in may read: sign in first.
    put(&app, "members", &png(1, 1), "s.png", None).await;
    assert_problem_raw(&fetch(&anon, "/api/members/file/s.png").await, 401);
    assert_eq!(fetch(&viewer, "/api/members/file/s.png").await.status, 200);
    // A parameter the route doesn't take.
    assert_eq!(
        fetch(&viewer, "/api/members/file/s.png?id=x").await.status,
        400
    );
}

fn assert_problem_raw(res: &Raw, status: u16) {
    assert_eq!(res.status.as_u16(), status, "{}", res.json());
    assert_eq!(res.header("content-type"), "application/problem+json");
    assert_eq!(res.json()["status"], status);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn active_content_is_sandboxed(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    // HTML is on the restricted list, so it takes allowing restricted types as well.
    collection(
        &app,
        "pages",
        json!({"upload": {"mimeTypes": ["text/*"], "allowRestrictedFileTypes": true}}),
    )
    .await;
    collection(&app, "images", json!({})).await;
    let anon = router(&pool, &d).await;
    let html = b"<!DOCTYPE html><html><script>alert(1)</script></html>";
    let doc = put(&app, "pages", html, "page.html", None).await;
    assert_eq!(doc["mimeType"], "text/html");
    let res = fetch(&anon, "/api/pages/file/page.html").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("content-type"), "text/html");
    assert_eq!(res.header("x-content-type-options"), "nosniff");
    assert_eq!(res.header("content-security-policy"), "sandbox");

    let mut polyglot = png(2, 2);
    polyglot.extend_from_slice(b"<html><script>alert(1)</script></html>");
    put(&app, "images", &polyglot, "poly.png", None).await;
    let res = fetch(&anon, "/api/images/file/poly.png").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("content-type"), "image/png");
    assert_eq!(res.header("x-content-type-options"), "nosniff");
    assert_eq!(res.header("content-security-policy"), "");

    let svg =
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="1"/></svg>"#;
    put(&app, "images", svg, "logo.svg", None).await;
    let res = fetch(&anon, "/api/images/file/logo.svg").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("content-type"), "image/svg+xml");
    assert_eq!(res.header("x-content-type-options"), "nosniff");
    assert_eq!(res.header("content-security-policy"), "sandbox");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn modify_response_headers_cant_override_security(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"upload": {"modifyResponseHeaders": {"X-Robots-Tag": "noindex"}}}),
    )
    .await;
    let anon = router(&pool, &d).await;
    put(&app, "images", &png(1, 1), "a.png", None).await;
    let res = fetch(&anon, "/api/images/file/a.png").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("x-robots-tag"), "noindex");
    assert_eq!(res.header("content-type"), "image/png");
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(
            &json!({"slug": "loose", "upload": {"modifyResponseHeaders":
                {"Content-Security-Policy": "default-src *"}}, "fields": []})
            .to_string(),
        ),
    )
    .await;
    assert_problem(&res, 422);
    assert!(
        res.json.to_string().contains("modifyResponseHeaders"),
        "{}",
        res.json
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_restored_version_serves_its_old_file(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"versions": {"drafts": true, "maxVersions": 3}}),
    )
    .await;
    let doc = put(&app, "images", &png(1, 1), "a.png", None).await;
    let id = doc["id"].as_str().unwrap().to_owned();
    let one = format!("/api/images/{id}");
    let a = held(&pool, &doc["id"], "draft_file").await.unwrap();
    let first = ok(&app, "GET", &format!("{one}/versions"), None).await["docs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let res = upload(&app, "PATCH", &one, Some(&png(2, 2)), "b.png", None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    ok(&app, "POST", &format!("{one}/publish"), None).await;
    // Versions: A's draft, B's draft, B published. Restoring A's records a fourth, which
    // prunes the first: A is now held by the draft and the new version alone.
    let restored = ok(
        &app,
        "POST",
        &format!("{one}/versions/{first}/restore"),
        None,
    )
    .await;
    assert_eq!(restored["filename"], "a.png");
    assert_eq!(
        restored["url"],
        "http://localhost:3000/api/images/file/a.png"
    );
    let draft_url = format!("{}?draft=true", path_of(&restored["url"]));
    let res = fetch(&app, &draft_url).await;
    assert_eq!(res.status, 200, "{}", res.json());
    assert_eq!(
        res.body,
        stored(&d.root, &files::get(&pool, a).await.unwrap().key)
    );
    ok(&app, "POST", &format!("{one}/publish"), None).await;
    assert_eq!(fetch(&app, &path_of(&restored["url"])).await.status, 200);

    // Replaced again in both copies, and the versions holding A pruned away: one cleanup.
    let cleanups = async || -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = $1 AND payload->>'file' = $2
               AND state IN ('queued', 'running')",
        )
        .bind(CLEANUP)
        .bind(a.to_string())
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    for name in ["c.png", "d.png", "e.png", "f.png"] {
        let res = upload(&app, "PATCH", &one, Some(&png(3, 3)), name, None).await;
        assert_eq!(res.status, 200, "{}", res.json);
        if name == "c.png" {
            ok(&app, "POST", &format!("{one}/publish"), None).await;
        }
    }
    let state: String = sqlx::query_scalar("SELECT state FROM files WHERE id = $1")
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "gone");
    assert_eq!(cleanups().await, 1);
    assert_eq!(fetch(&app, &draft_url).await.status, 404);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn urls_are_filled_on_every_read(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"versions": {"maxVersions": 5},
               "upload": {"imageSizes": [{"name": "thumbnail", "width": 10}],
                          "adminThumbnail": "thumbnail"}}),
    )
    .await;
    ok(
        &app,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "image", "type": "relationship", "relationTo": "images"}]})),
    )
    .await;
    collection(
        &app,
        "public",
        json!({"upload": {"storage": "r2",
        "imageSizes": [{"name": "thumbnail", "width": 10}]}}),
    )
    .await;
    collection(
        &app,
        "public_drafts",
        json!({"versions": {"drafts": true},
        "upload": {"storage": "r2"}}),
    )
    .await;
    collection(
        &app,
        "public_members",
        json!({"access": {"read": {"signedIn": true}},
        "upload": {"storage": "r2"}}),
    )
    .await;
    collection(
        &app,
        "scoped",
        json!({"upload": {"filenameCompoundIndex": ["alt"]}}),
    )
    .await;
    let anon = router(&pool, &d).await;
    let doc = put(&app, "images", &png(4, 4), "cat.png", None).await;
    let url = "http://localhost:3000/api/images/file/cat.png";
    assert_eq!(doc["url"], url);
    assert_eq!(
        doc["thumbnailURL"], url,
        "no thumbnail yet: the image itself"
    );
    let id = doc["id"].as_str().unwrap();
    let file = held(&pool, &doc["id"], "published_file").await.unwrap();
    seed_size(&pool, &d.root, file, "thumbnail").await;
    let small = "http://localhost:3000/api/images/file/cat-10x5.png";
    let got = ok(&anon, "GET", &format!("/api/images/{id}"), None).await;
    assert_eq!(
        (&got["url"], &got["thumbnailURL"]),
        (&json!(url), &json!(small))
    );
    assert_eq!(got["sizes"]["thumbnail"]["url"], small);
    let listed = ok(&anon, "GET", "/api/images", None).await;
    assert_eq!(listed["docs"][0]["url"], url);
    assert_eq!(listed["docs"][0]["sizes"]["thumbnail"]["url"], small);
    let post = ok(
        &app,
        "POST",
        "/api/posts",
        Some(json!({"title": "p", "image": id})),
    )
    .await;
    let populated = ok(
        &anon,
        "GET",
        &format!("/api/posts/{}?depth=1", post["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(populated["image"]["url"], url);
    assert_eq!(populated["image"]["thumbnailURL"], small);
    let selected = ok(
        &anon,
        "GET",
        &format!("/api/images/{id}?select[alt]=true"),
        None,
    )
    .await;
    assert!(selected.get("url").is_none(), "{selected}");
    let versions = ok(&app, "GET", &format!("/api/images/{id}/versions"), None).await;
    let vid = versions["docs"][0]["id"].as_str().unwrap();
    let version = ok(
        &app,
        "GET",
        &format!("/api/images/{id}/versions/{vid}"),
        None,
    )
    .await;
    assert_eq!(version["data"]["url"], url, "{version}");
    // Stored nowhere.
    let raw: Value = sqlx::query_scalar("SELECT published FROM documents WHERE id = $1")
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(raw.get("url").is_none() && raw["sizes"]["thumbnail"].get("url").is_none());

    // A public bucket: links go straight to it.
    let doc = put(&app, "public", &png(4, 4), "cat.png", None).await;
    let file = held(&pool, &doc["id"], "published_file").await.unwrap();
    let key = files::get(&pool, file).await.unwrap().key;
    assert_eq!(doc["url"], format!("https://media.tachyoncms.com/{key}"));
    seed_size(&pool, &d.r2, file, "thumbnail").await;
    let got = ok(&anon, "GET", "/api/public", None).await["docs"][0].clone();
    let dir = key.rsplit_once('/').unwrap().0;
    assert_eq!(
        got["sizes"]["thumbnail"]["url"],
        format!("https://media.tachyoncms.com/{dir}/cat-10x5.png")
    );
    // With drafts, never: a draft's object has no public URL.
    let doc = put(&app, "public_drafts", &png(4, 4), "cat.png", None).await;
    assert_eq!(
        doc["url"],
        "http://localhost:3000/api/public_drafts/file/cat.png"
    );
    // Nor when a signed-out caller can't read everything.
    let doc = put(&app, "public_members", &png(4, 4), "cat.png", None).await;
    assert_eq!(
        doc["url"],
        "http://localhost:3000/api/public_members/file/cat.png"
    );

    // Names shared across `filenameCompoundIndex` scopes carry the document.
    let one = put(
        &app,
        "scoped",
        &png(1, 1),
        "x.png",
        Some(json!({"alt": "a"})),
    )
    .await;
    let two = put(
        &app,
        "scoped",
        &png(2, 2),
        "x.png",
        Some(json!({"alt": "b"})),
    )
    .await;
    assert_eq!(two["filename"], "x.png", "another scope, the same name");
    for doc in [&one, &two] {
        let id = doc["id"].as_str().unwrap();
        assert_eq!(
            doc["url"],
            format!("http://localhost:3000/api/scoped/file/x.png?id={id}")
        );
        let file = held(&pool, &doc["id"], "published_file").await.unwrap();
        let res = fetch(&anon, &path_of(&doc["url"])).await;
        assert_eq!(res.status, 200);
        assert_eq!(res.header("etag"), format!("\"{file}-x.png\""));
    }
    assert_eq!(
        fetch(
            &anon,
            &format!("/api/scoped/file/x.png?id={}", Uuid::now_v7())
        )
        .await
        .status,
        404
    );
}
