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
        Ok(AuthConfig {
            public_url: self
                .public_url
                .clone()
                .unwrap_or_else(|| format!("http://localhost:{}", self.listen.port())),
            allowed_origins: a.allowed_origins.clone(),
            session_idle: duration("session_idle", &a.session_idle)?,
            session_absolute: duration("session_absolute", &a.session_absolute)?,
            lockout_attempts: a.lockout_attempts,
            lockout_window: duration("lockout_window", &a.lockout_window)?,
            lockout_duration: duration("lockout_duration", &a.lockout_duration)?,
            argon2_memory_kib: a.argon2_memory_kib,
            breach_check: a.breach_check,
            breach_check_url: a.breach_check_url.clone(),
        })
    }

    /// Defaults, then the TOML file if it exists, then `TACHYON_*` environment variables.
    // figment's own error type; this runs once at startup.
    #[allow(clippy::result_large_err)]
    pub fn load(path: &str) -> Result<Self, figment::Error> {
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
            out = Some(Config::load("tachyon.toml")?);
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
            let config = Config::load("tachyon.toml")?;
            assert_eq!(config.database_url, "postgres://env");
            assert_eq!(config.listen, "0.0.0.0:8080".parse().unwrap());
            Ok(())
        });
    }

    #[test]
    fn listens_on_localhost_by_default_and_file_is_optional() {
        Jail::expect_with(|jail| {
            jail.set_env("TACHYON_DATABASE_URL", "postgres://env");
            let config = Config::load("missing.toml")?;
            assert_eq!(config.listen, "127.0.0.1:3000".parse().unwrap());
            Ok(())
        });
    }

    #[test]
    fn database_url_is_required() {
        Jail::expect_with(|jail| {
            // Clearing the whole environment would race with tests that need DATABASE_URL.
            jail.set_env("TACHYON_DATABASE_URL", "");
            assert!(Config::load("missing.toml").is_err());
            Ok(())
        });
    }
}
