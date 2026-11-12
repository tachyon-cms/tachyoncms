use std::path::{Path, PathBuf};

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tachyon_media::storage::{
    Adapter, STAGING, SignedDownloads, Storage, StorageConfig, object_key, staging_key,
};
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
fn a_prefix_cant_be_a_staging_area() {
    let root = temp_root();
    for bad in ["_staging", "cms/_staging"] {
        let mut c = StorageConfig::local(&*root);
        c.prefix = Some(bad.into());
        let err = Storage::open("disk", &c).unwrap_err();
        assert!(err.starts_with("storage.disk.prefix: "), "{bad:?}: {err}");
    }
    let mut c = StorageConfig::local(&*root);
    c.prefix = Some("_staging/cms".into());
    assert!(Storage::open("disk", &c).is_ok());
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
async fn local_storage_cant_sign() {
    let root = temp_root();
    let mut c = StorageConfig::local(&*root);
    c.signed_downloads = SignedDownloads::On {
        mime_types: Vec::new(),
        expires_in: Duration::from_secs(60),
    };
    let err = Storage::open("disk", &c).unwrap_err();
    assert!(err.starts_with("storage.disk.signed_downloads: "), "{err}");
    let mut c = StorageConfig::local(&*root);
    c.client_uploads = true;
    let err = Storage::open("disk", &c).unwrap_err();
    assert!(err.starts_with("storage.disk.client_uploads: "), "{err}");
    // Opened as it may be, it signs nothing and takes no client's files.
    let disk = Storage::open("disk", &StorageConfig::local(&*root)).unwrap();
    assert!(!disk.takes_client_uploads());
    assert!(disk.signs("image/png").is_none());
    let key = object_key(None, "images", Uuid::now_v7(), "a.png").unwrap();
    assert!(
        disk.presign_get(&key, Duration::from_secs(60))
            .await
            .is_err()
    );
    assert!(
        disk.presign_put(&key, "image/png", Duration::from_secs(60))
            .await
            .is_err()
    );
}

#[test]
fn signed_links_last_seven_days_at_most() {
    let bucket = |expires_in| StorageConfig {
        adapter: Adapter::S3 {
            bucket: "b".into(),
            endpoint: None,
            region: Some("us-east-1".into()),
        },
        signed_downloads: SignedDownloads::On {
            mime_types: vec!["application/pdf".into()],
            expires_in,
        },
        ..StorageConfig::local("unused")
    };
    let week = Duration::from_secs(7 * 24 * 3600);
    let s3 = Storage::open("s3", &bucket(week)).unwrap();
    assert_eq!(s3.signs("application/pdf"), Some(week));
    assert_eq!(s3.signs("image/png"), None);
    for bad in [week + Duration::from_secs(1), Duration::ZERO] {
        let err = Storage::open("s3", &bucket(bad)).unwrap_err();
        assert!(
            err.starts_with("storage.s3.signed_downloads.expires_in: "),
            "{err}"
        );
    }
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

    client_put_round_trip(&storage, true).await;
}

/// A client's put through a signed URL to a staging key, with the headers its instructions
/// carry; the sweeper's one-level listing of it; a signed read of it. With `type_signed`, a
/// put of another type is refused by the bucket too.
async fn client_put_round_trip(storage: &Storage, type_signed: bool) {
    let key = staging_key();
    let ten = Duration::from_secs(600);
    let put = storage.presign_put(&key, "image/png", ten).await.unwrap();
    let client = reqwest::Client::new();
    let send = |mime: &str, body: &'static str| {
        let mut req = client.put(put.clone());
        for (name, value) in storage.put_headers(mime) {
            req = req.header(name, value.as_str().unwrap());
        }
        req.body(body).send()
    };
    if type_signed {
        let wrong = send("text/html", "<p>").await.unwrap();
        assert!(!wrong.status().is_success(), "another type is refused");
    }
    let done = send("image/png", "0123456789").await.unwrap();
    assert!(done.status().is_success(), "{}", done.status());
    assert_eq!(storage.head(&key).await.unwrap().size, 10);

    // Only the staging level itself is a sweeper's: nothing nested under it.
    let nested = format!("{STAGING}/{}/a.png", Uuid::new_v4());
    storage
        .put_file(&nested, &source(b"x"), "image/png")
        .await
        .unwrap();
    let soon = std::time::SystemTime::now() + Duration::from_secs(60);
    let listed = storage.staging_before(soon).await.unwrap();
    assert!(listed.contains(&key), "{listed:?}");
    assert!(!listed.contains(&nested), "{listed:?}");
    let earlier = std::time::SystemTime::now() - Duration::from_secs(3600);
    assert!(
        !storage
            .staging_before(earlier)
            .await
            .unwrap()
            .contains(&key)
    );

    let get = storage.presign_get(&key, ten).await.unwrap();
    let got = client.get(get).send().await.unwrap();
    assert_eq!(&got.bytes().await.unwrap()[..], b"0123456789");
    for k in [&key, &nested] {
        storage.delete(k).await.unwrap();
        assert!(storage.head(k).await.unwrap_err().is_not_found(), "{k}");
    }
}

/// Opens `c` with `vars` as its credentials (`access_key` and so on), leaving the process
/// environment alone.
fn open(name: &str, c: &StorageConfig, vars: &[(&str, &str)]) -> Result<Storage, String> {
    Storage::open_with(name, c, |key| {
        vars.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_owned())
    })
}

fn azure(endpoint: Option<&str>) -> StorageConfig {
    StorageConfig {
        adapter: Adapter::Azure {
            container: "media".into(),
            account: "acct".into(),
            endpoint: endpoint.map(Into::into),
        },
        ..StorageConfig::local("unused")
    }
}

fn gcs(endpoint: Option<&str>) -> StorageConfig {
    StorageConfig {
        adapter: Adapter::Gcs {
            bucket: "media".into(),
            endpoint: endpoint.map(Into::into),
        },
        ..StorageConfig::local("unused")
    }
}

/// `c` with client uploads and signed downloads on.
fn signing(mut c: StorageConfig) -> StorageConfig {
    c.client_uploads = true;
    c.signed_downloads = SignedDownloads::On {
        mime_types: Vec::new(),
        expires_in: Duration::from_secs(60),
    };
    c
}

/// An Azure account key: any base64 will do, as nothing checks it offline.
const AZURE_KEY: &str = "dGFjaHlvbi10ZXN0LWtleQ==";

fn query(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

#[tokio::test]
async fn azure_instructions_carry_blob_type() {
    let az = open("az", &signing(azure(None)), &[("access_key", AZURE_KEY)]).unwrap();
    assert!(!az.is_local());
    assert!(az.takes_client_uploads());
    assert_eq!(
        serde_json::Value::Object(az.put_headers("image/png")),
        json!({"content-type": "image/png", "x-ms-blob-type": "BlockBlob"})
    );
    let key = staging_key();
    let put = az
        .presign_put(&key, "image/png", Duration::from_secs(900))
        .await
        .unwrap();
    assert_eq!(put.scheme(), "https");
    assert_eq!(put.host_str(), Some("acct.blob.core.windows.net"));
    assert_eq!(put.path(), format!("/media/{key}"));
    assert_eq!(query(&put, "sp").as_deref(), Some("w"));
    assert_eq!(query(&put, "sr").as_deref(), Some("b"));
    assert!(query(&put, "sig").is_some());
    let get = az.presign_get(&key, Duration::from_secs(60)).await.unwrap();
    assert_eq!(query(&get, "sp").as_deref(), Some("r"));
    assert_eq!(get.path(), format!("/media/{key}"));

    // Only Azure needs the blob type.
    let s3 = StorageConfig {
        adapter: Adapter::S3 {
            bucket: "b".into(),
            endpoint: None,
            region: Some("us-east-1".into()),
        },
        ..StorageConfig::local("unused")
    };
    let dir = temp_root();
    let key_file = emulator_key(&dir);
    let g = open("g", &gcs(None), &[("service_account_path", &key_file)]).unwrap();
    for other in [open("s3", &s3, &[]).unwrap(), g] {
        assert_eq!(
            serde_json::Value::Object(other.put_headers("image/png")),
            json!({"content-type": "image/png"})
        );
    }
}

#[tokio::test]
async fn azure_links_point_at_the_configured_endpoint() {
    let azurite = "http://127.0.0.1:10000/devstoreaccount1";
    let mut c = signing(azure(Some(azurite)));
    if let Adapter::Azure { account, .. } = &mut c.adapter {
        *account = "devstoreaccount1".into();
    }
    let az = open("az", &c, &[("access_key", AZURE_KEY)]).unwrap();
    let key = staging_key();
    let put = az
        .presign_put(&key, "image/png", Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(
        put.as_str().split('?').next().unwrap(),
        format!("{azurite}/media/{key}")
    );
    let sovereign = "https://acct.blob.core.chinacloudapi.cn";
    let az = open(
        "az",
        &signing(azure(Some(sovereign))),
        &[("access_key", AZURE_KEY)],
    )
    .unwrap();
    let get = az.presign_get(&key, Duration::from_secs(60)).await.unwrap();
    assert_eq!(
        get.as_str().split('?').next().unwrap(),
        format!("{sovereign}/media/{key}")
    );
}

#[test]
fn signing_needs_a_key_that_signs() {
    let uploads = |mut c: StorageConfig| {
        c.client_uploads = true;
        c
    };
    let downloads = |mut c: StorageConfig| {
        c.signed_downloads = SignedDownloads::On {
            mime_types: Vec::new(),
            expires_in: Duration::from_secs(60),
        };
        c
    };
    let refused = |c: &StorageConfig, name: &str, vars: &[(&str, &str)], key: &str, var: &str| {
        let err = open(name, c, vars).unwrap_err();
        assert!(err.starts_with(&format!("storage.{name}.{key}: ")), "{err}");
        assert!(err.contains(var), "{err}");
        assert!(
            !err.contains("not-a-secret"),
            "the secret isn't echoed: {err}"
        );
    };
    let sas = [("sas_token", "sv=2022-11-02&sig=not-a-secret")];
    for (set, key) in [
        (
            uploads as fn(StorageConfig) -> StorageConfig,
            "client_uploads",
        ),
        (downloads, "signed_downloads"),
    ] {
        // A SAS token can't make another, and the machine's own identity signs only
        // through a request to Azure: both need the account key.
        refused(
            &set(azure(None)),
            "az",
            &sas,
            key,
            "TACHYON_STORAGE__AZ__ACCESS_KEY",
        );
        refused(
            &set(azure(None)),
            "az",
            &[],
            key,
            "TACHYON_STORAGE__AZ__ACCESS_KEY",
        );
        // Google signs for the machine only through its IAM API: a key file is needed.
        refused(
            &set(gcs(None)),
            "g",
            &[],
            key,
            "TACHYON_STORAGE__G__SERVICE_ACCOUNT_PATH",
        );
    }
    // Without signing, each still opens.
    assert!(open("az", &azure(None), &sas).is_ok());
    assert!(open("az", &azure(None), &[]).is_ok());

    let both = open("az", &azure(None), &[("access_key", AZURE_KEY), sas[0]]).unwrap_err();
    assert_eq!(
        both,
        "storage.az: set one of TACHYON_STORAGE__AZ__ACCESS_KEY and TACHYON_STORAGE__AZ__SAS_TOKEN, not both"
    );
    // An emulator has no identity to fall back on.
    assert_eq!(
        open("az", &azure(Some("http://127.0.0.1:10000/acct")), &[]).unwrap_err(),
        "storage.az: set TACHYON_STORAGE__AZ__ACCESS_KEY or TACHYON_STORAGE__AZ__SAS_TOKEN in the environment; an account with an endpoint needs one"
    );
    assert_eq!(
        open("g", &gcs(Some("http://127.0.0.1:4443")), &[]).unwrap_err(),
        "storage.g: set TACHYON_STORAGE__G__SERVICE_ACCOUNT_PATH in the environment; a bucket with an endpoint needs its key file"
    );
    assert!(
        open("az", &azure(None), &[("access_key", "not base64!")])
            .unwrap_err()
            .starts_with("storage.az: "),
    );

    // A key file that turns OAuth off (an emulator's) has no key to sign with.
    let dir = temp_root();
    let key_file = emulator_key(&dir);
    let path = key_file.as_str();
    assert!(open("g", &gcs(None), &[("service_account_path", path)]).is_ok());
    refused(
        &uploads(gcs(None)),
        "g",
        &[("service_account_path", path)],
        "client_uploads",
        "disable_oauth",
    );
    let missing = open(
        "g",
        &gcs(None),
        &[("service_account_path", "/nowhere/key.json")],
    )
    .unwrap_err();
    assert!(missing.starts_with("storage.g: "), "{missing}");
}

/// A key file as an emulator takes one: OAuth off, so its key is never read. Without one
/// a Google storage reads the machine's own credentials, which no test can count on.
fn emulator_key(dir: &Path) -> String {
    let file = dir.join("emulator-key.json");
    let key = json!({"private_key": "unused", "private_key_id": "1", "client_email": "a@b", "disable_oauth": true});
    std::fs::write(&file, key.to_string()).unwrap();
    file.to_str().unwrap().to_owned()
}

/// A service account key file over a fresh RSA key, made by `openssl`.
fn service_account(dir: &Path) -> PathBuf {
    let out = std::process::Command::new("openssl")
        .args([
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
        ])
        .output()
        .expect("openssl makes the test's RSA key");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let file = dir.join("service-account.json");
    std::fs::write(
        &file,
        json!({
            "private_key": String::from_utf8(out.stdout).unwrap(),
            "private_key_id": "test",
            "client_email": "cms@tachyon-test.iam.gserviceaccount.com",
        })
        .to_string(),
    )
    .unwrap();
    file
}

#[tokio::test]
async fn gcs_signs_with_its_service_account() {
    let dir = temp_root();
    let file = service_account(&dir);
    let vars = [("service_account_path", file.to_str().unwrap())];
    let g = open("g", &signing(gcs(None)), &vars).unwrap();
    assert!(g.takes_client_uploads());
    let key = staging_key();
    let put = g
        .presign_put(&key, "image/png", Duration::from_secs(900))
        .await
        .unwrap();
    assert_eq!(
        put.as_str().split('?').next().unwrap(),
        format!("https://storage.googleapis.com/media/{key}")
    );
    assert_eq!(
        query(&put, "X-Goog-SignedHeaders").as_deref(),
        Some("content-type;host"),
        "the type is signed"
    );
    assert!(query(&put, "X-Goog-Signature").is_some());
    let get = g.presign_get(&key, Duration::from_secs(60)).await.unwrap();
    assert_eq!(query(&get, "X-Goog-SignedHeaders").as_deref(), Some("host"));

    let emulator = "http://127.0.0.1:4443";
    let g = open("g", &signing(gcs(Some(emulator))), &vars).unwrap();
    let get = g.presign_get(&key, Duration::from_secs(60)).await.unwrap();
    assert_eq!(
        get.as_str().split('?').next().unwrap(),
        format!("{emulator}/media/{key}")
    );
}

/// Against Azurite: `TACHYON_TEST_AZURITE=http://127.0.0.1:10000/devstoreaccount1`, a
/// container named by `TACHYON_TEST_AZURE_CONTAINER` (default `tachyon`), the account named
/// by `TACHYON_TEST_AZURE_ACCOUNT` (default `devstoreaccount1`), its key in
/// `TACHYON_STORAGE__AZTEST__ACCESS_KEY`.
#[tokio::test]
#[ignore = "needs TACHYON_TEST_AZURITE (Azurite)"]
async fn azure_round_trip() {
    let endpoint = std::env::var("TACHYON_TEST_AZURITE").expect("TACHYON_TEST_AZURITE");
    let var = |k: &str, default: &str| std::env::var(k).unwrap_or_else(|_| default.into());
    let c = StorageConfig {
        adapter: Adapter::Azure {
            container: var("TACHYON_TEST_AZURE_CONTAINER", "tachyon"),
            account: var("TACHYON_TEST_AZURE_ACCOUNT", "devstoreaccount1"),
            endpoint: Some(endpoint),
        },
        prefix: Some("tests".into()),
        ..signing(StorageConfig::local("unused"))
    };
    let storage = Storage::open("aztest", &c).unwrap();
    round_trip(&storage).await;
    // A SAS signs no headers, so the type isn't held to: the server's check after the
    // put is what holds it.
    client_put_round_trip(&storage, false).await;
}

/// Against fake-gcs-server: `TACHYON_TEST_GCS_ENDPOINT=http://127.0.0.1:4443`, a bucket
/// named by `TACHYON_TEST_GCS_BUCKET` (default `tachyon`), and a service account key file
/// (any RSA key) at `TACHYON_STORAGE__GCSTEST__SERVICE_ACCOUNT_PATH`.
#[tokio::test]
#[ignore = "needs TACHYON_TEST_GCS_ENDPOINT (fake-gcs-server)"]
async fn gcs_round_trip() {
    let endpoint = std::env::var("TACHYON_TEST_GCS_ENDPOINT").expect("TACHYON_TEST_GCS_ENDPOINT");
    let c = StorageConfig {
        adapter: Adapter::Gcs {
            bucket: std::env::var("TACHYON_TEST_GCS_BUCKET").unwrap_or_else(|_| "tachyon".into()),
            endpoint: Some(endpoint),
        },
        prefix: Some("tests".into()),
        ..signing(StorageConfig::local("unused"))
    };
    let storage = Storage::open("gcstest", &c).unwrap();
    round_trip(&storage).await;
    // The emulator checks no signature, so only a real bucket refuses another type.
    client_put_round_trip(&storage, false).await;
}

/// Signs as the machine's identity would when it's granted to (`true`) or not (`false`).
#[derive(Debug)]
struct Identity(bool);

#[async_trait::async_trait]
impl object_store::signer::Signer for Identity {
    async fn signed_url(
        &self,
        _: object_store::signer::Method,
        path: &object_store::path::Path,
        _: Duration,
    ) -> object_store::Result<url::Url> {
        match self.0 {
            true => Ok(format!("https://signed.test/{path}").parse().unwrap()),
            false => Err(object_store::Error::Generic {
                store: "test",
                source: "403 AuthorizationPermissionMismatch".into(),
            }),
        }
    }
}

fn with_identity(mut c: StorageConfig) -> StorageConfig {
    c.sign_with_identity = true;
    signing(c)
}

#[test]
fn identity_signing_is_opt_in() {
    for (c, name) in [(signing(azure(None)), "az"), (signing(gcs(None)), "g")] {
        let err = open(name, &c, &[]).unwrap_err();
        assert!(err.contains("sign_with_identity = true"), "{err}");
    }
    let az = open("az", &with_identity(azure(None)), &[]).unwrap();
    assert!(az.takes_client_uploads());
    assert!(az.signs("image/png").is_some());
    // Local and S3 storages have no such identity to sign with.
    let s3 = StorageConfig {
        adapter: Adapter::S3 {
            bucket: "b".into(),
            endpoint: None,
            region: Some("us-east-1".into()),
        },
        ..StorageConfig::local("unused")
    };
    let root = temp_root();
    for (c, name) in [(s3, "s3"), (StorageConfig::local(&*root), "disk")] {
        let mut c = c;
        c.sign_with_identity = true;
        let err = open(name, &c, &[]).unwrap_err();
        assert!(
            err.starts_with(&format!("storage.{name}.sign_with_identity: ")),
            "{err}"
        );
    }
}

#[tokio::test]
async fn identity_signing_is_checked_at_startup() {
    let opted = |c: StorageConfig, granted: bool| {
        Storage::with_store(
            "s",
            Arc::new(object_store::memory::InMemory::new()),
            None,
            with_identity(c),
        )
        .with_signer(Arc::new(Identity(granted)))
    };
    for (c, grant) in [
        (azure(None), "generateUserDelegationKey"),
        (gcs(None), "iam.serviceAccounts.signBlob"),
    ] {
        assert!(opted(c.clone(), true).check_signing().await.is_ok());
        let err = opted(c, false).check_signing().await.unwrap_err();
        assert!(err.starts_with("storage.s.sign_with_identity: "), "{err}");
        assert!(err.contains(grant), "{err}");
        assert!(err.contains("AuthorizationPermissionMismatch"), "{err}");
    }
    // Without the opt-in the signer signs locally, so nothing is tried at startup.
    let keyed = Storage::with_store(
        "s",
        Arc::new(object_store::memory::InMemory::new()),
        None,
        signing(azure(None)),
    )
    .with_signer(Arc::new(Identity(false)));
    assert!(keyed.check_signing().await.is_ok());
    // Nor when nothing here is signed.
    let mut quiet = azure(None);
    quiet.sign_with_identity = true;
    let quiet = Storage::with_store(
        "s",
        Arc::new(object_store::memory::InMemory::new()),
        None,
        quiet,
    )
    .with_signer(Arc::new(Identity(false)));
    assert!(quiet.check_signing().await.is_ok());

    let all = tachyon_media::storage::Storages(std::collections::HashMap::from([
        ("a".to_owned(), Arc::new(opted(azure(None), true))),
        ("g".to_owned(), Arc::new(opted(gcs(None), false))),
    ]));
    assert!(all.check_signing().await.unwrap_err().contains("signBlob"));
}

/// A store that can't copy, so a copy that works went another way.
#[derive(Debug)]
struct NoCopy(object_store::memory::InMemory);

impl std::fmt::Display for NoCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NoCopy")
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for NoCopy {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.0.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.0.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.0.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures_util::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>
    {
        self.0.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.0.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.0.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        _: &object_store::path::Path,
        _: &object_store::path::Path,
        _: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        Err(object_store::Error::NotImplemented {
            operation: "copy".into(),
            implementer: "NoCopy".into(),
        })
    }
}

#[tokio::test]
async fn azure_copies_by_streaming() {
    let store = Arc::new(NoCopy(object_store::memory::InMemory::new()));
    let az = Storage::with_store("az", store.clone(), None, azure(None));
    let id = Uuid::now_v7();
    let (from, to) = (
        object_key(None, "images", id, "a.png").unwrap(),
        object_key(None, "images", id, "a-1.png").unwrap(),
    );
    // Over a part, so the put goes in more than one.
    let bytes: Vec<u8> = (0..12_000_000u32).map(|i| (i % 251) as u8).collect();
    az.put_file(&from, &source(&bytes), "image/png")
        .await
        .unwrap();
    az.copy(&from, &to).await.unwrap();
    let got = az.get_range(&to, None).await.unwrap();
    assert_eq!(
        got.attributes
            .get(&object_store::Attribute::ContentType)
            .map(|v| v.as_ref()),
        Some("image/png"),
        "the type goes with it"
    );
    assert!(got.bytes().await.unwrap() == bytes, "every byte is copied");
    // Other buckets keep their own server-side copy.
    let s3 = StorageConfig {
        adapter: Adapter::S3 {
            bucket: "b".into(),
            endpoint: None,
            region: None,
        },
        ..StorageConfig::local("unused")
    };
    let s3 = Storage::with_store("s3", store, None, s3);
    assert!(s3.copy(&from, &to).await.is_err());
}
