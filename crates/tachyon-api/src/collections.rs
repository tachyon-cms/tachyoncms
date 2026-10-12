//! `/api/admin/collections`. Unauthenticated until M3, which is why the server binds to localhost by default.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tachyon_core::diff::Renames;
use tachyon_core::{CollectionKind, CollectionSchema, FieldError, SchemaError};
use tachyon_db::collections::{self, Collection};
use tachyon_db::indexes::reconcile_indexes;
use tachyon_db::schema_change::{self, ChangeError};

use crate::documents::json_body;
use crate::{ApiError, AppState};

fn to_json(c: &Collection) -> Value {
    json!({ "id": c.id, "schemaVersion": c.schema_version, "schema": &c.schema.0 })
}

fn schema_error(e: SchemaError) -> ApiError {
    ApiError::Validation(vec![FieldError {
        path: String::new(),
        message: e.to_string(),
    }])
}

fn parse_schema(value: &Value) -> Result<CollectionSchema, ApiError> {
    let schema =
        CollectionSchema::deserialize(value).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    // Unknown keys first: a typo such as `nmae` should be named, not reported as a bad name.
    schema
        .reject_unknown_options(value)
        .and_then(|()| schema.check())
        .map_err(schema_error)?;
    Ok(schema)
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let docs: Vec<Value> = collections::list(&state.pool)
        .await?
        .iter()
        .map(to_json)
        .collect();
    Ok(Json(json!({ "docs": docs })))
}

pub async fn create(
    State(state): State<AppState>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body = json_body(body)?;
    let schema = parse_schema(&body)?;
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
    let created = collections::create(&state.pool, &schema).await?;
    // The schema is saved either way, so serve it even if its indexes failed to build.
    let indexed = reconcile_indexes(&state.pool, created.id).await;
    state.schemas.reload(&state.pool).await?;
    indexed?;
    Ok((StatusCode::CREATED, Json(to_json(&created))))
}

pub async fn get_one(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
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
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let (slug, req, schema) = change_request(path, body)?;
    let p = schema_change::preview(&state.pool, &slug, &schema, &req.renames, &[])
        .await
        .map_err(change_error)?;
    Ok(Json(json!(p)))
}

pub async fn update(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let (slug, req, schema) = change_request(path, body)?;
    let (c, p) = schema_change::apply(&state.pool, &slug, &schema, &req.renames, req.confirm)
        .await
        .map_err(change_error)?;
    let indexed = reconcile_indexes(&state.pool, c.id).await;
    state.schemas.reload(&state.pool).await?;
    indexed?;
    Ok(Json(json!({ "collection": to_json(&c), "preview": p })))
}
