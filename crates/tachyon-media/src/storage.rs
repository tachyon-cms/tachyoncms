//! Where files live: a local directory or a bucket, behind one interface.
//!
//! Every key is made here or by `object_key`, never taken from a request: a key that would
//! leave its storage (`..`, an empty segment, NUL) is refused before any store sees it.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use object_store::aws::AmazonS3Builder;
use object_store::buffered::BufWriter;
use object_store::local::LocalFileSystem;
use object_store::path::Path as Key;
use object_store::signer::Signer;
use object_store::{
    Attribute, Attributes, GetOptions, GetRange, GetResult, ObjectMeta, ObjectStore, ObjectStoreExt,
};
use tokio::io::AsyncWriteExt;
use url::Url;
use uuid::Uuid;

/// What kind of store a `[storage.<name>]` is, with what it needs to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adapter {
    Local {
        root: PathBuf,
    },
    S3 {
        bucket: String,
        endpoint: Option<String>,
        region: Option<String>,
    },
    Azure {
        container: String,
        account: String,
        endpoint: Option<String>,
    },
    Gcs {
        bucket: String,
    },
}

/// Whether files are served through signed bucket URLs: `false`, `true`, or
/// `{ mime_types, expires_in }` in the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignedDownloads {
    Off,
    /// `mime_types` empty signs every type.
    On {
        mime_types: Vec<String>,
        expires_in: Duration,
    },
}

/// One `[storage.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageConfig {
    pub adapter: Adapter,
    /// Put before every key, so several servers or sites can share a bucket.
    pub prefix: Option<String>,
    /// Where the bucket is reached by browsers, for direct file URLs.
    pub public_url: Option<Url>,
    pub client_uploads: bool,
    pub signed_downloads: SignedDownloads,
}

impl StorageConfig {
    /// A local directory with nothing else set.
    pub fn local(root: impl Into<PathBuf>) -> Self {
        Self {
            adapter: Adapter::Local { root: root.into() },
            prefix: None,
            public_url: None,
            client_uploads: false,
            signed_downloads: SignedDownloads::Off,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// A key or name that could leave its place in the store.
    #[error("{0}")]
    BadKey(String),
    #[error(transparent)]
    Store(#[from] object_store::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl StorageError {
    /// Whether the store has no object under the key.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Store(object_store::Error::NotFound { .. }))
    }
}

/// An open `[storage.<name>]`.
#[derive(Debug)]
pub struct Storage {
    name: String,
    store: Arc<dyn ObjectStore>,
    signer: Option<Arc<dyn Signer>>,
    config: StorageConfig,
}

/// The environment variable a storage's credential comes from, such as
/// `TACHYON_STORAGE__R2__ACCESS_KEY_ID` for `access_key_id` of `[storage.r2]`.
pub fn credential_var(storage: &str, key: &str) -> String {
    format!(
        "TACHYON_STORAGE__{}__{}",
        storage.to_ascii_uppercase(),
        key.to_ascii_uppercase()
    )
}

impl Storage {
    /// Opens storage `name`. A local root is created if missing and canonicalised. Bucket
    /// credentials come only from `TACHYON_STORAGE__<NAME>__ACCESS_KEY_ID`,
    /// `..._SECRET_ACCESS_KEY` and, for temporary ones, `..._SESSION_TOKEN`. Without them an
    /// AWS bucket falls back to the machine's own (an instance role); a bucket with an
    /// `endpoint` (R2, MinIO) has none, so there they're required.
    pub fn open(name: &str, c: &StorageConfig) -> Result<Storage, String> {
        if let Some(prefix) = &c.prefix {
            check_prefix(prefix).map_err(|e| format!("storage.{name}.prefix: {e}"))?;
        }
        let (store, signer): (Arc<dyn ObjectStore>, Option<Arc<dyn Signer>>) = match &c.adapter {
            Adapter::Local { root } => {
                std::fs::create_dir_all(root).map_err(|e| {
                    format!("storage.{name}.root: can't create {}: {e}", root.display())
                })?;
                let store = LocalFileSystem::new_with_prefix(root)
                    .map_err(|e| format!("storage.{name}.root: {e}"))?
                    // Each file's directory goes once its last object does.
                    .with_automatic_cleanup(true);
                (Arc::new(store), None)
            }
            Adapter::S3 {
                bucket,
                endpoint,
                region,
            } => {
                let mut b = AmazonS3Builder::new().with_bucket_name(bucket);
                if let Some(endpoint) = endpoint {
                    // MinIO and similar often run on plain http inside a private network.
                    b = b
                        .with_endpoint(endpoint)
                        .with_allow_http(endpoint.starts_with("http://"));
                }
                if let Some(region) = region {
                    b = b.with_region(region);
                }
                let var = |key: &str| std::env::var(credential_var(name, key)).ok();
                let (id, secret) = (var("access_key_id"), var("secret_access_key"));
                // Only AWS itself has an instance role to fall back on; elsewhere a missing
                // key would surface as a metadata-service timeout on every request.
                if endpoint.is_some() && (id.is_none() || secret.is_none()) {
                    return Err(format!(
                        "storage.{name}: set {} and {} in the environment; a bucket with an endpoint needs its keys",
                        credential_var(name, "access_key_id"),
                        credential_var(name, "secret_access_key"),
                    ));
                }
                if let Some(id) = id {
                    b = b.with_access_key_id(id);
                }
                if let Some(secret) = secret {
                    b = b.with_secret_access_key(secret);
                }
                if let Some(token) = var("session_token") {
                    b = b.with_token(token);
                }
                let s3 = Arc::new(b.build().map_err(|e| format!("storage.{name}: {e}"))?);
                (s3.clone(), Some(s3))
            }
            Adapter::Azure { .. } | Adapter::Gcs { .. } => {
                return Err(format!(
                    "storage.{name}: this build has no Azure or Google Cloud Storage support"
                ));
            }
        };
        Ok(Self::with_store(name, store, signer, c.clone()))
    }

    /// Storage over a store opened elsewhere, such as one a test wraps.
    pub fn with_store(
        name: &str,
        store: Arc<dyn ObjectStore>,
        signer: Option<Arc<dyn Signer>>,
        config: StorageConfig,
    ) -> Storage {
        Storage {
            name: name.to_owned(),
            store,
            signer,
            config,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn config(&self) -> &StorageConfig {
        &self.config
    }

    /// Signs bucket URLs; `None` for local storage.
    pub fn signer(&self) -> Option<&Arc<dyn Signer>> {
        self.signer.as_ref()
    }

    pub fn is_local(&self) -> bool {
        matches!(self.config.adapter, Adapter::Local { .. })
    }

    /// `key` under this storage's prefix, refused if it could leave it.
    fn key(&self, key: &str) -> Result<Key, StorageError> {
        let full = match &self.config.prefix {
            Some(p) => format!("{p}/{key}"),
            None => key.to_owned(),
        };
        checked(&full)
    }

    /// Streams the file at `from` to `key`, in parts when it's big. Returns its size.
    pub async fn put_file(&self, key: &str, from: &Path, mime: &str) -> Result<u64, StorageError> {
        let key = self.key(key)?;
        let mut file = tokio::fs::File::open(from).await?;
        let mut out = BufWriter::new(self.store.clone(), key);
        // The local store can't keep attributes; the type is known from the files table.
        if !self.is_local() {
            let mut a = Attributes::new();
            a.insert(Attribute::ContentType, mime.to_owned().into());
            out = out.with_attributes(a);
        }
        match tokio::io::copy(&mut file, &mut out).await {
            // A writer that began shutting down can't be aborted (`abort` panics then), so
            // parts a failed completion leaves are the bucket's lifecycle rule's to expire.
            Ok(n) => out.shutdown().await.map(|()| n).map_err(Into::into),
            Err(e) => {
                // Leaves no half-made multipart upload behind; the copy's error matters more.
                let _ = out.abort().await;
                Err(e.into())
            }
        }
    }

    /// `key`, or the bytes of it in `range`.
    pub async fn get_range(
        &self,
        key: &str,
        range: Option<Range<u64>>,
    ) -> Result<GetResult, StorageError> {
        let options = GetOptions::new().with_range(range.map(GetRange::Bounded));
        Ok(self.store.get_opts(&self.key(key)?, options).await?)
    }

    pub async fn head(&self, key: &str) -> Result<ObjectMeta, StorageError> {
        Ok(self.store.head(&self.key(key)?).await?)
    }

    pub async fn copy(&self, from: &str, to: &str) -> Result<(), StorageError> {
        Ok(self.store.copy(&self.key(from)?, &self.key(to)?).await?)
    }

    /// Deletes `key`. One that isn't there is already deleted.
    pub async fn delete(&self, key: &str) -> Result<(), StorageError> {
        match self.store.delete(&self.key(key)?).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Where a browser can fetch `key` straight from the bucket, when `public_url` is set.
    pub fn public_url(&self, key: &str) -> Option<String> {
        let mut url = self.config.public_url.clone()?;
        let key = self.key(key).ok()?;
        // Each segment percent-encoded, so `#`, `%` or a space in a name stays in the path.
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .extend(key.as_ref().split('/'));
        Some(url.into())
    }
}

/// Whether every `/`-separated segment of `path` is a plain name: none empty, `.` or
/// `..`, and no NUL or backslash anywhere. Keys, a storage's `prefix` and a collection's
/// `staticDir` all follow it.
fn plain_segments(path: &str) -> bool {
    !path.contains(['\0', '\\'])
        && path
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
}

/// A key prefix (a storage's `prefix`, a collection's `staticDir`): plain segments, such as
/// `media/images`.
pub fn check_prefix(prefix: &str) -> Result<(), String> {
    if plain_segments(prefix) && Key::parse(prefix).is_ok_and(|k| k.as_ref() == prefix) {
        Ok(())
    } else {
        Err(format!(
            "`{prefix}` has to be a relative path of plain names, like media/images: no empty, `.` or `..` parts, no leading or trailing /, and no backslash"
        ))
    }
}

/// `key` as a store path, refused unless every segment is a plain name.
fn checked(key: &str) -> Result<Key, StorageError> {
    let bad = || StorageError::BadKey(format!("`{key}` isn't a safe storage key"));
    if !plain_segments(key) {
        return Err(bad());
    }
    let parsed = Key::parse(key).map_err(|_| bad())?;
    // Parsing may normalise; a key that changed isn't the one asked for.
    if parsed.as_ref() != key {
        return Err(bad());
    }
    Ok(parsed)
}

/// The key of an uploaded original: `{staticDir}/{collection}/{file id}/{filename}`, without
/// the first part when the collection sets no `staticDir`. Sizes go under the same prefix.
/// A name or directory that could climb out of it is refused.
pub fn object_key(
    static_dir: Option<&str>,
    collection: &str,
    file: Uuid,
    filename: &str,
) -> Result<String, StorageError> {
    if filename.contains(['/', '\\', '\0']) || filename == ".." || filename == "." {
        return Err(StorageError::BadKey(format!(
            "`{filename}` isn't a safe file name"
        )));
    }
    let key = match static_dir {
        Some(dir) => format!("{dir}/{collection}/{file}/{filename}"),
        None => format!("{collection}/{file}/{filename}"),
    };
    checked(&key)?;
    Ok(key)
}

/// Every open storage by name. There's always a `default`.
#[derive(Debug)]
pub struct Storages(pub HashMap<String, Arc<Storage>>);

/// Where `default` is when the config names none.
pub const DEFAULT_ROOT: &str = "./uploads";

impl Storages {
    /// Opens each of `configs`, adding `default` (local, `./uploads`) unless it's there.
    pub fn open(configs: &BTreeMap<String, StorageConfig>) -> Result<Self, String> {
        let mut all = HashMap::new();
        for (name, c) in configs {
            all.insert(name.clone(), Arc::new(Storage::open(name, c)?));
        }
        if !all.contains_key("default") {
            let c = StorageConfig::local(DEFAULT_ROOT);
            all.insert("default".into(), Arc::new(Storage::open("default", &c)?));
        }
        Ok(Self(all))
    }

    pub fn get(&self, name: &str) -> Option<&Arc<Storage>> {
        self.0.get(name)
    }
}

impl Default for Storages {
    /// Only `default`, in `./uploads`, created if missing.
    fn default() -> Self {
        Self::open(&BTreeMap::new()).expect("./uploads can be created")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_plain_segments() {
        assert!(checked("a/b/c.png").is_ok());
        assert!(check_prefix("media/images").is_ok());
        for bad in ["", ".", "a//b", "./x", "x/", "/x", "a\\b", "../x", "a\0"] {
            assert!(check_prefix(bad).is_err(), "{bad:?}");
        }
        for bad in ["../x", "a/../b", "a//b", "/a", "a/", "a\0b", "a\\b", "./a"] {
            assert!(checked(bad).is_err(), "{bad:?}");
        }
    }
}
