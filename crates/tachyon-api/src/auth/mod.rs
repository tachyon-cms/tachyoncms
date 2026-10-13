//! Signing in: passwords, sessions, API keys, and who is asking.

pub(crate) mod password;
pub(crate) mod principal;
mod roles;
mod routes;
mod users;

pub(crate) use users::ACCOUNT_KEYS;

use std::time::Duration;

use tachyon_core::auth::{Action, Decision, decide, decide_anonymous};
use tachyon_db::auth::Limits;
use tachyon_db::documents::Actor;

pub(crate) use routes::routes;

use password::{BreachCheck, Hasher};

/// A new session token: 32 random bytes, sent as `tcs_<base64url>`.
pub(crate) fn new_session_token() -> String {
    use base64::Engine;
    let bytes: [u8; 32] = rand::random();
    format!(
        "tcs_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

/// What's stored for a token or key secret: its SHA-256, never the thing itself.
pub(crate) fn token_hash(token: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes()).into()
}

/// What `principal` may do with `action` on `collection`. Refused is a 401 when nobody is
/// signed in and a 403 otherwise.
pub(crate) fn authorize(
    principal: &principal::Principal,
    action: Action,
    collection: &str,
) -> Result<Decision, crate::ApiError> {
    use principal::Principal;
    let decision = match principal {
        Principal::Anonymous => decide_anonymous(action),
        Principal::User { identity, .. } => decide(&identity.abilities, None, action, collection),
        Principal::Key {
            identity, scopes, ..
        } => decide(&identity.abilities, Some(scopes), action, collection),
    };
    match (decision, principal) {
        (Decision::Deny, Principal::Anonymous) => {
            Err(crate::ApiError::Unauthorized("Sign in first.".into()))
        }
        (Decision::Deny, _) => Err(crate::ApiError::Forbidden(
            "You don't have permission to do this.".into(),
        )),
        (d, _) => Ok(d),
    }
}

/// Who the database should record as writing, and whether they may only touch their own.
pub(crate) fn actor(principal: &principal::Principal, decision: Decision) -> Option<Actor> {
    principal.identity().map(|i| Actor {
        id: i.user_id,
        own_only: decision == Decision::OwnOnly,
    })
}

/// What the sign-in code shares across requests.
pub(crate) struct AuthState {
    pub config: AuthConfig,
    pub hasher: Hasher,
    pub breach: Option<BreachCheck>,
}

impl AuthState {
    pub(crate) fn new(config: AuthConfig) -> Result<Self, String> {
        let hasher = Hasher::new(config.argon2_memory_kib)?;
        let breach = config
            .breach_check
            .then(|| BreachCheck::new(&config.breach_check_url));
        Ok(Self {
            config,
            hasher,
            breach,
        })
    }
}

/// Settings for signing in. `Default` suits local development and tests; the `tachyon`
/// binary builds production values from its configuration.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Where the admin and API are served, such as `https://cms.example.com`. Cookie-signed
    /// writes must come from here (or `allowed_origins`).
    pub public_url: String,
    pub allowed_origins: Vec<String>,
    pub session_idle: Duration,
    pub session_absolute: Duration,
    pub lockout_attempts: i32,
    pub lockout_window: Duration,
    pub lockout_duration: Duration,
    /// Argon2id memory in KiB; time cost 2 and parallelism 1 are fixed.
    pub argon2_memory_kib: u32,
    /// Refuse passwords that appear in known breaches.
    pub breach_check: bool,
    pub breach_check_url: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            public_url: "http://localhost:3000".into(),
            allowed_origins: Vec::new(),
            session_idle: Duration::from_secs(7 * 86_400),
            session_absolute: Duration::from_secs(30 * 86_400),
            lockout_attempts: 5,
            lockout_window: Duration::from_secs(15 * 60),
            lockout_duration: Duration::from_secs(15 * 60),
            argon2_memory_kib: 19_456,
            // Off by default so tests and offline development never call out; the server's
            // configuration turns it on.
            breach_check: false,
            breach_check_url: "https://api.pwnedpasswords.com".into(),
        }
    }
}

impl AuthConfig {
    pub(crate) fn limits(&self) -> Limits {
        Limits {
            idle: self.session_idle,
            absolute: self.session_absolute,
            attempts: self.lockout_attempts,
            window: self.lockout_window,
            lockout: self.lockout_duration,
        }
    }
}

/// For this crate's own tests: the API, set up and signed in as its first Admin.
#[cfg(test)]
pub(crate) async fn admin_router(pool: sqlx::PgPool) -> axum::Router {
    use axum::body::Body;
    use axum::http::{HeaderValue, Request};
    use tower::ServiceExt;
    let app = crate::router(pool).await.unwrap();
    let call = |uri: &'static str, body: serde_json::Value| {
        let app = app.clone();
        async move {
            let req = Request::post(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            app.oneshot(req).await.unwrap()
        }
    };
    let password = "a long enough passphrase";
    let setup =
        serde_json::json!({"email": "admin@example.com", "name": "Ada", "password": password});
    assert!(call("/api/setup", setup).await.status().is_success());
    let login = serde_json::json!({"email": "admin@example.com", "password": password});
    let res = call("/api/auth/login", login).await;
    let cookie = res.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let cookie = HeaderValue::from_str(&cookie).unwrap();
    app.layer(axum::middleware::map_request(
        move |mut req: Request<Body>| {
            let cookie = cookie.clone();
            async move {
                req.headers_mut().insert("cookie", cookie);
                req.headers_mut()
                    .insert("origin", HeaderValue::from_static("http://localhost:3000"));
                req
            }
        },
    ))
}
