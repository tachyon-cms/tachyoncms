//! `/api/users`: admin users. Profiles are documents in the `users` collection; email, role
//! and password live in their accounts and are changed here, never as document fields.

use std::collections::HashMap;

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde_json::{Map, Value};
use tachyon_core::auth::{Action, Decision, check_password};
use tachyon_core::{FieldError, is_email, validate_document};
use tachyon_db::auth::{self, NewAccount};
use tachyon_db::documents::{self, Actor};
use uuid::Uuid;

use super::principal::Principal;
use super::routes::{check_new_password, user_json};
use super::{actor, authorize};
use crate::access::{Access, Reach};
use crate::documents::{any_collection, json_body, unique_error};
use crate::populate::{check_links, link_ids, link_targets, prepare, vouched_links};
use crate::{ApiError, AppState};

pub(crate) use tachyon_db::auth::ACCOUNT_KEYS;

fn field(path: &str, message: &str) -> FieldError {
    FieldError {
        path: path.into(),
        message: message.into(),
    }
}

type Object = Map<String, Value>;

/// Splits a request body into account keys and the profile document.
fn split(body: Value) -> Result<(Object, Object), ApiError> {
    let Value::Object(mut profile) = body else {
        return Err(ApiError::Validation(vec![field(
            "",
            "expected a JSON object",
        )]));
    };
    let mut account = Map::new();
    for key in ACCOUNT_KEYS {
        if let Some(v) = profile.remove(*key) {
            account.insert((*key).to_owned(), v);
        }
    }
    Ok((account, profile))
}

fn text<'a>(account: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, ApiError> {
    match account.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(ApiError::Validation(vec![field(key, "expected text")])),
    }
}

fn manages_users(principal: &Principal) -> bool {
    authorize(principal, Action::Users, "users").is_ok()
}

/// Admins may act on anyone; everyone else only on themselves.
fn self_or_admin(principal: &Principal, id: Uuid) -> Result<(), ApiError> {
    let me = principal.require()?;
    if me.user_id == id || manages_users(principal) {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "You can only see and change your own account.".into(),
        ))
    }
}

/// Adds each user's email and role to their profile documents.
async fn with_accounts(state: &AppState, docs: &mut [Value]) -> Result<(), ApiError> {
    let ids: Vec<Uuid> = docs
        .iter()
        .filter_map(|d| d.get("id")?.as_str()?.parse().ok())
        .collect();
    let found: HashMap<Uuid, (String, String)> = auth::emails_and_roles(&state.pool, &ids)
        .await?
        .into_iter()
        .map(|(id, email, role)| (id, (email, role)))
        .collect();
    for doc in docs {
        let id = doc
            .get("id")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok());
        if let Some((email, role)) = id.and_then(|id: Uuid| found.get(&id)) {
            doc["email"] = email.clone().into();
            doc["role"] = role.clone().into();
        }
    }
    Ok(())
}

pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    authorize(&principal, Action::Users, "users")?;
    let users = any_collection(&state, auth::USERS)?;
    let access = Access::new(&state, principal, Reach::From(&users)).await?;
    let Json(mut body) = crate::documents::list_in(&state, &access, users, raw).await?;
    if let Some(Value::Array(docs)) = body.get_mut("docs") {
        with_accounts(&state, docs).await?;
    }
    Ok(Json(body))
}

pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = user_id(path)?;
    self_or_admin(&principal, id)?;
    Ok(Json(user_json(&state, id).await?))
}

fn user_id(path: Result<Path<String>, PathRejection>) -> Result<Uuid, ApiError> {
    let Path(id) = path.map_err(|_| ApiError::NotFound)?;
    id.parse().map_err(|_| ApiError::NotFound)
}

/// A role that exists, or a field error naming it.
async fn known_role(state: &AppState, role: &str) -> Result<(), ApiError> {
    if auth::role_abilities(&state.pool, role).await?.is_none() {
        return Err(ApiError::Validation(vec![field(
            "role",
            &format!("there's no role `{role}`"),
        )]));
    }
    Ok(())
}

pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let decision = authorize(&principal, Action::Users, "users")?;
    let (account, profile) = split(json_body(body)?)?;
    let email = text(&account, "email")?
        .unwrap_or_default()
        .trim()
        .to_owned();
    let password = text(&account, "password")?.unwrap_or_default().to_owned();
    let role = text(&account, "role")?.unwrap_or("viewer").to_owned();
    let mut errors = Vec::new();
    if !is_email(&email) {
        errors.push(field("email", "expected an email address"));
    }
    if let Err(m) = check_password(&password) {
        errors.push(field("password", m));
    }
    let users = any_collection(&state, auth::USERS)?;
    let data = validate_document(users.schema.data(), &Value::Object(profile), None);
    if let Err(more) = &data {
        errors.extend(more.iter().cloned());
    }
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    known_role(&state, &role).await?;
    check_new_password(&state, "password", &password).await?;
    let targets = link_targets(&state, &users).await?;
    let prepared = prepare(&targets, &users, data.map_err(ApiError::Validation)?)?;
    let hash = state
        .auth
        .hasher
        .hash(&password)
        .await
        .map_err(ApiError::Internal)?;
    let new = NewAccount {
        email: &email,
        name: "",
        password_hash: &hash,
        role: &role,
    };
    let id = auth::create_account_with(
        &state.pool,
        &users,
        &new,
        &prepared,
        false,
        actor(&principal, decision).map(|a| a.id),
    )
    .await
    .map_err(|e| unique_error(&users, e.into()))?;
    Ok((StatusCode::CREATED, Json(user_json(&state, id).await?)))
}

pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = user_id(path)?;
    self_or_admin(&principal, id)?;
    let admin = manages_users(&principal);
    let (account, profile) = split(json_body(body)?)?;
    let current = auth::account(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;

    // Everything is checked before anything is written, then written in one transaction,
    // so a refused part leaves the whole account as it was.
    let role = text(&account, "role")?;
    if let Some(role) = role {
        if !admin {
            return Err(ApiError::Forbidden(
                "Only an Admin can change a role.".into(),
            ));
        }
        known_role(&state, role).await?;
    }
    let email = text(&account, "email")?.map(str::trim);
    if let Some(email) = email {
        if !is_email(email) {
            return Err(ApiError::Validation(vec![field(
                "email",
                "expected an email address",
            )]));
        }
        if !admin {
            // Taking over an account starts with changing its email, so prove it's yours.
            let confirmed = match text(&account, "currentPassword")? {
                Some(p) => state.auth.hasher.verify(p, &current.password_hash).await,
                None => false,
            };
            if !confirmed {
                return Err(ApiError::Forbidden(
                    "Confirm your current password (currentPassword) to change your email.".into(),
                ));
            }
        }
    }
    let password_hash = match text(&account, "password")? {
        None => None,
        Some(password) => {
            if !admin {
                return Err(ApiError::Forbidden(
                    "Change your own password with POST /api/auth/password.".into(),
                ));
            }
            check_new_password(&state, "password", password).await?;
            let hash = state
                .auth
                .hasher
                .hash(password)
                .await
                .map_err(ApiError::Internal)?;
            Some(hash)
        }
    };
    let users = any_collection(&state, auth::USERS)?;
    let targets = link_targets(&state, &users).await?;
    let patch = Value::Object(profile);
    let apply = |current: &Map<String, Value>| {
        let data = validate_document(users.schema.data(), &patch, Some(current))
            .map_err(ApiError::Validation)?;
        prepare(&targets, &users, data)
    };
    let vouched = if patch.as_object().is_some_and(|p| !p.is_empty()) {
        // Links the profile gains must point at documents the caller can read (all of
        // them, for an Admin editing someone else's).
        let access = Access::new(&state, principal.clone(), Reach::From(&users)).await?;
        let stored = documents::get(&state.pool, users.id, id).await?.data.0;
        let next = apply(&stored)?;
        Some(check_links(&state, &access, &next, &link_ids(&users, &stored)).await?)
    } else {
        None
    };

    // The profile's row first, then the account's: the order a delete locks them in.
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(tachyon_db::DbError::from)?;
    if let Some(vouched) = &vouched {
        let who = principal.identity().map(|i| Actor {
            id: i.user_id,
            own_only: false,
        });
        documents::update_in(
            &mut tx,
            users.id,
            users.schema_version,
            id,
            who,
            None,
            |current| {
                let next = apply(current)?;
                vouched_links(&next, &link_ids(&users, current), vouched)?;
                Ok(next)
            },
        )
        .await
        .map_err(|e| unique_error(&users, e))?;
    }
    if let Some(role) = role {
        auth::set_role(&mut tx, id, role).await?;
    }
    if let Some(email) = email {
        // Saying the address is taken would tell any signed-in user who has an account.
        auth::set_email(&mut *tx, id, email)
            .await
            .map_err(|e| match e {
                tachyon_db::DbError::Conflict(_) => {
                    ApiError::Conflict("That email can't be used. Choose another.".into())
                }
                e => e.into(),
            })?;
    }
    if let Some(hash) = &password_hash {
        auth::set_password(&mut *tx, id, hash).await?;
        // Every other session of theirs ends; an Admin setting their own keeps this one.
        let keep = match &principal {
            Principal::User {
                identity, session, ..
            } if identity.user_id == id => Some(*session),
            _ => None,
        };
        auth::revoke_other_sessions(&mut *tx, id, keep).await?;
    }
    tx.commit().await.map_err(tachyon_db::DbError::from)?;
    Ok(Json(user_json(&state, id).await?))
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let id = user_id(path)?;
    authorize(&principal, Action::Users, "users")?;
    let users = any_collection(&state, auth::USERS)?;
    let who = actor(&principal, Decision::Allow);
    documents::delete_as(&state.pool, users.id, id, who, None).await?;
    Ok(StatusCode::NO_CONTENT)
}
