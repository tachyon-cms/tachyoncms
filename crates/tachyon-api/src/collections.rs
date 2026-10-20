//! `/api/admin/collections`. Unauthenticated until M3, which is why the server binds to localhost by default.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tachyon_core::access;
use tachyon_core::auth::Action;
use tachyon_core::diff::Renames;
use tachyon_core::{
    CollectionKind, CollectionSchema, FieldError, SchemaError, explain_parse_error,
};
use tachyon_db::collections::{self, Collection};
use tachyon_db::indexes::reconcile_indexes;
use tachyon_db::schema_change::{self, ChangeError};

use crate::auth::authorize;
use crate::auth::principal::Principal;
use crate::documents::json_body;
use crate::{ApiError, AppState};

fn to_json(c: &Collection) -> Value {
    json!({ "id": c.id, "schemaVersion": c.schema_version, "schema": c.schema.to_compact_json() })
}

fn schema_error(e: SchemaError) -> ApiError {
    ApiError::Validation(vec![FieldError {
        path: String::new(),
        message: e.to_string(),
    }])
}

fn parse_schema(value: &Value) -> Result<CollectionSchema, ApiError> {
    let schema = CollectionSchema::deserialize(value)
        .map_err(|e| ApiError::BadRequest(explain_parse_error(value, &e)))?;
    // Unknown keys first: a typo such as `nmae` should be named, not reported as a bad name.
    schema
        .reject_unknown_options(value)
        .and_then(|()| schema.check())
        .map_err(schema_error)?;
    Ok(schema)
}

/// Logged here so the cause survives even if the reload after it fails too.
fn log_index_failure(indexed: &Result<(), tachyon_db::DbError>, slug: &str) {
    if let Err(error) = indexed {
        tracing::warn!(%error, collection = slug, "couldn't build indexes; startup will retry");
    }
}

pub async fn list(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Value>, ApiError> {
    principal.require()?;
    let docs: Vec<Value> = collections::list(&state.pool)
        .await?
        .iter()
        .map(to_json)
        .collect();
    Ok(Json(json!({ "docs": docs })))
}

pub async fn create(
    State(state): State<AppState>,
    principal: Principal,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    authorize(&principal, Action::Schemas, "")?;
    let body = json_body(body)?;
    let schema = parse_schema(&body)?;
    if schema.slug == tachyon_db::auth::USERS {
        return Err(ApiError::Conflict(
            "users is built in; change its fields with PUT /api/admin/collections/users".into(),
        ));
    }
    for target in schema.relation_targets() {
        let ok = target == schema.slug
            || state
                .schemas
                .get(target)
                .is_some_and(|c| c.schema.kind == CollectionKind::Collection);
        if !ok {
            return Err(schema_error(SchemaError::InvalidChange(format!(
                "relationTo `{target}` doesn't match an existing collection"
            ))));
        }
    }
    check_virtuals(&state, &schema)?;
    check_rules(&state, &schema).await?;
    let created = collections::create(&state.pool, &schema).await?;
    // The schema is saved either way, so serve it even if its indexes failed to build.
    let indexed = reconcile_indexes(&state.pool, created.id).await;
    log_index_failure(&indexed, &schema.slug);
    state.schemas.reload(&state.pool).await?;
    indexed?;
    Ok((StatusCode::CREATED, Json(to_json(&created))))
}

pub async fn get_one(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    principal.require()?;
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let c = state.schemas.get(&slug).ok_or(ApiError::NotFound)?;
    Ok(Json(to_json(&c)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeRequest {
    schema: Value,
    #[serde(default)]
    renames: Renames,
    #[serde(default)]
    confirm: bool,
}

/// Every virtual field must read a field its targets have, and every join a field that links here.
fn check_virtuals(state: &AppState, schema: &CollectionSchema) -> Result<(), ApiError> {
    let lookup = |slug: &str| state.schemas.get(slug).map(|c| c.schema.0.clone());
    schema
        .check_virtual_targets(lookup)
        .and_then(|()| schema.check_join_targets(lookup))
        .map_err(schema_error)
}

/// A change to `schema` must not break the virtual or join fields other collections read
/// from it, such as a join on a relationship this change removes.
fn check_dependents(state: &AppState, schema: &CollectionSchema) -> Result<(), ApiError> {
    let lookup = |slug: &str| {
        if slug == schema.slug {
            Some(schema.clone())
        } else {
            state.schemas.get(slug).map(|c| c.schema.0.clone())
        }
    };
    for other in state.schemas.all() {
        let other = &other.schema.0;
        if other.slug == schema.slug {
            continue;
        }
        other
            .check_virtual_targets(lookup)
            .and_then(|()| other.check_join_targets(lookup))
            .map_err(|e| {
                schema_error(SchemaError::InvalidChange(format!(
                    "this change would break {}: {e}",
                    other.slug
                )))
            })?;
    }
    Ok(())
}

/// Every collection's access rules must hold with `schema` saved: its own, and the others',
/// which may filter through it or, when it's `users`, read its fields as `$user` values.
/// A role deleted after this check is caught where the schema is stored, under the lock
/// role deletes share (`tachyon_db::auth::check_rule_roles`).
// ponytail: a rule stored before these checks existed, which they'd now refuse, blocks
// saving every other collection until it's fixed. Fine before the first release, since no
// deployment holds rules yet; add a migration that reports them if that changes.
async fn check_rules(state: &AppState, schema: &CollectionSchema) -> Result<(), ApiError> {
    let roles = tachyon_db::auth::roles_list(&state.pool).await?;
    let roles: Vec<&str> = roles.iter().map(|r| r.slug.as_str()).collect();
    let lookup = |slug: &str| {
        if slug == schema.slug {
            Some(schema.clone())
        } else {
            state.schemas.get(slug).map(|c| c.schema.0.clone())
        }
    };
    let users = lookup(tachyon_db::auth::USERS)
        .map(|u| u.fields)
        .unwrap_or_default();
    access::check_rules(schema, &users, &roles, &lookup).map_err(schema_error)?;
    for other in state.schemas.all() {
        let other = &other.schema.0;
        if other.slug == schema.slug {
            continue;
        }
        access::check_rules(other, &users, &roles, &lookup).map_err(|e| {
            schema_error(SchemaError::InvalidChange(format!(
                "this change would break {}: {e}",
                other.slug
            )))
        })?;
    }
    Ok(())
}

fn change_request(
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(String, ChangeRequest, CollectionSchema), ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let req: ChangeRequest = serde_json::from_value(json_body(body)?)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let schema = parse_schema(&req.schema)?;
    Ok((slug, req, schema))
}

fn change_error(e: ChangeError) -> ApiError {
    match e {
        ChangeError::NotFound => ApiError::NotFound,
        ChangeError::Invalid(e) => schema_error(e),
        ChangeError::Duplicates(p) => ApiError::Validation(
            p.duplicates
                .iter()
                .map(|f| FieldError {
                    path: f.clone(),
                    message: "has duplicate values; make them unique first".into(),
                })
                .collect(),
        ),
        ChangeError::NeedsConfirmation(p) => ApiError::NeedsConfirmation(json!(*p)),
        ChangeError::Db(e) => e.into(),
    }
}

pub async fn preview(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    authorize(&principal, Action::Schemas, "")?;
    let (slug, req, schema) = change_request(path, body)?;
    check_virtuals(&state, &schema)?;
    check_dependents(&state, &schema)?;
    check_rules(&state, &schema).await?;
    let p = schema_change::preview(&state.pool, &slug, &schema, &req.renames, &[])
        .await
        .map_err(change_error)?;
    Ok(Json(json!(p)))
}

pub async fn update(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    authorize(&principal, Action::Schemas, "")?;
    let (slug, req, schema) = change_request(path, body)?;
    check_virtuals(&state, &schema)?;
    check_dependents(&state, &schema)?;
    check_rules(&state, &schema).await?;
    let (c, p) = schema_change::apply(&state.pool, &slug, &schema, &req.renames, req.confirm)
        .await
        .map_err(change_error)?;
    let indexed = reconcile_indexes(&state.pool, c.id).await;
    log_index_failure(&indexed, &slug);
    state.schemas.reload(&state.pool).await?;
    indexed?;
    Ok(Json(json!({ "collection": to_json(&c), "preview": p })))
}
