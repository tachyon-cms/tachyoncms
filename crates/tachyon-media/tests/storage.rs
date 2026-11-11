use std::path::{Path, PathBuf};

use std::time::Duration;

use tachyon_media::storage::{Adapter, Storage, StorageConfig, object_key};
use uuid::Uuid;

/// A path in a fresh directory under the system's temp dir, removed on drop.
struct Temp {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl std::ops::Deref for Temp {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

/// A fresh directory under the system's temp dir.
fn temp_root() -> Temp {
    let dir = tempfile::Builder::new()
        .prefix("tachyon-media-")
        .tempdir()
        .unwrap();
    let path = dir.path().to_owned();
    Temp { _dir: dir, path }
}

/// A file holding `bytes`, to put.
fn source(bytes: &[u8]) -> Temp {
    let Temp { _dir, path } = temp_root();
    let path = path.join("source");
    std::fs::write(&path, bytes).unwrap();
    Temp { _dir, path }
}

/// Puts, reads part back, copies, heads and deletes; then the objects are gone.
async fn round_trip(storage: &Storage) {
    let id = Uuid::now_v7();
    let key = object_key(None, "images", id, "a.png").unwrap();
    let copy = object_key(None, "images", id, "a-10x10.png").unwrap();
    let from = source(b"0123456789");
    assert_eq!(
        storage.put_file(&key, &from, "image/png").await.unwrap(),
        10
    );
    let part = storage.get_range(&key, Some(2..5)).await.unwrap();
    assert_eq!(&part.bytes().await.unwrap()[..], b"234");
    let whole = storage.get_range(&key, None).await.unwrap();
    assert_eq!(&whole.bytes().await.unwrap()[..], b"0123456789");
    storage.copy(&key, &copy).await.unwrap();
    assert_eq!(storage.head(&copy).await.unwrap().size, 10);
    for k in [&key, &copy] {
        storage.delete(k).await.unwrap();
        assert!(storage.head(k).await.is_err(), "{k} deleted");
        storage
            .delete(k)
            .await
            .expect("deleting what's gone is fine");
    }
}

#[tokio::test]
async fn local_store_stays_under_root() {
    let id = Uuid::now_v7();
    for bad in ["../x", "a/b", "..", "a\0b", "a\\b"] {
        assert!(object_key(None, "images", id, bad).is_err(), "{bad:?}");
    }
    assert_eq!(
        object_key(None, "images", id, "a.png").unwrap(),
        format!("images/{id}/a.png")
    );

    let base = temp_root();
    let root = base.join("not-there-yet");
    let storage = Storage::open("default", &StorageConfig::local(&root)).unwrap();
    assert!(storage.is_local());
    assert!(root.is_dir(), "the root is created");
    round_trip(&storage).await;
    assert!(
        !root.join("images").join(id.to_string()).exists(),
        "a file's directory goes with its objects"
    );
    // Nothing outside the root can be reached through a key.
    let outside = root.parent().unwrap().join("outside");
    std::fs::write(&outside, b"x").unwrap();
    assert!(storage.delete("../outside").await.is_err());
    assert!(storage.head("a/../../outside").await.is_err());
    assert!(outside.exists());
}

#[tokio::test]
async fn a_static_dir_goes_before_the_collection() {
    let root = temp_root();
    let storage = Storage::open("default", &StorageConfig::local(&*root)).unwrap();
    let id = Uuid::now_v7();
    let key = object_key(Some("media/images"), "images", id, "a.png").unwrap();
    assert_eq!(key, format!("media/images/images/{id}/a.png"));
    storage
        .put_file(&key, &source(b"x"), "image/png")
        .await
        .unwrap();
    assert!(
        root.join("media/images/images")
            .join(id.to_string())
            .join("a.png")
            .is_file()
    );
    for bad in ["../x", "/x", "a//b", "x/", ""] {
        assert!(
            object_key(Some(bad), "images", id, "a.png").is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn a_bad_prefix_stops_startup() {
    for bad in ["", "/", "a//b", "../x", "x/"] {
        let root = temp_root();
        let mut c = StorageConfig::local(&*root);
        c.prefix = Some(bad.into());
        let err = Storage::open("disk", &c).unwrap_err();
        assert!(err.starts_with("storage.disk.prefix: "), "{bad:?}: {err}");
    }
}

#[tokio::test]
async fn local_storage_takes_no_client_uploads() {
    let root = temp_root();
    let mut c = StorageConfig::local(&*root);
    c.client_uploads = true;
    let err = Storage::open("disk", &c).unwrap_err();
    assert!(err.starts_with("storage.disk.client_uploads: "), "{err}");
    // Opened as it may be, it signs no uploads.
    let disk = Storage::open("disk", &StorageConfig::local(&*root)).unwrap();
    assert!(!disk.takes_client_uploads());
    let key = object_key(None, "images", Uuid::now_v7(), "a.png").unwrap();
    assert!(
        disk.presign_put(&key, "image/png", Duration::from_secs(60))
            .await
            .is_err()
    );
}

#[test]
fn public_urls_encode_each_segment() {
    let root = temp_root();
    let mut c = StorageConfig::local(&*root);
    c.public_url = Some("https://cdn.example.com/media/".parse().unwrap());
    let storage = Storage::open("default", &c).unwrap();
    assert_eq!(
        storage.public_url("docs/1/report #2 100%.pdf").unwrap(),
        "https://cdn.example.com/media/docs/1/report%20%232%20100%25.pdf"
    );
    assert_eq!(
        storage.public_url("docs/1/café.png").unwrap(),
        "https://cdn.example.com/media/docs/1/caf%C3%A9.png"
    );
}

#[tokio::test]
async fn a_prefix_goes_before_every_key() {
    let root = temp_root();
    let mut c = StorageConfig::local(&*root);
    c.prefix = Some("site-a".into());
    c.public_url = Some("https://files.example.com/".parse().unwrap());
    let storage = Storage::open("default", &c).unwrap();
    let key = object_key(None, "images", Uuid::now_v7(), "a.png").unwrap();
    storage
        .put_file(&key, &source(b"x"), "image/png")
        .await
        .unwrap();
    assert!(root.join("site-a").join(&key).is_file());
    assert_eq!(
        storage.public_url(&key).unwrap(),
        format!("https://files.example.com/site-a/{key}")
    );
}

/// Against MinIO: `TACHYON_TEST_S3_ENDPOINT=http://127.0.0.1:9000`, a bucket named by
/// `TACHYON_TEST_S3_BUCKET` (default `tachyon`), credentials in
/// `TACHYON_STORAGE__S3TEST__ACCESS_KEY_ID` and `..._SECRET_ACCESS_KEY`.
#[tokio::test]
#[ignore = "needs TACHYON_TEST_S3_ENDPOINT (MinIO)"]
async fn s3_round_trip() {
    let endpoint = std::env::var("TACHYON_TEST_S3_ENDPOINT").expect("TACHYON_TEST_S3_ENDPOINT");
    let c = StorageConfig {
        adapter: Adapter::S3 {
            bucket: std::env::var("TACHYON_TEST_S3_BUCKET").unwrap_or_else(|_| "tachyon".into()),
            endpoint: Some(endpoint),
            region: Some("us-east-1".into()),
        },
        prefix: Some("tests".into()),
        ..StorageConfig::local("unused")
    };
    let storage = Storage::open("s3test", &c).unwrap();
    assert!(!storage.is_local());
    round_trip(&storage).await;

    // A client's put through a signed URL, which holds it to the type signed.
    let key = object_key(None, "images", Uuid::now_v7(), "a.png").unwrap();
    let ten = Duration::from_secs(600);
    let put = storage.presign_put(&key, "image/png", ten).await.unwrap();
    let client = reqwest::Client::new();
    let wrong = client
        .put(put.clone())
        .header("content-type", "text/html")
        .body("<p>")
        .send()
        .await
        .unwrap();
    assert!(!wrong.status().is_success(), "another type is refused");
    let done = client
        .put(put)
        .header("content-type", "image/png")
        .body("0123456789")
        .send()
        .await
        .unwrap();
    assert!(done.status().is_success(), "{}", done.status());
    assert_eq!(storage.head(&key).await.unwrap().size, 10);
    storage.delete(&key).await.unwrap();
}
