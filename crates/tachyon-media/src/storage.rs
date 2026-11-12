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
use object_store::azure::{AzureConfigKey, MicrosoftAzureBuilder};
use object_store::buffered::BufWriter;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as Key;
use object_store::signer::{HeaderValue, Method, SignedUrlOptions, Signer};
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
        /// Another base URL than Google's, for an emulator.
        endpoint: Option<String>,
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
    /// Signs through the machine's own Azure or Google identity, a request to the cloud
    /// that's tried once at startup (`Storage::check_signing`).
    pub sign_with_identity: bool,
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
            sign_with_identity: false,
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
    /// This storage can't make signed URLs: it's local, or its credentials can't sign.
    #[error("storage `{0}` can't sign URLs")]
    Unsigned(String),
}

impl StorageError {
    /// Whether the store has no object under the key.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Store(object_store::Error::NotFound { .. }))
    }
}

/// An open `[storage.<name>]`.
pub struct Storage {
    name: String,
    store: Arc<dyn ObjectStore>,
    signer: Option<Arc<dyn Signer>>,
    config: StorageConfig,
    /// For the requests the store makes no call for (an Azure staging key's delete).
    http: reqwest::Client,
}

/// The store and signer hold the credentials, and some print them: only what's set here.
impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage")
            .field("name", &self.name)
            .field("config", &self.config)
            .field("signs", &self.signer.is_some())
            .finish_non_exhaustive()
    }
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
    /// credentials come only from the environment, `TACHYON_STORAGE__<NAME>__` followed by:
    /// - S3: `ACCESS_KEY_ID`, `SECRET_ACCESS_KEY` and, for temporary ones, `SESSION_TOKEN`;
    /// - Azure: `ACCESS_KEY` (the account key) or `SAS_TOKEN`;
    /// - Google: `SERVICE_ACCOUNT_PATH`, a key file.
    ///
    /// Without them the machine's own are used (an instance role, a managed identity). A
    /// store with an `endpoint` (R2, MinIO, an emulator) has none, so there they're required.
    /// Signing (`client_uploads`, `signed_downloads`) needs credentials that sign here, with
    /// no request to the cloud: S3 keys or a role, an Azure account key, a Google key file.
    pub fn open(name: &str, c: &StorageConfig) -> Result<Storage, String> {
        Self::open_with(name, c, |var| std::env::var(var).ok())
    }

    /// `open`, with `var` reading each environment variable it needs (a credential such as
    /// `TACHYON_STORAGE__AZ__ACCESS_KEY`, or an Azure identity's `AZURE_CLIENT_ID`) in place
    /// of the process's environment: how a test opens a bucket without touching it. No
    /// setting reaches it.
    #[doc(hidden)]
    pub fn open_with(
        name: &str,
        c: &StorageConfig,
        var: impl Fn(&str) -> Option<String>,
    ) -> Result<Storage, String> {
        let cred = |key: &str| var(&credential_var(name, key));
        if let Some(prefix) = &c.prefix {
            check_prefix(prefix).map_err(|e| format!("storage.{name}.prefix: {e}"))?;
            // Another storage on the bucket would sweep this one's files as strays.
            if prefix.rsplit('/').next() == Some(STAGING) {
                return Err(format!(
                    "storage.{name}.prefix: `{prefix}` ends in `{STAGING}`, where client uploads wait to be checked; pick another name"
                ));
            }
        }
        // Whether the credentials sign, and why not: an `Err` refuses `client_uploads` and
        // `signed_downloads` at startup rather than failing each request. Checked in each
        // arm before its build, which may read the machine's own credentials.
        let (store, signer): (Arc<dyn ObjectStore>, Option<Arc<dyn Signer>>) = match &c.adapter {
            Adapter::Local { root } => {
                no_identity(name, c)?;
                check_signing_config(name, c, Err("local storage can't sign links; use a bucket"))?;
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
                no_identity(name, c)?;
                check_signing_config(name, c, Ok(()))?;
                let mut b = AmazonS3Builder::new().with_bucket_name(bucket);
                if let Some(raw) = endpoint {
                    // MinIO and similar often run on plain http inside a private network.
                    let url = endpoint_url(name, raw)?;
                    b = b
                        .with_allow_http(url.scheme() == "http")
                        .with_endpoint(trimmed(&url));
                }
                if let Some(region) = region {
                    b = b.with_region(region);
                }
                let (id, secret) = (cred("access_key_id"), cred("secret_access_key"));
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
                if let Some(token) = cred("session_token") {
                    b = b.with_token(token);
                }
                let s3 = Arc::new(b.build().map_err(|e| format!("storage.{name}: {e}"))?);
                (s3.clone(), Some(s3))
            }
            Adapter::Azure {
                container,
                account,
                endpoint,
            } => {
                if account.len() < 3
                    || account.len() > 24
                    || !account
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                {
                    return Err(format!(
                        "storage.{name}.account: an Azure storage account's name is 3 to 24 lowercase letters and digits"
                    ));
                }
                let mut b = MicrosoftAzureBuilder::new()
                    .with_account(account)
                    .with_container_name(container);
                let endpoint = endpoint
                    .as_deref()
                    .map(|raw| endpoint_url(name, raw))
                    .transpose()?;
                if let Some(url) = &endpoint {
                    // Azure signs a resource named after its host's account (the emulator's is
                    // in its path), so on another https host every signed link would be refused.
                    let own = url.host_str().is_some_and(|h| h.contains(account.as_str()));
                    if url.scheme() == "https" && !own && signs_anything(c) {
                        return Err(format!(
                            "storage.{name}.endpoint: signed links need the account's own host, like https://{account}.blob.core.windows.net, or an emulator's http endpoint"
                        ));
                    }
                    // Azurite, the emulator, serves plain http.
                    b = b
                        .with_endpoint(trimmed(url))
                        .with_allow_http(url.scheme() == "http");
                }
                let (key_var, sas_var) = (
                    credential_var(name, "access_key"),
                    credential_var(name, "sas_token"),
                );
                let signs = match (cred("access_key"), cred("sas_token")) {
                    (Some(_), Some(_)) => {
                        return Err(format!(
                            "storage.{name}: set one of {key_var} and {sas_var}, not both"
                        ));
                    }
                    (Some(key), None) => {
                        b = b.with_access_key(key);
                        Ok(())
                    }
                    (None, Some(sas)) => {
                        b = b.with_config(AzureConfigKey::SasKey, sas);
                        Err(format!(
                            "a SAS token can't sign links of its own; set the account key in {key_var} instead"
                        ))
                    }
                    (None, None) if endpoint.is_some() => {
                        return Err(format!(
                            "storage.{name}: set {key_var} or {sas_var} in the environment; an account with an endpoint needs one"
                        ));
                    }
                    // Without a key the machine's own identity is used, which signs only through
                    // a request to Azure: allowed when asked for, and tried at startup.
                    (None, None) => {
                        b = azure_identity(b, &var);
                        match c.sign_with_identity {
                            true => Ok(()),
                            false => Err(format!(
                                "the machine's own Azure identity signs links only through a request to Azure; set the account key in {key_var}, or sign_with_identity = true to sign that way (its role needs generateUserDelegationKey)"
                            )),
                        }
                    }
                };
                check_signing_config(name, c, signs.as_ref().map(|_| ()).map_err(String::as_str))?;
                let az = Arc::new(b.build().map_err(|e| format!("storage.{name}: {e}"))?);
                (az.clone(), signs.ok().map(|()| az as Arc<dyn Signer>))
            }
            Adapter::Gcs { bucket, endpoint } => {
                let endpoint = endpoint
                    .as_deref()
                    .map(|raw| endpoint_url(name, raw))
                    .transpose()?;
                // Google signs for its own host only, whatever host the link points at.
                if let Some(url) = &endpoint
                    && url.scheme() == "https"
                    && url.host_str() != Some("storage.googleapis.com")
                    && signs_anything(c)
                {
                    return Err(format!(
                        "storage.{name}.endpoint: signed links need Google's own host, https://storage.googleapis.com, or an emulator's http endpoint"
                    ));
                }
                // Always set, so a key file's own `gcs_base_url` can't send requests elsewhere.
                let base = endpoint
                    .as_ref()
                    .map_or_else(|| GCS_BASE.to_owned(), trimmed);
                let mut b = GoogleCloudStorageBuilder::new()
                    .with_bucket_name(bucket)
                    .with_base_url(&base);
                if let Some(url) = &endpoint {
                    // fake-gcs-server, the emulator, serves plain http.
                    b = b.with_client_options(
                        object_store::ClientOptions::new().with_allow_http(url.scheme() == "http"),
                    );
                }
                let path_var = credential_var(name, "service_account_path");
                let signs = match cred("service_account_path") {
                    Some(path) => {
                        let key = gcs_key_file(&path)
                            .map_err(|e| format!("storage.{name}: {path_var}: {e}"))?;
                        if key
                            .base_url
                            .is_some_and(|u| u.trim_end_matches('/') != base)
                        {
                            return Err(format!(
                                "storage.{name}: the key file in {path_var} names a gcs_base_url other than this storage's endpoint; set endpoint to it, or take it out of the file"
                            ));
                        }
                        if key.disable_oauth && endpoint.is_none() {
                            return Err(format!(
                                "storage.{name}: the key file in {path_var} sets disable_oauth, which is for an emulator; set endpoint to the emulator's address"
                            ));
                        }
                        b = b.with_service_account_path(path);
                        match key.disable_oauth {
                            false => Ok(()),
                            true => Err(format!(
                                "the key file in {path_var} sets disable_oauth, so it has no key to sign with"
                            )),
                        }
                    }
                    None if endpoint.is_some() => {
                        return Err(format!(
                            "storage.{name}: set {path_var} in the environment; a bucket with an endpoint needs its key file"
                        ));
                    }
                    // The machine's own credentials may sign only through Google's IAM API, and
                    // only when granted that too: allowed when asked for, and tried at startup.
                    None if c.sign_with_identity => Ok(()),
                    None => Err(format!(
                        "the machine's own Google credentials may sign links only through Google's IAM API; set a service account key file's path in {path_var}, or sign_with_identity = true to sign that way (it needs iam.serviceAccounts.signBlob)"
                    )),
                };
                check_signing_config(name, c, signs.as_ref().map(|_| ()).map_err(String::as_str))?;
                let gcs = Arc::new(b.build().map_err(|e| format!("storage.{name}: {e}"))?);
                (gcs.clone(), signs.ok().map(|()| gcs as Arc<dyn Signer>))
            }
        };
        Ok(Self::with_store(name, store, signer, c.clone()))
    }

    /// `self` signing URLs with `s`: how a test stands a local store in for a bucket. No
    /// setting reaches it.
    #[doc(hidden)]
    pub fn with_signer(mut self, s: Arc<dyn Signer>) -> Storage {
        self.signer = Some(s);
        self
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
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .expect("an HTTP client without a proxy or TLS options builds"),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn config(&self) -> &StorageConfig {
        &self.config
    }

    /// Signs bucket URLs; `None` for local storage and credentials that can't sign.
    pub fn signer(&self) -> Option<&Arc<dyn Signer>> {
        self.signer.as_ref()
    }

    pub fn is_local(&self) -> bool {
        matches!(self.config.adapter, Adapter::Local { .. })
    }

    /// Whether a client may put files here itself (`client_uploads`, and a store that
    /// signs).
    pub fn takes_client_uploads(&self) -> bool {
        self.config.client_uploads && self.signer.is_some()
    }

    /// How long a signed link to a file of type `mime` lasts, when this storage serves
    /// that type through signed links (`signed_downloads`).
    pub fn signs(&self, mime: &str) -> Option<Duration> {
        match &self.config.signed_downloads {
            SignedDownloads::On {
                mime_types,
                expires_in,
            } if self.signer.is_some() && tachyon_core::uploads::mime_allowed(mime_types, mime) => {
                Some(*expires_in)
            }
            _ => None,
        }
    }

    /// Whether any file here is served through signed links, so none may have a public
    /// bucket URL.
    pub fn signs_any(&self) -> bool {
        matches!(self.config.signed_downloads, SignedDownloads::On { .. })
    }

    /// With `sign_with_identity`, signs one link through the machine's identity, as startup
    /// does before serving: a grant that's missing stops the server there, not each request.
    pub async fn check_signing(&self) -> Result<(), String> {
        if !self.config.sign_with_identity || !(self.config.client_uploads || self.signs_any()) {
            return Ok(());
        }
        let grant = match self.config.adapter {
            Adapter::Azure { .. } => {
                "Microsoft.Storage/storageAccounts/blobServices/generateUserDelegationKey/action on the storage account, as the Storage Blob Delegator role does"
            }
            Adapter::Gcs { .. } => {
                "iam.serviceAccounts.signBlob on itself, as the Service Account Token Creator role does"
            }
            // No identity of theirs signs; `open` refuses the setting for them.
            Adapter::Local { .. } | Adapter::S3 { .. } => return Ok(()),
        };
        let probe = format!("{STAGING}/signing-check");
        match self.presign_get(&probe, Duration::from_secs(60)).await {
            Ok(_) => Ok(()),
            Err(e) => Err(format!(
                "storage.{}.sign_with_identity: the machine's identity couldn't sign a link ({e}); grant it {grant}",
                self.name
            )),
        }
    }

    fn signer_or_refuse(&self) -> Result<&Arc<dyn Signer>, StorageError> {
        self.signer
            .as_ref()
            .ok_or_else(|| StorageError::Unsigned(self.name.clone()))
    }

    /// A URL a client `PUT`s a file of type `mime` to at `key`, for `expires`. The type is
    /// signed: a `PUT` with another `content-type` is refused by the bucket.
    pub async fn presign_put(
        &self,
        key: &str,
        mime: &str,
        expires: Duration,
    ) -> Result<Url, StorageError> {
        let signer = self.signer_or_refuse()?;
        let mime = HeaderValue::from_str(mime)
            .map_err(|_| StorageError::BadKey(format!("`{mime}` isn't a content type")))?;
        // A SAS signs no headers, so Azure can't hold the client to the type; the server's
        // check of the bytes after the put is what does.
        if matches!(self.config.adapter, Adapter::Azure { .. }) {
            return Ok(signer
                .signed_url(Method::PUT, &self.key(key)?, expires)
                .await?);
        }
        let options = SignedUrlOptions::default().with_signed_header(
            object_store::signer::HeaderName::from_static("content-type"),
            mime,
        );
        Ok(signer
            .signed_url_opts(Method::PUT, &self.key(key)?, expires, &options)
            .await?)
    }

    /// A URL that fetches `key` for `expires`.
    pub async fn presign_get(&self, key: &str, expires: Duration) -> Result<Url, StorageError> {
        let signer = self.signer_or_refuse()?;
        Ok(signer
            .signed_url(Method::GET, &self.key(key)?, expires)
            .await?)
    }

    /// The headers a client sends with its `PUT` to a `presign_put` URL for type `mime`.
    pub fn put_headers(&self, mime: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut headers = serde_json::Map::new();
        headers.insert("content-type".into(), mime.into());
        // Azure's Put Blob refuses a request that doesn't say which kind of blob it makes.
        if matches!(self.config.adapter, Adapter::Azure { .. }) {
            headers.insert("x-ms-blob-type".into(), "BlockBlob".into());
        }
        headers
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
        let (from, to) = (self.key(from)?, self.key(to)?);
        // Azure's Copy Blob may answer before the copy is done, and nothing waits on it.
        // Streamed through here instead, the copy is whole when this returns.
        if matches!(self.config.adapter, Adapter::Azure { .. }) {
            return self.copy_streamed(&from, to).await;
        }
        Ok(self.store.copy(&from, &to).await?)
    }

    /// Reads `from` and writes it to `to` with its attributes, held a part at a time, as
    /// `put_file` holds a file.
    async fn copy_streamed(&self, from: &Key, to: Key) -> Result<(), StorageError> {
        use futures_util::TryStreamExt;
        let got = self.store.get(from).await?;
        let mut out =
            BufWriter::new(self.store.clone(), to).with_attributes(got.attributes.clone());
        let mut chunks = got.into_stream();
        let copied = async {
            while let Some(chunk) = chunks.try_next().await? {
                out.put(chunk).await?;
            }
            Ok::<_, object_store::Error>(())
        }
        .await;
        match copied {
            Ok(()) => out.shutdown().await.map_err(Into::into),
            Err(e) => {
                let _ = out.abort().await;
                Err(e.into())
            }
        }
    }

    /// The staging keys (`staging_key`'s shape, nothing else) in this storage's staging
    /// area, last written before `before`. One level only: whatever is nested under it isn't
    /// a staging key, so isn't for a sweep.
    pub async fn staging_before(
        &self,
        before: std::time::SystemTime,
    ) -> Result<Vec<String>, StorageError> {
        let listed = self
            .store
            .list_with_delimiter(Some(&self.key(STAGING)?))
            .await?;
        let strip = self.config.prefix.as_ref().map(|p| format!("{p}/"));
        let before = epoch_secs(before);
        Ok(listed
            .objects
            .into_iter()
            .filter(|m| m.last_modified.timestamp() < before)
            .map(|m| {
                let key = m.location.as_ref();
                strip
                    .as_deref()
                    .and_then(|p| key.strip_prefix(p))
                    .unwrap_or(key)
                    .to_owned()
            })
            .filter(|k| is_staging_key(k))
            .collect::<Vec<_>>())
        .map(|mut keys| {
            // In one order whatever the store lists in, as buckets list.
            keys.sort_unstable();
            keys
        })
    }

    /// Deletes staging key `key`, however a client left it. On Azure the upload link's write
    /// permission also lets a client lease or snapshot the blob, either of which a plain
    /// delete is refused for: so any lease is broken first, and snapshots go with it.
    pub async fn delete_staging(&self, key: &str) -> Result<(), StorageError> {
        if !matches!(self.config.adapter, Adapter::Azure { .. }) || self.signer.is_none() {
            return self.delete(key).await;
        }
        let failed = |source: String| {
            StorageError::Store(object_store::Error::Generic {
                store: "MicrosoftAzure",
                source: source.into(),
            })
        };
        let [lease, delete] = self.staging_delete_requests(key).await?;
        for (request, step) in [(lease, "break the lease on"), (delete, "delete")] {
            let res = self
                .http
                .execute(request)
                .await
                .map_err(|e| failed(format!("can't {step} `{key}`: {}", e.without_url())))?;
            match res.status().as_u16() {
                200..=299 => {}
                404 => return Ok(()),
                // A blob with no lease has none to break.
                409 if step.starts_with("break") => {}
                status => {
                    return Err(failed(format!(
                        "can't {step} `{key}`: Azure answered {status}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// The two requests `delete_staging` sends on Azure, each signed for `key` alone and
    /// for a minute: a lease break taking effect at once, then a delete with snapshots.
    #[doc(hidden)]
    pub async fn staging_delete_requests(
        &self,
        key: &str,
    ) -> Result<[reqwest::Request; 2], StorageError> {
        if !matches!(self.config.adapter, Adapter::Azure { .. }) {
            return Err(StorageError::BadKey(format!(
                "storage `{}` isn't Azure's; its staging keys delete as any key does",
                self.name
            )));
        }
        let signer = self.signer_or_refuse()?;
        let path = self.key(key)?;
        let minute = Duration::from_secs(60);
        // A SAS's `w` covers Lease Blob; the operation itself goes outside the signature.
        let mut lease = signer.signed_url(Method::PUT, &path, minute).await?;
        lease.query_pairs_mut().append_pair("comp", "lease");
        let delete = signer.signed_url(Method::DELETE, &path, minute).await?;
        let build = |r: reqwest::RequestBuilder| {
            r.build()
                .map_err(|e| StorageError::BadKey(format!("can't build a request: {e}")))
        };
        Ok([
            build(
                self.http
                    .put(lease)
                    .header("x-ms-lease-action", "break")
                    .header("x-ms-lease-break-period", "0")
                    .header("content-length", "0"),
            )?,
            build(
                self.http
                    .delete(delete)
                    .header("x-ms-delete-snapshots", "include"),
            )?,
        ])
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

/// The longest a signed link may last: what S3 itself allows.
pub const MAX_SIGNED: Duration = Duration::from_secs(7 * 24 * 3600);

/// Signing needs credentials that sign: a storage told to sign without them is a mistake
/// to catch at startup (`signs` says why not), as is a link meant to outlast what a bucket
/// signs, or what an Azure identity signs without a call to Azure each time.
fn check_signing_config(
    name: &str,
    c: &StorageConfig,
    signs: Result<(), &str>,
) -> Result<(), String> {
    if let Err(why) = signs {
        if c.client_uploads {
            return Err(format!("storage.{name}.client_uploads: {why}"));
        }
        if matches!(c.signed_downloads, SignedDownloads::On { .. }) {
            return Err(format!("storage.{name}.signed_downloads: {why}"));
        }
    }
    if let SignedDownloads::On { expires_in, .. } = &c.signed_downloads {
        if *expires_in > MAX_SIGNED || expires_in.is_zero() {
            return Err(format!(
                "storage.{name}.signed_downloads.expires_in: a link lasts between a second and 7 days"
            ));
        }
        // object_store keeps an Azure identity's delegation key only for links this short;
        // past it, every link would be a call to Azure, which Azure throttles.
        if c.sign_with_identity
            && matches!(c.adapter, Adapter::Azure { .. })
            && *expires_in > IDENTITY_SIGNED
        {
            return Err(format!(
                "storage.{name}.signed_downloads.expires_in: a link signed through the machine's Azure identity lasts 2 h at most, as a longer one costs a call to Azure each time; shorten it, or sign with the account key"
            ));
        }
    }
    Ok(())
}

/// The longest link an Azure identity signs without asking Azure for a key each time.
pub const IDENTITY_SIGNED: Duration = Duration::from_secs(2 * 3600);

/// `sign_with_identity` on a local or S3 storage, which have no such identity, is a mistake.
fn no_identity(name: &str, c: &StorageConfig) -> Result<(), String> {
    match c.sign_with_identity {
        true => Err(format!(
            "storage.{name}.sign_with_identity: only an Azure or Google storage signs through the machine's own identity"
        )),
        false => Ok(()),
    }
}

/// Whether `c` signs anything: client uploads or signed downloads.
fn signs_anything(c: &StorageConfig) -> bool {
    c.client_uploads || matches!(c.signed_downloads, SignedDownloads::On { .. })
}

/// Where Google Cloud Storage is reached when no `endpoint` says otherwise.
const GCS_BASE: &str = "https://storage.googleapis.com";

/// A store's `endpoint`: an http or https URL carrying no user, password, query or
/// fragment. Nothing of it is echoed, as a user or password in it would be a secret.
fn endpoint_url(name: &str, raw: &str) -> Result<Url, String> {
    let bad = |why: &str| Err(format!("storage.{name}.endpoint: {why}"));
    let Ok(url) = Url::parse(raw) else {
        return bad("isn't a URL; give a full address, like https://host:port");
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return bad("has to be an http:// or https:// address with a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return bad("can't carry a user or password; credentials come only from the environment");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return bad("can't carry a query or a fragment, which would go into every link");
    }
    Ok(url)
}

/// `url` as a store takes it: without a trailing `/`.
fn trimmed(url: &Url) -> String {
    url.as_str().trim_end_matches('/').to_owned()
}

/// The environment variables of an Azure identity: a user-assigned managed identity
/// (`AZURE_CLIENT_ID`, `AZURE_OBJECT_ID`, `AZURE_MSI_RESOURCE_ID`, `IDENTITY_ENDPOINT`) or
/// workload identity, as AKS sets it (`AZURE_CLIENT_ID`, `AZURE_TENANT_ID`,
/// `AZURE_FEDERATED_TOKEN_FILE`, `AZURE_AUTHORITY_HOST`). Unlike the builder's `from_env`,
/// nothing else: no key, token, endpoint or emulator switch, which would sign as another
/// credential or move the links' host.
fn azure_identity(
    mut b: MicrosoftAzureBuilder,
    var: &impl Fn(&str) -> Option<String>,
) -> MicrosoftAzureBuilder {
    for name in [
        "AZURE_CLIENT_ID",
        "AZURE_TENANT_ID",
        "AZURE_FEDERATED_TOKEN_FILE",
        "AZURE_AUTHORITY_HOST",
        "AZURE_OBJECT_ID",
        "AZURE_MSI_RESOURCE_ID",
        "AZURE_MSI_ENDPOINT",
    ] {
        if let (Some(value), Ok(key)) = (var(name), name.to_ascii_lowercase().parse()) {
            b = b.with_config(key, value);
        }
    }
    if let Some(endpoint) = var("IDENTITY_ENDPOINT") {
        b = b.with_msi_endpoint(endpoint);
    }
    b
}

/// What of a service account key file the storage checks itself.
struct GcsKeyFile {
    /// An emulator's: the key is never read, so nothing is signed with it.
    disable_oauth: bool,
    /// Where the file would send every request in place of Google.
    base_url: Option<String>,
}

/// Reads the key file at `path`. Its other fields are the store's to check.
fn gcs_key_file(path: &str) -> Result<GcsKeyFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("can't read {path}: {e}"))?;
    let key: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{path} isn't a key file: {e}"))?;
    Ok(GcsKeyFile {
        disable_oauth: key.get("disable_oauth") == Some(&serde_json::Value::Bool(true)),
        base_url: key
            .get("gcs_base_url")
            .and_then(|u| u.as_str())
            .map(str::to_owned),
    })
}

/// `t` in whole seconds since the epoch, as object times are compared.
fn epoch_secs(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
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

/// The one directory of a storage, under its `prefix`, that clients put files in before the
/// server checks them. A file's own keys are `{collection}/{file id}/…` (or under a
/// `staticDir`), which never begin with it, so no file is stored, served or linked there,
/// and one bucket lifecycle rule on it covers every collection.
pub const STAGING: &str = "_staging";

/// A new key in `STAGING`: `_staging/{random}`.
pub fn staging_key() -> String {
    format!("{STAGING}/{}", Uuid::new_v4())
}

/// Whether `key` is exactly what `staging_key` makes: `_staging/` and a UUID, nothing after.
pub fn is_staging_key(key: &str) -> bool {
    key.strip_prefix(STAGING)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|id| !id.contains('/') && Uuid::try_parse(id).is_ok())
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

    /// `Storage::check_signing` on each: the first that fails.
    pub async fn check_signing(&self) -> Result<(), String> {
        for storage in self.0.values() {
            storage.check_signing().await?;
        }
        Ok(())
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
    fn an_azure_identity_takes_only_identity_variables() {
        let env = [
            ("AZURE_CLIENT_ID", "client"),
            ("AZURE_TENANT_ID", "tenant"),
            ("AZURE_FEDERATED_TOKEN_FILE", "/var/run/secrets/token"),
            ("IDENTITY_ENDPOINT", "http://169.254.169.254/msi"),
            // What `from_env` would also take: another credential, another host.
            ("AZURE_STORAGE_ACCOUNT_KEY", "a2V5"),
            ("AZURE_STORAGE_SAS_TOKEN", "sig=x"),
            ("AZURE_STORAGE_USE_EMULATOR", "true"),
            ("AZURE_STORAGE_ENDPOINT", "https://evil.example"),
        ];
        let var = |name: &str| {
            env.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        };
        let b = azure_identity(MicrosoftAzureBuilder::new(), &var);
        let got = |key: AzureConfigKey| b.get_config_value(&key);
        // Workload identity, as AKS sets it, resolves from these three.
        assert_eq!(got(AzureConfigKey::ClientId).as_deref(), Some("client"));
        assert_eq!(got(AzureConfigKey::AuthorityId).as_deref(), Some("tenant"));
        assert_eq!(
            got(AzureConfigKey::FederatedTokenFile).as_deref(),
            Some("/var/run/secrets/token")
        );
        assert_eq!(
            got(AzureConfigKey::MsiEndpoint).as_deref(),
            Some("http://169.254.169.254/msi")
        );
        for other in [
            AzureConfigKey::AccessKey,
            AzureConfigKey::SasKey,
            AzureConfigKey::Endpoint,
        ] {
            assert_eq!(got(other), None, "{other:?}");
        }
        assert_eq!(got(AzureConfigKey::UseEmulator).as_deref(), Some("false"));
    }

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
