//! `/api/{collection}` and `/api/{collection}/{id}`.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::sync::Arc;

use tachyon_core::auth::Action;
use tachyon_core::query::value_kind;
use tachyon_core::select::{Select, project};
use tachyon_core::{CollectionKind, FieldError, validate_document};
use tachyon_db::DbError;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Document};
use tachyon_db::indexes::{IndexKind, index_name};
use tachyon_db::query::{self, ListQuery, get_many_readable};
use uuid::Uuid;

use crate::access::{Access, both};
use crate::joins::fill_joins;
use crate::params::{Joins, list_params, read_params};
use crate::populate::{
    Shapes, check_links, fill_virtuals, link_ids, link_targets, populate, prepare, to_value,
    vouched_links,
};
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
    access: Access,
    path: Result<Path<String>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    // Checked again in `list_in`; here so a key's 403 comes before an unknown slug's 404.
    access.scope(Action::Read, &slug)?;
    let c = collection(&state, &slug)?;
    list_in(&state, &access, c, raw).await
}

/// A page of the documents of `c` the caller may read, shaped and populated as the query
/// asks. Refused when they may read none of them.
pub(crate) async fn list_in(
    state: &AppState,
    access: &Access,
    c: Arc<Collection>,
    raw: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let read = access.permit(Action::Read, &c)?;
    let p = list_params(raw.as_deref(), &c, access)?;
    let filter = both(p.filter, read);
    let page = p.page.unwrap_or(1);
    let offset = (page - 1)
        .checked_mul(p.limit)
        .ok_or_else(|| ApiError::BadRequest("page is too large".into()))?;
    let q = ListQuery {
        filter: filter.as_ref(),
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
        .map(|d| {
            let mut v = to_value(d)?;
            access.strip(&c, &mut v);
            Ok((v, c.clone()))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    fill_virtuals(state, access, &mut docs, p.select.as_ref()).await?;
    fill_joins(
        state,
        access,
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
    let docs = populate(state, access, docs, p.depth, &p.populate).await?;
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

/// The document just written as JSON, with its relationships populated to `depth` as far as
/// the caller may read them; only its ID when they can't read the document itself.
async fn respond(
    state: &AppState,
    access: &Access,
    c: Arc<Collection>,
    doc: &Document,
    depth: u8,
) -> Result<Json<Value>, ApiError> {
    if !access.can_read(&state.pool, &c, doc.id).await? {
        return Ok(Json(json!({ "id": doc.id })));
    }
    let mut value = to_value(doc)?;
    access.strip(&c, &mut value);
    let mut items = vec![(value, c)];
    fill_virtuals(state, access, &mut items, None).await?;
    fill_joins(
        state,
        access,
        &mut items,
        None,
        &Joins::all(),
        depth,
        &Shapes::new(),
    )
    .await?;
    Ok(Json(
        populate(state, access, items, depth, &Shapes::new())
            .await?
            .remove(0),
    ))
}

pub async fn create(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let c = collection(&state, &slug)?;
    // A create rule can't depend on a document that doesn't exist yet. Schemas refuse such
    // rules when saved; one that got through anyway refuses.
    if access.permit(Action::Create, &c)?.is_some() {
        return Err(access.refuse());
    }
    let body = json_body(body)?;
    access.writes(&c, &body, Action::Create)?;
    let targets = link_targets(&state, &c).await?;
    let prepared = validate_document(c.schema.data(), &body, None)
        .map_err(ApiError::Validation)
        .and_then(|data| prepare(&targets, &c, data))
        .map_err(|e| access.hide(&c, e))?;
    check_links(&state, &access, &prepared, &[]).await?;
    let doc = documents::insert_as(
        &state.pool,
        c.id,
        c.schema_version,
        None,
        &prepared,
        access.actor(Action::Create, &c).map(|a| a.id),
    )
    .await
    .map_err(|e| access.hide(&c, unique_error(&c, e.into())))?;
    Ok((
        StatusCode::CREATED,
        respond(&state, &access, c, &doc, 1).await?,
    ))
}

pub async fn get(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<(String, String)>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    // Checked again later; here so a key's 403 comes before a malformed ID's 404.
    access.scope(Action::Read, &slug)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    let p = read_params(raw.as_deref(), &c, &access)?;
    let doc = access.read_one(&state.pool, &c, id).await?;
    let mut value = to_value(&doc)?;
    access.strip(&c, &mut value);
    let mut items = vec![(value, c)];
    fill_virtuals(&state, &access, &mut items, p.select.as_ref()).await?;
    fill_joins(
        &state,
        &access,
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
        populate(&state, &access, vec![(value, c)], p.depth, &p.populate)
            .await?
            .remove(0),
    ))
}

pub async fn update(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<(String, String)>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    // Checked again later; here so a key's 403 comes before a malformed ID's 404.
    access.scope(Action::Update, &slug)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    let guard = access
        .write_guard(&state.pool, Action::Update, &c, id)
        .await?;
    let patch = json_body(body)?;
    access.writes(&c, &patch, Action::Update)?;
    let targets = link_targets(&state, &c).await?;
    let apply = |current: &Map<String, Value>| {
        validate_document(c.schema.data(), &patch, Some(current))
            .map_err(ApiError::Validation)
            .and_then(|data| prepare(&targets, &c, data))
            .map_err(|e| access.hide(&c, e))
    };
    // Links the patch adds are checked against the stored document, then again against the
    // locked one, in case it changed in between.
    let stored = documents::get(&state.pool, c.id, id).await?.data.0;
    let vouched = check_links(&state, &access, &apply(&stored)?, &link_ids(&c, &stored)).await?;
    let doc = documents::update_as(
        &state.pool,
        c.id,
        c.schema_version,
        id,
        access.actor(Action::Update, &c),
        guard.as_ref(),
        |current| {
            let next = apply(current)?;
            vouched_links(&next, &link_ids(&c, current), &vouched)?;
            Ok(next)
        },
    )
    .await
    .map_err(|e| access.hide(&c, unique_error(&c, e)))?;
    respond(&state, &access, c, &doc, 1).await
}

pub async fn delete(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    // Checked again later; here so a key's 403 comes before a malformed ID's 404.
    access.scope(Action::Delete, &slug)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    let guard = access
        .write_guard(&state.pool, Action::Delete, &c, id)
        .await?;
    documents::delete_as(
        &state.pool,
        c.id,
        id,
        access.actor(Action::Delete, &c),
        guard.as_ref(),
    )
    .await
    .map_err(|e| match e {
        // How many link here would count documents the caller may not be able to see.
        DbError::Referenced(_) if !access.unscoped_admin() => {
            ApiError::Conflict("Other documents link to this one; remove those links first".into())
        }
        e => e.into(),
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// The documents that link to this one, among those the caller may read.
pub async fn references(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    // Checked again later; here so a key's 403 comes before a malformed ID's 404.
    access.scope(Action::Read, &slug)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    access.read_one(&state.pool, &c, id).await?;
    let mut refs = documents::references(&state.pool, id).await?;
    if !refs.is_empty() {
        let ids: Vec<Uuid> = refs.iter().map(|r| r.id).collect();
        let readable: HashSet<Uuid> =
            get_many_readable(&state.pool, &ids, &access.readable_with_globals())
                .await?
                .into_iter()
                .map(|(_, doc)| doc.id)
                .collect();
        // A link held in a field the caller can't read isn't theirs to see either. Admins
        // read every field, and see every link, so they can always tell what blocks a delete.
        let admin = access.caller().admin;
        refs.retain(|r| {
            readable.contains(&r.id)
                && (admin
                    || state
                        .schemas
                        .get(&r.collection)
                        .is_some_and(|from| access.reads(&from, &r.path)))
        });
    }
    Ok(Json(json!({ "docs": refs })))
}
