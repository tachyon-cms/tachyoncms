//! `/api/{collection}` and `/api/{collection}/{id}`.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;

use tachyon_core::{CollectionKind, FieldError, validate_document};
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Document};
use tachyon_db::indexes::{IndexKind, index_name};
use tachyon_db::query::{self, ListQuery};
use uuid::Uuid;

use crate::params::{depth_param, list_params};
use crate::populate::{populate, prepare, to_value};
use crate::{ApiError, AppState};

/// A collection (not a global) by slug, from the in-memory cache.
pub(crate) fn collection(state: &AppState, slug: &str) -> Result<Arc<Collection>, ApiError> {
    state
        .schemas
        .get(slug)
        .filter(|c| c.schema.kind == CollectionKind::Collection)
        .ok_or(ApiError::NotFound)
}

/// Turns a unique-index rejection into a field error, when the index belongs to one of `c`'s fields.
pub(crate) fn unique_error(c: &Collection, e: ApiError) -> ApiError {
    if let ApiError::Unique(constraint) = &e
        && let Some(f) =
            c.schema.fields.iter().find(|f| {
                f.is_unique() && index_name(c.id, &f.name, IndexKind::Unique) == *constraint
            })
    {
        return ApiError::Validation(vec![FieldError {
            path: f.name.clone(),
            message: "must be unique; another document already uses this value".into(),
        }]);
    }
    e
}

/// A malformed ID can't match any document, so it's a 404 rather than a 400.
fn parse_id(id: &str) -> Result<Uuid, ApiError> {
    id.parse().map_err(|_| ApiError::NotFound)
}

pub(crate) fn json_body(body: Result<Json<Value>, JsonRejection>) -> Result<Value, ApiError> {
    body.map(|Json(v)| v)
        .map_err(|e| ApiError::BadRequest(e.body_text()))
}

pub async fn list(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let c = collection(&state, &slug)?;
    let p = list_params(raw.as_deref(), &c)?;
    let page = p.page.unwrap_or(1);
    let offset = (page - 1)
        .checked_mul(p.limit)
        .ok_or_else(|| ApiError::BadRequest("page is too large".into()))?;
    let q = ListQuery {
        filter: p.filter.as_ref(),
        sort: p.sort.as_ref(),
        limit: p.limit,
        offset: if p.after.is_some() { 0 } else { offset },
        after: p.after.as_ref(),
        count: p.count,
    };
    let found = query::list(&state.pool, c.id, &q).await?;
    let docs = found
        .docs
        .iter()
        .map(|d| Ok((to_value(d)?, c.clone())))
        .collect::<Result<Vec<_>, ApiError>>()?;
    let docs = populate(&state, docs, p.depth).await?;
    let mut body = json!({ "docs": docs, "limit": p.limit });
    if p.after.is_none() {
        body["page"] = page.into();
    }
    if let Some(next) = found.next {
        body["nextCursor"] = next.encode().into();
    }
    if let Some(total) = found.total {
        body["totalDocs"] = total.into();
    }
    Ok(Json(body))
}

/// The document as JSON, with its relationships populated to `depth`.
async fn respond(
    state: &AppState,
    c: Arc<Collection>,
    doc: &Document,
    depth: u8,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        populate(state, vec![(to_value(doc)?, c)], depth)
            .await?
            .remove(0),
    ))
}

pub async fn create(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let c = collection(&state, &slug)?;
    let data = validate_document(&c.schema.fields, &json_body(body)?, None)
        .map_err(ApiError::Validation)?;
    let doc = documents::insert(&state.pool, c.id, None, &prepare(&state, &c, data))
        .await
        .map_err(|e| unique_error(&c, e.into()))?;
    Ok((StatusCode::CREATED, respond(&state, c, &doc, 1).await?))
}

pub async fn get(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let depth = depth_param(raw.as_deref())?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    let doc = documents::get(&state.pool, c.id, id).await?;
    respond(&state, c, &doc, depth).await
}

pub async fn update(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    let patch = json_body(body)?;
    let doc = documents::update(&state.pool, c.id, id, |current| {
        validate_document(&c.schema.fields, &patch, Some(current))
            .map(|data| prepare(&state, &c, data))
            .map_err(ApiError::Validation)
    })
    .await
    .map_err(|e| unique_error(&c, e))?;
    respond(&state, c, &doc, 1).await
}

pub async fn delete(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    documents::delete(&state.pool, c.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn references(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    documents::get(&state.pool, c.id, id).await?;
    Ok(Json(
        json!({ "docs": documents::references(&state.pool, id).await? }),
    ))
}
