//! The `upload` field: a relationship whose targets hold files.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use common::{ADMIN, PASSWORD, add_user, assert_problem, png, send, signed_in, upload};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_api::{Options, UploadsConfig};
use tachyon_db::documents::MISSING_LINK;
use tachyon_media::storage::{StorageConfig, Storages};

/// A test's spool and store, removed with everything in them when the test is done.
fn dir() -> tempfile::TempDir {
    let base = tempfile::Builder::new()
        .prefix("tachyon-upload-field-")
        .tempdir()
        .unwrap();
    for sub in ["spool", "store"] {
        std::fs::create_dir_all(base.path().join(sub)).unwrap();
    }
    base
}

async fn router(pool: &PgPool, d: &tempfile::TempDir) -> Router {
    let storages = Storages::open(&BTreeMap::from([(
        "default".into(),
        StorageConfig::local(d.path().join("store")),
    )]))
    .unwrap();
    let options = Options {
        uploads: UploadsConfig {
            temp_dir: d.path().join("spool"),
            storages: Arc::new(storages),
            ..UploadsConfig::default()
        },
        ..Options::default()
    };
    tachyon_api::router_with(pool.clone(), options)
        .await
        .unwrap()
}

async fn admin(pool: &PgPool, d: &tempfile::TempDir) -> Router {
    let app = router(pool, d).await;
    let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "setup: {}", res.json);
    signed_in(app, ADMIN).await
}

/// `body` is sent unless it's `null`.
async fn ok(app: &Router, method: &str, uri: &str, body: Value) -> Value {
    let text = (!body.is_null()).then(|| body.to_string());
    let res = send(app, method, uri, text.as_deref()).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

async fn schema(app: &Router, schema: Value) {
    ok(app, "POST", "/api/admin/collections", schema).await;
}

/// Uploads `bytes` as `name` to `slug`, and returns the document.
async fn put(app: &Router, slug: &str, bytes: &[u8], name: &str) -> Value {
    let res = upload(
        app,
        "POST",
        &format!("/api/{slug}"),
        Some(bytes),
        name,
        None,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    res.json
}

fn pdf() -> Vec<u8> {
    b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n".to_vec()
}

/// `media`, readable by admins only, and `posts`, readable by anyone, with a `cover`.
async fn blog(app: &Router) {
    schema(
        app,
        json!({"slug": "media", "upload": true, "access": {"read": {"role": ["admin"]}},
               "fields": [{"name": "alt", "type": "text"}]}),
    )
    .await;
    schema(
        app,
        json!({"slug": "posts", "access": {"read": true}, "fields": [
            {"name": "title", "type": "text"},
            {"name": "cover", "type": "upload", "relationTo": "media"}
        ]}),
    )
    .await;
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn upload_field_links_and_populates(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    blog(&app).await;
    let media = put(&app, "media", &png(4, 4), "cat.png").await;
    let id = media["id"].as_str().unwrap();
    let post = ok(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "Hi", "cover": id}),
    )
    .await;
    let url = format!("/api/posts/{}", post["id"].as_str().unwrap());

    let flat = ok(&app, "GET", &format!("{url}?depth=0"), json!(null)).await;
    assert_eq!(flat["cover"], id);
    let deep = ok(&app, "GET", &format!("{url}?depth=1"), json!(null)).await;
    assert_eq!(deep["cover"]["id"], id);
    assert_eq!(deep["cover"]["filename"], "cat.png");
    assert_eq!(deep["cover"]["url"], media["url"]);
    assert!(
        deep["cover"]["url"]
            .as_str()
            .is_some_and(|u| u.ends_with("/api/media/file/cat.png")),
        "{}",
        deep["cover"]
    );

    // The schema says what it is.
    let saved = ok(&app, "GET", "/api/admin/collections/posts", json!(null)).await;
    assert_eq!(saved["schema"]["fields"][1]["type"], "upload");

    let res = send(&app, "DELETE", &format!("/api/media/{id}"), None).await;
    assert_problem(&res, 409);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn upload_targets_must_be_uploads(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    blog(&app).await;
    let bad = json!({"slug": "pages", "fields": [
        {"name": "hero", "type": "upload", "relationTo": "posts"}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&bad.to_string()),
    )
    .await;
    assert_problem(&res, 422);
    let err = &res.json["errors"][0];
    assert_eq!(err["path"], "fields.hero.relationTo", "{}", res.json);
    assert!(err["message"].as_str().unwrap().contains("posts"), "{err}");

    // Turning upload off on a target an upload field points at names the field, confirm or not.
    let off = json!({"confirm": true, "schema": {"slug": "media", "fields": [{"name": "alt", "type": "text"}]}});
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/media",
        Some(&off.to_string()),
    )
    .await;
    assert_problem(&res, 409);
    assert!(
        res.json["detail"].as_str().unwrap().contains("posts.cover"),
        "{}",
        res.json
    );

    // minRows and maxRows need hasMany.
    let rows = json!({"slug": "pages", "fields": [
        {"name": "hero", "type": "upload", "relationTo": "media", "maxRows": 2}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&rows.to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert!(
        res.json.to_string().contains("only apply with hasMany"),
        "{}",
        res.json
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn filter_options_bind_writes(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(&app, json!({"slug": "media", "upload": true, "fields": []})).await;
    // Checked against the target's fields at save.
    let typo = json!({"slug": "posts", "fields": [
        {"name": "cover", "type": "upload", "relationTo": "media",
         "filterOptions": {"mimeTipe": {"like": "image"}}}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&typo.to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["path"], "fields.cover.filterOptions",
        "{}",
        res.json
    );

    schema(
        &app,
        json!({"slug": "posts", "fields": [
            {"name": "cover", "type": "upload", "relationTo": "media",
             "filterOptions": {"mimeType": {"like": "image"}}, "displayPreview": true}
        ]}),
    )
    .await;
    let saved = ok(&app, "GET", "/api/admin/collections/posts", json!(null)).await;
    assert_eq!(saved["schema"]["fields"][0]["displayPreview"], true);

    let doc = put(&app, "media", &pdf(), "terms.pdf").await;
    assert_eq!(doc["mimeType"], "application/pdf");
    let res = send(
        &app,
        "POST",
        "/api/posts",
        Some(&json!({"cover": doc["id"]}).to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "cover", "{}", res.json);
    assert_eq!(res.json["errors"][0]["message"], MISSING_LINK);

    let image = put(&app, "media", &png(4, 4), "cat.png").await;
    let res = send(
        &app,
        "POST",
        "/api/posts",
        Some(&json!({"cover": image["id"]}).to_string()),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn polymorphic_has_many_rows(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(&app, json!({"slug": "media", "upload": true, "fields": []})).await;
    schema(
        &app,
        json!({"slug": "downloads", "upload": true, "fields": []}),
    )
    .await;
    schema(
        &app,
        json!({"slug": "posts", "fields": [
            {"name": "files", "type": "upload", "relationTo": ["media", "downloads"],
             "hasMany": true, "maxRows": 2}
        ]}),
    )
    .await;
    let m = put(&app, "media", &png(4, 4), "a.png").await;
    let dl = put(&app, "downloads", &pdf(), "b.pdf").await;
    let rows = json!([
        {"relationTo": "media", "value": m["id"]},
        {"relationTo": "downloads", "value": dl["id"]}
    ]);
    let post = ok(&app, "POST", "/api/posts", json!({"files": rows})).await;
    let url = format!("/api/posts/{}", post["id"].as_str().unwrap());
    let flat = ok(&app, "GET", &format!("{url}?depth=0"), json!(null)).await;
    assert_eq!(flat["files"], rows);
    let deep = ok(&app, "GET", &format!("{url}?depth=1"), json!(null)).await;
    assert_eq!(deep["files"][1]["relationTo"], "downloads");
    assert_eq!(deep["files"][1]["value"]["filename"], "b.pdf");
    assert!(deep["files"][1]["value"]["url"].is_string(), "{}", deep);

    let third = put(&app, "media", &png(2, 2), "c.png").await;
    let mut three = rows.as_array().unwrap().clone();
    three.push(json!({"relationTo": "media", "value": third["id"]}));
    let res = send(
        &app,
        "PATCH",
        &url,
        Some(&json!({"files": three}).to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "files", "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn filters_through_upload(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    blog(&app).await;
    let media = put(&app, "media", &png(4, 4), "cat.png").await;
    let post = ok(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "Hi", "cover": media["id"]}),
    )
    .await;
    let image = "/api/posts?where[cover.mimeType][like]=image";
    let nothing = "/api/posts?where[cover.mimeType][like]=no-such-type";
    let found = ok(&app, "GET", image, json!(null)).await;
    assert_eq!(found["docs"][0]["id"], post["id"], "{found}");

    add_user(&pool, "viewer@example.com", "viewer").await;
    let viewer = signed_in(router(&pool, &d).await, "viewer@example.com").await;
    let hidden = send(&viewer, "GET", image, None).await;
    let missing = send(&viewer, "GET", nothing, None).await;
    assert_eq!(hidden.status, missing.status);
    assert_eq!(hidden.json, missing.json);
    assert_eq!(hidden.status, 200, "{}", hidden.json);
    assert_eq!(hidden.json["docs"], json!([]));
}
