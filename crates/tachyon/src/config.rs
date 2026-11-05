use std::net::SocketAddr;
use std::time::Duration;

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::Deserialize;
use tachyon_api::auth::AuthConfig;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub database_url: String,
    pub listen: SocketAddr,
    /// Where the admin and API are reached from browsers, such as `https://cms.example.com`.
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default)]
    pub auth: AuthSettings,
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

/// `15m`, `12h`, `7d`: a whole number and a unit.
fn duration(name: &str, text: &str) -> Result<Duration, String> {
    let bad = || {
        format!(
            "auth.{name}: `{text}` isn't a duration; use a number and s, m, h or d, like 15m or 7d"
        )
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
    let ok = web_url(origin).is_some_and(|u| {
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
        let session_idle = duration("session_idle", &a.session_idle)?;
        let session_absolute = duration("session_absolute", &a.session_absolute)?;
        if session_absolute < session_idle {
            return Err(format!(
                "auth.session_absolute ({}) is shorter than auth.session_idle ({}), so the idle limit would never apply",
                a.session_absolute, a.session_idle
            ));
        }
        Ok(AuthConfig {
            public_url: self
                .public_url
                .clone()
                .unwrap_or_else(|| format!("http://localhost:{}", self.listen.port())),
            allowed_origins: a.allowed_origins.clone(),
            session_idle,
            session_absolute,
            lockout_attempts: a.lockout_attempts,
            lockout_window: duration("lockout_window", &a.lockout_window)?,
            lockout_duration: duration("lockout_duration", &a.lockout_duration)?,
            argon2_memory_kib: a.argon2_memory_kib,
            breach_check: a.breach_check,
            breach_check_url: a.breach_check_url.clone(),
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
            .merge(Env::prefixed("TACHYON_").split("__"))
            .extract()?;
        if config.database_url.trim().is_empty() {
            return Err("database_url is empty".to_owned().into());
        }
        Ok(config)
    }
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // figment::Jail closures return figment::Error
mod tests {
    use super::Config;
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
                "unknown key `publc_url` in tachyon.toml; the top-level keys are database_url, listen, public_url, auth"
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
    fn database_url_is_required() {
        Jail::expect_with(|jail| {
            // Clearing the whole environment would race with tests that need DATABASE_URL.
            jail.set_env("TACHYON_DATABASE_URL", "");
            assert!(Config::load(None).is_err());
            Ok(())
        });
    }
}
