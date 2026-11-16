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

/// `posts` with a filtered `cover`, a filtered list `images`, a filtered polymorphic
/// `either` and an unfiltered `attachments`.
async fn filtered(app: &Router) {
    let images = json!({"mimeType": {"like": "image"}});
    schema(app, json!({"slug": "media", "upload": true, "fields": []})).await;
    schema(
        app,
        json!({"slug": "downloads", "upload": true, "fields": []}),
    )
    .await;
    schema(
        app,
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "cover", "type": "upload", "relationTo": "media", "filterOptions": images},
            {"name": "images", "type": "upload", "relationTo": "media", "hasMany": true,
             "filterOptions": images},
            {"name": "either", "type": "upload", "relationTo": ["media", "downloads"],
             "hasMany": true, "filterOptions": images},
            {"name": "attachments", "type": "upload", "relationTo": "media", "hasMany": true}
        ]}),
    )
    .await;
}

fn paths(res: &common::Res) -> Vec<(String, String)> {
    res.json["errors"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|e| {
            let text = |k: &str| e[k].as_str().unwrap_or_default().to_owned();
            (text("path"), text("message"))
        })
        .collect()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn filter_options_check_every_link_a_write_sends(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    filtered(&app).await;
    let pdf_m = put(&app, "media", &pdf(), "terms.pdf").await["id"].clone();
    let png_m = put(&app, "media", &png(4, 4), "cat.png").await["id"].clone();
    let pdf_d = put(&app, "downloads", &pdf(), "guide.pdf").await["id"].clone();
    let post = ok(&app, "POST", "/api/posts", json!({"attachments": [pdf_m]})).await;
    let url = format!("/api/posts/{}", post["id"].as_str().unwrap());
    let patch = |body: Value| {
        let (app, url) = (app.clone(), url.clone());
        async move { send(&app, "PATCH", &url, Some(&body.to_string())).await }
    };
    let missing = |path: &str| vec![(path.to_owned(), MISSING_LINK.to_owned())];

    // Already linked elsewhere in the document, it's still checked where it's sent.
    let res = patch(json!({"cover": pdf_m})).await;
    assert_problem(&res, 422);
    assert_eq!(paths(&res), missing("cover"));
    // Each row of a list.
    let res = patch(json!({"images": [png_m, pdf_m]})).await;
    assert_problem(&res, 422);
    assert_eq!(paths(&res), missing("images.1"));
    // Each target of a polymorphic field.
    let res = patch(json!({"either": [
        {"relationTo": "media", "value": png_m},
        {"relationTo": "downloads", "value": pdf_d}
    ]}))
    .await;
    assert_problem(&res, 422);
    assert_eq!(paths(&res), missing("either.1"));
    // A write that doesn't send a field doesn't check it.
    let res = patch(json!({"title": "Hi", "cover": png_m, "images": [png_m]})).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = patch(json!({"title": "Again"})).await;
    assert_eq!(res.status, 200, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn publishing_checks_filter_options(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(&app, json!({"slug": "media", "upload": true, "fields": []})).await;
    schema(
        &app,
        json!({"slug": "posts", "versions": {"drafts": true}, "fields": [
            {"name": "cover", "type": "upload", "relationTo": "media",
             "filterOptions": {"mimeType": {"like": "image"}}}
        ]}),
    )
    .await;
    let m = put(&app, "media", &png(4, 4), "cat.png").await;
    let id = m["id"].as_str().unwrap();
    let post = ok(&app, "POST", "/api/posts", json!({"cover": id})).await;
    assert_eq!(post["status"], "draft");
    // The image becomes a PDF after the draft linked it.
    let res = upload(
        &app,
        "PATCH",
        &format!("/api/media/{id}"),
        Some(&pdf()),
        "terms.pdf",
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let publish = format!("/api/posts/{}/publish", post["id"].as_str().unwrap());
    let res = send(&app, "POST", &publish, None).await;
    assert_problem(&res, 422);
    assert_eq!(paths(&res), vec![("cover".into(), MISSING_LINK.into())]);
}

/// `blog`, plus `photos`, which anyone reads but whose `filename` only admins read, and an
/// editor, who can't read `media`.
async fn guarded(pool: &PgPool, d: &tempfile::TempDir) -> (Router, Router, Value, Value) {
    let app = admin(pool, d).await;
    schema(
        &app,
        json!({"slug": "media", "upload": true, "access": {"read": {"role": ["admin"]}},
               "fields": []}),
    )
    .await;
    schema(
        &app,
        json!({"slug": "photos", "upload": true, "access": {"read": true}, "fields": [
            {"name": "filename", "type": "text", "access": {"read": {"role": ["admin"]}}}
        ]}),
    )
    .await;
    schema(
        &app,
        json!({"slug": "posts", "access": {"read": true}, "fields": [
            {"name": "title", "type": "text"},
            {"name": "cover", "type": "upload", "relationTo": "media",
             "filterOptions": {"mimeType": {"like": "image"}}},
            {"name": "photo", "type": "upload", "relationTo": "photos"}
        ]}),
    )
    .await;
    let m = put(&app, "media", &png(4, 4), "cat.png").await;
    let p = put(&app, "photos", &png(4, 4), "dog.png").await;
    let post = ok(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "Hi", "cover": m["id"], "photo": p["id"]}),
    )
    .await;
    add_user(pool, "editor@example.com", "editor").await;
    let editor = signed_in(router(pool, d).await, "editor@example.com").await;
    (app, editor, post, m)
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn hidden_media_populate_as_ids_and_round_trip(pool: PgPool) {
    let d = dir();
    let (_, editor, post, m) = guarded(&pool, &d).await;
    let url = format!("/api/posts/{}", post["id"].as_str().unwrap());
    let read = ok(&editor, "GET", &format!("{url}?depth=1"), json!(null)).await;
    assert_eq!(read["cover"], m["id"], "{read}");
    assert_eq!(read["photo"]["id"], post["photo"]["id"]);
    assert!(read["photo"].get("filename").is_none(), "{read}");
    assert!(read["photo"]["url"].is_null(), "{read}");

    // What the editor read goes back unchanged, the link they can't read included.
    let flat = ok(&editor, "GET", &format!("{url}?depth=0"), json!(null)).await;
    let body = json!({"title": "Edited", "cover": flat["cover"], "photo": flat["photo"]});
    let res = send(&editor, "PATCH", &url, Some(&body.to_string())).await;
    assert_eq!(res.status, 200, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn select_and_populate_shape_upload_fields(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    blog(&app).await;
    let m = put(&app, "media", &png(4, 4), "cat.png").await;
    let post = ok(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "Hi", "cover": m["id"]}),
    )
    .await;
    let url = format!("/api/posts/{}", post["id"].as_str().unwrap());
    let got = ok(
        &app,
        "GET",
        &format!("{url}?depth=1&populate[media][filename]=true"),
        json!(null),
    )
    .await;
    assert_eq!(got["cover"]["filename"], "cat.png", "{got}");
    assert!(got["cover"].get("mimeType").is_none(), "{got}");
    let got = ok(
        &app,
        "GET",
        &format!("{url}?depth=0&select[cover]=true"),
        json!(null),
    )
    .await;
    assert_eq!(got["cover"], m["id"]);
    assert!(got.get("title").is_none(), "{got}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_relationship_becomes_an_upload_keeping_its_links(pool: PgPool) {
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
            {"name": "cover", "type": "relationship", "relationTo": "media"}
        ]}),
    )
    .await;
    let m = put(&app, "media", &png(4, 4), "cat.png").await;
    let id = m["id"].as_str().unwrap();
    let post = ok(&app, "POST", "/api/posts", json!({"cover": id})).await;
    let url = format!("/api/posts/{}", post["id"].as_str().unwrap());
    let change = |field: Value| json!({"schema": {"slug": "posts", "fields": [field]}});

    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/posts",
        Some(
            &change(json!({"name": "cover", "type": "upload", "relationTo": "media"})).to_string(),
        ),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        ok(&app, "GET", &format!("{url}?depth=0"), json!(null)).await["cover"],
        id
    );
    assert_problem(
        &send(&app, "DELETE", &format!("/api/media/{id}"), None).await,
        409,
    );

    // Back, gaining a target: the link is wrapped, not dropped.
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/posts",
        Some(
            &change(json!({"name": "cover", "type": "relationship",
                           "relationTo": ["media", "downloads"]}))
            .to_string(),
        ),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        ok(&app, "GET", &format!("{url}?depth=0"), json!(null)).await["cover"],
        json!({"relationTo": "media", "value": id})
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn filter_options_wait_for_a_change_to_the_linked_document(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    filtered(&app).await;
    let m = put(&app, "media", &png(4, 4), "cat.png").await["id"].clone();
    let post = ok(&app, "POST", "/api/posts", json!({"title": "Hi"})).await;
    let url = format!("/api/posts/{}", post["id"].as_str().unwrap());

    // Another transaction is turning the image into a PDF, and hasn't committed yet.
    let mut other = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE documents SET published = jsonb_set(published, '{mimeType}', '\"application/pdf\"') \
         WHERE id = $1::uuid",
    )
    .bind(m.as_str().unwrap())
    .execute(&mut *other)
    .await
    .unwrap();
    let body = json!({"cover": m}).to_string();
    let write = tokio::spawn({
        let app = app.clone();
        async move { send(&app, "PATCH", &url, Some(&body)).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!write.is_finished(), "the check waits for the change");
    other.commit().await.unwrap();
    let res = write.await.unwrap();
    assert_problem(&res, 422);
    assert_eq!(paths(&res), vec![("cover".into(), MISSING_LINK.into())]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn hidden_links_stay_in_their_field(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "media", "upload": true, "access": {"read": {"role": ["admin"]}},
               "fields": []}),
    )
    .await;
    schema(
        &app,
        json!({"slug": "posts", "access": {"read": true}, "fields": [
            {"name": "cover", "type": "upload", "relationTo": "media",
             "filterOptions": {"mimeType": {"like": "image"}}},
            {"name": "extra", "type": "upload", "relationTo": "media"},
            {"name": "attachments", "type": "upload", "relationTo": "media", "hasMany": true}
        ]}),
    )
    .await;
    let x = put(&app, "media", &pdf(), "terms.pdf").await["id"].clone();
    let y = put(&app, "media", &png(4, 4), "cat.png").await["id"].clone();
    let post = ok(&app, "POST", "/api/posts", json!({"attachments": [x, y]})).await;
    let url = format!("/api/posts/{}", post["id"].as_str().unwrap());
    add_user(&pool, "editor@example.com", "editor").await;
    let editor = signed_in(router(&pool, &d).await, "editor@example.com").await;
    let patch = |body: Value| {
        let (editor, url) = (editor.clone(), url.clone());
        async move { send(&editor, "PATCH", &url, Some(&body.to_string())).await }
    };

    // A link the editor can't read can't move to another field, filtered or not.
    for field in ["cover", "extra"] {
        let res = patch(json!({field: x})).await;
        assert_problem(&res, 422);
        assert_eq!(paths(&res), vec![(field.into(), MISSING_LINK.into())]);
    }
    // It may stay where it is, in any order.
    let flat = ok(&editor, "GET", &format!("{url}?depth=0"), json!(null)).await;
    let res = patch(json!({"attachments": flat["attachments"]})).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = patch(json!({"attachments": [y, x]})).await;
    assert_eq!(res.status, 200, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn hidden_links_stay_in_their_block_type(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "media", "upload": true, "access": {"read": {"role": ["admin"]}},
               "fields": []}),
    )
    .await;
    schema(
        &app,
        json!({"slug": "posts", "access": {"read": true}, "fields": [
            {"name": "layout", "type": "blocks", "blocks": [
                {"slug": "gallery", "fields": [
                    {"name": "file", "type": "upload", "relationTo": "media",
                     "filterOptions": {"mimeType": {"like": "image"}}}
                ]},
                {"slug": "doc", "fields": [
                    {"name": "file", "type": "upload", "relationTo": "media"}
                ]}
            ]}
        ]}),
    )
    .await;
    let x = put(&app, "media", &pdf(), "terms.pdf").await["id"].clone();
    let post = ok(
        &app,
        "POST",
        "/api/posts",
        json!({"layout": [{"blockType": "doc", "file": x}]}),
    )
    .await;
    let url = format!("/api/posts/{}", post["id"].as_str().unwrap());
    add_user(&pool, "editor@example.com", "editor").await;
    let editor = signed_in(router(&pool, &d).await, "editor@example.com").await;

    let moved = json!({"layout": [{"blockType": "gallery", "file": x}]});
    let res = send(&editor, "PATCH", &url, Some(&moved.to_string())).await;
    assert_problem(&res, 422);
    assert_eq!(
        paths(&res),
        vec![("layout.0.file".into(), MISSING_LINK.into())]
    );

    let flat = ok(&editor, "GET", &format!("{url}?depth=0"), json!(null)).await;
    let same = json!({"layout": flat["layout"]});
    let res = send(&editor, "PATCH", &url, Some(&same.to_string())).await;
    assert_eq!(res.status, 200, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn hidden_links_keep_their_schema_position(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "media", "upload": true, "access": {"read": {"role": ["admin"]}},
               "fields": []}),
    )
    .await;
    // A row's own `blockType` text field mustn't make its key collide with another position.
    let row_fields = |nested: Value| {
        json!([
            {"name": "blockType", "type": "text"},
            {"name": "a", "type": "upload", "relationTo": "media"},
            {"name": "x", "type": "group", "fields": [
                {"name": "a", "type": "upload", "relationTo": "media",
                 "filterOptions": {"mimeType": {"like": "image"}}}
            ]},
            {"name": "b", "type": "group", "fields": [
                {"name": "a", "type": "upload", "relationTo": "media"}
            ]},
            {"name": "nested", "type": "array", "fields": nested}
        ])
    };
    schema(
        &app,
        json!({"slug": "posts", "access": {"read": true}, "fields": [
            {"name": "rows", "type": "array", "fields": row_fields(row_fields(json!([])))},
            {"name": "layout", "type": "blocks", "blocks": [
                {"slug": "doc", "fields": [
                    {"name": "file", "type": "upload", "relationTo": "media"}
                ]}
            ]}
        ]}),
    )
    .await;
    let x = put(&app, "media", &pdf(), "terms.pdf").await["id"].clone();
    let rows = json!([
        {"blockType": "x", "a": x},
        {"blockType": "b", "a": x},
        {"nested": [{"blockType": "x", "a": x}]}
    ]);
    let post = ok(
        &app,
        "POST",
        "/api/posts",
        json!({"rows": rows, "layout": [{"blockType": "doc", "file": x}]}),
    )
    .await;
    let url = format!("/api/posts/{}", post["id"].as_str().unwrap());
    add_user(&pool, "editor@example.com", "editor").await;
    let editor = signed_in(router(&pool, &d).await, "editor@example.com").await;
    let patch = |body: Value| {
        let (editor, url) = (editor.clone(), url.clone());
        async move { send(&editor, "PATCH", &url, Some(&body.to_string())).await }
    };

    // Into the filtered `x.a`, the unfiltered `b.a`, and the filtered `x.a` of a nested row.
    for (rows, at) in [
        (json!([{"x": {"a": x}}]), "rows.0.x.a"),
        (json!([{"b": {"a": x}}]), "rows.0.b.a"),
        (
            json!([{"nested": [{"x": {"a": x}}]}]),
            "rows.0.nested.0.x.a",
        ),
    ] {
        let res = patch(json!({"rows": rows})).await;
        assert_problem(&res, 422);
        assert_eq!(paths(&res), vec![(at.into(), MISSING_LINK.into())], "{at}");
    }
    // Unchanged, and reordered, the rows and blocks keep their hidden links.
    let flat = ok(&editor, "GET", &format!("{url}?depth=0"), json!(null)).await;
    let res = patch(json!({"rows": flat["rows"], "layout": flat["layout"]})).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let reordered: Vec<Value> = flat["rows"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .cloned()
        .collect();
    let res = patch(json!({"rows": reordered})).await;
    assert_eq!(res.status, 200, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn files_and_their_links_read_at_depth_save_back_unchanged(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    blog(&app).await;
    schema(
        &app,
        json!({"slug": "pages", "fields": [
            {"name": "cover", "type": "upload", "relationTo": "media"},
            {"name": "body", "type": "richText", "relationTo": ["media"],
             "upload": {"collections": {"media": {}}}}
        ]}),
    )
    .await;
    let media = put(&app, "media", &png(4, 4), "cat.png").await;
    let id = media["id"].as_str().unwrap();
    let url = format!("/api/media/{id}");
    let wait = format!("{url}?depth=0");
    // Sizes are made by a job; whatever it has done by now, the read goes back as it is.
    let file = ok(&app, "GET", &url, json!(null)).await;
    assert_eq!(file["filename"], "cat.png", "{file}");
    let saved = ok(&app, "PATCH", &url, file.clone()).await;
    let after = ok(&app, "GET", &wait, json!(null)).await;
    for key in [
        "filename", "mimeType", "filesize", "width", "height", "focalX", "focalY",
    ] {
        assert_eq!(saved[key], file[key], "{key}");
        assert_eq!(after[key], file[key], "{key}");
    }
    assert_eq!(after["createdAt"], file["createdAt"]);

    let page = ok(
        &app,
        "POST",
        "/api/pages",
        json!({"cover": id, "body": {"type": "doc", "content": [
            {"type": "upload", "attrs": {"relationTo": "media", "value": id}}
        ]}}),
    )
    .await;
    let page_url = format!("/api/pages/{}", page["id"].as_str().unwrap());
    let read = ok(&app, "GET", &page_url, json!(null)).await;
    assert_eq!(read["cover"]["filename"], "cat.png", "populated: {read}");
    assert_eq!(
        read["body"]["content"][0]["attrs"]["value"]["filename"],
        "cat.png"
    );
    ok(&app, "PATCH", &page_url, read).await;
    let flat = ok(&app, "GET", &format!("{page_url}?depth=0"), json!(null)).await;
    assert_eq!(flat["cover"], id);
    assert_eq!(flat["body"]["content"][0]["attrs"]["value"], id);

    // What a read adds beside the fields must be the document's own, however it's written:
    // an ID in capitals, a time to the millisecond (as a browser writes one) or as +00:00.
    let read = ok(&app, "GET", &format!("{page_url}?depth=0"), json!(null)).await;
    let created = time::OffsetDateTime::parse(
        read["createdAt"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let ms = created
        .replace_nanosecond(created.nanosecond() / 1_000_000 * 1_000_000)
        .unwrap();
    let iso = |t: time::OffsetDateTime| {
        let f = time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]"
        );
        t.format(&f).unwrap()
    };
    for (key, value) in [
        ("id", json!(read["id"].as_str().unwrap().to_uppercase())),
        ("createdAt", json!(format!("{}Z", iso(ms)))),
        ("createdAt", json!(format!("{}+00:00", iso(ms)))),
        ("updatedAt", json!(null)),
        ("status", json!("draft")),
        ("publishedAt", json!("2020-01-01T00:00:00Z")),
    ] {
        let body = json!({ key: value }).to_string();
        let res = send(&app, "PATCH", &page_url, Some(&body)).await;
        assert_eq!(res.status, 200, "{key}: {value}: {}", res.json);
    }
    let date = "expected a date and time, as a read gives it";
    let publish = "expected draft or published, as a read gives it; PATCH doesn't change it, publish with POST /api/pages/{id}/publish";
    for (key, value, message) in [
        (
            "id",
            json!("0199d5c4-0000-7000-8000-00000000dead"),
            "set by the server",
        ),
        ("id", json!(7), "set by the server"),
        (
            "createdAt",
            json!("2020-01-01T00:00:00Z"),
            "set by the server",
        ),
        ("createdAt", json!("yesterday"), "set by the server"),
        ("updatedAt", json!("garbage"), date),
        ("publishedAt", json!({"x": 1}), date),
        ("status", json!(5), publish),
        ("status", json!("live"), publish),
    ] {
        let body = json!({ key: value }).to_string();
        let res = send(&app, "PATCH", &page_url, Some(&body)).await;
        assert_problem(&res, 422);
        assert_eq!(
            res.json["errors"],
            json!([{"path": key, "message": message}]),
            "{key}: {value}"
        );
    }
}
