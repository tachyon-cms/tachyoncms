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
use serde_json::{Value, json};
use sqlx::PgConnection;
use tachyon_core::auth::Action;
use tachyon_core::query::Readable;
use tachyon_core::validate_document_in;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Prepared, copy_errors};
use tachyon_db::query::{self, ListQuery};
use tachyon_db::versions::record_version;
use tachyon_db::{DbError, DocCopy};
use uuid::Uuid;

use crate::access::{Access, both};
use crate::documents::{collection, mode_for, parse_id, respond, shaped, unique_error};
use crate::params::{no_params, trash_params};
use crate::populate::{Vouched, check_links, link_targets, prepare, to_value, vouched_links};
use crate::{ApiError, AppState};

/// A page of the trashed documents of a collection the caller may delete, most recently
/// deleted first unless sorted (`deletedAt` too), each with `deletedAt`. Links stay IDs.
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
    // A collection they can't read at all is refused as a plain list refuses it (Ruling T8-3).
    access.permit(Action::Read, &c)?;
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
        count: p.count,
        copy: read.copy,
        owner: read.owner,
    };
    let found = query::list_trash(&state.pool, c.id, &q, p.oldest_first).await?;
    let docs = found
        .docs
        .iter()
        .map(|d| {
            let mut v = to_value(d)?;
            access.strip(&c, &mut v);
            Ok(shaped(v, p.select.as_ref()))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let mut body = json!({
        "docs": docs,
        "limit": p.limit,
        "page": page,
        "hasNextPage": found.has_more,
    });
    if let Some(total) = found.total {
        body["totalDocs"] = total.into();
    }
    Ok(Json(body))
}

/// Takes a document out of the trash once each copy still validates against the current
/// schema (422), links only to documents that are there and the caller may read (422 at
/// the link), and clashes with no live document's unique value (422 at the field, Ruling
/// T5-1). With drafts, each error says which copy it's about, and both copies are checked
/// (Ruling T8-2). It's a write (Ruling T8-1): `updatedAt` moves, and a copy validation
/// changed is recorded as a version of its kind. Answers with the document as a write
/// answers.
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
    let stored = stored_in(&mut conn, &access, &c, &read, &targets, id, false).await?;
    drop(conn);
    let mut vouched = Vouched::new();
    let mut errors = Errors::default();
    for (copy, p, _) in &stored {
        match check_links(&state, &access, p, &[]).await {
            Ok(v) => vouched.extend(v),
            Err(e) => errors.add(&c, *copy, e)?,
        }
    }
    errors.check()?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let locked = stored_in(&mut tx, &access, &c, &read, &targets, id, true).await?;
    for (copy, p, _) in &locked {
        if let Err(e) = vouched_links(p, &[], &vouched) {
            errors.add(&c, *copy, e)?;
        }
    }
    errors.check()?;
    let by = access.user_id();
    let copies: Vec<_> = locked.iter().map(|(c, p, _)| (*c, p.clone())).collect();
    let doc = documents::restore_in(&mut tx, &c, id, &copies, by)
        .await
        .map_err(|e| {
            let e = match e {
                // Only published copies are unique.
                DbError::UniqueViolation(_) => match unique_error(&c, e.into()) {
                    ApiError::Validation(v) => {
                        ApiError::Validation(copy_errors(&c, DocCopy::Published, v))
                    }
                    e => e,
                },
                e => e.into(),
            };
            access.hide(&c, e, None)
        })?;
    if c.schema.versioned() {
        for (copy, p, changed) in &locked {
            if *changed {
                record_version(&mut tx, id, &c, *copy, &p.data, by).await?;
            }
        }
    }
    tx.commit().await.map_err(DbError::from)?;
    respond(&state, &access, c, &doc, read.copy, 1).await
}

/// Each copy's 422 errors, gathered so a restore reports both copies at once.
#[derive(Default)]
struct Errors(Vec<tachyon_core::FieldError>);

impl Errors {
    /// Keeps `e`'s field errors about `copy` of a document of `c`; any other error stops.
    fn add(&mut self, c: &Collection, copy: DocCopy, e: ApiError) -> Result<(), ApiError> {
        match e {
            ApiError::Validation(v) => {
                self.0.extend(copy_errors(c, copy, v));
                Ok(())
            }
            e => Err(e),
        }
    }

    fn check(&mut self) -> Result<(), ApiError> {
        match self.0.is_empty() {
            true => Ok(()),
            false => Err(ApiError::Validation(std::mem::take(&mut self.0))),
        }
    }
}

type Targets = std::collections::HashMap<String, Uuid>;

/// Trashed document `id` of `c`, each copy it has validated against the current schema
/// (the draft as a draft save, the published copy in full) and ready to write, with
/// whether validation changed it: a 404 when `read` doesn't reach it. With `lock`, inside
/// the caller's transaction, it's locked; otherwise it's only read.
async fn stored_in(
    conn: &mut PgConnection,
    access: &Access,
    c: &Collection,
    read: &Readable,
    targets: &Targets,
    id: Uuid,
    lock: bool,
) -> Result<Vec<(DocCopy, Prepared, bool)>, ApiError> {
    let reached = match lock {
        true => !query::lock_trash_in(conn, c.id, c.schema_version, read, Some(id))
            .await?
            .is_empty(),
        false => query::in_trash(conn, c.id, read, id).await?,
    };
    if !reached {
        return Err(ApiError::NotFound);
    }
    let (draft, published) = documents::trashed_in(conn, c.id, id).await?;
    let mut out = Vec::new();
    let mut errors = Errors::default();
    for (copy, data) in [(DocCopy::Draft, draft), (DocCopy::Published, published)] {
        let Some(data) = data else { continue };
        match validate_document_in(mode_for(copy), c.schema.data(), &json!({}), Some(&data))
            .map_err(ApiError::Validation)
            .and_then(|valid| prepare(targets, c, valid))
        {
            Ok(p) => {
                let changed = p.data != data;
                out.push((copy, p, changed));
            }
            Err(e) => errors.add(c, copy, access.hide(c, e, Some(&data)))?,
        }
    }
    errors.check()?;
    Ok(out)
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
    // shortcut: one transaction locks and purges the whole trash at once (ids bound as one
    // array); fine for thousands, batch it (and report progress) past ~50k trashed rows.
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
