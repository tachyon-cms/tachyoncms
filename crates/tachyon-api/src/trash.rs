//! The trash: `GET /api/{collection}/trash`, `POST /api/{collection}/trash/{id}/restore`,
//! `DELETE /api/{collection}/trash/{id}` and `DELETE /api/{collection}/trash`.
//!
//! Everything here takes the `delete` right, judged as a delete judges it, and an own-only
//! right reaches only what the caller created (`Access::trash_readable`). A trashed
//! document they can't reach is a 404. Globals have no trash, and neither has `users` yet:
//! restoring an account must bring back what deleting it ended (M3b-2).

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use tachyon_core::auth::Action;
use tachyon_core::query::Readable;
use tachyon_core::validate_document_in;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Prepared};
use tachyon_db::query::{self, ListQuery};
use tachyon_db::{DbError, DocCopy};
use uuid::Uuid;

use crate::access::{Access, both};
use crate::documents::{collection, mode_for, parse_id, respond, shaped, unique_error};
use crate::params::{no_params, trash_params};
use crate::populate::{Vouched, check_links, link_targets, prepare, to_value, vouched_links};
use crate::{ApiError, AppState};

/// A page of the trashed documents of a collection the caller may delete, most recently
/// deleted first, each with `deletedAt`. Links stay IDs.
pub async fn list(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<String>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    // Checked again later; here so a key's 403 comes before an unknown slug's 404.
    access.scope(Action::Delete, &slug)?;
    let c = collection(&state, &slug)?;
    let read = access.trash_readable(&c)?;
    let p = trash_params(raw.as_deref(), &c, &access)?;
    let filter = both(p.filter, read.filter);
    let page = p.page.unwrap_or(1);
    let offset = (page - 1)
        .checked_mul(p.limit)
        .ok_or_else(|| ApiError::BadRequest("page is too large".into()))?;
    let q = ListQuery {
        filter: filter.as_ref(),
        sort: p.sort.as_ref(),
        limit: p.limit,
        offset,
        after: None,
        count: false,
        copy: read.copy,
        owner: read.owner,
    };
    let found = query::list_trash(&state.pool, c.id, &q).await?;
    let docs = found
        .docs
        .iter()
        .map(|d| {
            let mut v = to_value(d)?;
            access.strip(&c, &mut v);
            Ok(shaped(v, p.select.as_ref()))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(Json(json!({
        "docs": docs,
        "limit": p.limit,
        "page": page,
        "hasNextPage": found.has_more,
    })))
}

/// Takes a document out of the trash, as it was, once each copy still validates against
/// the current schema (422), links only to documents that are there and the caller may
/// read (422 at the link), and clashes with no live document's unique value (422 at the
/// field, Ruling T5-1). Answers with the document as a write answers.
pub async fn restore(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<(String, String)>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    // Checked again later; here so a key's 403 comes before a malformed ID's 404.
    access.scope(Action::Delete, &slug)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    let read = access.trash_readable(&c)?;
    no_params(raw.as_deref())?;
    let targets = link_targets(&state, &c).await?;
    // Links are checked outside the transaction, as an update checks them, then held to
    // what was checked once the document is locked.
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    let stored = stored_in(&mut conn, &access, &c, &read, &targets, id).await?;
    drop(conn);
    let mut vouched = Vouched::new();
    for (_, p) in &stored {
        vouched.extend(check_links(&state, &access, p, &[]).await?);
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let locked = stored_in(&mut tx, &access, &c, &read, &targets, id).await?;
    for (_, p) in &locked {
        vouched_links(p, &[], &vouched)?;
    }
    let doc = documents::restore_in(&mut tx, &c, id, &locked, access.user_id())
        .await
        .map_err(|e| access.hide(&c, unique_error(&c, e.into()), None))?;
    tx.commit().await.map_err(DbError::from)?;
    respond(&state, &access, c, &doc, read.copy, 1).await
}

type Targets = std::collections::HashMap<String, Uuid>;

/// Trashed document `id` of `c`, locked, each copy it has validated against the current
/// schema (the draft as a draft save, the published copy in full) and ready to write: a
/// 404 when `read` doesn't reach it.
async fn stored_in(
    conn: &mut PgConnection,
    access: &Access,
    c: &Collection,
    read: &Readable,
    targets: &Targets,
    id: Uuid,
) -> Result<Vec<(DocCopy, Prepared)>, ApiError> {
    if query::lock_trash_in(conn, c.id, c.schema_version, read, Some(id))
        .await?
        .is_empty()
    {
        return Err(ApiError::NotFound);
    }
    let (draft, published) = documents::trashed_in(conn, c.id, id).await?;
    [(DocCopy::Draft, draft), (DocCopy::Published, published)]
        .into_iter()
        .filter_map(|(copy, data)| Some((copy, data?)))
        .map(|(copy, data): (DocCopy, Map<String, Value>)| {
            validate_document_in(mode_for(copy), c.schema.data(), &json!({}), Some(&data))
                .map_err(ApiError::Validation)
                .and_then(|valid| prepare(targets, c, valid))
                .map(|p| (copy, p))
                .map_err(|e| access.hide(c, e, Some(&data)))
        })
        .collect()
}

/// Deletes a trashed document for good, with its versions and jobs.
pub async fn purge(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<(String, String)>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<StatusCode, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    // Checked again later; here so a key's 403 comes before a malformed ID's 404.
    access.scope(Action::Delete, &slug)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    let read = access.trash_readable(&c)?;
    no_params(raw.as_deref())?;
    match purge_where(&state, &c, &read, Some(id)).await? {
        0 => Err(ApiError::NotFound),
        _ => Ok(StatusCode::NO_CONTENT),
    }
}

/// Empties the trash of what the caller may delete, and says how many went.
pub async fn empty(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<String>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    // Checked again later; here so a key's 403 comes before an unknown slug's 404.
    access.scope(Action::Delete, &slug)?;
    let c = collection(&state, &slug)?;
    let read = access.trash_readable(&c)?;
    no_params(raw.as_deref())?;
    let deleted = purge_where(&state, &c, &read, None).await?;
    Ok(Json(json!({ "deleted": deleted })))
}

/// Purges the trashed documents of `c` that `read` reaches (only `id` among them, when
/// given), locked in one transaction so none can be restored meanwhile.
async fn purge_where(
    state: &AppState,
    c: &Collection,
    read: &Readable,
    id: Option<Uuid>,
) -> Result<u64, ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let ids = query::lock_trash_in(&mut tx, c.id, c.schema_version, read, id).await?;
    let deleted = documents::purge_in(&mut tx, c.id, &ids).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(deleted)
}
