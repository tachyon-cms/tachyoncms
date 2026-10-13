//! `/api/setup` and `/api/auth/*`: the first Admin, signing in and out, sessions, passwords.

use axum::Json;
use axum::Router;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::header::{SET_COOKIE, USER_AGENT};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use tachyon_core::auth::check_password;
use tachyon_core::{FieldError, is_email};
use tachyon_db::DbError;
use tachyon_db::auth::{self, NewAccount};
use tachyon_db::documents;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use super::password::Breach;
use super::principal::{Principal, SESSION_COOKIE};
use super::{new_session_token, token_hash};
use crate::documents::json_body;
use crate::populate::to_value;
use crate::{ApiError, AppState};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/setup", get(setup_needed).post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/auth/sessions", get(sessions))
        .route("/api/auth/sessions/{id}", delete(end_session))
        .route("/api/auth/password", post(change_password))
}

fn parse<T: for<'de> Deserialize<'de>>(
    body: Result<Json<Value>, JsonRejection>,
) -> Result<T, ApiError> {
    serde_json::from_value(json_body(body)?).map_err(|e| ApiError::BadRequest(e.to_string()))
}

fn field(path: &str, message: &str) -> FieldError {
    FieldError {
        path: path.into(),
        message: message.into(),
    }
}

/// A new password must be long enough and not known from a breach.
async fn check_new_password(state: &AppState, path: &str, password: &str) -> Result<(), ApiError> {
    check_password(password).map_err(|m| ApiError::Validation(vec![field(path, m)]))?;
    if let Some(breach) = &state.auth.breach
        && breach.check(password).await == Breach::Found
    {
        return Err(ApiError::Validation(vec![field(
            path,
            "This password has appeared in a data breach. Choose another.",
        )]));
    }
    Ok(())
}

/// A user as the API shows them: their profile document plus email and role, never the hash.
pub(crate) async fn user_json(state: &AppState, user_id: Uuid) -> Result<Value, ApiError> {
    let account = auth::account(&state.pool, user_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let users = auth::ensure_users_collection(&state.pool).await?;
    let doc = documents::get(&state.pool, users.id, user_id).await?;
    let mut value = to_value(&doc)?;
    value["email"] = account.email.into();
    value["role"] = account.role.into();
    Ok(value)
}

async fn setup_needed(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({ "needed": !auth::has_accounts(&state.pool).await? }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupRequest {
    email: String,
    name: String,
    password: String,
}

async fn setup(
    State(state): State<AppState>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let req: SetupRequest = parse(body)?;
    let email = req.email.trim();
    let mut errors = Vec::new();
    if !is_email(email) {
        errors.push(field("email", "expected an email address"));
    }
    if req.name.trim().is_empty() {
        errors.push(field("name", "is required"));
    }
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    // Refuse early when setup has run, before spending time hashing.
    if auth::has_accounts(&state.pool).await? {
        return Err(setup_done());
    }
    check_new_password(&state, "password", &req.password).await?;
    let hash = state
        .auth
        .hasher
        .hash(&req.password)
        .await
        .map_err(ApiError::Internal)?;
    let users = auth::ensure_users_collection(&state.pool).await?;
    state.schemas.reload(&state.pool).await?;
    let new = NewAccount {
        email,
        name: req.name.trim(),
        password_hash: &hash,
        role: "admin",
    };
    let id = match auth::create_account(&state.pool, &users, &new, true).await {
        Ok(id) => id,
        Err(DbError::Conflict(_)) => return Err(setup_done()),
        Err(e) => return Err(e.into()),
    };
    Ok((StatusCode::CREATED, Json(user_json(&state, id).await?)))
}

fn setup_done() -> ApiError {
    ApiError::Conflict("Setup has already been done. Sign in instead.".into())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    email: String,
    password: String,
    /// Also return the session token, for front ends that send `Authorization: Bearer`.
    #[serde(default)]
    bearer: bool,
}

fn wrong() -> ApiError {
    ApiError::Unauthorized("Email or password is wrong.".into())
}

fn session_cookie(token: &str, max_age: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={max_age}"
    ))
    .expect("tokens are base64url")
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Response, ApiError> {
    let req: LoginRequest = parse(body)?;
    let limits = state.auth.config.limits();
    let hasher = &state.auth.hasher;
    // Every refusal takes about the same time and says the same thing, so nobody can learn
    // which emails have accounts or which accounts are locked.
    let Some(account) = auth::account_by_email(&state.pool, req.email.trim()).await? else {
        hasher.dummy_verify().await;
        return Err(wrong());
    };
    let matches = hasher.verify(&req.password, &account.password_hash).await;
    if account.locked {
        return Err(wrong());
    }
    if !matches {
        auth::record_failure(&state.pool, account.user_id, &limits).await?;
        return Err(wrong());
    }
    auth::reset_failures(&state.pool, account.user_id).await?;

    let token = new_session_token();
    let user_agent = headers.get(USER_AGENT).and_then(|v| v.to_str().ok());
    auth::create_session(
        &state.pool,
        account.user_id,
        &token_hash(&token),
        &limits,
        user_agent,
        None,
    )
    .await?;
    let expires = OffsetDateTime::now_utc() + limits.absolute;
    let mut body = json!({
        "user": user_json(&state, account.user_id).await?,
        "exp": expires.format(&Rfc3339).map_err(|e| ApiError::Internal(e.to_string()))?,
    });
    if req.bearer {
        body["token"] = token.clone().into();
    }
    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        session_cookie(&token, limits.absolute.as_secs()),
    );
    Ok(response)
}

async fn logout(State(state): State<AppState>, principal: Principal) -> Result<Response, ApiError> {
    let Principal::User {
        identity, session, ..
    } = &principal
    else {
        return Err(ApiError::Unauthorized("Sign in first.".into()));
    };
    auth::revoke_session(&state.pool, identity.user_id, *session).await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, session_cookie("", 0));
    Ok(response)
}

async fn me(State(state): State<AppState>, principal: Principal) -> Result<Json<Value>, ApiError> {
    let identity = principal.require()?;
    Ok(Json(user_json(&state, identity.user_id).await?))
}

async fn sessions(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Value>, ApiError> {
    let identity = principal.require()?;
    let current = match &principal {
        Principal::User { session, .. } => Some(*session),
        _ => None,
    };
    let docs: Vec<Value> = auth::list_sessions(&state.pool, identity.user_id)
        .await?
        .into_iter()
        .map(|s| {
            let mut v = json!(s);
            v["current"] = (Some(s.id) == current).into();
            v
        })
        .collect();
    Ok(Json(json!({ "docs": docs })))
}

async fn end_session(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let identity = principal.require()?;
    let Path(id) = path.map_err(|_| ApiError::NotFound)?;
    let id: Uuid = id.parse().map_err(|_| ApiError::NotFound)?;
    if auth::revoke_session(&state.pool, identity.user_id, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordChange {
    current: String,
    new: String,
}

/// Changing a password ends every other session, so a stolen one stops working.
async fn change_password(
    State(state): State<AppState>,
    principal: Principal,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Principal::User {
        identity, session, ..
    } = &principal
    else {
        return Err(match principal {
            Principal::Anonymous => ApiError::Unauthorized("Sign in first.".into()),
            _ => ApiError::Forbidden(
                "Change a password from a signed-in session, not with an API key.".into(),
            ),
        });
    };
    let req: PasswordChange = parse(body)?;
    let account = auth::account(&state.pool, identity.user_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !state
        .auth
        .hasher
        .verify(&req.current, &account.password_hash)
        .await
    {
        return Err(ApiError::Forbidden(
            "Your current password is wrong.".into(),
        ));
    }
    check_new_password(&state, "new", &req.new).await?;
    let hash = state
        .auth
        .hasher
        .hash(&req.new)
        .await
        .map_err(ApiError::Internal)?;
    auth::set_password(&state.pool, identity.user_id, &hash).await?;
    auth::revoke_other_sessions(&state.pool, identity.user_id, Some(*session)).await?;
    Ok(StatusCode::NO_CONTENT)
}
