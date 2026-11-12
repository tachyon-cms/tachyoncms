//! Hostile and awkward files, and what's stored and served for each; and the PROBES test for
//! uploads: whatever a caller without the right to a file reads, asks or filters by, nothing
//! of that file comes back.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use common::fixtures;
use common::{
    ADMIN, PASSWORD, Res, add_user, assert_problem, png, send, signed_in, upload, with_key,
};
use http_body_util::BodyExt;
use object_store::local::LocalFileSystem;
use object_store::path::Path as Key;
use object_store::signer::{Method, SignedUrlOptions, Signer};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_api::{JobsConfig, Options, UploadsConfig};
use tachyon_db::files;
use tachyon_media::storage::{SignedDownloads, Storage, StorageConfig, Storages};
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

/// Signs nothing: its URLs name the key they were made for.
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
        _: Duration,
        _: &SignedUrlOptions,
    ) -> object_store::Result<Url> {
        let mut url = Url::parse("https://bucket.test/").unwrap();
        url.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .extend(path.as_ref().split('/'));
        url.query_pairs_mut().append_pair("method", method.as_str());
        Ok(url)
    }
}

/// A test's spool directory, its local `default` storage, and `bucket`, a local store that
/// takes client uploads and signs every download.
struct Dirs {
    temp: PathBuf,
    root: PathBuf,
    bucket: PathBuf,
    _base: tempfile::TempDir,
}

fn dirs() -> Dirs {
    let base = tempfile::Builder::new()
        .prefix("tachyon-leaks-")
        .tempdir()
        .unwrap();
    let d = Dirs {
        temp: base.path().join("spool"),
        root: base.path().join("store"),
        bucket: base.path().join("bucket"),
        _base: base,
    };
    for dir in [&d.temp, &d.root, &d.bucket] {
        std::fs::create_dir_all(dir).unwrap();
    }
    d
}

fn options(d: &Dirs, workers: usize) -> Options {
    let local = Storage::open("default", &StorageConfig::local(&d.root)).unwrap();
    let mut c = StorageConfig::local(&d.bucket);
    c.client_uploads = true;
    c.signed_downloads = SignedDownloads::On {
        mime_types: Vec::new(),
        expires_in: Duration::from_secs(600),
    };
    let store = LocalFileSystem::new_with_prefix(&d.bucket).unwrap();
    let bucket =
        Storage::with_store("bucket", Arc::new(store), None, c).with_signer(Arc::new(FakeSigner));
    let storages = Storages(HashMap::from([
        ("default".into(), Arc::new(local)),
        ("bucket".into(), Arc::new(bucket)),
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

/// The API set up and signed in as its first Admin, with `workers` running jobs.
async fn admin_with(pool: &PgPool, d: &Dirs, workers: usize) -> Router {
    let app = tachyon_api::router_with(pool.clone(), options(d, workers))
        .await
        .unwrap();
    let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "setup: {}", res.json);
    signed_in(app, ADMIN).await
}

async fn admin(pool: &PgPool, d: &Dirs) -> Router {
    let app = router(pool, d).await;
    let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "setup: {}", res.json);
    signed_in(app, ADMIN).await
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Res {
    send(app, method, uri, body.map(|b| b.to_string()).as_deref()).await
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let res = call(app, method, uri, body).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
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
}

async fn fetch(app: &Router, uri: &str) -> Raw {
    fetch_as(app, "GET", uri).await
}

async fn fetch_as(app: &Router, method: &str, uri: &str) -> Raw {
    fetch_with(app, method, uri, &[]).await
}

async fn fetch_with(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)]) -> Raw {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req.body(Body::empty()).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    Raw {
        status,
        headers,
        body,
    }
}

/// The bytes stored for document `doc`'s file.
async fn stored(pool: &PgPool, d: &Dirs, doc: &Value) -> Vec<u8> {
    let file = file_of(pool, doc["id"].as_str().unwrap()).await;
    std::fs::read(d.root.join(files::get(pool, file).await.unwrap().key)).unwrap()
}

fn exif_in(bytes: &[u8]) -> Option<img_parts::Bytes> {
    use img_parts::ImageEXIF;
    img_parts::DynImage::from_bytes(bytes.to_vec().into())
        .ok()
        .flatten()
        .and_then(|i| i.exif())
}

fn spool_is_empty(d: &Dirs) -> bool {
    std::fs::read_dir(&d.temp).unwrap().next().is_none()
}

#[test]
fn generated_fixtures_say_what_they_claim() {
    let dims = |bytes: &[u8]| {
        image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap()
    };
    for (name, bomb) in fixtures::header_bombs() {
        let (w, h) = dims(&bomb);
        assert!(
            u64::from(w) * u64::from(h) >= 100_000_000,
            "{name}: {w}×{h}"
        );
        assert!(bomb.len() < 5_000, "{name}: {} bytes", bomb.len());
    }
    assert_eq!(dims(&fixtures::animated_webp(1000, 1000, 51)), (1000, 1000));
    for poly in [fixtures::png_html(3, 2), fixtures::png_zip(3, 2)] {
        assert_eq!(dims(&poly), (3, 2));
    }
    let zip = fixtures::png_zip(1, 1);
    assert_eq!(&zip[zip.len() - 22..zip.len() - 18], b"PK\x05\x06");
}

/// What a fixture comes to: a 201 with this type, size and frame count, or a 422 at `file`
/// with this message.
enum Outcome {
    Stored(&'static str, (u32, u32), usize),
    Refused(String),
}

fn too_many(pixels: u64) -> Outcome {
    Outcome::Refused(format!(
        "this image is {pixels} pixels, all its frames counted; the limit is 50000000"
    ))
}

/// Every fixture, committed and generated, and what it comes to.
fn corpus() -> Vec<(String, Vec<u8>, Outcome)> {
    use Outcome::*;
    let mut all: Vec<(String, Vec<u8>, Outcome)> = [
        ("gps.jpg", Stored("image/jpeg", (32, 16), 1)),
        // Stored upright: the 64×32 picture turned a quarter, as EXIF orientation 6 says.
        ("turned.jpg", Stored("image/jpeg", (32, 64), 1)),
        ("cmyk.jpg", Stored("image/jpeg", (24, 16), 1)),
        ("moving.gif", Stored("image/gif", (16, 12), 3)),
        ("moving.webp", Stored("image/webp", (16, 12), 3)),
        (
            "truncated.png",
            Refused("this image is cut off before its end".into()),
        ),
        ("style-script.svg", Stored("image/svg+xml", (40, 20), 1)),
        ("external-use.svg", Stored("image/svg+xml", (30, 30), 1)),
    ]
    .into_iter()
    .map(|(n, o)| (n.to_owned(), fixtures::committed(n), o))
    .collect();
    all.push((
        "png-html.png".into(),
        fixtures::png_html(3, 2),
        Stored("image/png", (3, 2), 1),
    ));
    all.push((
        "png-zip.png".into(),
        fixtures::png_zip(3, 2),
        Stored("image/png", (3, 2), 1),
    ));
    let claimed = [
        ("png", 10_000_000_000),
        ("jpeg", 4_225_000_000),
        ("gif", 4_294_836_225),
        ("webp", 100_000_000),
        ("bmp", 900_000_000),
    ];
    for ((kind, bomb), (named, pixels)) in fixtures::header_bombs().into_iter().zip(claimed) {
        assert_eq!(kind, named);
        all.push((format!("header-{kind}.{kind}"), bomb, too_many(pixels)));
    }
    for (kind, bomb) in fixtures::frame_bombs() {
        all.push((format!("frames-{kind}.{kind}"), bomb, too_many(51_000_000)));
    }
    all
}

/// The frames in a stored GIF or WebP; 1 for anything else.
fn frames_in(bytes: &[u8], mime: &str) -> usize {
    use image::AnimationDecoder;
    match mime {
        "image/gif" => image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
            .unwrap()
            .into_frames()
            .count(),
        "image/webp" => bytes.windows(4).filter(|w| w == b"ANMF").count().max(1),
        _ => 1,
    }
}

/// Each fixture, uploaded to a collection that strips metadata and to one that keeps it:
/// refused with the stated message and nothing left behind, or stored with the stated
/// type, size and frames, EXIF only where `withMetadata` keeps it, and served as the
/// sniffed type, never sniffed again.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn fixture_corpus(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    for (slug, upload) in [
        ("images", json!(true)),
        ("originals", json!({"withMetadata": true})),
    ] {
        ok(
            &app,
            "POST",
            "/api/admin/collections",
            Some(json!({"slug": slug, "upload": upload, "fields": []})),
        )
        .await;
    }
    let anon = router(&pool, &d).await;
    let tail = fixtures::POLYGLOT_TAIL.as_bytes();
    let holds = |bytes: &[u8], what: &[u8]| bytes.windows(what.len()).any(|w| w == what);
    for (name, bytes, outcome) in corpus() {
        for slug in ["images", "originals"] {
            let at = format!("{slug}: {name}");
            let res = upload(
                &app,
                "POST",
                &format!("/api/{slug}"),
                Some(&bytes),
                &name,
                None,
            )
            .await;
            let (mime, (w, h), frames) = match &outcome {
                Outcome::Refused(message) => {
                    assert_problem(&res, 422);
                    assert_eq!(res.json["errors"][0]["path"], "file", "{at}");
                    assert_eq!(res.json["errors"][0]["message"], *message, "{at}");
                    continue;
                }
                Outcome::Stored(mime, dims, frames) => (*mime, *dims, *frames),
            };
            assert_eq!(res.status, 201, "{at}: {}", res.json);
            assert_eq!(res.json["mimeType"], mime, "{at}");
            assert_eq!(
                (&res.json["width"], &res.json["height"]),
                (&json!(w), &json!(h)),
                "{at}"
            );
            let kept = stored(&pool, &d, &res.json).await;
            assert_eq!(res.json["filesize"], kept.len(), "{at}");
            assert_eq!(frames_in(&kept, mime), frames, "{at}");
            let keeps = slug == "originals";
            if mime != "image/svg+xml" {
                let made = image::load_from_memory(&kept).unwrap();
                // Kept with its EXIF, the turned JPEG keeps its pixels as taken, and the
                // orientation that shows them upright.
                let pixels = match (name.as_str(), keeps) {
                    ("turned.jpg", true) => (h, w),
                    _ => (w, h),
                };
                assert_eq!((made.width(), made.height()), pixels, "{at}: as stored");
                // Turned the way orientation 6 says, a quarter clockwise: the source's
                // top-left corner is now the top right.
                if name == "turned.jpg" && !keeps {
                    let source = image::load_from_memory(&bytes).unwrap().to_rgb8();
                    let want = image::imageops::rotate90(&source);
                    let got = made.to_rgb8();
                    for (x, y) in [(1, 1), (w - 2, 1), (1, h - 2), (w - 2, h - 2)] {
                        let (a, b) = (want.get_pixel(x, y).0, got.get_pixel(x, y).0);
                        let off = a.iter().zip(b).map(|(a, b)| a.abs_diff(b)).max().unwrap();
                        assert!(off < 40, "{at}: ({x}, {y}) is {b:?}, turned it's {a:?}");
                    }
                }
            }
            assert_eq!(
                exif_in(&kept).is_some(),
                keeps && exif_in(&bytes).is_some(),
                "{at}: EXIF only with withMetadata"
            );
            match name.as_str() {
                // Kept as uploaded, GPS and all, only when asked to.
                "gps.jpg" | "turned.jpg" if keeps => assert_eq!(kept, bytes, "{at}"),
                "style-script.svg" | "external-use.svg" => {
                    let text = String::from_utf8(kept.clone()).unwrap();
                    for gone in ["script", "alert", "evil.example", "data:", "CDATA"] {
                        assert!(!text.contains(gone), "{at}: {gone} survived: {text}");
                    }
                    if name == "external-use.svg" {
                        assert!(text.contains(r##"<use href="#r"/>"##), "{at}: {text}");
                    }
                }
                // What follows the PNG isn't metadata: it goes whatever withMetadata
                // keeps, and the file ends where the PNG does.
                "png-html.png" | "png-zip.png" => {
                    assert!(!holds(&kept, tail), "{at}");
                    assert!(!holds(&kept, b"PK\x05\x06"), "{at}");
                    assert!(kept.ends_with(b"IEND\xAE\x42\x60\x82"), "{at}");
                }
                _ => {}
            }
            let served = fetch(
                &anon,
                &format!(
                    "/api/{slug}/file/{}",
                    res.json["filename"].as_str().unwrap()
                ),
            )
            .await;
            assert_eq!(served.status, 200, "{at}");
            assert_eq!(served.body, kept, "{at}");
            assert_eq!(served.header("content-type"), mime, "{at}");
            assert_eq!(served.header("x-content-type-options"), "nosniff", "{at}");
            let sandbox = if mime == "image/svg+xml" {
                "sandbox"
            } else {
                ""
            };
            assert_eq!(served.header("content-security-policy"), sandbox, "{at}");
        }
    }
    // Only what was stored has a row, and nothing is left in the spool.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM files WHERE state = 'live'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let stored_count = corpus()
        .iter()
        .filter(|(_, _, o)| matches!(o, Outcome::Stored(..)))
        .count();
    assert_eq!(rows, 2 * stored_count as i64);
    let all: i64 = sqlx::query_scalar("SELECT count(*) FROM files")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(all, rows, "a refused file reserves nothing");
    assert!(spool_is_empty(&d));
}

/// What a caller learns from an answer: everything but the request's own ID.
fn seen_as(raw: &Raw) -> (u16, BTreeMap<String, String>, Value) {
    let headers = raw
        .headers
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "date" | "x-request-id"))
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let mut body = serde_json::from_slice(&raw.body)
        .unwrap_or_else(|_| Value::String(format!("{:?}", raw.body)));
    if let Some(o) = body.as_object_mut() {
        o.remove("requestId");
    }
    (raw.status.as_u16(), headers, body)
}

fn id_of(v: &Value) -> String {
    v["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no id: {v}"))
        .to_owned()
}

/// The file the draft copy of document `id` holds, or the published one.
async fn file_of(pool: &PgPool, id: &str) -> Uuid {
    sqlx::query_scalar("SELECT coalesce(draft_file, published_file) FROM documents WHERE id = $1")
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

/// `name` with its `stem` swapped for one no file has.
fn unknown(name: &str, stem: &str) -> String {
    name.replacen(stem, "nothingfile", 1)
}

/// The uploads site. `media` keeps drafts, signs every download from `bucket`, makes a
/// 4-pixel `thumb` (with WebP and AVIF copies), and shows `filename` to Authors and up
/// only, so not to Viewers or anyone signed out.
/// `downloads` are read only by whoever owns them. `vault` is for Editors, and so is the
/// folder `Hush Folder`, which lists only it.
///
/// `seen` is published, filed in `Open Folder`, with a crop in its draft. `ghost` was never
/// published: only its draft, filed in `Ghost Folder`, holds `ghostfile.png`. `priv` is the
/// Admin's own download, `privfile.png`. `post` is published linking `seen` and `priv`
/// (an `upload` field each, and an upload node each), with a draft linking `ghost` the same
/// ways; `gone`, in the trash, links `ghost` from its draft.
struct Site {
    admin: Router,
    seen: String,
    ghost: String,
    /// Every name `ghost`'s file is stored under: the original, its size, its formats.
    ghost_names: Vec<String>,
    ghost_file: Uuid,
    priv_doc: String,
    priv_file: Uuid,
    post: String,
    hush: String,
    below: String,
    open: String,
    /// The versions of `ghost`, and the version of `post` whose draft links it.
    ghost_versions: Vec<String>,
    post_draft_version: String,
}

const HUSH: &str = "Hush Folder";

async fn uploads_site(pool: &PgPool, d: &Dirs) -> Site {
    let admin = admin_with(pool, d, 1).await;
    let hidden_from_viewers = json!({"read": {"role": ["admin", "editor", "author"]}});
    for schema in [
        json!({"slug": "media", "versions": {"drafts": true}, "folders": true,
               "upload": {"storage": "bucket", "imageSizes": [{"name": "thumb", "width": 4}]},
               "fields": [{"name": "alt", "type": "text"},
                          {"name": "filename", "type": "text", "access": hidden_from_viewers}]}),
        json!({"slug": "downloads", "upload": true,
               "access": {"read": {"owner": "$user.id"}, "create": true},
               "fields": [{"name": "owner", "type": "relationship", "relationTo": "users"}]}),
        json!({"slug": "vault", "folders": true, "access": {"read": {"role": "editor"}},
               "fields": [{"name": "title", "type": "text"}]}),
        json!({"slug": "posts", "versions": {"drafts": true}, "fields": [
            {"name": "title", "type": "text"},
            {"name": "cover", "type": "upload", "relationTo": "media"},
            {"name": "file", "type": "upload", "relationTo": "downloads"},
            {"name": "hero", "type": "upload", "relationTo": "media",
             "filterOptions": {"alt": {"equals": "Nothing"}}},
            {"name": "body", "type": "richText", "relationTo": ["media", "downloads"],
             "upload": {"collections": {"media": {}, "downloads": {}}}}
        ]}),
    ] {
        ok(&admin, "POST", "/api/admin/collections", Some(schema)).await;
    }
    let folder = |name: &'static str, parent: Option<String>, collections: Value| {
        let admin = admin.clone();
        async move {
            let mut body = json!({"name": name, "collections": collections});
            if let Some(p) = parent {
                body["folder"] = p.into();
            }
            id_of(&ok(&admin, "POST", "/api/folders", Some(body)).await)
        }
    };
    let hush = folder(HUSH, None, json!(["vault"])).await;
    let below = folder("Below", Some(hush.clone()), json!([])).await;
    let open = folder("Open Folder", None, json!(["media"])).await;
    let ghost_folder = folder("Ghost Folder", None, json!(["media"])).await;

    let put = |slug: &'static str, bytes: Vec<u8>, name: &'static str, payload: Value| {
        let admin = admin.clone();
        async move {
            let res = upload(
                &admin,
                "POST",
                &format!("/api/{slug}?wait=true"),
                Some(&bytes),
                name,
                Some(&payload),
            )
            .await;
            assert_eq!(res.status, 201, "{name}: {}", res.json);
            res.json
        }
    };
    let seen = put(
        "media",
        png(8, 8),
        "seenfile.png",
        json!({"alt": "Seen", "folder": open}),
    )
    .await;
    let seen = id_of(&seen);
    ok(&admin, "POST", &format!("/api/media/{seen}/publish"), None).await;
    let crop = json!({"uploadEdits": {"crop": {"x": 0, "y": 0, "width": 4, "height": 4}}});
    ok(
        &admin,
        "PATCH",
        &format!("/api/media/{seen}?wait=true"),
        Some(crop),
    )
    .await;
    let ghost = put(
        "media",
        png(6, 6),
        "ghostfile.png",
        json!({"alt": "Ghost", "folder": ghost_folder}),
    )
    .await;
    let ghost = id_of(&ghost);
    let admin_id: Uuid = sqlx::query_scalar("SELECT user_id FROM accounts WHERE email = $1")
        .bind(ADMIN)
        .fetch_one(pool)
        .await
        .unwrap();
    let private = put(
        "downloads",
        png(5, 5),
        "privfile.png",
        json!({"owner": admin_id}),
    )
    .await;
    let private = id_of(&private);

    let node =
        |to: &str, id: &str| json!({"type": "upload", "attrs": {"relationTo": to, "value": id}});
    let body = |nodes: Vec<Value>| json!({"type": "doc", "content": nodes});
    let post = ok(
        &admin,
        "POST",
        "/api/posts",
        Some(json!({"title": "Pub", "cover": seen, "file": private,
                    "body": body(vec![node("media", &seen), node("downloads", &private)])})),
    )
    .await;
    let post = id_of(&post);
    ok(&admin, "POST", &format!("/api/posts/{post}/publish"), None).await;
    ok(
        &admin,
        "PATCH",
        &format!("/api/posts/{post}"),
        Some(json!({"cover": ghost, "body": body(vec![node("media", &ghost)])})),
    )
    .await;
    let gone = ok(
        &admin,
        "POST",
        "/api/posts",
        Some(json!({"title": "Gone", "cover": ghost, "body": body(vec![node("media", &ghost)])})),
    )
    .await;
    ok(
        &admin,
        "DELETE",
        &format!("/api/posts/{}", id_of(&gone)),
        None,
    )
    .await;

    let names = |file: Uuid| async move {
        let mut names: Vec<String> =
            sqlx::query_scalar("SELECT unnest(names) FROM files WHERE id = $1")
                .bind(file)
                .fetch_all(pool)
                .await
                .unwrap();
        names.sort();
        names
    };
    let ghost_file = file_of(pool, &ghost).await;
    let ghost_names = names(ghost_file).await;
    // The original, its thumb, and the thumb's WebP and AVIF copies.
    assert_eq!(ghost_names.len(), 4, "{ghost_names:?}");
    assert!(ghost_names.iter().all(|n| n.starts_with("ghostfile")));
    let versions = |path: String| {
        let admin = admin.clone();
        async move {
            let list = ok(&admin, "GET", &path, None).await;
            list["docs"]
                .as_array()
                .unwrap()
                .iter()
                .map(id_of)
                .collect::<Vec<_>>()
        }
    };
    let ghost_versions = versions(format!("/api/media/{ghost}/versions")).await;
    assert!(!ghost_versions.is_empty());
    // Newest first: the draft linking `ghost`.
    let post_draft_version = versions(format!("/api/posts/{post}/versions")).await[0].clone();
    Site {
        ghost_versions,
        post_draft_version,
        admin,
        seen,
        ghost,
        ghost_names,
        ghost_file,
        priv_file: file_of(pool, &private).await,
        priv_doc: private.clone(),
        post,
        hush,
        below,
        open,
    }
}

/// Pairs of requests, the first about something the caller mustn't learn of, the second
/// the same request about something that doesn't exist: each pair must be answered alike.
/// `viewer` adds the probes on what only callers below Author mustn't see, `seen`'s
/// filename.
fn probes(s: &Site, viewer: bool) -> Vec<(String, String, String)> {
    let missing = Uuid::now_v7().to_string();
    let get = |hit: String, miss: String| ("GET".to_owned(), hit, miss);
    let mut out = Vec::new();
    for draft in ["", "?draft=true"] {
        let mut names: Vec<(String, &str)> = s
            .ghost_names
            .iter()
            .map(|n| (n.clone(), "ghostfile"))
            .collect();
        names.push(("privfile.png".into(), "privfile"));
        if viewer {
            names.push(("seenfile.png".into(), "seenfile"));
        }
        for (name, stem) in names {
            let slug = if stem == "privfile" {
                "downloads"
            } else {
                "media"
            };
            out.push(get(
                format!("/api/{slug}/file/{name}{draft}"),
                format!("/api/{slug}/file/{}{draft}", unknown(&name, stem)),
            ));
        }
    }
    for q in ["", "?draft=true", "?depth=2", "?depth=2&draft=true"] {
        for (path, hit) in [
            ("/api/media/", s.ghost.as_str()),
            ("/api/folders/", s.hush.as_str()),
        ] {
            out.push(get(
                format!("{path}{hit}{q}"),
                format!("{path}{missing}{q}"),
            ));
        }
    }
    let g = &s.ghost;
    let post = &s.post;
    let mut by_version = Vec::new();
    for v in &s.ghost_versions {
        by_version.push((
            format!("/api/media/{g}/versions/{v}"),
            format!("/api/media/{g}/versions/{missing}"),
        ));
    }
    let v = &s.post_draft_version;
    by_version.push((
        format!("/api/posts/{post}/versions/{v}"),
        format!("/api/posts/{post}/versions/{missing}"),
    ));
    for (hit, miss) in by_version {
        out.push(get(hit.clone(), miss.clone()));
        out.push((
            "POST".to_owned(),
            format!("{hit}/restore"),
            format!("{miss}/restore"),
        ));
    }
    for tail in [
        "/references",
        "/versions",
        "/diff?from=published&to=draft",
        "/schedule",
    ] {
        out.push(get(
            format!("/api/media/{g}{tail}"),
            format!("/api/media/{missing}{tail}"),
        ));
    }
    let thumb = s
        .ghost_names
        .iter()
        .find(|n| n.ends_with(".png") && n.as_str() != "ghostfile.png")
        .unwrap();
    let mut wheres = vec![
        (
            "posts",
            "cover.filename",
            "ghostfile.png".to_owned(),
            "nothingfile.png".to_owned(),
        ),
        ("posts", "cover", g.clone(), missing.clone()),
        ("posts", "cover.id", g.clone(), missing.clone()),
        ("posts", "cover.alt", "Ghost".into(), "Nobody".into()),
        (
            "posts",
            "cover.sizes.thumb.filename",
            thumb.clone(),
            unknown(thumb, "ghostfile"),
        ),
        (
            "posts",
            "cover.folder.name",
            "Ghost%20Folder".into(),
            "Nowhere".into(),
        ),
        (
            "posts",
            "file.filename",
            "privfile.png".into(),
            "nothingfile.png".into(),
        ),
        (
            "media",
            "filename",
            "ghostfile.png".into(),
            "nothingfile.png".into(),
        ),
        ("media", "id", g.clone(), missing.clone()),
        ("media", "alt", "Ghost".into(), "Nobody".into()),
        (
            "media",
            "folder.name",
            "Ghost%20Folder".into(),
            "Nowhere".into(),
        ),
        (
            "downloads",
            "filename",
            "privfile.png".into(),
            "nothingfile.png".into(),
        ),
        (
            "folders",
            "name",
            HUSH.replace(' ', "%20"),
            "Nowhere".into(),
        ),
        (
            "folders",
            "folder.name",
            HUSH.replace(' ', "%20"),
            "Nowhere".into(),
        ),
    ];
    if viewer {
        wheres.push((
            "posts",
            "cover.filename",
            "seenfile.png".into(),
            "nothingfile.png".into(),
        ));
        wheres.push((
            "media",
            "filename",
            "seenfile.png".into(),
            "nothingfile.png".into(),
        ));
    }
    for (slug, path, hit, miss) in wheres {
        for q in ["", "&draft=true", "&count=true"] {
            let probe = |v: &str| format!("/api/{slug}?where[{path}][equals]={v}{q}");
            out.push(get(probe(&hit), probe(&miss)));
        }
    }
    // Emptying a folder no one may see, or into one.
    let del = |hit: String, miss: String| ("DELETE".to_owned(), hit, miss);
    out.push(del(
        format!("/api/folders/{}?moveContentsTo=root", s.hush),
        format!("/api/folders/{missing}?moveContentsTo=root"),
    ));
    out.push(del(
        format!("/api/folders/{}?moveContentsTo={}", s.open, s.hush),
        format!("/api/folders/{}?moveContentsTo={missing}", s.open),
    ));
    out
}

/// The reads whose every answer must hold nothing of the hidden files, for each format,
/// depth and copy.
fn reads(s: &Site) -> Vec<String> {
    let mut out = Vec::new();
    for draft in ["", "&draft=true"] {
        for format in ["json", "html", "markdown", "text"] {
            for depth in [0, 2] {
                for doc in [
                    "/api/posts".to_owned(),
                    format!("/api/posts/{}", s.post),
                    "/api/media".to_owned(),
                    format!("/api/media/{}", s.seen),
                    "/api/downloads".to_owned(),
                    "/api/folders".to_owned(),
                    format!("/api/folders/{}", s.below),
                ] {
                    out.push(format!("{doc}?depth={depth}&format={format}{draft}"));
                }
            }
        }
    }
    let (post, seen) = (&s.post, &s.seen);
    out.extend([
        format!("/api/posts/{post}/references"),
        format!("/api/media/{seen}/references"),
        format!("/api/posts/{post}/versions"),
        format!("/api/media/{seen}/versions"),
        format!("/api/posts/{post}/diff?from=published&to=draft"),
        format!("/api/media/{seen}/diff?from=published&to=draft"),
        "/api/posts/trash".to_owned(),
        "/api/media/trash".to_owned(),
        "/api/folders?depth=1&count=true".to_owned(),
    ]);
    out
}

/// For anyone signed out, a Viewer, a Viewer's read key and another Author, nothing reveals
/// the draft-only file, the Admin's own download or the Editors' folder: not the file route
/// (each name, signed or not, with or without `?draft=true`), lists, reads at any depth or
/// format, filters through links, references, versions, diffs, the trash, folders and
/// emptying them, upload instructions, claims, or a pasted URL's refusal.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn uploads_never_leak(pool: PgPool) {
    let d = dirs();
    let s = uploads_site(&pool, &d).await;
    add_user(&pool, "viewer@example.com", "viewer").await;
    add_user(&pool, "other@example.com", "author").await;
    // A key is its owner with less: the Viewer's, so the Admin's own download is no more
    // its owner's than anyone else's.
    let viewer = signed_in(router(&pool, &d).await, "viewer@example.com").await;
    let read_key = ok(
        &viewer,
        "POST",
        "/api/auth/keys",
        Some(json!({"name": "site",
                    "scopes": ["posts:read", "media:read", "downloads:read", "folders:read"]})),
    )
    .await["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let callers = [
        ("anonymous", router(&pool, &d).await),
        ("viewer", viewer),
        ("read key", with_key(router(&pool, &d).await, &read_key)),
        (
            "other author",
            signed_in(router(&pool, &d).await, "other@example.com").await,
        ),
    ];
    let mut words = vec![
        s.ghost.clone(),
        s.ghost_file.to_string(),
        "ghostfile".to_owned(),
        s.priv_file.to_string(),
        "privfile".to_owned(),
        HUSH.to_owned(),
    ];
    let leaks =
        |words: &[String], text: &str| words.iter().find(|w| text.contains(w.as_str())).cloned();
    let text_of = |raw: &Raw| {
        format!(
            "{} {}",
            raw.header("location"),
            String::from_utf8_lossy(&raw.body)
        )
    };

    // The Admin sees what the others mustn't, so the checks below bite.
    let (ghost_file, ghost) = (s.ghost_file.to_string(), s.ghost.as_str());
    for (url, word) in [
        (format!("/api/posts/{}?draft=true", s.post), "ghostfile"),
        (
            format!("/api/posts/{}?draft=true&format=html", s.post),
            "ghostfile",
        ),
        (format!("/api/media/{ghost}?draft=true"), "ghostfile"),
        ("/api/media/file/ghostfile.png".to_owned(), &ghost_file),
        ("/api/downloads/file/privfile.png".to_owned(), "PNG"),
        (format!("/api/folders/{}?depth=1", s.below), HUSH),
        ("/api/posts/trash".to_owned(), ghost),
    ] {
        let raw = fetch(&s.admin, &url).await;
        assert!(
            raw.status.is_success() || raw.status == 302,
            "{url}: {}",
            raw.status
        );
        assert!(text_of(&raw).contains(word), "{url}: {}", text_of(&raw));
    }
    for (_, hit, miss) in probes(&s, true) {
        if ["moveContentsTo", "/schedule", "/restore"]
            .iter()
            .any(|w| hit.contains(w))
        {
            continue;
        }
        let drafts = hit.contains("draft=true");
        let differs = if hit.contains("/file/") {
            true
        } else if ["/api/folders", "/api/downloads", "seenfile"]
            .iter()
            .any(|w| hit.contains(w))
        {
            !drafts
        } else {
            drafts
        };
        if differs {
            let (a, b) = (fetch(&s.admin, &hit).await, fetch(&s.admin, &miss).await);
            assert_ne!(seen_as(&a), seen_as(&b), "the Admin's {hit}");
        }
    }

    for (who, app) in &callers {
        // `filename` is for Authors and up: the key is the Viewer's, so held to its rule.
        let viewer = *who != "other author";
        if viewer {
            // Its sizes aren't hidden, and are named for it; the original's own name is.
            words.push("seenfile.png".to_owned());
        }
        for url in reads(&s) {
            let raw = fetch(app, &url).await;
            assert_eq!(
                leaks(&words, &text_of(&raw)),
                None,
                "{who}: {url}: {}",
                text_of(&raw)
            );
        }
        let now = httpdate::fmt_http_date(std::time::SystemTime::now());
        for (method, hit, miss) in probes(&s, viewer) {
            let (a, b) = (
                fetch_as(app, &method, &hit).await,
                fetch_as(app, &method, &miss).await,
            );
            assert_eq!(seen_as(&a), seen_as(&b), "{who}: {method} {hit}");
            assert_eq!(leaks(&words, &text_of(&a)), None, "{who}: {hit}");
            // The file route answers HEAD, ranges and conditions only for what it finds.
            if !hit.contains("/file/") {
                continue;
            }
            let asks: [(&str, &[(&str, &str)]); 4] = [
                ("HEAD", &[]),
                ("GET", &[("range", "bytes=0-0")]),
                ("GET", &[("if-none-match", "*")]),
                ("GET", &[("if-modified-since", &now)]),
            ];
            for (method, headers) in asks {
                let (a, b) = (
                    fetch_with(app, method, &hit, headers).await,
                    fetch_with(app, method, &miss, headers).await,
                );
                assert_eq!(
                    seen_as(&a),
                    seen_as(&b),
                    "{who}: {method} {headers:?} {hit}"
                );
            }
        }
        // Linking what they can't read is refused as linking what isn't there, through an
        // upload field, a filtered one (the hidden file matches no filter either) and an
        // upload node.
        let missing = Uuid::now_v7().to_string();
        let node = |to: &str, id: &str| {
            json!({"type": "doc", "content": [
                {"type": "upload", "attrs": {"relationTo": to, "value": id}}]})
        };
        let links = |id: &str, private: &str| {
            [
                json!({"title": "t", "cover": id}),
                json!({"title": "t", "hero": id}),
                json!({"title": "t", "body": node("media", id)}),
                json!({"title": "t", "file": private}),
                json!({"title": "t", "body": node("downloads", private)}),
            ]
        };
        for (hit, miss) in links(&s.ghost, &s.priv_doc)
            .into_iter()
            .zip(links(&missing, &missing))
        {
            let (a, b) = (
                call(app, "POST", "/api/posts", Some(hit.clone())).await,
                call(app, "POST", "/api/posts", Some(miss)).await,
            );
            assert!(a.status.is_client_error(), "{who}: {hit}: {}", a.json);
            let scrubbed = |r: &Res| {
                let mut v = r.json.clone();
                if let Some(o) = v.as_object_mut() {
                    o.remove("requestId");
                }
                (r.status, no_ids(&v.to_string()))
            };
            assert_eq!(scrubbed(&a), scrubbed(&b), "{who}: {hit}");
        }
        if viewer {
            words.pop();
        }
    }

    // Shipped since: the crop in `seen`'s draft, client uploads, pasting a URL.
    let issue = |app: Router, name: &'static str| async move {
        let body = json!({"filename": name, "mimeType": "image/png", "filesize": 70});
        call(&app, "POST", "/api/media/upload-instructions", Some(body)).await
    };
    let put_to = |given: &Value, bytes: &[u8]| {
        let url = Url::parse(given["request"]["url"].as_str().unwrap()).unwrap();
        let path = d.bucket.join(url.path().trim_start_matches('/'));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        given["file"].clone()
    };
    let mut claims = Vec::new();
    for name in ["claim-a.png", "claim-b.png", "claim-c.png"] {
        let given = issue(s.admin.clone(), name).await;
        assert_eq!(given.status, 200, "{}", given.json);
        claims.push(put_to(&given.json, &png(2, 2)));
    }
    sqlx::query("UPDATE files SET created_at = now() - interval '2 hours' WHERE filename = $1")
        .bind("claim-b.png")
        .execute(&pool)
        .await
        .unwrap();
    ok(
        &s.admin,
        "POST",
        "/api/media",
        Some(json!({"file": claims[2]})),
    )
    .await;
    let mut made_up = claims[0].clone();
    made_up["filename"] = "made-up.png".into();
    claims.push(made_up);
    let open_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let pastes = [
        format!("http://{}/a.png", open_port.local_addr().unwrap()),
        "http://127.0.0.1:1/a.png".to_owned(),
        "http://localhost/a.png".to_owned(),
    ];
    let scrub = |res: &Res| {
        let mut v = res.json.clone();
        if let Some(o) = v.as_object_mut() {
            o.remove("requestId");
        }
        (
            res.status,
            no_ids(
                &v.to_string()
                    .replace("ghostfile", "X")
                    .replace("nothingfile", "X"),
            ),
        )
    };

    for (who, app) in &callers {
        // Asking where to put a file named like the hidden one, the same as any other name.
        let (a, b) = (
            issue(app.clone(), "ghostfile.png").await,
            issue(app.clone(), "nothingfile.png").await,
        );
        // Whoever may add files learns only that the name is taken: see
        // `a_guessed_filename_shows_only_that_it_is_taken`.
        if *who != "other author" {
            assert_eq!(scrub(&a), scrub(&b), "{who}: {}", a.json);
        }
        // Whether it's someone else's, past its time, taken, or made up, a claim is
        // refused alike.
        let refusals: Vec<_> = futures_util::future::join_all(
            claims
                .iter()
                .map(|f| call(app, "POST", "/api/media", Some(json!({"file": f})))),
        )
        .await;
        for r in &refusals {
            assert!(r.status.is_client_error(), "{who}: {}", r.json);
            assert_eq!(scrub(r), scrub(&refusals[0]), "{who}: {}", r.json);
        }
        // A staging key is never a file's name.
        if a.status == 200 {
            let staged = Url::parse(a.json["request"]["url"].as_str().unwrap()).unwrap();
            let key = staged.path().trim_start_matches('/').to_owned();
            let uuid = key.rsplit('/').next().unwrap().to_owned();
            for app in [app, &s.admin] {
                let miss = fetch(app, &format!("/api/media/file/{}", Uuid::new_v4())).await;
                for hit in [
                    format!("/api/media/file/{uuid}"),
                    format!("/api/media/file/{}", key.replace('/', "%2F")),
                ] {
                    let got = fetch(app, &hit).await;
                    assert_eq!(seen_as(&got), seen_as(&miss), "{who}: {hit}");
                }
            }
        }
        // Without the right to drafts, asking for one gets what not asking does, but for
        // never being cached; with it, the crop.
        let (plain, draft) = (
            fetch(app, "/api/media/file/seenfile.png").await,
            fetch(app, "/api/media/file/seenfile.png?draft=true").await,
        );
        let mut plain_seen = seen_as(&plain);
        plain_seen.1.insert(
            "cache-control".into(),
            draft.header("cache-control").to_owned(),
        );
        assert_eq!(plain_seen, seen_as(&draft), "{who}");
        // A file is signed for whoever may read it, and only them.
        if *who == "other author" {
            assert_eq!(plain.status, 302, "{who}");
            assert!(
                plain
                    .header("location")
                    .starts_with("https://bucket.test/media/")
            );
        } else {
            assert_eq!(plain.status, 404, "{who}");
        }
        // A pasted URL's refusal names no address and doesn't say whether it answers.
        let pasted: Vec<_> = futures_util::future::join_all(
            pastes
                .iter()
                .map(|u| call(app, "POST", "/api/media", Some(json!({"file": {"url": u}})))),
        )
        .await;
        for r in &pasted {
            assert_eq!(scrub(r), scrub(&pasted[0]), "{who}: {}", r.json);
            assert!(
                !r.json.to_string().contains("127.0.0.1"),
                "{who}: {}",
                r.json
            );
        }
    }
    let (plain, draft) = (
        fetch(&s.admin, "/api/media/file/seenfile.png").await,
        fetch(&s.admin, "/api/media/file/seenfile.png?draft=true").await,
    );
    assert_ne!(
        plain.header("location"),
        draft.header("location"),
        "the crop"
    );
}

/// `text` with every UUID in it replaced by `<id>`.
fn no_ids(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let candidate = rest.get(..36).filter(|c| Uuid::parse_str(c).is_ok());
        match candidate {
            Some(_) => {
                out.push_str("<id>");
                rest = &rest[36..];
            }
            None => {
                let c = rest.chars().next().unwrap();
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

/// Turning `upload` off counts, and releases, only the tenant's own files: another
/// tenant's collection of the same name, holding a file in two documents, is left alone.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn removing_uploads_stays_in_its_tenant(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    ok(
        &app,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "media", "upload": true, "fields": []})),
    )
    .await;
    let doc = upload(&app, "POST", "/api/media", Some(&png(2, 2)), "a.png", None).await;
    assert_eq!(doc.status, 201, "{}", doc.json);
    let doc = id_of(&doc.json);
    let file = file_of(&pool, &doc).await;

    // The other tenant's copy of all three, row for row.
    let (tenant, theirs) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = pool.begin().await.unwrap();
    for sql in [
        "INSERT INTO tenants (id, name) VALUES ($1, 'other')",
        "CREATE TEMP TABLE c ON COMMIT DROP AS SELECT * FROM collections WHERE slug = 'media'",
        "UPDATE c SET id = $2, tenant_id = $1",
        "INSERT INTO collections SELECT * FROM c",
        "CREATE TEMP TABLE f ON COMMIT DROP AS SELECT * FROM files WHERE id = $3",
        "UPDATE f SET id = $2, tenant_id = $1, collection_id = $2",
        "INSERT INTO files SELECT * FROM f",
        "CREATE TEMP TABLE d ON COMMIT DROP AS SELECT * FROM documents WHERE id = $4",
        "UPDATE d SET id = gen_random_uuid(), tenant_id = $1, collection_id = $2,
                      published_file = $2, draft_file = NULL",
        "INSERT INTO documents SELECT * FROM d",
        "UPDATE d SET id = gen_random_uuid()",
        "INSERT INTO documents SELECT * FROM d",
    ] {
        sqlx::query(sql)
            .bind(tenant)
            .bind(theirs)
            .bind(file)
            .bind(doc.parse::<Uuid>().unwrap())
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    tx.commit().await.unwrap();
    let holders = "SELECT count(*) FROM documents WHERE tenant_id = $1 AND published_file = $2";
    let count_theirs = async || -> i64 {
        sqlx::query_scalar(holders)
            .bind(tenant)
            .bind(theirs)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(count_theirs().await, 2);

    let off =
        |confirm: bool| json!({"schema": {"slug": "media", "fields": []}, "confirm": confirm});
    let res = call(
        &app,
        "PUT",
        "/api/admin/collections/media",
        Some(off(false)),
    )
    .await;
    assert_eq!(res.status, 409, "{}", res.json);
    assert_eq!(res.json["preview"]["holdingFiles"], 1, "{}", res.json);
    assert!(
        res.json["detail"]
            .as_str()
            .unwrap()
            .starts_with("1 document holds files;"),
        "{}",
        res.json
    );
    ok(&app, "PUT", "/api/admin/collections/media", Some(off(true))).await;
    let state = |id: Uuid| {
        sqlx::query_scalar::<_, String>("SELECT state FROM files WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
    };
    assert_eq!(state(file).await.unwrap(), "gone");
    assert_eq!(state(theirs).await.unwrap(), "live");
    assert_eq!(count_theirs().await, 2);
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = 'file_cleanup' AND payload->>'file' = $1",
    )
    .bind(theirs.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(queued, 0);
}

/// Accepted, as the reference does it (F18: one document per filename). Names are unique
/// across the collection, drafts included, so an Author can tell that some document holds a
/// name they guessed, from the number added to theirs: not which document holds it, nor
/// anything in it. Upload instructions and a plain upload say the same.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_guessed_filename_shows_only_that_it_is_taken(pool: PgPool) {
    let d = dirs();
    let app = admin(&pool, &d).await;
    ok(
        &app,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "media", "versions": {"drafts": true},
                    "upload": {"storage": "bucket"}, "fields": []})),
    )
    .await;
    let ghost = upload(
        &app,
        "POST",
        "/api/media",
        Some(&png(2, 2)),
        "ghostfile.png",
        None,
    )
    .await;
    assert_eq!(ghost.status, 201, "{}", ghost.json);
    let (ghost, file) = (
        id_of(&ghost.json),
        file_of(&pool, &id_of(&ghost.json)).await,
    );
    add_user(&pool, "other@example.com", "author").await;
    let other = signed_in(router(&pool, &d).await, "other@example.com").await;

    let issue = |name: &str| {
        let body = json!({"filename": name, "mimeType": "image/png", "filesize": 70});
        call(&other, "POST", "/api/media/upload-instructions", Some(body))
    };
    let (taken, free) = (issue("ghostfile.png").await, issue("nothingfile.png").await);
    assert_eq!(
        taken.json["file"]["filename"], "ghostfile-1.png",
        "{}",
        taken.json
    );
    assert_eq!(
        free.json["file"]["filename"], "nothingfile.png",
        "{}",
        free.json
    );
    let made = upload(
        &other,
        "POST",
        "/api/media",
        Some(&png(3, 3)),
        "ghostfile.png",
        None,
    )
    .await;
    assert_eq!(made.status, 201, "{}", made.json);
    // `-1` is held by the instructions above.
    assert_eq!(made.json["filename"], "ghostfile-2.png");
    for res in [&taken, &made] {
        let text = res.json.to_string();
        for word in [ghost.clone(), file.to_string()] {
            assert!(!text.contains(&word), "{word} in {text}");
        }
    }
    // Its own document and bytes stay its owner's.
    let mine = fetch(&other, "/api/media/file/ghostfile-2.png").await;
    assert_eq!(mine.status, 302);
    assert!(!mine.header("location").contains(&file.to_string()));
    let res = fetch(&other, "/api/media/file/ghostfile.png").await;
    assert_eq!(res.status, 404);
}
