//! Hashing passwords with Argon2id, and checking them against known breaches.

use std::sync::Arc;
use std::time::Duration;

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use sha1::{Digest, Sha1};

/// Argon2id with the configured memory, time cost 2 and parallelism 1 (the OWASP baseline).
/// Hashing runs off the async threads, since it's deliberately slow.
#[derive(Clone)]
pub(crate) struct Hasher {
    argon2: Argon2<'static>,
    /// A real hash of a random password, so an unknown email costs a real verify.
    dummy: Arc<String>,
}

impl Hasher {
    pub(crate) fn new(memory_kib: u32) -> Result<Self, String> {
        let params = Params::new(memory_kib, 2, 1, None).map_err(|e| e.to_string())?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let random: [u8; 32] = rand::random();
        let dummy = argon2
            .hash_password(&random)
            .map_err(|e| e.to_string())?
            .to_string();
        Ok(Self {
            argon2,
            dummy: Arc::new(dummy),
        })
    }

    pub(crate) async fn hash(&self, password: &str) -> Result<String, String> {
        let argon2 = self.argon2.clone();
        let password = password.to_owned();
        tokio::task::spawn_blocking(move || {
            argon2
                .hash_password(password.as_bytes())
                .map(|h| h.to_string())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Whether `password` matches `hash`. A malformed hash never matches.
    pub(crate) async fn verify(&self, password: &str, hash: &str) -> bool {
        let argon2 = self.argon2.clone();
        let (password, hash) = (password.to_owned(), hash.to_owned());
        tokio::task::spawn_blocking(move || {
            PasswordHash::new(&hash)
                .is_ok_and(|parsed| argon2.verify_password(password.as_bytes(), &parsed).is_ok())
        })
        .await
        .unwrap_or(false)
    }

    /// Spends the time a real check takes, for logins with an unknown email.
    pub(crate) async fn dummy_verify(&self) {
        let dummy = self.dummy.as_str().to_owned();
        self.verify("not the password", &dummy).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Breach {
    Found,
    Clear,
    /// The service couldn't be asked; treated as clear, with a warning logged.
    Unknown,
}

/// Asks the Have I Been Pwned range API whether a password is known from a breach. Only
/// the first five characters of its SHA-1 leave the server (k-anonymity), and responses are
/// padded so their size says nothing either.
#[derive(Clone)]
pub(crate) struct BreachCheck {
    client: reqwest::Client,
    base: String,
}

impl BreachCheck {
    pub(crate) fn new(base: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .user_agent("TachyonCMS")
            .build()
            .expect("an HTTP client with a timeout always builds");
        Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    pub(crate) async fn check(&self, password: &str) -> Breach {
        let hash = hex::encode_upper(Sha1::digest(password.as_bytes()));
        let (prefix, suffix) = hash.split_at(5);
        let response = self
            .client
            .get(format!("{}/range/{prefix}", self.base))
            .header("Add-Padding", "true")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status);
        let body = match response {
            Ok(r) => r.text().await,
            Err(e) => Err(e),
        };
        match body {
            Ok(body) => {
                let found = body.lines().any(|line| {
                    line.split_once(':').is_some_and(|(s, count)| {
                        s.trim().eq_ignore_ascii_case(suffix) && count.trim() != "0"
                    })
                });
                if found { Breach::Found } else { Breach::Clear }
            }
            Err(error) => {
                tracing::warn!(%error, "couldn't check the password against known breaches");
                Breach::Unknown
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::Path;
    use axum::routing::get;
    use std::time::{Duration, Instant};

    fn hasher() -> Hasher {
        // Small memory keeps the tests quick; production uses the configured 19 MiB.
        Hasher::new(1024).unwrap()
    }

    #[tokio::test]
    async fn hashes_verify_and_differ_each_time() {
        let h = hasher();
        let a = h.hash("correct horse battery").await.unwrap();
        let b = h.hash("correct horse battery").await.unwrap();
        assert_ne!(a, b, "a fresh salt each time");
        assert!(a.starts_with("$argon2id$"));
        assert!(h.verify("correct horse battery", &a).await);
        assert!(!h.verify("wrong horse battery", &a).await);
        assert!(!h.verify("anything", "not a hash").await);
    }

    #[tokio::test]
    async fn the_dummy_check_takes_as_long_as_a_real_one() {
        let h = Hasher::new(19_456).unwrap();
        let real = h.hash("correct horse battery").await.unwrap();
        let start = Instant::now();
        h.verify("nope nope nope", &real).await;
        let verify = start.elapsed();
        let start = Instant::now();
        h.dummy_verify().await;
        let dummy = start.elapsed();
        let (lo, hi) = (verify.min(dummy), verify.max(dummy));
        assert!(hi < lo * 3, "verify {verify:?} vs dummy {dummy:?}");
    }

    /// A stand-in for the breach service: knows one password.
    async fn stub(knows: &'static str, delay: Duration) -> String {
        let hash = hex::encode_upper(<sha1::Sha1 as sha1::Digest>::digest(knows.as_bytes()));
        let (prefix, suffix) = (hash[..5].to_owned(), hash[5..].to_owned());
        let app = Router::new().route(
            "/range/{prefix}",
            get(move |Path(p): Path<String>| {
                let (prefix, suffix) = (prefix.clone(), suffix.clone());
                async move {
                    tokio::time::sleep(delay).await;
                    if p == prefix {
                        format!("0000000000000000000000000000000000A:0\r\n{suffix}:42\r\n")
                    } else {
                        "0000000000000000000000000000000000A:0\r\n".to_owned()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn breach_check_finds_a_listed_password() {
        let url = stub("password123456", Duration::ZERO).await;
        let check = BreachCheck::new(&url);
        assert_eq!(check.check("password123456").await, Breach::Found);
        assert_eq!(check.check("a much better passphrase").await, Breach::Clear);
    }

    #[tokio::test]
    async fn breach_check_passes_when_the_service_is_down_or_slow() {
        let check = BreachCheck::new("http://127.0.0.1:9");
        assert_eq!(check.check("password123456").await, Breach::Unknown);
        let slow = stub("password123456", Duration::from_secs(5)).await;
        let check = BreachCheck::new(&slow);
        let start = Instant::now();
        assert_eq!(check.check("password123456").await, Breach::Unknown);
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "gives up after about 2 seconds"
        );
    }
}
