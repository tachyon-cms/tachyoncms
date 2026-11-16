use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use tachyon_api::auth::AuthConfig;
use tachyon_api::{JobsConfig, UploadLimits, UploadsConfig};
use tachyon_db::files::PENDING_GRACE;
use tachyon_media::storage::{Adapter, SignedDownloads, StorageConfig, Storages, credential_var};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub database_url: String,
    /// The most connections the server opens to Postgres.
    #[serde(default = "default_pool")]
    pub database_pool: u32,
    pub listen: SocketAddr,
    /// Where the admin and API are reached from browsers, such as `https://cms.example.com`.
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default)]
    pub auth: AuthSettings,
    #[serde(default)]
    pub jobs: JobsSettings,
    #[serde(default)]
    pub uploads: UploadsSettings,
    /// `[storage.<name>]`; `default` is local `./uploads` unless set here.
    #[serde(default)]
    pub storage: BTreeMap<String, StorageSettings>,
}

/// `[auth]` in the config file (or `TACHYON_AUTH__*` in the environment).
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthSettings {
    pub session_idle: String,
    pub session_absolute: String,
    pub lockout_attempts: i32,
    pub lockout_window: String,
    pub lockout_duration: String,
    pub argon2_memory_kib: u32,
    pub breach_check: bool,
    pub breach_check_url: String,
    pub allowed_origins: Vec<String>,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            session_idle: "7d".into(),
            session_absolute: "30d".into(),
            lockout_attempts: 5,
            lockout_window: "15m".into(),
            lockout_duration: "15m".into(),
            argon2_memory_kib: 19_456,
            breach_check: true,
            breach_check_url: "https://api.pwnedpasswords.com".into(),
            allowed_origins: Vec::new(),
        }
    }
}

/// `[jobs]` in the config file (or `TACHYON_JOBS__*` in the environment).
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobsSettings {
    /// Workers running scheduled work on this server; 0 leaves it to other servers.
    pub workers: usize,
}

impl Default for JobsSettings {
    fn default() -> Self {
        Self { workers: 2 }
    }
}

/// `[uploads]` in the config file (or `TACHYON_UPLOADS__*` in the environment).
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadsSettings {
    /// `25MB` (25,000,000 bytes) or `25MiB` (26,214,400).
    pub max_file_size: String,
    pub max_pixels: u64,
    pub temp_dir: PathBuf,
    pub timeout: String,
}

impl Default for UploadsSettings {
    fn default() -> Self {
        Self {
            max_file_size: "25MB".into(),
            max_pixels: 50_000_000,
            temp_dir: "/var/tmp/tachyon".into(),
            timeout: "10m".into(),
        }
    }
}

/// `[storage.<name>]`. Credentials aren't settings: they come only from
/// `TACHYON_STORAGE__<NAME>__ACCESS_KEY_ID` and `..._SECRET_ACCESS_KEY` (S3),
/// `..._ACCESS_KEY` or `..._SAS_TOKEN` (Azure) and `..._SERVICE_ACCOUNT_PATH` (Google), and
/// in the file they're an error, so a secret never sits in a config that gets copied around.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageSettings {
    /// `local`, `s3` (also R2, MinIO and other S3-compatible stores), `azure` or `gcs`.
    pub adapter: String,
    pub root: Option<PathBuf>,
    pub bucket: Option<String>,
    /// Azure's blob container, and the storage account it's in.
    pub container: Option<String>,
    pub account: Option<String>,
    pub prefix: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub public_url: Option<String>,
    #[serde(default)]
    pub client_uploads: bool,
    /// Signs through the machine's own Azure or Google identity, with no key here.
    #[serde(default)]
    pub sign_with_identity: bool,
    #[serde(default)]
    pub signed_downloads: SignedSetting,
    /// Only to name the right variable when one is found in the file.
    access_key_id: Option<serde::de::IgnoredAny>,
    secret_access_key: Option<serde::de::IgnoredAny>,
    session_token: Option<serde::de::IgnoredAny>,
    access_key: Option<serde::de::IgnoredAny>,
    sas_token: Option<serde::de::IgnoredAny>,
    service_account_path: Option<serde::de::IgnoredAny>,
}

/// The credential keys of a `[storage.<name>]`, each read from its own environment variable.
const CREDENTIALS: [&str; 6] = [
    "access_key_id",
    "secret_access_key",
    "session_token",
    "access_key",
    "sas_token",
    "service_account_path",
];

/// `signed_downloads = false | true | { mime_types = [..], expires_in = "2h" }`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SignedSetting {
    Flag(bool),
    Options(SignedOptions),
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SignedOptions {
    pub mime_types: Vec<String>,
    pub expires_in: String,
}

impl Default for SignedOptions {
    fn default() -> Self {
        Self {
            mime_types: Vec::new(),
            expires_in: "2h".into(),
        }
    }
}

impl Default for SignedSetting {
    fn default() -> Self {
        Self::Flag(false)
    }
}

/// The keys a `[storage.<name>]` may set: its fields but the credentials.
static STORAGE_KEYS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    field_names::<StorageSettings>()
        .iter()
        .copied()
        .filter(|k| !CREDENTIALS.contains(k))
        .collect()
});

/// `e` with an unknown `[storage.<name>]` key offered only the keys that can be set there,
/// not the credentials, which would only be refused.
fn settable_only(e: figment::Error) -> figment::Error {
    let errors: Vec<figment::Error> = e
        .into_iter()
        .map(|mut e| {
            if let figment::error::Kind::UnknownField(_, expected) = &mut e.kind
                && expected.contains(&CREDENTIALS[0])
            {
                *expected = &STORAGE_KEYS;
            }
            e
        })
        .collect();
    // Iterated newest first; chained back oldest first.
    errors
        .into_iter()
        .rev()
        .reduce(|older, newer| older.chain(newer))
        .expect("an error is at least itself")
}

/// Environment variables that are storage credentials, which the settings never read.
fn is_credential_var(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.starts_with("storage__") && CREDENTIALS.iter().any(|c| key.ends_with(&format!("__{c}")))
}

/// `25MB`, `25MiB`, `512KB`, `1GB` or plain bytes. `MB` is 1,000,000 bytes, `MiB` 1,048,576.
fn byte_size(text: &str) -> Option<u64> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let n: u64 = number.parse().ok()?;
    let unit: u64 = match unit.trim() {
        "" | "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(unit).filter(|&b| b > 0)
}

/// Can files be made in `dir`? Creates it if missing.
fn check_writable(dir: &std::path::Path) -> Result<(), String> {
    let bad =
        |e: std::io::Error| format!("uploads.temp_dir: can't write to {}: {e}", dir.display());
    std::fs::create_dir_all(dir).map_err(bad)?;
    let probe = dir.join(format!(".tachyon-probe-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(bad)?;
    std::fs::remove_file(&probe).map_err(bad)
}

impl StorageSettings {
    fn to_config(&self, name: &str) -> Result<StorageConfig, String> {
        let set = [
            self.access_key_id.is_some(),
            self.secret_access_key.is_some(),
            self.session_token.is_some(),
            self.access_key.is_some(),
            self.sas_token.is_some(),
            self.service_account_path.is_some(),
        ];
        for (key, set) in CREDENTIALS.into_iter().zip(set) {
            if set {
                return Err(format!(
                    "storage.{name}.{key} can't be in the config file; set {} in the environment instead",
                    credential_var(name, key)
                ));
            }
        }
        let needs = |value: &Option<String>, key: &str| {
            value.clone().ok_or_else(|| {
                format!(
                    "storage.{name}.{key} is required for adapter `{}`",
                    self.adapter
                )
            })
        };
        let adapter = match self.adapter.as_str() {
            "local" => Adapter::Local {
                root: self.root.clone().ok_or_else(|| {
                    format!("storage.{name}.root is required for adapter `local`")
                })?,
            },
            "s3" => Adapter::S3 {
                bucket: needs(&self.bucket, "bucket")?,
                endpoint: self.endpoint.clone(),
                region: self.region.clone(),
            },
            "azure" => Adapter::Azure {
                container: needs(&self.container, "container")?,
                account: needs(&self.account, "account")?,
                endpoint: self.endpoint.clone(),
            },
            "gcs" => Adapter::Gcs {
                bucket: needs(&self.bucket, "bucket")?,
                endpoint: self.endpoint.clone(),
            },
            other => {
                return Err(format!(
                    "storage.{name}.adapter: `{other}` isn't one; use local, s3, azure or gcs"
                ));
            }
        };
        let public_url = match &self.public_url {
            None => None,
            Some(u) => Some(web_url(u).ok_or_else(|| {
                format!("storage.{name}.public_url must be a full web address, like https://files.example.com")
            })?),
        };
        let signed_downloads = match &self.signed_downloads {
            SignedSetting::Flag(false) => SignedDownloads::Off,
            SignedSetting::Flag(true) => SignedDownloads::On {
                mime_types: Vec::new(),
                expires_in: Duration::from_secs(7200),
            },
            SignedSetting::Options(o) => SignedDownloads::On {
                mime_types: o.mime_types.clone(),
                expires_in: duration(
                    &format!("storage.{name}.signed_downloads.expires_in"),
                    &o.expires_in,
                )?,
            },
        };
        Ok(StorageConfig {
            adapter,
            prefix: self.prefix.clone(),
            public_url,
            client_uploads: self.client_uploads,
            signed_downloads,
            sign_with_identity: self.sign_with_identity,
        })
    }
}

/// More workers than this would mostly wait on each other's database connections.
const MAX_WORKERS: usize = 16;

fn default_pool() -> u32 {
    10
}

/// Ten times Postgres's default `max_connections`: a bigger pool is a typo, not a plan.
const MAX_POOL: u32 = 1000;

/// `15m`, `12h`, `7d`: a whole number and a unit.
fn duration(name: &str, text: &str) -> Result<Duration, String> {
    let bad = || {
        format!("{name}: `{text}` isn't a duration; use a number and s, m, h or d, like 15m or 7d")
    };
    let t = text.trim();
    let Some((last, _)) = t.char_indices().last() else {
        return Err(bad());
    };
    let (number, unit) = t.split_at(last);
    let n: u64 = number.parse().map_err(|_| bad())?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return Err(bad()),
    };
    if secs == 0 {
        return Err(bad());
    }
    Ok(Duration::from_secs(secs))
}

/// Read when `--config` isn't given, and only if it exists.
const DEFAULT_CONFIG: &str = "tachyon.toml";

/// A misspelt key in the file would otherwise be dropped and its default used. Only the file
/// is checked: `TACHYON_*` variables that aren't settings are left alone.
#[allow(clippy::result_large_err)]
fn check_file_keys(path: &str) -> Result<(), figment::Error> {
    let known = field_names::<Config>();
    let keys: figment::value::Dict = Figment::from(Toml::file(path)).extract()?;
    match keys.keys().find(|k| !known.contains(&k.as_str())) {
        Some(key) => Err(format!(
            "unknown key `{key}` in {path}; the top-level keys are {}",
            known.join(", ")
        )
        .into()),
        None => Ok(()),
    }
}

/// A struct's field names, as its derived `Deserialize` hands them to `deserialize_struct`,
/// so a field added to `Config` is a known key without being listed anywhere else.
fn field_names<T: serde::de::DeserializeOwned>() -> &'static [&'static str] {
    use serde::de::{Error, Visitor, value};

    struct Names<'a>(&'a mut &'static [&'static str]);

    impl<'de> serde::Deserializer<'de> for Names<'_> {
        type Error = value::Error;

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, value::Error> {
            *self.0 = fields;
            Err(value::Error::custom("only reading the field names"))
        }

        fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, value::Error> {
            Err(value::Error::custom("not a struct"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map enum
            identifier ignored_any
        }
    }

    let mut names: &'static [&'static str] = &[];
    let _ = T::deserialize(Names(&mut names));
    names
}

/// `endpoint` as a place: its scheme, host (one trailing dot dropped), port and path
/// without its slashes, so spellings of one address compare equal. Empty when unset.
fn endpoint_place(endpoint: Option<&str>) -> String {
    let Some(raw) = endpoint else {
        return String::new();
    };
    match web_url(raw) {
        Some(u) => {
            let host = u.host_str().unwrap_or_default();
            format!(
                "{}://{}:{}/{}",
                u.scheme(),
                host.strip_suffix('.').unwrap_or(host),
                u.port_or_known_default().unwrap_or_default(),
                u.path().trim_matches('/')
            )
        }
        None => raw.to_owned(),
    }
}

/// A local `root` as a place: absolute, `.` dropped and `..` folded by name, since the
/// directory may not exist yet to be resolved.
fn root_place(root: &std::path::Path) -> String {
    use std::path::Component;
    let absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let mut out = std::path::PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.display().to_string()
}

/// An `http` or `https` address with a host, as `public_url` and each allowed origin must be.
fn web_url(text: &str) -> Option<url::Url> {
    url::Url::parse(text).ok().filter(|u| {
        matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty())
    })
}

/// An origin is a scheme, host and optional port: no user, password, path, query or fragment.
/// One that has them would never match a request's `Origin`, so writes from that site would be
/// refused.
fn check_origin(origin: &str) -> Result<(), String> {
    let url = web_url(origin);
    // url accepts `*` in a host, but origins are matched exactly, so `https://*.example.com`
    // would match nothing rather than every subdomain.
    if url
        .as_ref()
        .and_then(|u| u.host_str())
        .is_some_and(|h| h.contains('*'))
    {
        return Err(format!(
            "auth.allowed_origins: `{origin}` uses a wildcard, which isn't supported; list each origin, like https://www.example.com"
        ));
    }
    let ok = url.is_some_and(|u| {
        u.username().is_empty()
            && u.password().is_none()
            && u.path() == "/"
            && u.query().is_none()
            && u.fragment().is_none()
    });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "auth.allowed_origins: `{origin}` isn't an origin; use a scheme and host, like https://www.example.com"
        ))
    }
}

/// While no account exists the API is waiting for its first Admin; say how to make one.
pub fn setup_warning(has_accounts: bool) -> Option<&'static str> {
    (!has_accounts).then_some(
        "setup needed: create the first Admin with POST /api/setup (see the README's First run)",
    )
}

impl Config {
    /// Serving beyond this machine needs a public URL: cookie-signed writes are only accepted
    /// from it, so without one nobody could sign in from a browser.
    pub fn check_serve(&self) -> Result<(), String> {
        if self
            .public_url
            .as_deref()
            .is_some_and(|url| web_url(url).is_none())
        {
            return Err(
                "public_url must be a full web address, like https://cms.example.com".into(),
            );
        }
        if self.public_url.is_none() && !self.listen.ip().to_canonical().is_loopback() {
            return Err(
                "set public_url (such as https://cms.example.com) when listening beyond this machine"
                    .into(),
            );
        }
        Ok(())
    }

    /// The sign-in settings the API runs with.
    pub fn auth_config(&self) -> Result<AuthConfig, String> {
        let a = &self.auth;
        for origin in &a.allowed_origins {
            check_origin(origin)?;
        }
        // Zero would lock an account on its first wrong password, so anyone could lock out
        // anyone.
        if a.lockout_attempts < 1 {
            return Err(format!(
                "auth.lockout_attempts is {}; it has to be at least 1",
                a.lockout_attempts
            ));
        }
        let session_idle = duration("auth.session_idle", &a.session_idle)?;
        let session_absolute = duration("auth.session_absolute", &a.session_absolute)?;
        if session_absolute < session_idle {
            return Err(format!(
                "auth.session_absolute ({}) is shorter than auth.session_idle ({}), so the idle limit would never apply",
                a.session_absolute, a.session_idle
            ));
        }
        Ok(AuthConfig {
            public_url: self.cms_url(),
            allowed_origins: a.allowed_origins.clone(),
            session_idle,
            session_absolute,
            lockout_attempts: a.lockout_attempts,
            lockout_window: duration("auth.lockout_window", &a.lockout_window)?,
            lockout_duration: duration("auth.lockout_duration", &a.lockout_duration)?,
            argon2_memory_kib: a.argon2_memory_kib,
            breach_check: a.breach_check,
            breach_check_url: a.breach_check_url.clone(),
        })
    }

    /// The job workers the API runs with, given the database pool's size. A worker running a
    /// schema change holds two connections (its heartbeat and the change), so the workers must
    /// leave at least one for requests.
    pub fn jobs_config(&self, pool_connections: u32) -> Result<JobsConfig, String> {
        let workers = self.jobs.workers;
        if workers > MAX_WORKERS {
            return Err(format!(
                "jobs.workers is {workers}; it can be at most {MAX_WORKERS}"
            ));
        }
        if workers > 0 && 2 * workers + 1 > pool_connections as usize {
            let most = (pool_connections as usize).saturating_sub(1) / 2;
            let needed = 2 * workers + 1;
            return Err(format!(
                "jobs.workers is {workers}, but the database pool has {pool_connections} connections; each worker can hold two and requests need one, so use at most {most} or raise database_pool to {needed}"
            ));
        }
        Ok(JobsConfig {
            workers,
            ..JobsConfig::default()
        })
    }

    /// The database pool, `database_pool` connections at most.
    pub fn pool_options(&self) -> PgPoolOptions {
        PgPoolOptions::new().max_connections(self.database_pool)
    }

    /// Where people reach the CMS: `public_url`, else this machine on the listening port.
    fn cms_url(&self) -> String {
        self.public_url
            .clone()
            .unwrap_or_else(|| format!("http://localhost:{}", self.listen.port()))
    }

    /// Each `[storage.<name>]` as the storage reads it, checked but not opened. A bucket's
    /// `public_url` on the CMS's own host and port is refused: its files have no `sandbox`
    /// there, so an SVG or HTML upload would run as the CMS.
    fn storage_configs(&self) -> Result<BTreeMap<String, StorageConfig>, String> {
        // `cms.example.com.` is the same host as `cms.example.com`.
        let origin = |u: &url::Url| {
            let host = u
                .host_str()
                .map(|h| h.strip_suffix('.').unwrap_or(h).to_owned());
            (host, u.port_or_known_default())
        };
        let cms = web_url(&self.cms_url()).map(|u| origin(&u));
        let mut configs = BTreeMap::new();
        for (name, s) in &self.storage {
            // Environment variables are named from it, upper-cased and split on `__`.
            let plain = name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if !plain || name.contains("__") || name.starts_with('_') || name.ends_with('_') {
                return Err(format!(
                    "storage.{name}: a storage name is lowercase letters and digits, with single _ between them"
                ));
            }
            let config = s.to_config(name)?;
            if config
                .public_url
                .as_ref()
                .map(origin)
                .is_some_and(|o| Some(o) == cms)
            {
                return Err(format!(
                    "storage.{name}.public_url is on the CMS's own address (public_url), where an uploaded SVG or HTML file would run as the CMS; serve the bucket from another host, like https://files.example.com"
                ));
            }
            configs.insert(name.clone(), config);
        }
        // Two names for one place would each sweep the other's `_staging/`.
        let place = |c: &StorageConfig| {
            let at = match &c.adapter {
                Adapter::Local { root } => vec!["local".to_owned(), root_place(root)],
                Adapter::S3 {
                    bucket, endpoint, ..
                } => {
                    vec![
                        "s3".into(),
                        bucket.clone(),
                        endpoint_place(endpoint.as_deref()),
                    ]
                }
                Adapter::Azure {
                    container,
                    account,
                    endpoint,
                } => vec![
                    "azure".into(),
                    account.clone(),
                    container.clone(),
                    endpoint_place(endpoint.as_deref()),
                ],
                Adapter::Gcs { bucket, endpoint } => {
                    vec![
                        "gcs".into(),
                        bucket.clone(),
                        endpoint_place(endpoint.as_deref()),
                    ]
                }
            };
            let prefix = c.prefix.as_deref().unwrap_or_default().trim_matches('/');
            (at, prefix.to_owned())
        };
        for (i, (a, ca)) in configs.iter().enumerate() {
            if let Some((b, _)) = configs
                .iter()
                .skip(i + 1)
                .find(|(_, cb)| place(cb) == place(ca))
            {
                return Err(format!(
                    "storage.{a} and storage.{b} are the same place (adapter, bucket or root, and prefix), so each would sweep the other's _staging/; give one its own prefix"
                ));
            }
        }
        Ok(configs)
    }

    /// What a schema's `upload` is checked against, for `tachyon schema`: the storages'
    /// names and kinds and the pixel cap. Nothing is created or written, and no credential
    /// is needed, so it works on a machine that only reaches the database.
    pub fn upload_limits(&self) -> Result<UploadLimits, String> {
        let mut storages: BTreeMap<String, bool> = self
            .storage_configs()?
            .into_iter()
            .map(|(name, c)| (name, matches!(c.adapter, Adapter::Local { .. })))
            .collect();
        storages.entry("default".into()).or_insert(true);
        Ok(UploadLimits {
            storages,
            max_pixels: self.uploads.max_pixels,
        })
    }

    /// Upload limits and the storages they go to, each storage opened (a local root is
    /// created). Refuses, naming the key, a size or duration it can't read, `max_pixels`
    /// of 0, a `temp_dir` it can't write to and a credential in the file.
    pub fn uploads_config(&self) -> Result<UploadsConfig, String> {
        let u = &self.uploads;
        let max_file_size = byte_size(&u.max_file_size).ok_or_else(|| {
            format!(
                "uploads.max_file_size: `{}` isn't a size; use a number and B, KB, MB, GB, KiB, MiB or GiB, like 25MB",
                u.max_file_size
            )
        })?;
        if u.max_pixels < 1 {
            return Err("uploads.max_pixels is 0; it has to be at least 1".into());
        }
        check_writable(&u.temp_dir)?;
        let configs = self.storage_configs()?;
        let timeout = duration("uploads.timeout", &u.timeout)?;
        // A reserved upload not taken within the grace is cleaned up, so a request still
        // streaming by then would lose its file and leave the object behind.
        if timeout >= PENDING_GRACE {
            return Err(format!(
                "uploads.timeout is {}; it has to be under {} min, after which an unfinished upload is cleaned up",
                u.timeout,
                PENDING_GRACE.as_secs() / 60
            ));
        }
        Ok(UploadsConfig {
            max_file_size,
            max_pixels: u.max_pixels,
            temp_dir: u.temp_dir.clone(),
            timeout,
            storages: Arc::new(Storages::open(&configs)?),
        })
    }

    /// Defaults, then the TOML file, then `TACHYON_*` environment variables. With no
    /// `--config`, `tachyon.toml` is read if it exists; a file named with `--config` must exist.
    // figment's own error type; this runs once at startup.
    #[allow(clippy::result_large_err)]
    pub fn load(named: Option<&str>) -> Result<Self, figment::Error> {
        let path = named.unwrap_or(DEFAULT_CONFIG);
        if named.is_some() && !std::path::Path::new(path).exists() {
            return Err(format!("the config file {path} doesn't exist").into());
        }
        check_file_keys(path)?;
        let config: Self = Figment::new()
            .merge(Serialized::default("listen", "127.0.0.1:3000"))
            .merge(Toml::file(path))
            // TACHYON_AUTH__BREACH_CHECK=false sets [auth] breach_check.
            .merge(
                Env::prefixed("TACHYON_")
                    // Read by the storage itself, never kept with the settings.
                    .filter(|k| !is_credential_var(k.as_str()))
                    .split("__"),
            )
            .extract()
            .map_err(settable_only)?;
        if config.database_url.trim().is_empty() {
            return Err("database_url is empty".to_owned().into());
        }
        match config.database_pool {
            0 => {
                return Err("database_pool is 0; the server needs at least 1 connection"
                    .to_owned()
                    .into());
            }
            n if n > MAX_POOL => {
                return Err(format!("database_pool is {n}; it can be at most {MAX_POOL}").into());
            }
            _ => {}
        }
        Ok(config)
    }
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // figment::Jail closures return figment::Error
mod tests {
    use super::{Adapter, Config};
    use figment::Jail;

    fn load(toml: &str) -> Config {
        let mut out = None;
        Jail::expect_with(|jail| {
            jail.create_file("tachyon.toml", toml)?;
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            out = Some(Config::load(None)?);
            Ok(())
        });
        out.unwrap()
    }

    #[test]
    fn public_url_is_required_off_loopback() {
        for listen in ["0.0.0.0:3000", "192.168.1.5:3000", "[::]:3000"] {
            let config = load(&format!("listen = \"{listen}\""));
            assert_eq!(
                config.check_serve().unwrap_err(),
                "set public_url (such as https://cms.example.com) when listening beyond this machine",
                "{listen}"
            );
            let with_url = load(&format!(
                "listen = \"{listen}\"\npublic_url = \"https://cms.example.com\""
            ));
            assert!(with_url.check_serve().is_ok());
        }
        for listen in ["127.0.0.1:3000", "[::1]:8080", "[::ffff:127.0.0.1]:3000"] {
            let config = load(&format!("listen = \"{listen}\""));
            assert!(config.check_serve().is_ok(), "{listen}");
        }
        assert_eq!(
            load("listen = \"127.0.0.1:4000\"")
                .auth_config()
                .unwrap()
                .public_url,
            "http://localhost:4000",
            "locally the public URL defaults to this machine"
        );
    }

    #[test]
    fn public_url_must_be_a_full_web_address() {
        for bad in [
            "cms.example.com",
            "ftp://cms.example.com",
            "https://",
            "not a url",
        ] {
            let config = load(&format!("public_url = \"{bad}\""));
            assert_eq!(
                config.check_serve().unwrap_err(),
                "public_url must be a full web address, like https://cms.example.com",
                "{bad}"
            );
        }
        assert!(
            load("public_url = \"https://cms.example.com/admin\"")
                .check_serve()
                .is_ok()
        );
    }

    #[test]
    fn auth_settings_have_safe_defaults_and_read_durations() {
        let auth = load("").auth_config().unwrap();
        assert!(
            auth.breach_check,
            "the server checks breaches unless told not to"
        );
        assert_eq!(
            auth.session_idle,
            std::time::Duration::from_secs(7 * 86_400)
        );
        assert_eq!(auth.lockout_window, std::time::Duration::from_secs(15 * 60));
        let custom = load("[auth]\nsession_idle = \"12h\"\nlockout_attempts = 3\nbreach_check = false\nallowed_origins = [\"https://www.example.com\"]")
            .auth_config()
            .unwrap();
        assert_eq!(
            custom.session_idle,
            std::time::Duration::from_secs(12 * 3600)
        );
        assert_eq!(custom.lockout_attempts, 3);
        assert!(!custom.breach_check);
        assert_eq!(custom.allowed_origins, ["https://www.example.com"]);
        assert_eq!(
            load("[auth]\nsession_idle = \"soon\"")
                .auth_config()
                .unwrap_err(),
            "auth.session_idle: `soon` isn't a duration; use a number and s, m, h or d, like 15m or 7d"
        );
    }

    #[test]
    fn auth_settings_that_could_never_work_stop_startup() {
        for bad in [
            "www.example.com",
            "ftp://www.example.com",
            "https://www.example.com/admin",
            "https://www.example.com/?x=1",
            "https://www.example.com/#top",
            "https://user@www.example.com",
            "https://user:secret@www.example.com",
            "https://",
        ] {
            assert_eq!(
                load(&format!("[auth]\nallowed_origins = [\"{bad}\"]"))
                    .auth_config()
                    .unwrap_err(),
                format!(
                    "auth.allowed_origins: `{bad}` isn't an origin; use a scheme and host, like https://www.example.com"
                ),
                "{bad}"
            );
        }
        for wildcard in [
            "https://*.example.com",
            "https://www.*.com",
            "http://*:8080",
        ] {
            assert_eq!(
                load(&format!("[auth]\nallowed_origins = [\"{wildcard}\"]"))
                    .auth_config()
                    .unwrap_err(),
                format!(
                    "auth.allowed_origins: `{wildcard}` uses a wildcard, which isn't supported; list each origin, like https://www.example.com"
                ),
                "{wildcard}"
            );
        }
        let ok = load(
            "[auth]\nallowed_origins = [\"https://www.example.com\", \"http://localhost:4321\", \"https://shop.example.com/\"]",
        )
        .auth_config()
        .unwrap();
        assert_eq!(ok.allowed_origins.len(), 3);

        for attempts in [0, -1] {
            assert_eq!(
                load(&format!("[auth]\nlockout_attempts = {attempts}"))
                    .auth_config()
                    .unwrap_err(),
                format!("auth.lockout_attempts is {attempts}; it has to be at least 1")
            );
        }
        assert!(load("[auth]\nlockout_attempts = 1").auth_config().is_ok());

        assert_eq!(
            load("[auth]\nsession_idle = \"7d\"\nsession_absolute = \"1d\"")
                .auth_config()
                .unwrap_err(),
            "auth.session_absolute (1d) is shorter than auth.session_idle (7d), so the idle limit would never apply"
        );
        assert!(
            load("[auth]\nsession_idle = \"1d\"\nsession_absolute = \"1d\"")
                .auth_config()
                .is_ok(),
            "the same length is fine"
        );
    }

    #[test]
    fn setup_is_flagged_until_it_runs() {
        assert!(
            super::setup_warning(false)
                .unwrap()
                .contains("POST /api/setup")
        );
        assert!(super::setup_warning(true).is_none());
    }

    #[test]
    fn env_overrides_file() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "tachyon.toml",
                "database_url = \"postgres://file\"\nlisten = \"0.0.0.0:8080\"\n",
            )?;
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            let config = Config::load(Some("tachyon.toml"))?;
            assert_eq!(config.database_url, "postgres://env");
            assert_eq!(config.listen, "0.0.0.0:8080".parse().unwrap());
            Ok(())
        });
    }

    #[test]
    fn listens_on_localhost_by_default_and_file_is_optional() {
        Jail::expect_with(|jail| {
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            let config = Config::load(None)?;
            assert_eq!(config.listen, "127.0.0.1:3000".parse().unwrap());
            Ok(())
        });
    }

    #[test]
    fn the_known_keys_come_from_the_struct() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Two {
            one: u8,
            #[serde(rename = "second_one")]
            two: Option<String>,
        }
        assert_eq!(super::field_names::<Two>(), ["one", "second_one"]);
        let config = super::field_names::<Config>();
        assert!(config.contains(&"database_url") && config.contains(&"auth"));
    }

    #[test]
    fn a_named_config_file_must_exist() {
        Jail::expect_with(|jail| {
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            let err = Config::load(Some("/etc/tachyon/prod.toml")).unwrap_err();
            assert_eq!(
                err.to_string(),
                "the config file /etc/tachyon/prod.toml doesn't exist"
            );
            Ok(())
        });
    }

    #[test]
    fn unknown_keys_in_the_file_are_errors_but_not_in_the_environment() {
        Jail::expect_with(|jail| {
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            jail.create_file("tachyon.toml", "publc_url = \"https://cms.example.com\"\n")?;
            let err = Config::load(None).unwrap_err();
            assert_eq!(
                err.to_string(),
                "unknown key `publc_url` in tachyon.toml; the top-level keys are database_url, database_pool, listen, public_url, auth, jobs, uploads, storage"
            );
            // A variable that isn't a setting, such as one for another tool, is left alone.
            jail.create_file(
                "tachyon.toml",
                "database_url = \"postgres://file\"\nlisten = \"127.0.0.1:3000\"\npublic_url = \"https://cms.example.com\"\n[auth]\nbreach_check = false\n",
            )?;
            jail.set_env("TACHYON_SOMETHING_ELSE", "1");
            assert!(Config::load(None)?.public_url.is_some());
            Ok(())
        });
    }

    #[test]
    fn jobs_run_two_workers_unless_told_otherwise() {
        assert_eq!(load("").jobs_config(10).unwrap().workers, 2);
        assert_eq!(
            load("[jobs]\nworkers = 0").jobs_config(10).unwrap().workers,
            0
        );
        Jail::expect_with(|jail| {
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            jail.create_file("tachyon.toml", "[jobs]\nwokers = 4\n")?;
            let err = Config::load(None).unwrap_err().to_string();
            assert!(err.contains("unknown field: found `wokers`"), "{err}");
            jail.create_file("tachyon.toml", "[jobs]\nworkers = -1\n")?;
            assert!(Config::load(None).is_err(), "a negative count");
            jail.create_file("tachyon.toml", "[jobs]\nworkers = 4\n")?;
            jail.set_env("TACHYON_JOBS__WORKERS", "1");
            assert_eq!(Config::load(None)?.jobs_config(10).unwrap().workers, 1);
            Ok(())
        });
    }

    #[test]
    fn jobs_workers_fit_the_pool_and_the_cap() {
        let at = |n: usize, pool: u32| load(&format!("[jobs]\nworkers = {n}")).jobs_config(pool);
        assert_eq!(at(16, 33).unwrap().workers, 16);
        assert_eq!(
            at(17, 40).unwrap_err(),
            "jobs.workers is 17; it can be at most 16"
        );
        assert_eq!(at(4, 10).unwrap().workers, 4);
        assert_eq!(
            at(5, 10).unwrap_err(),
            "jobs.workers is 5, but the database pool has 10 connections; each worker can hold two and requests need one, so use at most 4 or raise database_pool to 11"
        );
        assert_eq!(
            at(1, 2).unwrap_err(),
            "jobs.workers is 1, but the database pool has 2 connections; each worker can hold two and requests need one, so use at most 0 or raise database_pool to 3"
        );
        assert!(at(0, 1).is_ok(), "no workers fit any pool");
    }

    #[test]
    fn the_pool_is_ten_unless_set_and_the_workers_fit_it() {
        let config = load("");
        assert_eq!(config.pool_options().get_max_connections(), 10);
        assert_eq!(
            config.jobs_config(10).unwrap().workers,
            2,
            "the default workers fit the default pool"
        );
        let config = load("database_pool = 33\n[jobs]\nworkers = 16");
        let pool = config.pool_options().get_max_connections();
        assert_eq!(pool, 33);
        assert_eq!(config.jobs_config(pool).unwrap().workers, 16);
        let config = load("database_pool = 32\n[jobs]\nworkers = 16");
        assert_eq!(
            config
                .jobs_config(config.pool_options().get_max_connections())
                .unwrap_err(),
            "jobs.workers is 16, but the database pool has 32 connections; each worker can hold two and requests need one, so use at most 15 or raise database_pool to 33"
        );
        Jail::expect_with(|jail| {
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            jail.set_env("TACHYON_DATABASE_POOL", "33");
            assert_eq!(Config::load(None)?.pool_options().get_max_connections(), 33);
            Ok(())
        });
    }

    #[test]
    fn the_pool_needs_one_connection_and_at_most_a_thousand() {
        Jail::expect_with(|jail| {
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            for (pool, err) in [
                (
                    "0",
                    "database_pool is 0; the server needs at least 1 connection",
                ),
                ("1001", "database_pool is 1001; it can be at most 1000"),
            ] {
                jail.set_env("TACHYON_DATABASE_POOL", pool);
                assert_eq!(Config::load(None).unwrap_err().to_string(), err);
            }
            jail.set_env("TACHYON_DATABASE_POOL", "-1");
            assert!(Config::load(None).is_err(), "a negative size");
            jail.set_env("TACHYON_DATABASE_POOL", "1000");
            assert_eq!(Config::load(None)?.database_pool, 1000);
            Ok(())
        });
    }

    #[test]
    fn database_url_is_required() {
        Jail::expect_with(|jail| {
            // Clearing the whole environment would race with tests that need DATABASE_URL.
            jail.set_env("TACHYON_DATABASE_URL", "");
            assert!(Config::load(None).is_err());
            Ok(())
        });
    }

    /// `uploads_config` for `toml`, inside the jail: a local root is relative to the jail's
    /// directory, and no other test moves it meanwhile.
    fn uploads(toml: &str) -> Result<tachyon_api::UploadsConfig, String> {
        uploads_with(toml, &[])
    }

    /// `uploads`, with `env` set too.
    fn uploads_with(
        toml: &str,
        env: &[(&str, &str)],
    ) -> Result<tachyon_api::UploadsConfig, String> {
        let mut out = None;
        Jail::expect_with(|jail| {
            jail.create_file("tachyon.toml", toml)?;
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            for (k, v) in env {
                jail.set_env(k, v);
            }
            out = Some(Config::load(None)?.uploads_config());
            Ok(())
        });
        out.unwrap()
    }

    #[test]
    fn two_storages_cant_be_one_place() {
        let keys = [
            ("TACHYON_STORAGE__A__ACCESS_KEY_ID", "id"),
            ("TACHYON_STORAGE__A__SECRET_ACCESS_KEY", "secret"),
            ("TACHYON_STORAGE__B__ACCESS_KEY_ID", "id"),
            ("TACHYON_STORAGE__B__SECRET_ACCESS_KEY", "secret"),
        ];
        let refused = "storage.a and storage.b are the same place (adapter, bucket or root, and prefix), so each would sweep the other's _staging/; give one its own prefix";
        let s3 = |name: &str, extra: &str| {
            format!(
                "[storage.{name}]\nadapter = \"s3\"\nbucket = \"media\"\nregion = \"auto\"\n{extra}\n"
            )
        };
        let local = |name: &str, extra: &str| {
            format!("[storage.{name}]\nadapter = \"local\"\nroot = \"shared\"\n{extra}\n")
        };
        for toml in [
            format!("{}{}", s3("a", ""), s3("b", "")),
            format!(
                "{}{}",
                s3("a", "prefix = \"site\""),
                s3("b", "prefix = \"/site/\"")
            ),
            format!("{}{}", local("a", ""), local("b", "")),
            format!(
                "{}{}",
                local("a", ""),
                "[storage.b]\nadapter = \"local\"\nroot = \"./shared/\"\n"
            ),
            format!(
                "{}{}",
                local("a", ""),
                "[storage.b]\nadapter = \"local\"\nroot = \"other/../shared\"\n"
            ),
        ]
        .into_iter()
        // One endpoint, spelt five ways.
        .chain(
            [
                "https://acct.r2.cloudflarestorage.com/",
                "https://acct.r2.cloudflarestorage.com:443",
                "https://ACCT.R2.cloudflarestorage.com",
                "https://acct.r2.cloudflarestorage.com./",
            ]
            .map(|other| {
                format!(
                    "{}{}",
                    s3("a", "endpoint = \"https://acct.r2.cloudflarestorage.com\""),
                    s3("b", &format!("endpoint = \"{other}\""))
                )
            }),
        ) {
            assert_eq!(uploads_with(&toml, &keys).unwrap_err(), refused, "{toml}");
        }
        // A root spelt relative and absolute: the jail is the working directory.
        Jail::expect_with(|jail| {
            let absolute = jail.directory().join("shared");
            let toml = format!(
                "{}[storage.b]\nadapter = \"local\"\nroot = \"{}\"\n",
                local("a", ""),
                absolute.display()
            );
            jail.create_file("tachyon.toml", &toml)?;
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            assert_eq!(Config::load(None)?.uploads_config().unwrap_err(), refused);
            Ok(())
        });
        for toml in [
            format!("{}{}", s3("a", ""), s3("b", "prefix = \"other\"")),
            format!(
                "{}{}",
                s3("a", ""),
                s3("b", "endpoint = \"https://acct.r2.cloudflarestorage.com\"")
            ),
            format!(
                "{}{}",
                local("a", ""),
                "[storage.b]\nadapter = \"local\"\nroot = \"elsewhere\"\n"
            ),
            format!("{}{}", local("a", ""), s3("b", "")),
        ] {
            assert!(uploads_with(&toml, &keys).is_ok(), "{toml}");
        }
    }

    #[test]
    fn a_bucket_cant_share_the_cms_address() {
        let toml = |cms: &str, bucket: &str| {
            format!(
                "public_url = \"{cms}\"\n[storage.r2]\nadapter = \"s3\"\nbucket = \"media\"\nregion = \"auto\"\npublic_url = \"{bucket}\"\n"
            )
        };
        let keys = [
            ("TACHYON_STORAGE__R2__ACCESS_KEY_ID", "id"),
            ("TACHYON_STORAGE__R2__SECRET_ACCESS_KEY", "secret"),
        ];
        let refused = "storage.r2.public_url is on the CMS's own address (public_url), where an uploaded SVG or HTML file would run as the CMS; serve the bucket from another host, like https://files.example.com";
        for (cms, bucket) in [
            ("https://cms.example.com", "https://cms.example.com/files"),
            ("https://cms.example.com", "https://CMS.example.com:443/"),
            ("https://cms.example.com", "https://cms.example.com./files"),
            ("https://cms.example.com.", "https://cms.example.com/files"),
            ("http://localhost:3000", "http://localhost:3000/media"),
        ] {
            assert_eq!(
                uploads_with(&toml(cms, bucket), &keys).unwrap_err(),
                refused,
                "{bucket}"
            );
        }
        for (cms, bucket) in [
            ("https://cms.example.com", "https://files.example.com"),
            ("https://cms.example.com", "https://cms.example.com:8443"),
            ("http://localhost:3000", "http://localhost:3001"),
        ] {
            assert!(uploads_with(&toml(cms, bucket), &keys).is_ok(), "{bucket}");
        }
        // No public_url: the CMS is at localhost on its port.
        let default = "listen = \"127.0.0.1:3000\"\n[storage.r2]\nadapter = \"s3\"\nbucket = \"media\"\nregion = \"auto\"\npublic_url = \"http://localhost:3000/m\"\n";
        assert_eq!(uploads_with(default, &keys).unwrap_err(), refused);
    }

    #[test]
    fn uploads_and_storage_config_defaults() {
        let u = uploads("").unwrap();
        assert_eq!(u.max_file_size, 25_000_000);
        assert_eq!(u.max_pixels, 50_000_000);
        assert_eq!(u.timeout, std::time::Duration::from_secs(600));
        assert_eq!(u.temp_dir, std::path::Path::new("/var/tmp/tachyon"));
        let default = u.storages.get("default").unwrap();
        assert!(default.is_local());
        assert_eq!(u.storages.0.len(), 1);

        let at = uploads;
        assert_eq!(
            at("[uploads]\nmax_file_size = \"25MiB\"")
                .unwrap()
                .max_file_size,
            26_214_400
        );
        for bad in ["25 megabytes", "MB", "0MB", "-1MB", "99999999999999GB"] {
            assert_eq!(
                at(&format!("[uploads]\nmax_file_size = \"{bad}\"")).unwrap_err(),
                format!(
                    "uploads.max_file_size: `{bad}` isn't a size; use a number and B, KB, MB, GB, KiB, MiB or GiB, like 25MB"
                ),
            );
        }
        assert_eq!(
            at("[uploads]\nmax_pixels = 0").unwrap_err(),
            "uploads.max_pixels is 0; it has to be at least 1"
        );
        assert!(
            at("[uploads]\ntimeout = \"soon\"")
                .unwrap_err()
                .starts_with("uploads.timeout: `soon` isn't a duration")
        );
        let unwritable = at("[uploads]\ntemp_dir = \"/dev/null/tachyon\"").unwrap_err();
        assert!(
            unwritable.starts_with("uploads.temp_dir: can't write to /dev/null/tachyon"),
            "{unwritable}"
        );

        let root = std::env::temp_dir().join(format!("tachyon-config-{}", std::process::id()));
        let keys = [
            ("TACHYON_STORAGE__R2__ACCESS_KEY_ID", "id"),
            ("TACHYON_STORAGE__R2__SECRET_ACCESS_KEY", "secret"),
        ];
        let r2 = uploads_with(&format!("[storage.r2]\nadapter = \"s3\"\nbucket = \"media\"\nendpoint = \"https://acct.r2.cloudflarestorage.com\"\nregion = \"auto\"\npublic_url = \"https://files.example.com\"\n[storage.default]\nadapter = \"local\"\nroot = \"{}\"", root.display()), &keys).unwrap();
        assert!(root.is_dir(), "a local root is created");
        assert!(!r2.storages.get("r2").unwrap().is_local());
        assert!(r2.storages.get("default").unwrap().is_local());
        assert_eq!(
            r2.storages
                .get("r2")
                .unwrap()
                .public_url("a/b.png")
                .unwrap(),
            "https://files.example.com/a/b.png"
        );
        std::fs::remove_dir_all(&root).unwrap();

        Jail::expect_with(|jail| {
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            jail.create_file(
                "tachyon.toml",
                "[storage.r2]\nadapter = \"s3\"\nbuckt = \"media\"\n",
            )?;
            let err = Config::load(None).unwrap_err().to_string();
            assert!(err.contains("unknown field: found `buckt`"), "{err}");
            // Only what can be set is offered: credentials come from the environment.
            assert!(
                err.contains("`bucket`") && err.contains("`signed_downloads`"),
                "{err}"
            );
            for key in super::CREDENTIALS {
                assert!(!err.contains(&format!("`{key}`")), "{key}: {err}");
            }
            jail.create_file("tachyon.toml", "[uploads]\nmax_size = \"1MB\"\n")?;
            let err = Config::load(None).unwrap_err().to_string();
            assert!(err.contains("unknown field: found `max_size`"), "{err}");
            Ok(())
        });
        assert_eq!(
            at("[storage.r2]\nadapter = \"s3\"").unwrap_err(),
            "storage.r2.bucket is required for adapter `s3`"
        );
        assert_eq!(
            at("[storage.disk]\nadapter = \"ftp\"").unwrap_err(),
            "storage.disk.adapter: `ftp` isn't one; use local, s3, azure or gcs"
        );
        for name in ["R2", "r__2", "_r2", "r2_", "r-2"] {
            assert_eq!(
                at(&format!(
                    "[storage.\"{name}\"]\nadapter = \"s3\"\nbucket = \"x\""
                ))
                .unwrap_err(),
                format!(
                    "storage.{name}: a storage name is lowercase letters and digits, with single _ between them"
                )
            );
        }
        assert!(at("[storage.old_r2]\nadapter = \"s3\"\nbucket = \"x\"").is_ok());
        for timeout in ["1h", "60m", "2h"] {
            assert_eq!(
                at(&format!("[uploads]\ntimeout = \"{timeout}\"")).unwrap_err(),
                format!(
                    "uploads.timeout is {timeout}; it has to be under 60 min, after which an unfinished upload is cleaned up"
                )
            );
        }
        assert_eq!(
            at("[uploads]\ntimeout = \"59m\"").unwrap().timeout,
            std::time::Duration::from_secs(59 * 60)
        );
        // R2 and MinIO have no instance role to fall back on.
        let r2 =
            "[storage.r2]\nadapter = \"s3\"\nbucket = \"m\"\nendpoint = \"http://127.0.0.1:9000\"";
        for env in [&keys[..1], &keys[1..], &[]] {
            assert_eq!(
                uploads_with(r2, env).unwrap_err(),
                "storage.r2: set TACHYON_STORAGE__R2__ACCESS_KEY_ID and TACHYON_STORAGE__R2__SECRET_ACCESS_KEY in the environment; a bucket with an endpoint needs its keys"
            );
        }
        assert!(uploads_with(r2, &keys).is_ok());
        assert_eq!(
            at("[storage.r2]\nadapter = \"s3\"\nbucket = \"m\"\nprefix = \"a//b\"").unwrap_err(),
            "storage.r2.prefix: `a//b` has to be a relative path of plain names, like media/images: no empty, `.` or `..` parts, no leading or trailing /, and no backslash"
        );
        let signed = at("[storage.r2]\nadapter = \"s3\"\nbucket = \"m\"\nsigned_downloads = { mime_types = [\"application/pdf\"], expires_in = \"15m\" }").unwrap();
        assert_eq!(
            signed.storages.get("r2").unwrap().config().signed_downloads,
            tachyon_media::storage::SignedDownloads::On {
                mime_types: vec!["application/pdf".into()],
                expires_in: std::time::Duration::from_secs(900),
            }
        );
    }

    #[test]
    fn azure_and_gcs_config_parse() {
        let at = uploads;
        assert_eq!(
            at("[storage.az]\nadapter = \"azure\"\naccount = \"acct\"").unwrap_err(),
            "storage.az.container is required for adapter `azure`"
        );
        assert_eq!(
            at("[storage.az]\nadapter = \"azure\"\ncontainer = \"media\"").unwrap_err(),
            "storage.az.account is required for adapter `azure`"
        );
        assert_eq!(
            at("[storage.g]\nadapter = \"gcs\"").unwrap_err(),
            "storage.g.bucket is required for adapter `gcs`"
        );
        let azure = "[storage.az]\nadapter = \"azure\"\ncontainer = \"media\"\naccount = \"acct\"\nendpoint = \"https://acct.blob.core.chinacloudapi.cn\"\nclient_uploads = true\n";
        // Read from the environment by the storage, not taken for a setting in the file.
        let key = [(
            "TACHYON_STORAGE__AZ__ACCESS_KEY",
            "dGFjaHlvbi10ZXN0LWtleQ==",
        )];
        let az = uploads_with(azure, &key).unwrap();
        let az = az.storages.get("az").unwrap();
        assert_eq!(
            az.config().adapter,
            Adapter::Azure {
                container: "media".into(),
                account: "acct".into(),
                endpoint: Some("https://acct.blob.core.chinacloudapi.cn".into()),
            }
        );
        assert!(az.takes_client_uploads());
        // No key: the machine's identity signs only when asked to.
        let ambient = "[storage.az]\nadapter = \"azure\"\ncontainer = \"media\"\naccount = \"acct\"\nclient_uploads = true\n";
        assert!(
            uploads(ambient)
                .unwrap_err()
                .contains("or sign_with_identity = true to sign that way")
        );
        let opted = uploads(&format!("{ambient}sign_with_identity = true\n")).unwrap();
        let opted = opted.storages.get("az").unwrap();
        assert!(opted.config().sign_with_identity);
        assert!(opted.takes_client_uploads());
        let sas = [("TACHYON_STORAGE__AZ__SAS_TOKEN", "sv=1&sig=x")];
        assert!(
            uploads_with(azure, &sas)
                .unwrap_err()
                .starts_with("storage.az.client_uploads: a SAS token can't sign links")
        );

        // An emulator's key file, so the machine's own Google credentials aren't read.
        let file =
            std::env::temp_dir().join(format!("tachyon-gcs-key-{}.json", std::process::id()));
        std::fs::write(
            &file,
            r#"{"private_key": "unused", "private_key_id": "1", "client_email": "a@b", "disable_oauth": true}"#,
        )
        .unwrap();
        let path = file.to_str().unwrap();
        let g = uploads_with(
            "[storage.g]\nadapter = \"gcs\"\nbucket = \"media\"\nendpoint = \"http://127.0.0.1:4443\"\n",
            &[("TACHYON_STORAGE__G__SERVICE_ACCOUNT_PATH", path)],
        );
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            g.unwrap().storages.get("g").unwrap().config().adapter,
            Adapter::Gcs {
                bucket: "media".into(),
                endpoint: Some("http://127.0.0.1:4443".into()),
            }
        );

        for (section, key) in [
            (
                "adapter = \"azure\"\ncontainer = \"m\"\naccount = \"a\"",
                "access_key",
            ),
            (
                "adapter = \"azure\"\ncontainer = \"m\"\naccount = \"a\"",
                "sas_token",
            ),
            ("adapter = \"gcs\"\nbucket = \"m\"", "service_account_path"),
        ] {
            let err = at(&format!("[storage.x]\n{section}\n{key} = \"in-the-file\"")).unwrap_err();
            assert_eq!(
                err,
                format!(
                    "storage.x.{key} can't be in the config file; set TACHYON_STORAGE__X__{} in the environment instead",
                    key.to_ascii_uppercase()
                )
            );
            assert!(!err.contains("in-the-file"), "the secret isn't echoed");
        }
    }

    #[test]
    fn credentials_only_from_env() {
        for key in ["access_key_id", "secret_access_key", "session_token"] {
            let err = uploads(&format!(
                "[storage.r2]\nadapter = \"s3\"\nbucket = \"media\"\n{key} = \"AKIA-in-the-file\""
            ))
            .unwrap_err();
            assert_eq!(
                err,
                format!(
                    "storage.r2.{key} can't be in the config file; set TACHYON_STORAGE__R2__{} in the environment instead",
                    key.to_ascii_uppercase()
                )
            );
            assert!(!err.contains("AKIA"), "the secret isn't echoed");
        }
        Jail::expect_with(|jail| {
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            jail.set_env("TACHYON_STORAGE__R2__ACCESS_KEY_ID", "AKIA-env");
            jail.set_env("TACHYON_STORAGE__R2__SECRET_ACCESS_KEY", "secret-env");
            jail.set_env("TACHYON_STORAGE__R2__SESSION_TOKEN", "token-env");
            jail.set_env("TACHYON_STORAGE__R2__BUCKET", "from-env");
            jail.create_file("tachyon.toml", "[storage.r2]\nadapter = \"s3\"\n")?;
            let config = Config::load(None)?;
            let uploads = config.uploads_config().unwrap();
            assert!(
                uploads.storages.get("r2").is_some(),
                "other keys still come from the environment"
            );
            assert!(
                !["AKIA-env", "secret-env", "token-env"]
                    .iter()
                    .any(|c| format!("{config:?}").contains(c)),
                "credentials aren't settings"
            );
            Ok(())
        });
    }

    #[test]
    fn schema_checks_need_only_the_settings() {
        Jail::expect_with(|jail| {
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            // An unwritable spool, a local root that doesn't exist and an R2 bucket with no
            // keys in the environment: serving would refuse all three.
            jail.create_file(
                "tachyon.toml",
                "[uploads]\ntemp_dir = \"/dev/null/tachyon\"\nmax_pixels = 1000\n[storage.disk]\nadapter = \"local\"\nroot = \"never-made\"\n[storage.r2]\nadapter = \"s3\"\nbucket = \"m\"\nendpoint = \"https://acct.r2.cloudflarestorage.com\"\n",
            )?;
            let config = Config::load(None)?;
            let limits = config.upload_limits().unwrap();
            assert_eq!(
                limits.storages,
                std::collections::BTreeMap::from([
                    ("default".to_owned(), true),
                    ("disk".to_owned(), true),
                    ("r2".to_owned(), false),
                ])
            );
            assert_eq!(limits.max_pixels, 1000);
            for made in ["never-made", "uploads"] {
                assert!(
                    !jail.directory().join(made).exists(),
                    "{made} isn't created"
                );
            }
            assert!(config.uploads_config().is_err(), "serving would refuse it");
            // A setting that's wrong whatever runs is still refused.
            jail.create_file(
                "tachyon.toml",
                "[storage.r2]\nadapter = \"s3\"\nbucket = \"m\"\naccess_key_id = \"x\"\n",
            )?;
            assert!(Config::load(None)?.upload_limits().is_err());
            Ok(())
        });
    }
}
