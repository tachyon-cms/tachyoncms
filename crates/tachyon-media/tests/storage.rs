use std::path::PathBuf;

use tachyon_media::storage::{Adapter, Storage, StorageConfig, object_key};
use uuid::Uuid;

/// A fresh directory under the system's temp dir.
fn temp_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tachyon-media-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A file holding `bytes`, to put.
fn source(bytes: &[u8]) -> PathBuf {
    let path = temp_root().join("source");
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Puts, reads part back, copies, heads and deletes; then the objects are gone.
async fn round_trip(storage: &Storage) {
    let id = Uuid::now_v7();
    let key = object_key("images", id, "a.png").unwrap();
    let copy = object_key("images", id, "a-10x10.png").unwrap();
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
        assert!(object_key("images", id, bad).is_err(), "{bad:?}");
    }
    assert_eq!(
        object_key("images", id, "a.png").unwrap(),
        format!("images/{id}/a.png")
    );

    let root = temp_root().join("not-there-yet");
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
async fn a_prefix_goes_before_every_key() {
    let root = temp_root();
    let mut c = StorageConfig::local(&root);
    c.prefix = Some("site-a".into());
    c.public_url = Some("https://files.example.com/".parse().unwrap());
    let storage = Storage::open("default", &c).unwrap();
    let key = object_key("images", Uuid::now_v7(), "a.png").unwrap();
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
}
