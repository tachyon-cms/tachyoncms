//! `/api/admin/collections`. Unauthenticated until M3, which is why the server binds to localhost by default.

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tachyon_core::{CollectionSchema, FieldError};
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
    let schema: CollectionSchema =
        serde_json::from_value(body).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    schema.check().map_err(|e| {
        ApiError::Validation(vec![FieldError {
            path: String::new(),
            message: e.to_string(),
        }])
    })?;
    let created = collections::create(&state.pool, &schema).await?;
    Ok((StatusCode::CREATED, Json(to_json(&created))))
}
