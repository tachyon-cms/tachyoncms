//! `/api/{collection}` and `/api/{collection}/{id}`.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tachyon_core::validate_document;
use tachyon_db::collections::{self, Collection};
use tachyon_db::documents::{self, Document};
use uuid::Uuid;

use crate::{ApiError, AppState};

const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 100;

#[derive(Deserialize)]
pub struct ListParams {
    limit: Option<i64>,
    page: Option<i64>,
    #[serde(default)]
    count: bool,
}

// NOTE(M2): one schema lookup per request; M2 replaces it with the ArcSwap cache refreshed by LISTEN/NOTIFY.
async fn collection(state: &AppState, slug: &str) -> Result<Collection, ApiError> {
    Ok(collections::get_by_slug(&state.pool, slug).await?)
}

/// A malformed ID can't match any document, so it's a 404 rather than a 400.
fn parse_id(id: &str) -> Result<Uuid, ApiError> {
    id.parse().map_err(|_| ApiError::NotFound)
}

fn json_body(body: Result<Json<Value>, JsonRejection>) -> Result<Value, ApiError> {
    body.map(|Json(v)| v)
        .map_err(|e| ApiError::BadRequest(e.body_text()))
}

pub async fn list(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let Query(params) = params.map_err(|e| ApiError::BadRequest(e.body_text()))?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    let page = params.page.unwrap_or(1);
    if limit < 1 {
        return Err(ApiError::BadRequest("limit must be at least 1".into()));
    }
    if page < 1 {
        return Err(ApiError::BadRequest("page must be at least 1".into()));
    }
    let limit = limit.min(MAX_LIMIT);
    let offset = (page - 1)
        .checked_mul(limit)
        .ok_or_else(|| ApiError::BadRequest("page is too large".into()))?;

    let c = collection(&state, &slug).await?;
    let found = documents::list(&state.pool, c.id, limit, offset, params.count).await?;
    let mut body = json!({ "docs": found.docs, "page": page, "limit": limit });
    if let Some(total) = found.total {
        body["totalDocs"] = total.into();
    }
    Ok(Json(body))
}

pub async fn create(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Document>), ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let c = collection(&state, &slug).await?;
    let data = validate_document(&c.schema.fields, &json_body(body)?, None)
        .map_err(ApiError::Validation)?;
    Ok((
        StatusCode::CREATED,
        Json(documents::insert(&state.pool, c.id, &data).await?),
    ))
}

pub async fn get(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Json<Document>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug).await?;
    Ok(Json(documents::get(&state.pool, c.id, id).await?))
}

pub async fn update(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Document>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug).await?;
    let patch = json_body(body)?;
    let doc = documents::update(&state.pool, c.id, id, |current| {
        validate_document(&c.schema.fields, &patch, Some(current)).map_err(ApiError::Validation)
    })
    .await?;
    Ok(Json(doc))
}

pub async fn delete(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug).await?;
    documents::delete(&state.pool, c.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
