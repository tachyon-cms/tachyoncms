//! An upload's `imageSizes`, made on the jobs worker and written wherever the file is
//! described: its row, both copies of the document and its versions.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{ADMIN, PASSWORD, Res, send, signed_in, upload};
use http_body_util::BodyExt;
use image::{DynamicImage, Rgb, RgbImage};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_api::{JobsConfig, Options, UploadsConfig};
use tachyon_db::files;
use tachyon_media::fixtures;
use tachyon_media::storage::{StorageConfig, Storages};
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

struct Dirs {
    temp: PathBuf,
    root: PathBuf,
}

fn dirs() -> Dirs {
    let base = std::env::temp_dir().join(format!("tachyon-sizes-{}", Uuid::now_v7()));
    let d = Dirs {
        temp: base.join("spool"),
        root: base.join("store"),
    };
    std::fs::create_dir_all(&d.temp).unwrap();
    std::fs::create_dir_all(&d.root).unwrap();
    d
}

fn options(d: &Dirs, workers: usize, timeout: Duration) -> Options {
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
            timeout,
            ..UploadsConfig::default()
        },
        ..Options::default()
    }
}

/// The API over `d` with `workers`, set up and signed in as its first Admin.
async fn admin(pool: &PgPool, d: &Dirs, workers: usize) -> Router {
    admin_with(pool, d, workers, Duration::from_secs(600)).await
}

async fn admin_with(pool: &PgPool, d: &Dirs, workers: usize, timeout: Duration) -> Router {
    let app = tachyon_api::router_with(pool.clone(), options(d, workers, timeout))
        .await
        .unwrap();
    let setup = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&setup.to_string())).await;
    if res.status != 201 {
        // Already set up by an earlier router over the same database.
        assert_eq!(res.status, 403, "setup: {}", res.json);
    }
    signed_in(app, ADMIN).await
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let res = send(app, method, uri, body.map(|b| b.to_string()).as_deref()).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

/// Creates `slug`, an upload collection with `upload`, `extra` merged in.
async fn collection(app: &Router, slug: &str, upload: Value, extra: Value) {
    let mut schema = json!({
        "slug": slug, "upload": upload,
        "fields": [{"name": "alt", "type": "text"}]
    });
    schema
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    ok(app, "POST", "/api/admin/collections", Some(schema)).await;
}

fn png(img: DynamicImage) -> Vec<u8> {
    fixtures::encoded(&img, image::ImageFormat::Png)
}

async fn post(app: &Router, uri: &str, bytes: &[u8], name: &str, payload: Option<Value>) -> Res {
    upload(app, "POST", uri, Some(bytes), name, payload.as_ref()).await
}

/// The bytes and status `url` (a full URL) is served with.
async fn fetch(app: &Router, url: &Value) -> (StatusCode, String, Vec<u8>) {
    let url = Url::parse(url.as_str().unwrap_or_else(|| panic!("no url: {url}"))).unwrap();
    let uri = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    };
    let res = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let kind = res
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, kind, body)
}

/// Polls `check` until it holds, for up to 60 s.
async fn until<F: AsyncFn() -> bool>(what: &str, check: F) {
    for _ in 0..600 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting until {what}");
}

async fn jobs_in(pool: &PgPool, state: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'image_sizes' AND state = $1")
        .bind(state)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn held(pool: &PgPool, id: &str) -> Uuid {
    sqlx::query_scalar("SELECT coalesce(published_file, draft_file) FROM documents WHERE id = $1")
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Each url in `entry` and its `formats`, served with its `mimeType`.
async fn serves(app: &Router, entry: &Value) {
    let mut all = vec![entry];
    all.extend(entry["formats"].as_object().unwrap().values());
    for e in all {
        let (status, kind, body) = fetch(app, &e["url"]).await;
        assert_eq!(status, StatusCode::OK, "{e}");
        assert_eq!(kind, e["mimeType"].as_str().unwrap(), "{e}");
        assert_eq!(body.len() as u64, e["filesize"].as_u64().unwrap(), "{e}");
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn sizes_fill_in_from_the_job(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d, 1).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "thumbnail", "width": 400, "height": 300},
                              {"name": "card", "width": 768}]}),
        json!({}),
    )
    .await;
    let image = png(fixtures::gradient(1000, 800));
    let res = post(&app, "/api/images?wait=true", &image, "a.png", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let sizes = &res.json["sizes"];
    let thumb = &sizes["thumbnail"];
    assert_eq!(
        (
            &thumb["width"],
            &thumb["height"],
            &thumb["mimeType"],
            &thumb["filename"]
        ),
        (
            &json!(400),
            &json!(300),
            &json!("image/png"),
            &json!("a-400x300.png")
        ),
        "{sizes}"
    );
    assert_eq!(
        thumb["formats"]["avif"]["mimeType"], "image/avif",
        "{sizes}"
    );
    assert_eq!(thumb["formats"]["avif"]["filename"], "a-400x300.avif");
    assert_eq!(
        thumb["formats"]["webp"]["mimeType"], "image/webp",
        "{sizes}"
    );
    let card = &sizes["card"];
    assert_eq!(
        (&card["width"], &card["height"]),
        (&json!(768), &json!(614))
    );
    assert_eq!(card["filename"], "a-768x614.png");
    serves(&app, thumb).await;
    serves(&app, card).await;
    assert_eq!(jobs_in(&pool, "done").await, 1);
    // `url`s are worked out on read; the row holds the rest.
    let id = res.json["id"].as_str().unwrap();
    let file = files::get(&pool, held(&pool, id).await).await.unwrap();
    assert!(file.sizes["thumbnail"].get("url").is_none());
    assert!(file.names.contains(&"a-768x614.webp".to_owned()));
    // Read back unchanged, the sizes don't count as a change.
    let read = ok(&app, "GET", &format!("/api/images/{id}"), None).await;
    let back = json!({"alt": "x", "sizes": read["sizes"]});
    let patched = ok(&app, "PATCH", &format!("/api/images/{id}"), Some(back)).await;
    assert_eq!(patched["sizes"], read["sizes"]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn sizes_center_on_the_focal_point(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d, 1).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "square", "width": 200, "height": 200}]}),
        json!({}),
    )
    .await;
    let halves = RgbImage::from_fn(1000, 500, |x, _| {
        if x < 500 {
            Rgb([255, 0, 0])
        } else {
            Rgb([0, 0, 255])
        }
    });
    let image = png(DynamicImage::ImageRgb8(halves));
    let res = post(
        &app,
        "/api/images?wait=true",
        &image,
        "a.png",
        Some(json!({"focalX": 10})),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let square = &res.json["sizes"]["square"];
    assert_eq!(square["width"], 200, "{square}");
    let (status, _, body) = fetch(&app, &square["url"]).await;
    assert_eq!(status, StatusCode::OK);
    let made = image::load_from_memory(&body).unwrap().to_rgb8();
    let [r, _, b] = made.get_pixel(100, 100).0;
    assert!(r > 200 && b < 50, "the centre is red: {r} {b}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn small_images_skip_sizes(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d, 1).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "big", "width": 400, "height": 400}]}),
        json!({}),
    )
    .await;
    collection(
        &app,
        "grown",
        json!({"imageSizes": [{"name": "big", "width": 400, "height": 400,
                               "withoutEnlargement": false}]}),
        json!({}),
    )
    .await;
    let image = png(fixtures::gradient(100, 100));
    let res = post(&app, "/api/images?wait=true", &image, "a.png", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(jobs_in(&pool, "done").await, 1, "{}", res.json);
    let big = &res.json["sizes"]["big"];
    assert_eq!(
        big,
        &json!({"url": null, "width": null, "height": null, "mimeType": null,
                "filesize": null, "filename": null}),
    );
    let file = files::get(&pool, held(&pool, res.json["id"].as_str().unwrap()).await)
        .await
        .unwrap();
    assert!(file.sizes["big"].is_object(), "skipped, not pending");
    let res = post(&app, "/api/grown?wait=true", &image, "a.png", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let big = &res.json["sizes"]["big"];
    assert_eq!(
        (&big["width"], &big["height"]),
        (&json!(400), &json!(400)),
        "{big}"
    );
    serves(&app, big).await;
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_slow_job_cannot_overwrite_a_newer_file(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d, 0).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "thumb", "width": 50}]}),
        json!({"versions": {"maxVersions": 10}}),
    )
    .await;
    let res = post(
        &app,
        "/api/images",
        &png(fixtures::gradient(200, 100)),
        "a.png",
        None,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let id = res.json["id"].as_str().unwrap().to_owned();
    let a = held(&pool, &id).await;
    // A's job is queued, not yet done, when B replaces the file.
    let res = upload(
        &app,
        "PATCH",
        &format!("/api/images/{id}"),
        Some(&png(fixtures::gradient(300, 300))),
        "b.png",
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let b = held(&pool, &id).await;
    assert_ne!(a, b);
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1, Duration::from_secs(600)))
        .await
        .unwrap();
    until("both jobs are done", async || {
        jobs_in(&pool, "done").await == 2
    })
    .await;
    let doc = ok(&app, "GET", &format!("/api/images/{id}"), None).await;
    assert_eq!(doc["sizes"]["thumb"]["height"], 50, "B's: {doc}");
    assert_eq!(doc["sizes"]["thumb"]["filename"], "b-50x50.png");
    let versions: Vec<(Option<Uuid>, Value)> =
        sqlx::query_as("SELECT file_id, data FROM versions WHERE document_id = $1")
            .bind(id.parse::<Uuid>().unwrap())
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(versions.iter().any(|(f, _)| *f == Some(a)));
    for (file, data) in &versions {
        let want = if *file == Some(a) { 25 } else { 50 };
        assert_eq!(data["sizes"]["thumb"]["height"], want, "{data}");
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn sizes_reach_drafts_published_and_versions(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d, 0).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "thumb", "width": 50}]}),
        json!({"versions": {"drafts": true, "maxVersions": 10}}),
    )
    .await;
    let res = post(
        &app,
        "/api/images",
        &png(fixtures::gradient(200, 100)),
        "a.png",
        None,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let id = res.json["id"].as_str().unwrap().to_owned();
    ok(&app, "POST", &format!("/api/images/{id}/publish"), None).await;
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1, Duration::from_secs(600)))
        .await
        .unwrap();
    until("the job is done", async || {
        jobs_in(&pool, "done").await == 1
    })
    .await;
    let doc: Uuid = id.parse().unwrap();
    let (file, published, draft): (Option<Uuid>, Value, Value) = sqlx::query_as(
        "SELECT published_file, published, draft FROM documents WHERE id = $1 AND draft_file = published_file",
    )
    .bind(doc)
    .fetch_one(&pool)
    .await
    .unwrap();
    let row = files::get(&pool, file.unwrap()).await.unwrap();
    let sizes = Value::Object(row.sizes.0.clone());
    assert_eq!(sizes["thumb"]["height"], 25, "{sizes}");
    assert_eq!(published["sizes"], sizes);
    assert_eq!(draft["sizes"], sizes);
    let versions: Vec<Value> = sqlx::query_scalar("SELECT data FROM versions WHERE file_id = $1")
        .bind(file)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(versions.len() >= 2, "{versions:?}");
    for v in &versions {
        assert_eq!(v["sizes"], sizes);
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn wait_without_workers_answers_anyway(pool: PgPool) {
    let d = dirs();
    let app = admin_with(&pool, &d, 0, Duration::from_secs(2)).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "thumb", "width": 50}]}),
        json!({}),
    )
    .await;
    let started = Instant::now();
    let res = post(
        &app,
        "/api/images?wait=true",
        &png(fixtures::gradient(200, 100)),
        "a.png",
        None,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(res.json["sizes"], json!({"thumb": null}));
    assert_eq!(jobs_in(&pool, "queued").await, 1);
    // Nothing to wait for, a JSON write answers at once.
    let id = res.json["id"].as_str().unwrap();
    let started = Instant::now();
    ok(
        &app,
        "PATCH",
        &format!("/api/images/{id}?wait=true"),
        Some(json!({"alt": "x"})),
    )
    .await;
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_released_file_fails_its_job(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d, 0).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "thumb", "width": 50}]}),
        json!({}),
    )
    .await;
    let res = post(
        &app,
        "/api/images",
        &png(fixtures::gradient(200, 100)),
        "a.png",
        None,
    )
    .await;
    let id = res.json["id"].as_str().unwrap().to_owned();
    ok(&app, "DELETE", &format!("/api/images/{id}"), None).await;
    ok(&app, "DELETE", &format!("/api/images/trash/{id}"), None).await;
    let _worker = tachyon_api::router_with(pool.clone(), options(&d, 1, Duration::from_secs(600)))
        .await
        .unwrap();
    until("the job fails", async || {
        jobs_in(&pool, "failed").await == 1
    })
    .await;
    let error: String =
        sqlx::query_scalar("SELECT last_error FROM jobs WHERE kind = 'image_sizes'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(error, "file released");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_restored_files_sizes_keep_serving(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d, 1).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "thumb", "width": 50}]}),
        json!({"versions": {"maxVersions": 10}}),
    )
    .await;
    let image = png(fixtures::gradient(200, 100));
    let first = post(&app, "/api/images?wait=true", &image, "a.png", None).await;
    assert_eq!(first.status, 201, "{}", first.json);
    let id = first.json["id"].as_str().unwrap().to_owned();
    let old = first.json["sizes"]["thumb"].clone();
    assert_eq!(old["filename"], "a-50x25.png");
    ok(&app, "DELETE", &format!("/api/images/{id}"), None).await;
    // Another document takes the name while the first is in the trash.
    let other = post(&app, "/api/images?wait=true", &image, "a.png", None).await;
    assert_eq!(other.status, 201, "{}", other.json);
    let restored = ok(
        &app,
        "POST",
        &format!("/api/images/trash/{id}/restore"),
        None,
    )
    .await;
    assert_eq!(restored["filename"], "a-1.png");
    let thumb = &restored["sizes"]["thumb"];
    assert_eq!(thumb["filename"], "a-1-50x25.png", "{restored}");
    assert_eq!(thumb["formats"]["webp"]["filename"], "a-1-50x25.webp");
    serves(&app, thumb).await;
    // The other document's sizes are its own.
    serves(&app, &other.json["sizes"]["thumb"]).await;
    let file = files::get(&pool, held(&pool, &id).await).await.unwrap();
    assert!(
        file.names.contains(&"a-1-50x25.avif".to_owned()),
        "{:?}",
        file.names
    );
    assert!(!file.names.contains(&"a-50x25.avif".to_owned()));
    let versions: Vec<Value> = sqlx::query_scalar("SELECT data FROM versions WHERE file_id = $1")
        .bind(file.id)
        .fetch_all(&pool)
        .await
        .unwrap();
    for v in &versions {
        assert_eq!(v["filename"], "a-1.png");
        assert_eq!(v["sizes"]["thumb"]["filename"], "a-1-50x25.png", "{v}");
    }
    // The old names' objects are cleaned up once nothing serves them.
    let dir = d.root.join(file.key.rsplit_once('/').unwrap().0);
    until("the old objects are gone", async || {
        !dir.join("a-50x25.png").exists() && !dir.join("a-50x25.webp").exists()
    })
    .await;
    assert!(dir.join("a-1-50x25.png").exists());
}
