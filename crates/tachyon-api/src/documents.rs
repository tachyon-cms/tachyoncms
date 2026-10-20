//! `/api/{collection}` and `/api/{collection}/{id}`.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;

use tachyon_core::auth::Action;
use tachyon_core::query::value_kind;
use tachyon_core::select::{Select, project};
use tachyon_core::{CollectionKind, FieldError, validate_document};
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Document};
use tachyon_db::indexes::{IndexKind, index_name};
use tachyon_db::query::{self, ListQuery};
use uuid::Uuid;

use crate::auth::principal::Principal;
use crate::auth::{actor, authorize};
use crate::joins::fill_joins;
use crate::params::{Joins, list_params, read_params};
use crate::populate::{Shapes, fill_virtuals, link_targets, populate, prepare, to_value};
use crate::{ApiError, AppState};

/// A collection (not a global) by slug, from the in-memory cache.
pub(crate) fn collection(state: &AppState, slug: &str) -> Result<Arc<Collection>, ApiError> {
    // Users have their own routes, which check who's asking and keep accounts apart. This is
    // decided on the slug the route resolved to, not the path, so `/api/%75sers` can't slip
    // past them.
    if slug == tachyon_db::auth::USERS {
        return Err(ApiError::NotFound);
    }
    any_collection(state, slug)
}

/// A collection by slug, including `users`: only for code that has already decided the
/// caller may reach it.
pub(crate) fn any_collection(state: &AppState, slug: &str) -> Result<Arc<Collection>, ApiError> {
    state
        .schemas
        .get(slug)
        .filter(|c| c.schema.kind == CollectionKind::Collection)
        .ok_or(ApiError::NotFound)
}

/// Turns a unique-index rejection into a field error, when the index belongs to one of `c`'s fields.
pub(crate) fn unique_error(c: &Collection, e: ApiError) -> ApiError {
    let fields = c.schema.data();
    if let ApiError::Unique(constraint) = &e
        && let Some(f) = fields.iter().find(|f| {
            f.is_unique()
                && value_kind(&f.kind)
                    .is_some_and(|v| index_name(c.id, &f.name, IndexKind::Unique, v) == *constraint)
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
    body.map(|Json(v)| v).map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::BodyTooLarge
        } else {
            ApiError::BadRequest(e.body_text())
        }
    })
}

pub async fn list(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    authorize(&principal, Action::Read, &slug)?;
    let c = collection(&state, &slug)?;
    list_in(&state, c, raw).await
}

/// A page of `c`'s documents, shaped and populated as the query asks. The caller has
/// already authorized the read.
pub(crate) async fn list_in(
    state: &AppState,
    c: Arc<Collection>,
    raw: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let state = state.clone();
    let p = list_params(raw.as_deref(), &c, &state.schemas)?;
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
    let mut docs = found
        .docs
        .iter()
        .map(|d| Ok((to_value(d)?, c.clone())))
        .collect::<Result<Vec<_>, ApiError>>()?;
    fill_virtuals(&state, &mut docs, p.select.as_ref()).await?;
    fill_joins(
        &state,
        &mut docs,
        p.select.as_ref(),
        &p.joins,
        p.depth,
        &p.populate,
    )
    .await?;
    let docs = docs
        .into_iter()
        .map(|(v, c)| (shaped(v, p.select.as_ref()), c))
        .collect();
    let docs = populate(&state, docs, p.depth, &p.populate).await?;
    let mut body = json!({ "docs": docs, "limit": p.limit });
    if p.after.is_none() {
        body["page"] = page.into();
    }
    body["hasNextPage"] = found.has_more.into();
    if let Some(next) = found.next {
        body["nextCursor"] = next.encode().into();
    }
    if let Some(total) = found.total {
        body["totalDocs"] = total.into();
    }
    Ok(Json(body))
}

/// `value` cut down to what `select` asks for.
pub(crate) fn shaped(mut value: Value, select: Option<&Select>) -> Value {
    if let (Some(select), Value::Object(doc)) = (select, &mut value) {
        project(select, doc);
    }
    value
}

/// The document as JSON, with its relationships populated to `depth`.
async fn respond(
    state: &AppState,
    c: Arc<Collection>,
    doc: &Document,
    depth: u8,
) -> Result<Json<Value>, ApiError> {
    let mut items = vec![(to_value(doc)?, c)];
    fill_virtuals(state, &mut items, None).await?;
    fill_joins(
        state,
        &mut items,
        None,
        &Joins::all(),
        depth,
        &Shapes::new(),
    )
    .await?;
    Ok(Json(
        populate(state, items, depth, &Shapes::new())
            .await?
            .remove(0),
    ))
}

pub async fn create(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let c = collection(&state, &slug)?;
    let decision = authorize(&principal, Action::Create, &slug)?;
    let data = validate_document(c.schema.data(), &json_body(body)?, None)
        .map_err(ApiError::Validation)?;
    let targets = link_targets(&state, &c).await?;
    let doc = documents::insert_as(
        &state.pool,
        c.id,
        c.schema_version,
        None,
        &prepare(&targets, &c, data)?,
        actor(&principal, decision).map(|a| a.id),
    )
    .await
    .map_err(|e| unique_error(&c, e.into()))?;
    Ok((StatusCode::CREATED, respond(&state, c, &doc, 1).await?))
}

pub async fn get(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<(String, String)>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    authorize(&principal, Action::Read, &slug)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    let p = read_params(raw.as_deref(), &c, &state.schemas)?;
    let doc = documents::get(&state.pool, c.id, id).await?;
    let mut items = vec![(to_value(&doc)?, c)];
    fill_virtuals(&state, &mut items, p.select.as_ref()).await?;
    fill_joins(
        &state,
        &mut items,
        p.select.as_ref(),
        &p.joins,
        p.depth,
        &p.populate,
    )
    .await?;
    let (value, c) = items.remove(0);
    let value = shaped(value, p.select.as_ref());
    Ok(Json(
        populate(&state, vec![(value, c)], p.depth, &p.populate)
            .await?
            .remove(0),
    ))
}

pub async fn update(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<(String, String)>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    let decision = authorize(&principal, Action::Update, &slug)?;
    let patch = json_body(body)?;
    let targets = link_targets(&state, &c).await?;
    let who = actor(&principal, decision);
    let doc = documents::update_as(
        &state.pool,
        c.id,
        c.schema_version,
        id,
        who,
        None,
        |current| {
            let data = validate_document(c.schema.data(), &patch, Some(current))
                .map_err(ApiError::Validation)?;
            prepare(&targets, &c, data)
        },
    )
    .await
    .map_err(|e| unique_error(&c, e))?;
    respond(&state, c, &doc, 1).await
}

pub async fn delete(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    let decision = authorize(&principal, Action::Delete, &slug)?;
    documents::delete_as(&state.pool, c.id, id, actor(&principal, decision), None).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn references(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    authorize(&principal, Action::Read, &slug)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    documents::get(&state.pool, c.id, id).await?;
    Ok(Json(
        json!({ "docs": documents::references(&state.pool, id).await? }),
    ))
}
