//! A crop sent as JSON gets an upload's time limit for fetching and cutting the original,
//! as a pasted URL's fetch does, in a process of its own so its timings don't queue for
//! decode permits behind other tests' images.

mod common;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use common::{ADMIN, PASSWORD, Res, send, signed_in, upload};
use futures_util::stream::BoxStream;
use object_store::local::LocalFileSystem;
use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_api::{Options, UploadsConfig};
use tachyon_media::fixtures;
use tachyon_media::storage::{Storage, StorageConfig, Storages};

/// How long reading an object takes.
const SLOW: Duration = Duration::from_millis(2600);

/// A local store whose reads take `SLOW`.
#[derive(Debug)]
struct Slow(LocalFileSystem);

impl fmt::Display for Slow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Slow({})", self.0)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Slow {
    async fn put_opts(
        &self,
        location: &Key,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.0.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Key,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.0.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Key,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        tokio::time::sleep(SLOW).await;
        self.0.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Key>>,
    ) -> BoxStream<'static, object_store::Result<Key>> {
        self.0.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Key>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.0.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Key>) -> object_store::Result<ListResult> {
        self.0.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Key,
        to: &Key,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.0.copy_opts(from, to, options).await
    }
}

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> Res {
    send(app, method, uri, Some(&body.to_string())).await
}

/// Limits: normal 2 s, uploads 5 s. The crop's read alone takes 2.6 s, 600 ms past the
/// normal limit and 2.4 s inside the upload one, so only the longer limit lets it finish.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_json_crop_gets_the_upload_limit(pool: PgPool) {
    let base = tempfile::Builder::new()
        .prefix("tachyon-crop-time-")
        .tempdir()
        .unwrap();
    let (temp, root) = (base.path().join("spool"), base.path().join("store"));
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Slow(LocalFileSystem::new_with_prefix(&root).unwrap()));
    let storage = Storage::with_store("default", store, None, StorageConfig::local(&root));
    let options = Options {
        request_timeout: Some(Duration::from_secs(2)),
        uploads: UploadsConfig {
            temp_dir: temp.clone(),
            timeout: Duration::from_secs(5),
            storages: Arc::new(Storages(HashMap::from([(
                "default".to_owned(),
                Arc::new(storage),
            )]))),
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
    let schema = json!({"slug": "images", "upload": true, "fields": []});
    let res = call(&app, "POST", "/api/admin/collections", schema).await;
    assert!(res.status.is_success(), "{}", res.json);
    let image = fixtures::encoded(&fixtures::gradient(100, 80), image::ImageFormat::Png);
    let res = upload(&app, "POST", "/api/images", Some(&image), "a.png", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let one = format!("/api/images/{}", res.json["id"].as_str().unwrap());
    let crop = json!({"uploadEdits": {"crop": {"x": 0, "y": 0, "width": 50, "height": 40}}});
    let started = Instant::now();
    let res = call(&app, "PATCH", &one, crop).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["width"], 50);
    assert!(started.elapsed() >= SLOW, "the read was slow");
    // A write that touches no file keeps the normal limit, and is quick anyway.
    let res = call(&app, "PATCH", &one, json!({"focalX": 50})).await;
    assert_eq!(res.status, 200, "{}", res.json);
}
