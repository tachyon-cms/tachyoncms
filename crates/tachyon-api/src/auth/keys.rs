//! `/api/auth/keys`: API keys for servers and front ends. A key acts as its owner, narrowed
//! by its scopes, and its secret is shown once and stored only as a hash.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use tachyon_core::FieldError;
use tachyon_core::auth::{Scope, parse_scopes};
use tachyon_db::auth::{self, ApiKeyInfo};
use time::OffsetDateTime;
use uuid::Uuid;

use super::principal::Principal;
use super::token_hash;
use crate::documents::json_body;
use crate::{ApiError, AppState};

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct NewKey {
    name: String,
    scopes: Vec<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
}

fn field(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::Validation(vec![FieldError {
        path: path.into(),
        message: message.into(),
    }])
}

/// Keys are made and managed from a signed-in session, never with another key, so a leaked
/// key can't mint more.
fn session_user(principal: &Principal) -> Result<&tachyon_db::auth::Identity, ApiError> {
    match principal {
        Principal::User { identity, .. } => Ok(identity),
        Principal::Key { .. } => Err(ApiError::Forbidden(
            "Manage API keys from a signed-in session, not with another key.".into(),
        )),
        Principal::Anonymous => Err(ApiError::Unauthorized("Sign in first.".into())),
    }
}

/// A fresh prefix (shown, to tell keys apart) and secret (shown once).
fn new_secret() -> (String, String) {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let bytes: [u8; 8] = rand::random();
    let prefix = bytes
        .iter()
        .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
        .collect();
    let secret: [u8; 32] = rand::random();
    (
        prefix,
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret),
    )
}

/// The key's details plus, this once, the key itself.
fn shown(info: &ApiKeyInfo, secret: &str) -> Json<Value> {
    let mut v = json!(info);
    v["key"] = format!("tcy_{}_{secret}", info.prefix).into();
    Json(v)
}

pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let me = session_user(&principal)?;
    let req: NewKey = serde_json::from_value(json_body(body)?)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if req.name.trim().is_empty() {
        return Err(field("name", "is required"));
    }
    let scopes = parse_scopes(&req.scopes).map_err(|m| field("scopes", m))?;
    if scopes.contains(&Scope::Admin) && !me.abilities.users {
        return Err(ApiError::Forbidden(
            "Only an Admin can make a key with the admin scope.".into(),
        ));
    }
    if req
        .expires_at
        .is_some_and(|t| t <= OffsetDateTime::now_utc())
    {
        return Err(field("expiresAt", "must be in the future"));
    }
    // A prefix collision is very unlikely (36^8), but a fresh draw is cheaper than a 500.
    for _ in 0..3 {
        let (prefix, secret) = new_secret();
        match auth::create_api_key(
            &state.pool,
            me.user_id,
            req.name.trim(),
            &prefix,
            &token_hash(&secret),
            &req.scopes,
            req.expires_at,
        )
        .await
        {
            Ok(info) => return Ok((StatusCode::CREATED, shown(&info, &secret))),
            Err(tachyon_db::DbError::UniqueViolation(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(ApiError::Internal(
        "couldn't draw a unique key prefix".into(),
    ))
}

pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Value>, ApiError> {
    let me = session_user(&principal)?;
    Ok(Json(
        json!({ "docs": auth::list_api_keys(&state.pool, me.user_id).await? }),
    ))
}

fn key_id(path: Result<Path<String>, PathRejection>) -> Result<Uuid, ApiError> {
    let Path(id) = path.map_err(|_| ApiError::NotFound)?;
    id.parse().map_err(|_| ApiError::NotFound)
}

pub(crate) async fn revoke(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let me = session_user(&principal)?;
    if auth::revoke_api_key(&state.pool, me.user_id, key_id(path)?).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

pub(crate) async fn rotate(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let me = session_user(&principal)?;
    let id = key_id(path)?;
    for _ in 0..3 {
        let (prefix, secret) = new_secret();
        match auth::rotate_api_key(&state.pool, me.user_id, id, &prefix, &token_hash(&secret)).await
        {
            Ok(Some(info)) => return Ok((StatusCode::CREATED, shown(&info, &secret))),
            Ok(None) => return Err(ApiError::NotFound),
            Err(tachyon_db::DbError::UniqueViolation(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(ApiError::Internal(
        "couldn't draw a unique key prefix".into(),
    ))
}
