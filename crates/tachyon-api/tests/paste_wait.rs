//! A pasted URL's `?wait=true`, in a process of its own, so its timings don't queue for
//! decode permits (`DECODES`, per process) behind other tests' images.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use common::{ADMIN, PASSWORD, Res, png, send, signed_in, upload};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_api::{Options, UploadsConfig};
use tachyon_media::fixtures;
use tachyon_media::storage::{StorageConfig, Storages};

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> Res {
    send(app, method, uri, Some(&body.to_string())).await
}

fn pasted(url: &str, alt: &str) -> Value {
    json!({"alt": alt, "file": {"url": url}})
}

/// A signed-in `?wait=true` paste whose sizes job never finishes answers with what's made
/// so far, as call 5 says: its wait ends a second before its cap (the upload limit from
/// arrival), well before the request's own limit, so it never answers 503 after the
/// document is made. Limits: normal 3 s, uploads 4 s; each wait ends about 3 s in, 4 s
/// before the request would.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_pasted_waits_end_before_the_request_does(pool: PgPool) {
    let base = tempfile::Builder::new()
        .prefix("tachyon-paste-wait-")
        .tempdir()
        .unwrap();
    let (temp, root) = (base.path().join("spool"), base.path().join("store"));
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let storages = Storages::open(&BTreeMap::from([(
        "default".into(),
        StorageConfig::local(&root),
    )]))
    .unwrap();
    // No workers, so the sizes job never runs.
    let options = Options {
        request_timeout: Some(Duration::from_secs(3)),
        uploads: UploadsConfig {
            temp_dir: temp.clone(),
            timeout: Duration::from_secs(4),
            storages: Arc::new(storages),
            ..UploadsConfig::default()
        },
        ..Options::default()
    };
    let app = tachyon_api::router_with(pool.clone(), options)
        .await
        .unwrap();
    let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "setup: {}", res.json);
    let app = signed_in(app, ADMIN).await;
    let schema = json!({
        "slug": "images", "fields": [{"name": "alt", "type": "text"}],
        "upload": {"skipSafeFetch": true, "imageSizes": [{"name": "t", "width": 1}]}
    });
    let res = call(&app, "POST", "/api/admin/collections", schema).await;
    assert!(res.status.is_success(), "{}", res.json);
    let image = png(3, 2);
    let addr = fixtures::serve(move |_| {
        let head = format!("Content-Length: {}", image.len());
        vec![fixtures::response("200 OK", &[&head], &image)]
    })
    .await;
    let url = format!("http://{addr}/cat.png");
    let res = call(&app, "POST", "/api/images?wait=true", pasted(&url, "x")).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["sizes"], json!({"t": null}), "made so far");
    // A multipart body with only `_payload` comes in with the upload limit, and ends the same.
    let payload = pasted(&url, "m");
    let res = upload(
        &app,
        "POST",
        "/api/images?wait=true",
        None,
        "",
        Some(&payload),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["sizes"], json!({"t": null}), "made so far");
    assert_eq!(res.json["alt"], "m");
    let id = res.json["id"].as_str().unwrap();
    let uri = format!("/api/images/{id}?wait=true");
    let res = call(&app, "PATCH", &uri, pasted(&url, "y")).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["alt"], "y");
}
