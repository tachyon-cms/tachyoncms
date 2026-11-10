//! Creating and replacing files with `multipart/form-data`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use common::{
    ADMIN, PASSWORD, Res, add_user, assert_problem, multipart, png, send, send_body, signed_in,
    upload,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_api::{JobsConfig, Options, UploadsConfig};
use tachyon_db::files::{self, CLEANUP, NewFile};
use tachyon_media::fixtures;
use tachyon_media::storage::{StorageConfig, Storages, object_key};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// A spool directory and a storage root of a test's own.
struct Dirs {
    temp: PathBuf,
    root: PathBuf,
}

fn dirs() -> Dirs {
    let base = std::env::temp_dir().join(format!("tachyon-uploads-{}", Uuid::now_v7()));
    let d = Dirs {
        temp: base.join("spool"),
        root: base.join("store"),
    };
    std::fs::create_dir_all(&d.temp).unwrap();
    std::fs::create_dir_all(&d.root).unwrap();
    d
}

fn options(d: &Dirs, workers: usize) -> Options {
    let storages = Storages::open(&BTreeMap::from([(
        "default".into(),
        StorageConfig::local(&d.root),
    )]))
    .unwrap();
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

/// The API over `d`, set up and signed in as its first Admin.
async fn admin(pool: &PgPool, d: &Dirs) -> Router {
    let app = router(pool, d).await;
    let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "setup: {}", res.json);
    signed_in(app, ADMIN).await
}

/// The API over `d`, signed in as a new user with `role`.
async fn as_role(pool: &PgPool, d: &Dirs, email: &str, role: &str) -> Router {
    add_user(pool, email, role).await;
    signed_in(router(pool, d).await, email).await
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Res {
    send(app, method, uri, body.map(|b| b.to_string()).as_deref()).await
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let res = call(app, method, uri, body).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

/// Creates `slug`, an upload collection with an `alt` text field, `extra` merged in.
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

/// A multipart body to `uri` with a 200 kB file that's cut off halfway: a handler that reads
/// it answers 400, so a 403 shows it was never read.
async fn cut_off(app: &Router, method: &str, uri: &str) -> Res {
    let (kind, body) = multipart(Some(&[b'a'; 200_000]), "a.txt", None);
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> = vec![
        Ok(body[..body.len() / 2].to_vec()),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "gone",
        )),
    ];
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", kind);
    send_body(
        app,
        req,
        Body::from_stream(futures_util::stream::iter(chunks)),
    )
    .await
}

/// Everything under `dir`, recursively.
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files_in(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// The file the copy of document `id` holds.
async fn held(pool: &PgPool, id: &str, column: &str) -> Option<Uuid> {
    let sql = match column {
        "published_file" => "SELECT published_file FROM documents WHERE id = $1",
        _ => "SELECT draft_file FROM documents WHERE id = $1",
    };
    sqlx::query_scalar(sql)
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn state_of(pool: &PgPool, file: Uuid) -> String {
    files::get(pool, file).await.unwrap().state
}

async fn file_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM files")
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

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn upload_creates_a_document(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"upload": {"imageSizes": [{"name": "thumbnail", "width": 100}]}}),
    )
    .await;
    let image = png(3, 2);
    let res = upload(
        &app,
        "POST",
        "/api/images",
        Some(&image),
        "cat.png",
        Some(&json!({"alt": "x"})),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let doc = &res.json;
    assert_eq!(doc["filename"], "cat.png");
    assert_eq!(doc["mimeType"], "image/png");
    assert_eq!(doc["filesize"], image.len());
    assert_eq!(doc["width"], 3, "{doc}");
    assert_eq!(doc["height"], 2, "{doc}");
    assert!(doc["blurhash"].is_string(), "{doc}");
    assert_eq!(doc["focalX"], 50);
    assert_eq!(doc["focalY"], 50);
    assert_eq!(doc["alt"], "x");
    assert_eq!(doc["sizes"], json!({"thumbnail": null}));
    let id = doc["id"].as_str().unwrap();
    let file = held(&pool, id, "published_file").await.expect("a file");
    let row = files::get(&pool, file).await.unwrap();
    assert_eq!(row.state, "live");
    assert_eq!(row.filename, "cat.png");
    assert_eq!(std::fs::read(d.root.join(&row.key)).unwrap(), image);
    assert!(files_in(&d.temp).is_empty(), "the spool file is gone");
    // A second file under the same name is numbered.
    let res = upload(&app, "POST", "/api/images", Some(&image), "cat.png", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["filename"], "cat-1.png");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn sniffing_ignores_the_name_and_type(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"upload": {"mimeTypes": ["image/*"]}}),
    )
    .await;
    let html = b"<!DOCTYPE html><html><script>alert(1)</script></html>";
    let res = upload(&app, "POST", "/api/images", Some(html), "a.png", None).await;
    assert_problem(&res, 415);
    let detail = res.json["detail"].as_str().unwrap();
    assert!(detail.contains("text/html"), "{detail}");
    assert!(detail.contains("image/*"), "{detail}");
    assert_eq!(file_rows(&pool).await, 0);
    assert!(files_in(&d.root).is_empty());
    assert!(files_in(&d.temp).is_empty());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn restricted_types_are_refused(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "files", json!({})).await;
    collection(
        &app,
        "anything",
        json!({"upload": {"allowRestrictedFileTypes": true}}),
    )
    .await;
    let mut elf = b"\x7fELF\x02\x01\x01\x00".to_vec();
    elf.extend_from_slice(&[0; 64]);
    let script = b"#!/bin/sh\necho hi\n".to_vec();
    let mut pe = b"MZ\x90\0\x03\0\0\0".to_vec();
    pe.resize(64, 0);
    let mut macho = b"\xCF\xFA\xED\xFE".to_vec();
    macho.resize(64, 0);
    for (bytes, name) in [
        (&elf, "tool.bin"),
        (&script, "notes.txt"),
        (&pe, "a.bin"),
        (&macho, "b.bin"),
    ] {
        let res = upload(&app, "POST", "/api/files", Some(bytes), name, None).await;
        assert_problem(&res, 415);
        let res = upload(&app, "POST", "/api/anything", Some(bytes), name, None).await;
        assert_eq!(res.status, 201, "{name}: {}", res.json);
    }
    // A plain text file named like a script is refused by its name.
    let res = upload(&app, "POST", "/api/files", Some(b"hello"), "run.sh", None).await;
    assert_problem(&res, 415);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn over_the_limit_is_413(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "files", json!({"upload": {"filesizeLimit": 1000}})).await;
    let res = upload(
        &app,
        "POST",
        "/api/files",
        Some(&[b'a'; 1001]),
        "a.txt",
        None,
    )
    .await;
    assert_problem(&res, 413);
    let detail = res.json["detail"].as_str().unwrap();
    assert!(detail.contains("1000"), "{detail}");
    assert!(files_in(&d.temp).is_empty());
    assert_eq!(file_rows(&pool).await, 0);
    let res = upload(
        &app,
        "POST",
        "/api/files",
        Some(&[b'a'; 1000]),
        "a.txt",
        None,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn an_aborted_upload_leaves_no_spool_file(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "files", json!({})).await;
    let (kind, body) = multipart(Some(&[b'a'; 200_000]), "a.txt", None);
    let half = body[..body.len() / 2].to_vec();
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> = vec![
        Ok(half),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "gone",
        )),
    ];
    let req = Request::builder()
        .method("POST")
        .uri("/api/files")
        .header("content-type", kind);
    let res = send_body(
        &app,
        req,
        Body::from_stream(futures_util::stream::iter(chunks)),
    )
    .await;
    assert_problem(&res, 400);
    assert!(files_in(&d.temp).is_empty(), "{:?}", files_in(&d.temp));
    assert_eq!(file_rows(&pool).await, 0);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn stray_parts_are_refused(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "files", json!({})).await;
    let b = common::BOUNDARY;
    let part = |name: &str| {
        format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"a.txt\"\r\n\r\nhello\r\n"
        )
    };
    for (body, named) in [
        (
            format!("{}{}--{b}--\r\n", part("file"), part("file")),
            "file",
        ),
        (
            format!("{}{}--{b}--\r\n", part("file"), part("extra")),
            "extra",
        ),
        (
            {
                let payload = format!(
                    "--{b}\r\nContent-Disposition: form-data; name=\"_payload\"\r\n\r\n{{}}\r\n"
                );
                format!("{payload}{payload}--{b}--\r\n")
            },
            "one `_payload`",
        ),
        (
            format!("--{b}\r\nContent-Disposition: form-data\r\n\r\nhello\r\n--{b}--\r\n"),
            "has no name",
        ),
    ] {
        let req = Request::builder()
            .method("POST")
            .uri("/api/files")
            .header("content-type", format!("multipart/form-data; boundary={b}"));
        let res = send_body(&app, req, Body::from(body)).await;
        assert_problem(&res, 400);
        let detail = res.json["detail"].as_str().unwrap();
        assert!(detail.contains(named), "{detail}");
    }
    assert!(files_in(&d.temp).is_empty());
    assert_eq!(file_rows(&pool).await, 0);
    // A `_payload` that isn't JSON.
    let (kind, _) = multipart(None, "", None);
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"_payload\"\r\n\r\n{{nope\r\n--{b}--\r\n"
    );
    let req = Request::builder()
        .method("POST")
        .uri("/api/files")
        .header("content-type", kind);
    assert_problem(&send_body(&app, req, Body::from(body)).await, 400);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn files_required_on_create(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "files", json!({})).await;
    collection(
        &app,
        "optional",
        json!({"upload": {"filesRequiredOnCreate": false}}),
    )
    .await;
    let alt = json!({"alt": "x"});
    for res in [
        call(&app, "POST", "/api/files", Some(alt.clone())).await,
        upload(&app, "POST", "/api/files", None, "", Some(&alt)).await,
    ] {
        assert_problem(&res, 422);
        assert_eq!(res.json["errors"][0]["path"], "file", "{}", res.json);
    }
    let doc = ok(&app, "POST", "/api/optional", Some(alt)).await;
    assert_eq!(doc["alt"], "x");
    assert_eq!(
        held(&pool, doc["id"].as_str().unwrap(), "published_file").await,
        None
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_failed_insert_leaves_nothing_stored(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"fields": [
            {"name": "alt", "type": "text", "maxLength": 3},
            {"name": "code", "type": "text", "unique": true}
        ]}),
    )
    .await;
    let image = png(1, 1);
    // Refused by validation before anything is stored.
    let res = upload(
        &app,
        "POST",
        "/api/images",
        Some(&image),
        "a.png",
        Some(&json!({"alt": "too long"})),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "alt");
    assert_eq!(file_rows(&pool).await, 0);
    assert!(files_in(&d.root).is_empty());
    assert!(files_in(&d.temp).is_empty());

    // Refused inside the document's transaction, once the object is stored: the value is
    // taken, which only the insert finds out.
    let code = json!({"code": "x"});
    let first = upload(
        &app,
        "POST",
        "/api/images",
        Some(&image),
        "a.png",
        Some(&code),
    )
    .await;
    assert_eq!(first.status, 201, "{}", first.json);
    let res = upload(
        &app,
        "POST",
        "/api/images",
        Some(&image),
        "b.png",
        Some(&code),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "code");
    let (file, state): (Uuid, String) =
        sqlx::query_as("SELECT id, state FROM files WHERE filename = 'b.png'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "gone", "no document holds it, and none can take it");
    let key = files::get(&pool, file).await.unwrap().key;
    assert!(
        d.root.join(&key).exists(),
        "stored before the insert failed"
    );
    let due: bool = sqlx::query_scalar(
        "SELECT run_at <= now() FROM jobs WHERE kind = $1 AND payload->>'file' = $2
           AND state = 'queued'",
    )
    .bind(CLEANUP)
    .bind(file.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(due, "its cleanup is due now, not in an hour");
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    until("its row is deleted", async || {
        files::get(&pool, file).await.is_err()
    })
    .await;
    assert!(!d.root.join(&key).exists(), "its object is deleted");
    assert!(files_in(&d.temp).is_empty());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn pending_uploads_expire(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "files", json!({})).await;
    let c = tachyon_db::collections::get_by_slug(&pool, "files")
        .await
        .unwrap();
    // A reserve and its object, as an upload leaves them when it dies before its commit.
    let f = NewFile {
        collection_id: c.id,
        storage: "default",
        filename: "lost.txt",
        scope: "",
        mime_type: "text/plain",
        filesize: 5,
        width: None,
        height: None,
        focal: (50.0, 50.0),
        blurhash: None,
        issued_to: None,
        document: None,
    };
    let file = files::reserve(&pool, &f, |id, name| {
        object_key(None, "files", id, name).map_err(|e| e.to_string())
    })
    .await
    .unwrap();
    let source = d.temp.join("source");
    std::fs::write(&source, b"hello").unwrap();
    let storages = Storages::open(&BTreeMap::from([(
        "default".into(),
        StorageConfig::local(&d.root),
    )]))
    .unwrap();
    storages
        .get("default")
        .unwrap()
        .put_file(&file.key, &source, "text/plain")
        .await
        .unwrap();
    std::fs::remove_file(&source).unwrap();
    // Within the grace, its cleanup isn't due yet.
    let run_at: OffsetDateTime =
        sqlx::query_scalar("SELECT run_at FROM jobs WHERE kind = $1 AND payload->>'file' = $2")
            .bind(CLEANUP)
            .bind(file.id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(run_at > OffsetDateTime::now_utc() + time::Duration::minutes(59));
    // The grace runs out: the reserve and its cleanup both move two hours back.
    sqlx::query("UPDATE files SET created_at = now() - interval '2 hours' WHERE id = $1")
        .bind(file.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE jobs SET run_at = now() - interval '1 hour' WHERE kind = $1 AND payload->>'file' = $2",
    )
    .bind(CLEANUP)
    .bind(file.id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    until("the reserve is deleted", async || {
        files::get(&pool, file.id).await.is_err()
    })
    .await;
    assert!(!d.root.join(&file.key).exists());
    drop(app);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn replace_keeps_the_old_file_for_versions(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "images", json!({"versions": {"maxVersions": 10}})).await;
    let doc = upload(&app, "POST", "/api/images", Some(&png(1, 1)), "a.png", None).await;
    assert_eq!(doc.status, 201, "{}", doc.json);
    let id = doc.json["id"].as_str().unwrap().to_owned();
    let old = held(&pool, &id, "published_file").await.unwrap();
    // As an image's placeholder will be, once there are some.
    sqlx::query(
        "UPDATE documents SET published = published || '{\"blurhash\": \"LKO2\"}' WHERE id = $1",
    )
    .bind(id.parse::<Uuid>().unwrap())
    .execute(&pool)
    .await
    .unwrap();
    let bigger = png(4, 4);
    let res = upload(
        &app,
        "PATCH",
        &format!("/api/images/{id}"),
        Some(&bigger),
        "a.png",
        Some(&json!({"alt": "new"})),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["filename"], "a.png", "the same name");
    assert_eq!(res.json["filesize"], bigger.len());
    assert_eq!(res.json["alt"], "new");
    assert!(res.json["blurhash"].is_string(), "{}", res.json);
    assert_ne!(res.json["blurhash"], "LKO2", "not the old file's");
    let new = held(&pool, &id, "published_file").await.unwrap();
    assert_ne!(new, old);
    assert_eq!(state_of(&pool, new).await, "live");
    assert_eq!(state_of(&pool, old).await, "live", "a version holds it");
    let old_key = files::get(&pool, old).await.unwrap().key;
    assert!(d.root.join(old_key).exists());
}

/// `doc` without the document's own metadata, which no PATCH writes back.
fn writable(doc: &Value) -> Value {
    let mut back = doc.clone();
    for k in ["id", "createdAt", "updatedAt", "status"] {
        back.as_object_mut().unwrap().remove(k);
    }
    back
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn server_set_fields_are_refused(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"upload": {"imageSizes": [{"name": "thumbnail", "width": 100}]}}),
    )
    .await;
    let res = upload(
        &app,
        "POST",
        "/api/images",
        Some(&png(1, 1)),
        "a.png",
        Some(&json!({"filesize": 1})),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "filesize");
    assert_eq!(res.json["errors"][0]["message"], "set by the server");
    assert_eq!(file_rows(&pool).await, 0);
    let doc = upload(&app, "POST", "/api/images", Some(&png(1, 1)), "a.png", None).await;
    assert_eq!(doc.status, 201, "{}", doc.json);
    let id = doc.json["id"].as_str().unwrap().to_owned();
    let one = format!("/api/images/{id}");
    let file = held(&pool, &id, "published_file").await;
    // A JSON write can't set them either.
    let res = call(&app, "PATCH", &one, Some(json!({"mimeType": "text/html"}))).await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "mimeType");
    // What a read gave back, unchanged, focal point included, writes fine; so do the values
    // filled on read, which are never stored.
    let got = ok(&app, "GET", &one, None).await;
    let mut back = writable(&got);
    back["alt"] = "edited".into();
    back["url"] = "/somewhere".into();
    back["sizes"]["thumbnail"] = json!({"url": "/somewhere-small"});
    let res = ok(&app, "PATCH", &one, Some(back)).await;
    assert_eq!(res["alt"], "edited");
    assert_eq!(res["filename"], got["filename"]);
    assert_eq!(res["focalX"], 50);
    assert_eq!(res["width"], 1, "{res}");
    assert_eq!(res["height"], 1, "{res}");
    assert_eq!(res["sizes"], json!({"thumbnail": null}));
    assert_eq!(res["filesize"], got["filesize"]);
    let again = ok(&app, "GET", &one, None).await;
    assert_eq!(again["width"], 1);
    assert_eq!(again["sizes"], json!({"thumbnail": null}));
    assert_eq!(
        held(&pool, &id, "published_file").await,
        file,
        "the same file"
    );
    // A body read before another write replaced the file describes a file the document no
    // longer holds: refused against the document as it is now.
    let stale = writable(&again);
    let res = upload(&app, "PATCH", &one, Some(&png(2, 2)), "b.png", None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = call(&app, "PATCH", &one, Some(stale)).await;
    assert_problem(&res, 422);
    let paths: Vec<&str> = res.json["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"filename"), "{paths:?}");
    assert_eq!(ok(&app, "GET", &one, None).await["filename"], "b.png");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn uploads_obey_access(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"fields": [
            {"name": "alt", "type": "text"},
            {"name": "secret", "type": "text",
             "access": {"create": {"role": "admin"}, "update": {"role": "admin"}}}
        ]}),
    )
    .await;
    collection(
        &app,
        "locked",
        json!({"access": {"update": {"alt": "open"}}}),
    )
    .await;
    let stored = || files_in(&d.root);
    // Each refusal comes before the body is read: reading it would be a 400.
    let viewer = as_role(&pool, &d, "viewer@example.com", "viewer").await;
    assert_problem(&cut_off(&viewer, "POST", "/api/images").await, 403);
    assert_eq!(file_rows(&pool).await, 0);
    let author = as_role(&pool, &d, "author@example.com", "author").await;
    let other = as_role(&pool, &d, "other@example.com", "author").await;
    let theirs = upload(
        &other,
        "POST",
        "/api/images",
        Some(&png(1, 1)),
        "a.png",
        None,
    )
    .await;
    assert_eq!(theirs.status, 201, "{}", theirs.json);
    let id = theirs.json["id"].as_str().unwrap();
    // Authors read every document here, so it's a 403 rather than a 404, as a JSON PATCH.
    assert_problem(
        &cut_off(&author, "PATCH", &format!("/api/images/{id}")).await,
        403,
    );
    let closed = upload(
        &app,
        "POST",
        "/api/locked",
        Some(&png(1, 1)),
        "c.png",
        Some(&json!({"alt": "closed"})),
    )
    .await;
    assert_eq!(closed.status, 201, "{}", closed.json);
    let editor = as_role(&pool, &d, "editor@example.com", "editor").await;
    let at = format!("/api/locked/{}", closed.json["id"].as_str().unwrap());
    assert_problem(&cut_off(&editor, "PATCH", &at).await, 403);
    assert_eq!(file_rows(&pool).await, 2);
    let before = stored();
    // A refusal that depends on what's sent: after the body is read, before anything is
    // reserved or stored.
    let secret = json!({"secret": "x"});
    let res = upload(
        &editor,
        "POST",
        "/api/images",
        Some(&png(1, 1)),
        "s.png",
        Some(&secret),
    )
    .await;
    assert_problem(&res, 403);
    let res = upload(
        &editor,
        "PATCH",
        &format!("/api/images/{id}"),
        Some(&png(1, 1)),
        "s.png",
        Some(&secret),
    )
    .await;
    assert_problem(&res, 403);
    assert_eq!(file_rows(&pool).await, 2, "nothing reserved");
    assert_eq!(stored(), before, "nothing stored");
    assert!(files_in(&d.temp).is_empty(), "the spool is gone");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn restoring_a_version_brings_its_file_back(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"versions": {"maxVersions": 10},
               "upload": {"imageSizes": [{"name": "thumbnail", "width": 100}]}}),
    )
    .await;
    let image = png(1, 1);
    let doc = upload(&app, "POST", "/api/images", Some(&image), "a.png", None).await;
    assert_eq!(doc.status, 201, "{}", doc.json);
    let id = doc.json["id"].as_str().unwrap().to_owned();
    let one = format!("/api/images/{id}");
    let old = held(&pool, &id, "published_file").await.unwrap();
    let first = ok(&app, "GET", &format!("{one}/versions"), None).await["docs"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let res = upload(&app, "PATCH", &one, Some(&png(2, 2)), "b.png", None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_ne!(held(&pool, &id, "published_file").await, Some(old));
    let restored = ok(
        &app,
        "POST",
        &format!("{one}/versions/{first}/restore"),
        None,
    )
    .await;
    assert_eq!(restored["filename"], "a.png");
    assert_eq!(restored["filesize"], image.len());
    assert_eq!(restored["width"], 1, "{restored}");
    assert_eq!(restored["height"], 1, "{restored}");
    assert_eq!(restored["sizes"], json!({"thumbnail": null}));
    assert_eq!(held(&pool, &id, "published_file").await, Some(old));
    assert_eq!(state_of(&pool, old).await, "live");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_scheduled_publish_carries_the_drafts_file(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "images", json!({"versions": {"drafts": true}})).await;
    let doc = upload(&app, "POST", "/api/images", Some(&png(1, 1)), "a.png", None).await;
    assert_eq!(doc.status, 201, "{}", doc.json);
    let id = doc.json["id"].as_str().unwrap().to_owned();
    let draft = held(&pool, &id, "draft_file")
        .await
        .expect("the draft holds it");
    assert_eq!(held(&pool, &id, "published_file").await, None);
    let when = (OffsetDateTime::now_utc() + time::Duration::hours(1))
        .replace_nanosecond(0)
        .unwrap()
        .format(&Rfc3339)
        .unwrap();
    ok(
        &app,
        "PUT",
        &format!("/api/images/{id}/schedule"),
        Some(json!({"publishAt": when})),
    )
    .await;
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'publish'")
        .execute(&pool)
        .await
        .unwrap();
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1))
        .await
        .unwrap();
    until("it's published", async || {
        held(&pool, &id, "published_file").await.is_some()
    })
    .await;
    assert_eq!(held(&pool, &id, "published_file").await, Some(draft));
    assert_eq!(state_of(&pool, draft).await, "live");
    let anon = router(&pool, &d).await;
    let live = ok(&anon, "GET", &format!("/api/images/{id}"), None).await;
    assert_eq!(live["filename"], "a.png");
    assert_eq!(live["width"], 1, "{live}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn endless_part_headers_are_refused(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "files", json!({"upload": {"filesizeLimit": 1000}})).await;
    let b = common::BOUNDARY;
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nX-Long: {}\r\n\r\nhello\r\n--{b}--\r\n",
        "a".repeat(1_000_000)
    );
    // An empty header block that never ends, and a preamble with no boundary: the parser
    // would buffer either until the whole-stream cap.
    let empty = format!("--{b}\r\n\r\n{}", "a".repeat(1_000_000));
    let preamble = "a".repeat(1_000_000);
    for body in [body, empty, preamble] {
        let req = Request::builder()
            .method("POST")
            .uri("/api/files")
            .header("content-type", format!("multipart/form-data; boundary={b}"));
        let res = send_body(&app, req, Body::from(body)).await;
        assert_problem(&res, 400);
        assert!(
            res.json["detail"].as_str().unwrap().contains("16384"),
            "{}",
            res.json
        );
    }
    assert!(files_in(&d.temp).is_empty());
    assert_eq!(file_rows(&pool).await, 0);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_file_may_be_bigger_than_a_json_body(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "files",
        json!({"upload": {"filesizeLimit": 12_000_000}}),
    )
    .await;
    let big = vec![b'a'; 11_000_000];
    let res = upload(&app, "POST", "/api/files", Some(&big), "big.txt", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["filesize"], 11_000_000);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_server_limit_applies_without_a_collection_one(pool: PgPool) {
    let d = dirs();
    let mut o = options(&d, 0);
    o.uploads.max_file_size = 1000;
    let app = tachyon_api::router_with(pool.clone(), o).await.unwrap();
    let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    assert_eq!(
        send(&app, "POST", "/api/setup", Some(&body.to_string()))
            .await
            .status,
        201
    );
    let app = signed_in(app, ADMIN).await;
    collection(&app, "files", json!({})).await;
    let res = upload(
        &app,
        "POST",
        "/api/files",
        Some(&[b'a'; 1001]),
        "a.txt",
        None,
    )
    .await;
    assert_problem(&res, 413);
    assert!(
        res.json["detail"].as_str().unwrap().contains("1000"),
        "{}",
        res.json
    );
    let res = upload(
        &app,
        "POST",
        "/api/files",
        Some(&[b'a'; 1000]),
        "a.txt",
        None,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
}

#[cfg(unix)]
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_failed_store_gives_the_file_up(pool: PgPool) {
    use std::os::unix::fs::PermissionsExt;
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "files", json!({})).await;
    let mode = |m| std::fs::set_permissions(&d.root, std::fs::Permissions::from_mode(m)).unwrap();
    mode(0o500);
    let res = upload(&app, "POST", "/api/files", Some(b"hello"), "a.txt", None).await;
    mode(0o755);
    assert_problem(&res, 500);
    let (file, state): (Uuid, String) = sqlx::query_as("SELECT id, state FROM files")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "gone");
    let due: bool = sqlx::query_scalar(
        "SELECT run_at <= now() FROM jobs WHERE kind = $1 AND payload->>'file' = $2
           AND state = 'queued'",
    )
    .bind(CLEANUP)
    .bind(file.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(due, "its cleanup is due now");
    assert!(files_in(&d.temp).is_empty());
}

/// The bytes of the one object stored under `d`.
fn stored(d: &Dirs) -> Vec<u8> {
    let found = files_in(&d.root);
    assert_eq!(found.len(), 1, "{found:?}");
    std::fs::read(&found[0]).unwrap()
}

fn exif_in(bytes: &[u8]) -> Option<img_parts::Bytes> {
    use img_parts::ImageEXIF;
    img_parts::DynImage::from_bytes(bytes.to_vec().into())
        .unwrap()
        .and_then(|i| i.exif())
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn decompression_bomb_is_refused_before_decoding(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "images", json!({})).await;
    // Its pixel data is junk: a decode would fail on that, not on the size.
    let bomb = fixtures::png_header(100_000, 100_000);
    assert!(bomb.len() < 5_000);
    let res = upload(&app, "POST", "/api/images", Some(&bomb), "bomb.png", None).await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "file");
    let message = res.json["errors"][0]["message"].as_str().unwrap();
    // The headers' refusal, not a decoder's: only the probe words it this way.
    assert_eq!(
        message,
        "this image is 10000000000 pixels, all its frames counted; the limit is 50000000"
    );
    assert_eq!(file_rows(&pool).await, 0);
    assert!(files_in(&d.temp).is_empty());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn frames_count_toward_the_cap(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "images", json!({})).await;
    let res = upload(
        &app,
        "POST",
        "/api/images",
        Some(&fixtures::gif(1000, 1000, 50)),
        "fifty.gif",
        None,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(
        (&res.json["width"], &res.json["height"]),
        (&json!(1000), &json!(1000))
    );
    let res = upload(
        &app,
        "POST",
        "/api/images",
        Some(&fixtures::gif(1000, 1000, 51)),
        "more.gif",
        None,
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "file");
    let message = res.json["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("51000000"), "{message}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn gps_is_stripped(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "images", json!({})).await;
    collection(&app, "originals", json!({"upload": {"withMetadata": true}})).await;
    let jpeg = fixtures::with_exif(
        fixtures::encoded(&fixtures::gradient(16, 8), image::ImageFormat::Jpeg),
        fixtures::exif(None, true),
    );
    let res = upload(&app, "POST", "/api/images", Some(&jpeg), "gps.jpg", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let kept = stored(&d);
    assert_eq!(exif_in(&kept), None);
    assert_eq!(res.json["filesize"], kept.len());
    assert_eq!(
        (&res.json["width"], &res.json["height"]),
        (&json!(16), &json!(8))
    );
    for f in files_in(&d.root) {
        std::fs::remove_file(f).unwrap();
    }
    let res = upload(&app, "POST", "/api/originals", Some(&jpeg), "gps.jpg", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(stored(&d), jpeg, "as uploaded");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn orientation_survives_stripping(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "images", json!({})).await;
    let jpeg = fixtures::with_exif(
        fixtures::encoded(&fixtures::gradient(400, 200), image::ImageFormat::Jpeg),
        fixtures::exif(Some(6), false),
    );
    let res = upload(&app, "POST", "/api/images", Some(&jpeg), "turned.jpg", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(
        (&res.json["width"], &res.json["height"]),
        (&json!(200), &json!(400))
    );
    let kept = stored(&d);
    assert_eq!(exif_in(&kept), None);
    let upright = image::load_from_memory(&kept).unwrap();
    assert_eq!((upright.width(), upright.height()), (200, 400));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn cmyk_jpeg_decodes(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "images", json!({})).await;
    let cmyk = fixtures::cmyk_jpeg(24, 16);
    let res = upload(&app, "POST", "/api/images", Some(&cmyk), "print.jpg", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(
        (&res.json["width"], &res.json["height"]),
        (&json!(24), &json!(16))
    );
    assert!(res.json["blurhash"].is_string(), "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn transforms_apply_to_the_original(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"upload": {"resizeOptions": {"width": 100}, "formatOptions": {"format": "webp"}}}),
    )
    .await;
    let png = fixtures::encoded(&fixtures::gradient(300, 200), image::ImageFormat::Png);
    let res = upload(&app, "POST", "/api/images", Some(&png), "cat.png", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["mimeType"], "image/webp");
    assert_eq!(res.json["filename"], "cat.webp");
    assert_eq!(
        (&res.json["width"], &res.json["height"]),
        (&json!(100), &json!(67))
    );
    let kept = stored(&d);
    assert_eq!(res.json["filesize"], kept.len());
    assert_eq!(
        image::guess_format(&kept).unwrap(),
        image::ImageFormat::WebP
    );
    let made = image::load_from_memory(&kept).unwrap();
    assert_eq!((made.width(), made.height()), (100, 67));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn sharp_only_options_are_422(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    let schema = json!({"slug": "images", "fields": [],
        "upload": {"imageSizes": [{"name": "t", "width": 10, "position": "attention"}]}});
    let res = call(&app, "POST", "/api/admin/collections", Some(schema)).await;
    assert_problem(&res, 422);
    // Option-type errors name their path in the message, as every schema check does.
    let message = res.json["errors"][0]["message"].as_str().unwrap();
    assert!(
        message.starts_with("upload.imageSizes.0.position: `attention`"),
        "{message}"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn svg_is_sanitised(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "images", json!({})).await;
    let dirty = br#"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="80" onload="alert(1)"><script>alert(2)</script><foreignObject><div/></foreignObject><a href="javascript:alert(3)"/><use href="https://x/y#a"/><rect width="1"/></svg>"#;
    let res = upload(&app, "POST", "/api/images", Some(dirty), "logo.svg", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["mimeType"], "image/svg+xml");
    assert_eq!(
        (&res.json["width"], &res.json["height"]),
        (&json!(120), &json!(80))
    );
    assert_eq!(res.json["blurhash"], Value::Null);
    let kept = stored(&d);
    assert_eq!(res.json["filesize"], kept.len());
    let text = String::from_utf8(kept.clone()).unwrap();
    for gone in [
        "onload",
        "script",
        "alert",
        "foreignObject",
        "javascript",
        "https://x",
    ] {
        assert!(!text.contains(gone), "{gone} survived: {text}");
    }
    assert_eq!(tachyon_media::svg::sanitize_svg(&kept).unwrap(), kept);
    let sized = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10em" height="50%"/>"#;
    let res = upload(&app, "POST", "/api/images", Some(sized), "em.svg", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(
        (&res.json["width"], &res.json["height"]),
        (&Value::Null, &Value::Null)
    );
    let other = br#"<?xml version="1.0"?><feed><svg/></feed>"#;
    let res = upload(&app, "POST", "/api/images", Some(other), "feed.svg", None).await;
    assert_problem(&res, 415);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn decodes_queue_for_a_permit(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "images", json!({})).await;
    let big = image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(
        8000,
        6000,
        image::Luma([90]),
    ));
    let png = fixtures::encoded(&big, image::ImageFormat::Png);
    drop(big);
    let watching = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let watcher = {
        let (watching, temp) = (watching.clone(), d.temp.clone());
        tokio::spawn(async move {
            let mut most = 0;
            while watching.load(std::sync::atomic::Ordering::Relaxed) {
                most = most.max(files_in(&temp).len());
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            most
        })
    };
    let uploads = (0..8).map(|i| {
        let (app, png) = (app.clone(), &png);
        async move {
            upload(
                &app,
                "POST",
                "/api/images",
                Some(png),
                &format!("{i}.png"),
                None,
            )
            .await
        }
    });
    let results = futures_util::future::join_all(uploads).await;
    watching.store(false, std::sync::atomic::Ordering::Relaxed);
    let most = watcher.await.unwrap();
    for res in &results {
        assert_eq!(res.status, 201, "{}", res.json);
        assert_eq!(
            (&res.json["width"], &res.json["height"]),
            (&json!(8000), &json!(6000))
        );
    }
    assert_eq!(tachyon_api::decodes_peak(), 2);
    assert!(most <= 8 + 2, "{most} files in the spool directory at once");
    assert!(files_in(&d.temp).is_empty());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_cancelled_upload_keeps_its_permit_until_the_work_ends(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"upload": {"resizeOptions": {"width": 100}, "formatOptions": {"format": "jpeg"}}}),
    )
    .await;
    let big = image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(
        8000,
        6000,
        image::Luma([90]),
    ));
    let png = Arc::new(fixtures::encoded(&big, image::ImageFormat::Png));
    drop(big);
    let tasks: Vec<_> = (0..6)
        .map(|i| {
            let (app, png) = (app.clone(), png.clone());
            tokio::spawn(async move {
                upload(
                    &app,
                    "POST",
                    "/api/images",
                    Some(&png),
                    &format!("{i}.png"),
                    None,
                )
                .await
            })
        })
        .collect();
    // Two decodes are under way, the rest wait for a permit: then the clients go, and two
    // more uploads come in while the abandoned work still runs.
    until("two decodes run", async || {
        tachyon_media::image::working_now() >= 2
    })
    .await;
    for t in &tasks {
        t.abort();
    }
    let late = (0..2).map(|i| {
        let (app, png) = (app.clone(), png.clone());
        async move {
            let name = format!("late{i}.png");
            upload(&app, "POST", "/api/images", Some(&png), &name, None).await
        }
    });
    for res in futures_util::future::join_all(late).await {
        assert_eq!(res.status, 201, "{}", res.json);
    }
    until("the abandoned work has cleaned up", async || {
        files_in(&d.temp).is_empty()
    })
    .await;
    // Counted inside the work itself, apart from the permit: what really ran at once.
    let ran = tachyon_media::image::working_peak();
    assert!(ran <= 2, "{ran} decodes ran at once");
}
