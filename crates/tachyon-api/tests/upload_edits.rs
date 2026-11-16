//! Cropping a stored image, or moving its focal point, without uploading it again: each
//! edit makes a new file under the same name, and the old one stays for the versions that
//! hold it.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{ADMIN, PASSWORD, Res, add_user, assert_problem, send, signed_in, upload};
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
    /// Removed, with everything in it, when the test is done.
    _base: tempfile::TempDir,
}

fn dirs() -> Dirs {
    let base = tempfile::Builder::new()
        .prefix("tachyon-edits-")
        .tempdir()
        .unwrap();
    let d = Dirs {
        temp: base.path().join("spool"),
        root: base.path().join("store"),
        _base: base,
    };
    std::fs::create_dir_all(&d.temp).unwrap();
    std::fs::create_dir_all(&d.root).unwrap();
    d
}

/// The API over `d`, with a jobs worker so sizes get made.
async fn app(pool: &PgPool, d: &Dirs) -> Router {
    let storages = Storages::open(&BTreeMap::from([(
        "default".into(),
        StorageConfig::local(&d.root),
    )]))
    .unwrap();
    let options = Options {
        jobs: JobsConfig {
            workers: 1,
            ..JobsConfig::default()
        },
        uploads: UploadsConfig {
            temp_dir: d.temp.clone(),
            storages: Arc::new(storages),
            timeout: Duration::from_secs(600),
            ..UploadsConfig::default()
        },
        ..Options::default()
    };
    tachyon_api::router_with(pool.clone(), options)
        .await
        .unwrap()
}

/// `app`, set up and signed in as its first Admin.
async fn admin(pool: &PgPool, d: &Dirs) -> Router {
    let app = app(pool, d).await;
    let setup = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&setup.to_string())).await;
    assert_eq!(res.status, 201, "setup: {}", res.json);
    signed_in(app, ADMIN).await
}

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> Res {
    send(app, method, uri, Some(&body.to_string())).await
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let res = send(app, method, uri, body.map(|b| b.to_string()).as_deref()).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

/// Creates `slug`, an upload collection with `upload`, `extra` merged into the schema.
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

/// Uploads `bytes` as `name` to `slug`, waiting for its sizes.
async fn post(app: &Router, slug: &str, bytes: &[u8], name: &str) -> Value {
    let res = upload(
        app,
        "POST",
        &format!("/api/{slug}?wait=true"),
        Some(bytes),
        name,
        None,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    res.json
}

/// The bytes `url` (a full URL) is served with, and the status.
async fn fetch(app: &Router, url: &Value) -> (StatusCode, Vec<u8>) {
    let (status, _, body) = fetch_with(app, url).await;
    (status, body)
}

/// `fetch`, with the response's headers.
async fn fetch_with(app: &Router, url: &Value) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
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
    let (status, headers) = (res.status(), res.headers().clone());
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, body)
}

/// The image `url` serves, decoded.
async fn served(app: &Router, url: &Value) -> DynamicImage {
    let (status, body) = fetch(app, url).await;
    assert_eq!(status, StatusCode::OK, "{url}");
    image::load_from_memory(&body).unwrap()
}

async fn held(pool: &PgPool, id: &Value) -> Uuid {
    sqlx::query_scalar("SELECT coalesce(draft_file, published_file) FROM documents WHERE id = $1")
        .bind(id.as_str().unwrap().parse::<Uuid>().unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// Every object in the store.
fn objects(d: &Dirs) -> usize {
    fn walk(p: &std::path::Path) -> usize {
        std::fs::read_dir(p)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                if e.file_type().unwrap().is_dir() {
                    walk(&e.path())
                } else {
                    1
                }
            })
            .sum()
    }
    walk(&d.root)
}

/// Left half red, right half blue.
fn halves(w: u32, h: u32) -> Vec<u8> {
    png(DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, _| {
        if x < w / 2 {
            Rgb([255, 0, 0])
        } else {
            Rgb([0, 0, 255])
        }
    })))
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn crop_cuts_the_original_then_sizes(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "small", "width": 200}]}),
        json!({}),
    )
    .await;
    let image = png(fixtures::gradient(1000, 800));
    let px = json!({"x": 100, "y": 100, "width": 400, "height": 300, "unit": "px"});
    let percent = json!({"x": 10, "y": 12.5, "width": 40, "height": 37.5, "unit": "%"});
    let mut cut = Vec::new();
    for (crop, multipart) in [(px, false), (percent, true)] {
        let doc = post(&app, "images", &image, "a.png").await;
        let one = format!("/api/images/{}?wait=true", doc["id"].as_str().unwrap());
        let before = held(&pool, &doc["id"]).await;
        // Moved first, so the crop is seen putting it back to the centre.
        ok(&app, "PATCH", &one, Some(json!({"focalX": 20}))).await;
        let edits = json!({"uploadEdits": {"crop": crop}});
        let res = if multipart {
            upload(&app, "PATCH", &one, None, "", Some(&edits)).await
        } else {
            call(&app, "PATCH", &one, edits).await
        };
        assert_eq!(res.status, 200, "{}", res.json);
        let got = &res.json;
        assert_eq!((&got["width"], &got["height"]), (&json!(400), &json!(300)));
        assert_eq!((&got["focalX"], &got["focalY"]), (&json!(50), &json!(50)));
        assert_eq!(got["filename"], doc["filename"], "never renamed");
        assert_eq!(got["url"], doc["url"]);
        assert_eq!(got["mimeType"], "image/png");
        assert!(got["blurhash"].is_string());
        let small = &got["sizes"]["small"];
        assert_eq!(
            (&small["width"], &small["height"]),
            (&json!(200), &json!(150)),
            "{small}"
        );
        let after = held(&pool, &doc["id"]).await;
        assert_ne!(after, before, "a new file");
        let row = files::get(&pool, after).await.unwrap();
        assert_eq!((row.width, row.height), (Some(400), Some(300)));
        assert_eq!(row.filesize, got["filesize"].as_i64().unwrap());
        let stored = served(&app, &got["url"]).await;
        assert_eq!((stored.width(), stored.height()), (400, 300));
        let small = served(&app, &small["url"]).await;
        assert_eq!((small.width(), small.height()), (200, 150));
        cut.push(stored.to_rgb8());
    }
    assert_eq!(cut[0], cut[1], "the same rectangle, in pixels or percent");
    // From the 1000×800 gradient at (100, 100): its corner is the original's pixel there.
    let original = fixtures::gradient(1000, 800).to_rgb8();
    assert_eq!(cut[0].get_pixel(0, 0), original.get_pixel(100, 100));
    // A crop and a focal point together: the point is on the cropped image.
    let doc = post(&app, "images", &image, "b.png").await;
    let one = format!("/api/images/{}", doc["id"].as_str().unwrap());
    let got = ok(
        &app,
        "PATCH",
        &one,
        Some(json!({"uploadEdits": {
            "crop": {"x": 0, "y": 0, "width": 500, "height": 400},
            "focalPoint": {"x": 10, "y": 90}
        }})),
    )
    .await;
    assert_eq!(
        (&got["width"], &got["focalX"], &got["focalY"]),
        (&json!(500), &json!(10), &json!(90))
    );
    let row = files::get(&pool, held(&pool, &doc["id"]).await)
        .await
        .unwrap();
    assert_eq!((row.focal_x, row.focal_y), (10.0, 90.0));
}

/// Crop coordinates are on the image as it's served: upright, as `width` and `height`
/// give it, whether the original was turned on upload or keeps its EXIF orientation.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn crops_follow_the_picture_as_shown(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "photos", json!(true), json!({})).await;
    collection(&app, "kept", json!({"withMetadata": true}), json!({})).await;
    let jpeg = fixtures::with_exif(
        fixtures::encoded(&fixtures::gradient(400, 200), image::ImageFormat::Jpeg),
        fixtures::exif(Some(6), false),
    );
    for slug in ["photos", "kept"] {
        let doc = post(&app, slug, &jpeg, "a.jpg").await;
        assert_eq!(
            (&doc["width"], &doc["height"]),
            (&json!(200), &json!(400)),
            "{slug}"
        );
        let one = format!("/api/{slug}/{}", doc["id"].as_str().unwrap());
        let crop = json!({"uploadEdits": {"crop": {"x": 0, "y": 0, "width": 200, "height": 100}}});
        let got = ok(&app, "PATCH", &one, Some(crop)).await;
        assert_eq!(
            (&got["width"], &got["height"]),
            (&json!(200), &json!(100)),
            "{slug}"
        );
        assert_eq!(got["mimeType"], "image/jpeg");
        let stored = served(&app, &got["url"]).await;
        assert_eq!(
            (stored.width(), stored.height()),
            (200, 100),
            "{slug}: upright"
        );
        let past = json!({"uploadEdits": {"crop": {"x": 0, "y": 0, "width": 201, "height": 10}}});
        let res = call(&app, "PATCH", &one, past).await;
        assert_problem(&res, 422);
        assert_eq!(res.json["errors"][0]["path"], "uploadEdits.crop.width");
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn focal_point_moves_and_regenerates(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "square", "width": 200, "height": 200}]}),
        json!({"versions": {"maxVersions": 10}}),
    )
    .await;
    let doc = post(&app, "images", &halves(1000, 500), "a.png").await;
    let id = doc["id"].as_str().unwrap();
    let one = format!("/api/images/{id}?wait=true");
    let first = held(&pool, &doc["id"]).await;
    let first_square = served(&app, &doc["sizes"]["square"]["url"]).await.to_rgb8();
    let objects_before = objects(&d);
    let got = ok(
        &app,
        "PATCH",
        &one,
        Some(json!({"uploadEdits": {"focalPoint": {"x": 20, "y": 80}}})),
    )
    .await;
    let second = held(&pool, &doc["id"]).await;
    assert_ne!(second, first, "a new file");
    assert_eq!((&got["focalX"], &got["focalY"]), (&json!(20), &json!(80)));
    assert_eq!(got["filename"], doc["filename"], "never renamed");
    assert_eq!(got["url"], doc["url"]);
    assert_eq!(
        (&got["width"], &got["filesize"]),
        (&doc["width"], &doc["filesize"])
    );
    let row = files::get(&pool, second).await.unwrap();
    assert_eq!(
        (row.focal_x, row.focal_y, row.state.as_str()),
        (20.0, 80.0, "live")
    );
    assert!(objects(&d) > objects_before, "copied, not moved");
    // The original, copied as it was; the square moved left, onto the red.
    let (_, before) = fetch(&app, &doc["url"]).await;
    let (_, now) = fetch(&app, &got["url"]).await;
    assert_eq!(before, now);
    let square = served(&app, &got["sizes"]["square"]["url"]).await.to_rgb8();
    assert_ne!(square, first_square, "the sizes changed");
    let [r, _, b] = square.get_pixel(100, 100).0;
    assert!(r > 200 && b < 50, "centred on the red: {r} {b}");
    // The old file stays for the version holding it, and comes back with it.
    assert_eq!(files::get(&pool, first).await.unwrap().state, "live");
    // A plain focal write that changes it does the same.
    let got = ok(&app, "PATCH", &one, Some(json!({"focalX": 30}))).await;
    let third = held(&pool, &doc["id"]).await;
    assert_ne!(third, second);
    assert_eq!((&got["focalX"], &got["focalY"]), (&json!(30), &json!(80)));
    assert_eq!(got["url"], doc["url"]);
    let row = files::get(&pool, third).await.unwrap();
    assert_eq!((row.focal_x, row.focal_y), (30.0, 80.0));
    assert!(
        got["sizes"]["square"]["url"].is_string(),
        "{}",
        got["sizes"]
    );
    // Sent back unchanged, it keeps the file.
    let got = ok(&app, "PATCH", &one, Some(json!({"focalX": 30, "alt": "x"}))).await;
    assert_eq!(held(&pool, &doc["id"]).await, third, "the same file");
    assert_eq!(got["alt"], "x");
    // Restoring the first version brings its file, focal point and sizes back.
    let versions = ok(&app, "GET", &format!("/api/images/{id}/versions"), None).await;
    // Newest first: the last is the upload's.
    let oldest = versions["docs"].as_array().unwrap().last().unwrap();
    let restored = ok(
        &app,
        "POST",
        &format!(
            "/api/images/{id}/versions/{}/restore",
            oldest["id"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert_eq!(held(&pool, &doc["id"]).await, first);
    assert_eq!(restored["focalX"], 50);
    assert_eq!(
        served(&app, &restored["sizes"]["square"]["url"])
            .await
            .to_rgb8(),
        first_square
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn edits_obey_options_and_bounds(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "fixed",
        json!({"crop": false, "focalPoint": false, "imageSizes": [{"name": "s", "width": 10}]}),
        json!({}),
    )
    .await;
    collection(
        &app,
        "free",
        json!({"focalPoint": false, "imageSizes": [{"name": "s", "width": 10}]}),
        json!({}),
    )
    .await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "s", "width": 10}]}),
        json!({}),
    )
    .await;
    collection(
        &app,
        "notes",
        json!({"mimeTypes": ["text/plain", "image/svg+xml"]}),
        json!({}),
    )
    .await;
    let image = png(fixtures::gradient(100, 80));
    let refused = async |uri: &str, body: Value, status: u16, path: Option<&str>| {
        let res = call(&app, "PATCH", uri, body).await;
        assert_problem(&res, status);
        if let Some(path) = path {
            assert_eq!(res.json["errors"][0]["path"], path, "{}", res.json);
        }
    };
    let crop =
        |x, y, w, h| json!({"uploadEdits": {"crop": {"x": x, "y": y, "width": w, "height": h}}});
    let focal = |x, y| json!({"uploadEdits": {"focalPoint": {"x": x, "y": y}}});

    let fixed = post(&app, "fixed", &image, "a.png").await;
    let one = format!("/api/fixed/{}", fixed["id"].as_str().unwrap());
    refused(&one, crop(0, 0, 10, 10), 422, Some("uploadEdits.crop")).await;
    refused(&one, focal(10, 10), 422, Some("uploadEdits.focalPoint")).await;
    // Without focalPoint there are no focal fields: naming one is naming any unknown field.
    assert!(
        fixed.get("focalX").is_none() && fixed.get("focalY").is_none(),
        "{fixed}"
    );
    let before = held(&pool, &fixed["id"]).await;
    for sent in [json!({"focalX": 10}), json!({"focalY": "a"})] {
        let res = call(&app, "PATCH", &one, sent.clone()).await;
        assert_problem(&res, 422);
        let key = sent.as_object().unwrap().keys().next().unwrap().clone();
        assert_eq!(
            res.json["errors"],
            json!([{"path": key, "message": "unknown field"}]),
            "{sent}"
        );
    }
    assert_eq!(held(&pool, &fixed["id"]).await, before);
    // A crop there writes no focal point either.
    let free = post(&app, "free", &image, "a.png").await;
    let free_one = format!("/api/free/{}", free["id"].as_str().unwrap());
    let got = ok(&app, "PATCH", &free_one, Some(crop(0, 0, 10, 10))).await;
    assert_eq!(
        (got["width"].clone(), got.get("focalX")),
        (json!(10), None),
        "{got}"
    );

    let doc = post(&app, "images", &image, "a.png").await;
    let one = format!("/api/images/{}", doc["id"].as_str().unwrap());
    refused(
        &one,
        crop(50, 0, 51, 10),
        422,
        Some("uploadEdits.crop.width"),
    )
    .await;
    refused(&one, crop(0, 80, 1, 1), 422, Some("uploadEdits.crop.y")).await;
    refused(&one, crop(0, 0, 0, 10), 422, Some("uploadEdits.crop.width")).await;
    refused(&one, crop(-1, 0, 10, 10), 422, Some("uploadEdits.crop.x")).await;
    refused(&one, focal(101, 50), 422, Some("uploadEdits.focalPoint.x")).await;
    refused(
        &one,
        json!({"uploadEdits": {"focalPoint": {"x": 50, "y": "a"}}}),
        422,
        Some("uploadEdits.focalPoint.y"),
    )
    .await;
    refused(
        &one,
        json!({"uploadEdits": {"rotate": 90}}),
        422,
        Some("uploadEdits.rotate"),
    )
    .await;
    refused(
        &one,
        json!({"uploadEdits": {"crop": {"x": 0, "y": 0, "width": 1, "height": 1, "unit": "em"}}}),
        422,
        Some("uploadEdits.crop.unit"),
    )
    .await;
    refused(&one, json!({"focalX": 101}), 422, Some("focalX")).await;
    // With a new file, there's nothing stored to edit.
    let before = held(&pool, &doc["id"]).await;
    let edits = crop(0, 0, 10, 10);
    let res = upload(&app, "PATCH", &one, Some(&image), "b.png", Some(&edits)).await;
    assert_problem(&res, 400);
    assert_eq!(held(&pool, &doc["id"]).await, before);

    // Text and SVG aren't images an edit can remake.
    for (bytes, name) in [
        (b"just words".to_vec(), "a.txt"),
        (
            b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\"/>".to_vec(),
            "a.svg",
        ),
    ] {
        let res = upload(&app, "POST", "/api/notes", Some(&bytes), name, None).await;
        assert_eq!(res.status, 201, "{}", res.json);
        let one = format!("/api/notes/{}", res.json["id"].as_str().unwrap());
        refused(&one, crop(0, 0, 1, 1), 422, Some("uploadEdits")).await;
        refused(&one, focal(1, 1), 422, Some("uploadEdits")).await;
        let before = held(&pool, &res.json["id"]).await;
        let got = ok(&app, "PATCH", &one, Some(json!({"focalX": 1}))).await;
        assert_eq!(got["focalX"], 1);
        assert_eq!(held(&pool, &res.json["id"]).await, before, "only the field");
    }

    // A crop that fails once it's decoded leaves no file row, object or job behind.
    let row = files::get(&pool, held(&pool, &doc["id"]).await)
        .await
        .unwrap();
    let mut broken = image.clone();
    broken.truncate(60);
    std::fs::write(d.root.join(&row.key), &broken).unwrap();
    let (rows, stored) = (
        count(&pool, "SELECT count(*) FROM files").await,
        objects(&d),
    );
    let jobs = count(
        &pool,
        "SELECT count(*) FROM jobs WHERE kind = 'image_sizes'",
    )
    .await;
    refused(&one, crop(0, 0, 10, 10), 422, Some("uploadEdits.crop")).await;
    assert_eq!(count(&pool, "SELECT count(*) FROM files").await, rows);
    assert_eq!(objects(&d), stored);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM jobs WHERE kind = 'image_sizes'"
        )
        .await,
        jobs
    );
    assert_eq!(
        std::fs::read_dir(&d.temp).unwrap().count(),
        0,
        "no spool left"
    );
}

/// An edit is a write to the file and to the focal fields: the same rules as any write.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn edits_obey_field_rules(pool: PgPool) {
    let d = dirs();
    let admin = admin(&pool, &d).await;
    collection(
        &admin,
        "images",
        json!(true),
        json!({"fields": [
            {"name": "alt", "type": "text"},
            {"name": "focalX", "type": "number", "access": {"update": {"role": "admin"}}}
        ]}),
    )
    .await;
    let doc = post(&admin, "images", &png(fixtures::gradient(100, 80)), "a.png").await;
    let one = format!("/api/images/{}", doc["id"].as_str().unwrap());
    add_user(&pool, "ed@example.com", "editor").await;
    let editor = signed_in(app(&pool, &d).await, "ed@example.com").await;
    let before = held(&pool, &doc["id"]).await;
    for body in [
        json!({"uploadEdits": {"focalPoint": {"x": 10, "y": 10}}}),
        json!({"uploadEdits": {"crop": {"x": 0, "y": 0, "width": 10, "height": 10}}}),
        json!({"focalX": 10}),
    ] {
        let res = call(&editor, "PATCH", &one, body).await;
        assert_problem(&res, 403);
    }
    assert_eq!(held(&pool, &doc["id"]).await, before);
    assert_eq!(count(&pool, "SELECT count(*) FROM files").await, 1);
    // Their other writes still go through, the focal point untouched.
    let got = ok(&editor, "PATCH", &one, Some(json!({"alt": "x"}))).await;
    assert_eq!(got["alt"], "x");
}

/// On a collection with drafts, an edit is a draft save: the published copy keeps its file
/// until the draft is published. Read as a draft, the file's urls ask for the draft's, and
/// the route gives it to who may read drafts; anyone else gets the published file at that
/// url, exactly as without the parameter.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn edits_are_draft_saves(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "s", "width": 20}]}),
        json!({"versions": {"drafts": true}, "access": {"read": true}}),
    )
    .await;
    let doc = post(&app, "images", &png(fixtures::gradient(100, 80)), "a.png").await;
    let id = doc["id"].as_str().unwrap();
    ok(&app, "POST", &format!("/api/images/{id}/publish"), None).await;
    let files_of = async || -> (Uuid, Uuid) {
        sqlx::query_as("SELECT published_file, draft_file FROM documents WHERE id = $1")
            .bind(id.parse::<Uuid>().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let (published, _) = files_of().await;
    let crop = json!({"uploadEdits": {"crop": {"x": 0, "y": 0, "width": 50, "height": 40}}});
    let got = ok(
        &app,
        "PATCH",
        &format!("/api/images/{id}?wait=true"),
        Some(crop),
    )
    .await;
    assert_eq!(got["width"], 50);
    let (still, draft) = files_of().await;
    assert_eq!(still, published, "the published copy keeps its file");
    assert_ne!(draft, published);
    let url = &got["url"];
    assert_eq!(
        url,
        "http://localhost:3000/api/images/file/a.png?draft=true"
    );
    let small = &got["sizes"]["s"]["url"];
    assert!(small.as_str().unwrap().ends_with("?draft=true"), "{small}");
    // Who may read drafts gets the cropped file, and its size, never to be cached.
    let cache = |h: &axum::http::HeaderMap| h["cache-control"].to_str().unwrap().to_owned();
    let (_, headers, body) = fetch_with(&app, url).await;
    assert_eq!(cache(&headers), "private, no-store");
    let image = image::load_from_memory(&body).unwrap();
    assert_eq!((image.width(), image.height()), (50, 40));
    let (_, headers, body) = fetch_with(&app, small).await;
    assert_eq!(cache(&headers), "private, no-store", "a size too");
    assert_eq!(image::load_from_memory(&body).unwrap().width(), 20);
    // Anyone else gets the published file there, with the plain url's headers but for the
    // one every `?draft=true` answer carries, whoever asks.
    let anonymous = self::app(&pool, &d).await;
    let plain = json!("http://localhost:3000/api/images/file/a.png");
    let (status, mut headers, body) = fetch_with(&anonymous, url).await;
    let (status_without, mut headers_without, body_without) = fetch_with(&anonymous, &plain).await;
    assert_eq!((status, status_without), (StatusCode::OK, StatusCode::OK));
    assert_eq!(cache(&headers), "private, no-store");
    assert_eq!(
        cache(&headers_without),
        "public, no-cache",
        "the plain url keeps its own"
    );
    // Every request has its own ID; nothing else may differ.
    for h in [&mut headers, &mut headers_without] {
        h.remove("x-request-id");
        h.remove("cache-control");
    }
    assert_eq!(headers, headers_without);
    assert!(body == body_without, "the same bytes");
    let image = image::load_from_memory(&body).unwrap();
    assert_eq!(
        (image.width(), image.height()),
        (100, 80),
        "the published file"
    );
    // A read of the published copy links to it plainly.
    let read = ok(&app, "GET", &format!("/api/images/{id}"), None).await;
    assert_eq!(read["url"], plain);
    assert_eq!(read["width"], 100);
    ok(&app, "POST", &format!("/api/images/{id}/publish"), None).await;
    assert_eq!(files_of().await, (draft, draft));
    let image = served(&anonymous, &plain).await;
    assert_eq!((image.width(), image.height()), (50, 40), "published now");
}

/// A write that sends back the focal point it read, after another edit remade the file
/// for a new one, is refused: writing the old value over the new file would leave the
/// document and its file disagreeing.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_stale_focal_point_isnt_written_back(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "images", json!(true), json!({})).await;
    let doc = post(&app, "images", &png(fixtures::gradient(100, 80)), "a.png").await;
    let id: Uuid = doc["id"].as_str().unwrap().parse().unwrap();
    let one = format!("/api/images/{id}");
    // The first write reads the document, then waits before it locks it.
    let pause = tachyon_api::pause_next_update(id);
    let first = tokio::spawn({
        let (app, one) = (app.clone(), one.clone());
        async move { call(&app, "PATCH", &one, json!({"focalX": 50, "alt": "stale"})).await }
    });
    pause.reached().await;
    let edits = json!({"uploadEdits": {"focalPoint": {"x": 20, "y": 30}}});
    let got = ok(&app, "PATCH", &one, Some(edits)).await;
    assert_eq!(got["focalX"], 20);
    let edited = held(&pool, &doc["id"]).await;
    pause.resume();
    let res = first.await.unwrap();
    assert_problem(&res, 409);
    assert_eq!(
        held(&pool, &doc["id"]).await,
        edited,
        "the edit's file stays"
    );
    let now = ok(&app, "GET", &one, None).await;
    assert_eq!((&now["focalX"], &now["focalY"]), (&json!(20), &json!(30)));
    assert_ne!(now["alt"], "stale");
    let row = files::get(&pool, edited).await.unwrap();
    assert_eq!(
        (row.focal_x, row.focal_y, row.state.as_str()),
        (20.0, 30.0, "live")
    );
}

/// A focal write that read the document before another edit remade its file is refused,
/// even when the file it then finds is that edit's: it would cut sizes around a point the
/// document doesn't end at.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_focal_edit_racing_another_is_refused(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(&app, "images", json!(true), json!({})).await;
    let doc = post(&app, "images", &png(fixtures::gradient(100, 80)), "a.png").await;
    let id: Uuid = doc["id"].as_str().unwrap().parse().unwrap();
    let one = format!("/api/images/{id}");
    let pause = tachyon_api::pause_next_update(id);
    let first = tokio::spawn({
        let (app, one) = (app.clone(), one.clone());
        async move { call(&app, "PATCH", &one, json!({"focalX": 70})).await }
    });
    pause.reached().await;
    let edits = json!({"uploadEdits": {"focalPoint": {"x": 20, "y": 30}}});
    ok(&app, "PATCH", &one, Some(edits)).await;
    let edited = held(&pool, &doc["id"]).await;
    let rows = count(&pool, "SELECT count(*) FROM files WHERE state <> 'gone'").await;
    pause.resume();
    let res = first.await.unwrap();
    assert_problem(&res, 409);
    assert_eq!(held(&pool, &doc["id"]).await, edited);
    let now = ok(&app, "GET", &one, None).await;
    assert_eq!((&now["focalX"], &now["focalY"]), (&json!(20), &json!(30)));
    assert_eq!(
        count(&pool, "SELECT count(*) FROM files WHERE state <> 'gone'").await,
        rows,
        "the refused write's file is given up"
    );
}

/// `uploadEdits` with nothing in it, or beside focal values that aren't numbers, makes
/// nothing.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn empty_or_bad_edits_make_nothing(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    collection(
        &app,
        "images",
        json!({"imageSizes": [{"name": "s", "width": 10}]}),
        json!({}),
    )
    .await;
    let doc = post(&app, "images", &png(fixtures::gradient(100, 80)), "a.png").await;
    let one = format!("/api/images/{}", doc["id"].as_str().unwrap());
    let file = held(&pool, &doc["id"]).await;
    let rows = count(&pool, "SELECT count(*) FROM files").await;
    let jobs = count(
        &pool,
        "SELECT count(*) FROM jobs WHERE kind = 'image_sizes'",
    )
    .await;
    let got = ok(
        &app,
        "PATCH",
        &one,
        Some(json!({"uploadEdits": {}, "alt": "x"})),
    )
    .await;
    assert_eq!(got["alt"], "x");
    let crop = json!({"x": 0, "y": 0, "width": 10, "height": 10});
    for (body, path) in [
        (
            json!({"focalX": "abc", "uploadEdits": {"crop": crop}}),
            "focalX",
        ),
        (
            json!({"focalX": 30, "focalY": "abc", "uploadEdits": {"crop": crop}}),
            "focalY",
        ),
        (
            json!({"focalY": [], "uploadEdits": {"focalPoint": {"x": 1, "y": 1}}}),
            "focalY",
        ),
    ] {
        let res = call(&app, "PATCH", &one, body).await;
        assert_problem(&res, 422);
        assert_eq!(res.json["errors"][0]["path"], path, "{}", res.json);
    }
    assert_eq!(held(&pool, &doc["id"]).await, file);
    assert_eq!(count(&pool, "SELECT count(*) FROM files").await, rows);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM jobs WHERE kind = 'image_sizes'"
        )
        .await,
        jobs
    );
    assert_eq!(ok(&app, "GET", &one, None).await["focalX"], 50);
}

/// A new file writes what it sets, so field rules on those fields judge it: a rule on
/// `filename` stops a replacement that renames the file, and a rule on `width` stops a crop,
/// on update and on create. What a write leaves as it was isn't judged.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn field_rules_cover_what_a_new_file_sets(pool: PgPool) {
    let d = dirs();
    let admin = admin(&pool, &d).await;
    let admins = json!({"role": "admin"});
    collection(
        &admin,
        "named",
        json!(true),
        json!({"fields": [{"name": "filename", "type": "text", "access": {"update": admins, "create": admins}}]}),
    )
    .await;
    collection(
        &admin,
        "sized",
        json!(true),
        json!({"fields": [{"name": "width", "type": "number", "access": {"update": admins}}]}),
    )
    .await;
    let image = png(fixtures::gradient(100, 80));
    let named = post(&admin, "named", &image, "a.png").await;
    let sized = post(&admin, "sized", &image, "a.png").await;
    add_user(&pool, "ed@example.com", "editor").await;
    let editor = signed_in(app(&pool, &d).await, "ed@example.com").await;
    let named_one = format!("/api/named/{}", named["id"].as_str().unwrap());
    let sized_one = format!("/api/sized/{}", sized["id"].as_str().unwrap());
    let crop = json!({"uploadEdits": {"crop": {"x": 0, "y": 0, "width": 10, "height": 10}}});
    let rows = count(&pool, "SELECT count(*) FROM files").await;

    // `filename` locked: a renaming replacement and a create with a file are refused.
    let res = upload(&editor, "PATCH", &named_one, Some(&image), "b.png", None).await;
    assert_problem(&res, 403);
    let res = upload(&editor, "POST", "/api/named", Some(&image), "c.png", None).await;
    assert_problem(&res, 403);
    // `width` locked: a crop changes it, a focal point alone doesn't.
    assert_problem(&call(&editor, "PATCH", &sized_one, crop.clone()).await, 403);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM files").await,
        rows,
        "nothing made"
    );
    let focal = json!({"uploadEdits": {"focalPoint": {"x": 10, "y": 10}}});
    ok(&editor, "PATCH", &sized_one, Some(focal)).await;
    // A crop and a same-name replacement keep `filename`, so its rule doesn't stop them.
    let got = ok(&editor, "PATCH", &named_one, Some(crop)).await;
    assert_eq!(
        (&got["width"], &got["filename"]),
        (&json!(10), &json!("a.png"))
    );
    let res = upload(&editor, "PATCH", &named_one, Some(&image), "a.png", None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["width"], 100);
    // The Admin the rules allow is judged the same way, and passes.
    let res = upload(&admin, "PATCH", &named_one, Some(&image), "b.png", None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["filename"], "b.png");
}

/// A refusal of a focal value beside an edit is hidden like any other validation error at
/// a field the caller can't read.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn bad_focal_values_hide_like_other_errors(pool: PgPool) {
    let d = dirs();
    let admin = admin(&pool, &d).await;
    collection(
        &admin,
        "images",
        json!(true),
        json!({"fields": [
            {"name": "alt", "type": "text"},
            {"name": "focalX", "type": "number", "access": {"read": {"role": "admin"}}}
        ]}),
    )
    .await;
    let doc = post(&admin, "images", &png(fixtures::gradient(100, 80)), "a.png").await;
    let one = format!("/api/images/{}", doc["id"].as_str().unwrap());
    add_user(&pool, "ed@example.com", "editor").await;
    let editor = signed_in(app(&pool, &d).await, "ed@example.com").await;
    let crop = json!({"x": 0, "y": 0, "width": 10, "height": 10});
    let body = json!({"focalX": "x", "uploadEdits": {"crop": crop}});
    let res = call(&admin, "PATCH", &one, body.clone()).await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["path"], "focalX",
        "who reads it is told"
    );
    let res = call(&editor, "PATCH", &one, body).await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "", "{}", res.json);
    assert!(!res.json.to_string().contains("focalX"), "{}", res.json);
}
