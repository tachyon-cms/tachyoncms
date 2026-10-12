use std::net::SocketAddr;

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub database_url: String,
    pub listen: SocketAddr,
}

impl Config {
    /// Defaults, then the TOML file if it exists, then `TACHYON_*` environment variables.
    // figment's own error type; this runs once at startup.
    #[allow(clippy::result_large_err)]
    pub fn load(path: &str) -> Result<Self, figment::Error> {
        let config: Self = Figment::new()
            .merge(Serialized::default("listen", "127.0.0.1:3000"))
            .merge(Toml::file(path))
            .merge(Env::prefixed("TACHYON_"))
            .extract()?;
        if config.database_url.trim().is_empty() {
            return Err("database_url is empty".to_owned().into());
        }
        Ok(config)
    }
}

/// Until sign-in ships, anyone who can reach the server can change content, so say so when
/// it listens anywhere but this machine.
pub fn exposure_warning(addr: SocketAddr) -> Option<&'static str> {
    (!addr.ip().to_canonical().is_loopback()).then_some(
        "listening beyond this machine without authentication: anyone who can reach this address can change content. Bind to 127.0.0.1 until sign-in ships.",
    )
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // figment::Jail closures return figment::Error
mod tests {
    use super::Config;
    use figment::Jail;

    #[test]
    fn warns_only_off_loopback() {
        let warn = |a: &str| super::exposure_warning(a.parse().unwrap());
        assert!(warn("127.0.0.1:3000").is_none());
        assert!(warn("[::1]:3000").is_none());
        assert!(warn("0.0.0.0:3000").is_some());
        assert!(warn("192.168.1.5:3000").is_some());
        assert!(warn("[::]:3000").is_some());
        assert!(
            warn("[::ffff:127.0.0.1]:3000").is_none(),
            "IPv4 loopback written as IPv6"
        );
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
