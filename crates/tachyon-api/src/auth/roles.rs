//! `/api/admin/roles`: the built-in roles, and custom ones an Admin adds.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tachyon_core::FieldError;
use tachyon_core::access::names_role;
use tachyon_core::auth::{Abilities, Action};
use tachyon_db::auth;

use super::authorize;
use super::principal::Principal;
use crate::documents::json_body;
use crate::{ApiError, AppState};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRole {
    slug: String,
    name: String,
    abilities: Abilities,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleChange {
    name: String,
    abilities: Abilities,
}

fn parse<T: for<'de> Deserialize<'de>>(
    body: Result<Json<Value>, JsonRejection>,
) -> Result<T, ApiError> {
    serde_json::from_value(json_body(body)?).map_err(|e| ApiError::BadRequest(e.to_string()))
}

fn check(slug: &str, name: &str) -> Result<(), ApiError> {
    let mut errors = Vec::new();
    let ok = slug.len() <= 64
        && slug.starts_with(|c: char| c.is_ascii_lowercase())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        errors.push(FieldError {
            path: "slug".into(),
            message: "use lowercase letters, digits and underscores, starting with a letter".into(),
        });
    }
    if name.trim().is_empty() {
        errors.push(FieldError {
            path: "name".into(),
            message: "is required".into(),
        });
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::Validation(errors))
    }
}

pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Value>, ApiError> {
    principal.require()?;
    Ok(Json(
        json!({ "docs": auth::roles_list(&state.pool).await? }),
    ))
}

pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    authorize(&principal, Action::Users, "users")?;
    let role: NewRole = parse(body)?;
    check(&role.slug, &role.name)?;
    if auth::role_abilities(&state.pool, &role.slug)
        .await?
        .is_some()
    {
        return Err(ApiError::Conflict(format!(
            "role `{}` already exists",
            role.slug
        )));
    }
    auth::role_upsert(&state.pool, &role.slug, role.name.trim(), &role.abilities).await?;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({"slug": role.slug, "name": role.name.trim(), "builtin": false, "abilities": role.abilities}),
        ),
    ))
}

pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    authorize(&principal, Action::Users, "users")?;
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let change: RoleChange = parse(body)?;
    check(&slug, &change.name)?;
    if auth::role_abilities(&state.pool, &slug).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    auth::role_upsert(&state.pool, &slug, change.name.trim(), &change.abilities).await?;
    Ok(Json(
        json!({"slug": slug, "name": change.name.trim(), "builtin": false, "abilities": change.abilities}),
    ))
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    authorize(&principal, Action::Users, "users")?;
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    // A rule naming a role that's gone would never match; refuse rather than leave it stale.
    // ponytail: a schema saved between this check and the delete can still name the role;
    // its rule then matches nobody (no account can hold a deleted role), so it fails closed.
    let collections = tachyon_db::collections::list(&state.pool).await?;
    if let Some(c) = collections.iter().find(|c| names_role(&c.schema.0, &slug)) {
        return Err(ApiError::Conflict(format!(
            "access rules in {} name the role `{slug}`; change them first",
            c.schema.0.slug
        )));
    }
    auth::role_delete(&state.pool, &slug).await?;
    Ok(StatusCode::NO_CONTENT)
}
