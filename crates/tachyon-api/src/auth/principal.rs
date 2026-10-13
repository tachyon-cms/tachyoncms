//! Who is asking: an API key, a session (bearer or cookie), or nobody.

use axum::extract::{FromRequestParts, Request, State};
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, COOKIE, ORIGIN};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tachyon_core::auth::{Scope, parse_scopes};
use tachyon_db::auth::{self, Identity};
use uuid::Uuid;

use super::token_hash;
use crate::{ApiError, AppState};

/// The session cookie. `__Host-` makes browsers insist on `Secure`, `Path=/` and no `Domain`.
pub(crate) const SESSION_COOKIE: &str = "__Host-tachyon_session";

#[derive(Debug, Clone)]
pub(crate) enum Principal {
    Anonymous,
    /// Signed in with a session, sent as a cookie (`cookie: true`) or a bearer token.
    User {
        identity: Identity,
        session: Uuid,
        cookie: bool,
    },
    /// An API key acting for its owner, narrowed by its scopes.
    Key {
        identity: Identity,
        scopes: Vec<Scope>,
    },
}

impl Principal {
    pub(crate) fn identity(&self) -> Option<&Identity> {
        match self {
            Self::Anonymous => None,
            Self::User { identity, .. } | Self::Key { identity, .. } => Some(identity),
        }
    }

    /// The signed-in person, or a 401.
    pub(crate) fn require(&self) -> Result<&Identity, ApiError> {
        self.identity()
            .ok_or_else(|| ApiError::Unauthorized("Sign in first.".into()))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Principal {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<Principal>()
            .cloned()
            .unwrap_or(Principal::Anonymous))
    }
}

/// Middleware: works out the principal once per request. Credentials that are present but
/// don't work are a 401, never a quiet fall back to anonymous, so a broken client finds out.
pub(crate) async fn resolve(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let p = match principal(&state, req.headers()).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    // A browser sends cookies with requests other sites start, so a cookie-signed write must
    // also come from one of ours. Bearer tokens and keys aren't sent automatically.
    if matches!(p, Principal::User { cookie: true, .. })
        && !req.method().is_safe()
        && !same_site(&state, req.headers())
    {
        return ApiError::Forbidden("This request came from another site.".into()).into_response();
    }
    req.extensions_mut().insert(p);
    next.run(req).await
}

/// Whether `Origin` is the configured public URL or one of the allowed origins.
fn same_site(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let config = &state.auth.config;
    // Compared as origins (scheme, host, port), so a public URL with a path still matches.
    let normal = |s: &str| {
        url::Url::parse(s)
            .ok()
            .map(|u| u.origin().ascii_serialization())
    };
    let Some(origin) = normal(origin) else {
        return false;
    };
    std::iter::once(&config.public_url)
        .chain(&config.allowed_origins)
        .any(|o| normal(o).as_deref() == Some(origin.as_str()))
}

async fn principal(state: &AppState, headers: &HeaderMap) -> Result<Principal, ApiError> {
    let limits = state.auth.config.limits();
    if let Some(value) = headers.get(AUTHORIZATION) {
        let malformed = || {
            ApiError::Unauthorized(
                "Use Authorization: Bearer followed by an API key or a session token.".into(),
            )
        };
        let token = value
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .ok_or_else(malformed)?;
        if let Some(rest) = token.strip_prefix("tcy_") {
            return key(state, rest).await;
        }
        if token.starts_with("tcs_") {
            let (session, identity) =
                auth::session_by_hash(&state.pool, &token_hash(token), &limits)
                    .await?
                    .ok_or_else(|| {
                        ApiError::Unauthorized("This session has ended. Sign in again.".into())
                    })?;
            return Ok(Principal::User {
                identity,
                session,
                cookie: false,
            });
        }
        return Err(malformed());
    }
    let Some(token) = session_cookie(headers) else {
        return Ok(Principal::Anonymous);
    };
    // An expired or revoked cookie just means signed out: browsers send stale cookies.
    Ok(
        match auth::session_by_hash(&state.pool, &token_hash(&token), &limits).await? {
            Some((session, identity)) => Principal::User {
                identity,
                session,
                cookie: true,
            },
            None => Principal::Anonymous,
        },
    )
}

/// `tcy_<prefix>_<secret>`: found by its prefix, checked by its secret's hash.
async fn key(state: &AppState, rest: &str) -> Result<Principal, ApiError> {
    let refused = || {
        ApiError::Unauthorized("This API key isn't valid, or has expired or been revoked.".into())
    };
    let (prefix, secret) = rest.split_once('_').ok_or_else(refused)?;
    let (row, identity) = auth::api_key_by_prefix(&state.pool, prefix)
        .await?
        .ok_or_else(refused)?;
    if !bool::from(token_hash(secret).as_slice().ct_eq(&row.secret_hash)) {
        return Err(refused());
    }
    auth::touch_api_key(&state.pool, row.id).await?;
    // Scopes were checked when the key was made, so this only fails on a hand-edited row.
    let scopes = parse_scopes(&row.scopes).map_err(|_| refused())?;
    Ok(Principal::Key { identity, scopes })
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.to_owned())
}
