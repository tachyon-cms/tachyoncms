//! `/api/admin/collections`. Unauthenticated until M3, which is why the server binds to localhost by default.

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tachyon_core::{CollectionKind, CollectionSchema, FieldError};
use tachyon_db::collections::{self, Collection};

use crate::{ApiError, AppState};

fn to_json(c: &Collection) -> Value {
    json!({ "id": c.id, "schemaVersion": c.schema_version, "schema": &c.schema.0 })
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
    let Json(body) = body.map_err(|e| ApiError::BadRequest(e.body_text()))?;
    let schema =
        CollectionSchema::deserialize(&body).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    schema
        .check()
        .and_then(|()| schema.reject_unknown_options(&body))
        .map_err(|e| {
            ApiError::Validation(vec![FieldError {
                path: String::new(),
                message: e.to_string(),
            }])
        })?;
    for target in schema.relation_targets() {
        let ok = target == schema.slug
            || state
                .schemas
                .get(target)
                .is_some_and(|c| c.schema.kind == CollectionKind::Collection);
        if !ok {
            return Err(ApiError::Validation(vec![FieldError {
                path: String::new(),
                message: format!("relationTo `{target}` doesn't match an existing collection"),
            }]));
        }
    }
    let created = collections::create(&state.pool, &schema).await?;
    // NOTE: indexes are built after the row commits; schema apply (M2b) reconciles any that failed.
    tachyon_db::indexes::create_field_indexes(&state.pool, created.id, &created.schema.0).await?;
    state.schemas.reload(&state.pool).await?;
    Ok((StatusCode::CREATED, Json(to_json(&created))))
}
