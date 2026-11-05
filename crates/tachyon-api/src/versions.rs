//! Publishing and history: `/api/{collection}/{id}/publish`, `…/unpublish`, `…/versions`,
//! `…/versions/{vid}`, `…/versions/{vid}/restore` and `…/diff`, and the same for globals
//! under `/api/globals/{slug}/…`. Versions are read at depth 0: links stay IDs.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, RawQuery, State};
use serde_json::{Map, Value, json};
use tachyon_core::auth::Action;
use tachyon_core::history::diff_documents;
use tachyon_core::validate::Mode;
use tachyon_core::{CollectionKind, validate_document_in};
use tachyon_db::DbError;
use tachyon_db::DocCopy;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Document};
use tachyon_db::versions;
use uuid::Uuid;

use crate::access::Access;
use crate::documents::{collection, mode_for, parse_id, record, unique_error};
use crate::globals::global;
use crate::params::{diff_params, no_params, page_params};
use crate::populate::{
    check_links, link_ids, link_targets, prepare, published_links, vouched_links,
};
use crate::{ApiError, AppState};

/// The document a route names: one of a collection, or a global's own.
pub(crate) struct Doc {
    pub(crate) c: Arc<Collection>,
    pub(crate) id: Uuid,
}

impl Doc {
    pub(crate) fn global(&self) -> bool {
        self.c.schema.kind == CollectionKind::Global
    }

    /// What changing it is called: a global is only ever updated as a whole.
    fn update(&self) -> Action {
        match self.global() {
            true => Action::UpdateGlobal,
            false => Action::Update,
        }
    }
}

/// A collection's document, with a key's 403 for `action` coming before a bad ID's 404.
pub(crate) fn in_collection(
    state: &AppState,
    access: &Access,
    action: Action,
    slug: &str,
    id: &str,
) -> Result<Doc, ApiError> {
    access.scope(action, slug)?;
    let id = parse_id(id)?;
    Ok(Doc {
        c: collection(state, slug)?,
        id,
    })
}

/// A global's document.
pub(crate) fn of_global(state: &AppState, slug: String) -> Result<Doc, ApiError> {
    let c = global(state, Ok(Path(slug)))?;
    Ok(Doc { id: c.id, c })
}

/// The document just written, as `copy`, the way a save answers.
async fn answer(
    state: &AppState,
    access: &Access,
    d: &Doc,
    doc: &Document,
    copy: DocCopy,
) -> Result<Json<Value>, ApiError> {
    match d.global() {
        true => crate::globals::respond(state, access, d.c.clone(), doc, copy, 1).await,
        false => crate::documents::respond(state, access, d.c.clone(), doc, copy, 1).await,
    }
}

/// Publishes the draft: it must pass full validation, and link only to documents that are
/// published and that the caller may read.
async fn publish_doc(
    state: &AppState,
    access: &Access,
    d: Doc,
    raw: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let c = &d.c;
    // A collection's handler checked it before reading the ID.
    if d.global() {
        access.scope(Action::Publish, &c.schema.slug)?;
    }
    no_params(raw.as_deref())?;
    let guard = access.publish_guard(&state.pool, c, d.id).await?;
    let targets = link_targets(state, c).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let draft = documents::lock_in(
        &mut tx,
        c.id,
        c.schema_version,
        d.id,
        DocCopy::Draft,
        guard.filter.as_ref(),
        guard.owner,
    )
    .await?;
    let prepared = validate_document_in(Mode::Full, c.schema.data(), &json!({}), Some(&draft))
        .map_err(ApiError::Validation)
        .and_then(|data| prepare(&targets, c, data))
        .map_err(|e| access.hide(c, e, Some(&draft)))?;
    published_links(state, access, &prepared)
        .await
        .map_err(|e| access.hide(c, e, Some(&draft)))?;
    let doc = documents::publish_in(&mut tx, c, d.id, Some(&prepared), access.user_id())
        .await
        .map_err(|e| access.hide(c, unique_error(c, e.into()), None))?;
    tx.commit().await.map_err(DbError::from)?;
    answer(state, access, &d, &doc, DocCopy::Published).await
}

/// Takes the published copy down; the draft stays.
async fn unpublish_doc(
    state: &AppState,
    access: &Access,
    d: Doc,
    raw: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let c = &d.c;
    // A collection's handler checked it before reading the ID.
    if d.global() {
        access.scope(Action::Publish, &c.schema.slug)?;
    }
    no_params(raw.as_deref())?;
    let guard = access.publish_guard(&state.pool, c, d.id).await?;
    // The answer is the draft, its links read as a draft's are.
    let copy = access.write_copy(c)?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    documents::lock_in(
        &mut tx,
        c.id,
        c.schema_version,
        d.id,
        DocCopy::Draft,
        guard.filter.as_ref(),
        guard.owner,
    )
    .await?;
    let doc = documents::unpublish_in(&mut tx, c, d.id, access.user_id()).await?;
    tx.commit().await.map_err(DbError::from)?;
    answer(state, access, &d, &doc, copy).await
}

/// A page of versions, newest first, without their data.
async fn list_versions(
    state: &AppState,
    access: &Access,
    d: Doc,
    raw: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let drafts = access.versions_of(&state.pool, &d.c, d.id).await?;
    let (limit, page) = page_params(raw.as_deref())?;
    let offset = (page - 1)
        .checked_mul(limit)
        .ok_or_else(|| ApiError::BadRequest("page is too large".into()))?;
    let mut docs = versions::list(&state.pool, d.id, drafts, limit + 1, offset).await?;
    let more = docs.len() as i64 > limit;
    docs.truncate(limit as usize);
    Ok(Json(json!({
        "docs": docs,
        "limit": limit,
        "page": page,
        "hasNextPage": more,
    })))
}

/// Version `vid` of the document, as stored: a 404 when it was a draft and `drafts` (from
/// `versions_of`) says the caller can't read those.
async fn stored_version(
    state: &AppState,
    d: &Doc,
    vid: &str,
    drafts: bool,
) -> Result<versions::Version, ApiError> {
    let vid = parse_id(vid)?;
    let v = versions::get(&state.pool, d.id, vid).await?;
    match v.kind == DocCopy::Draft.as_str() && !drafts {
        true => Err(ApiError::NotFound),
        false => Ok(v),
    }
}

/// `stored_version`, without the fields the caller can't read.
async fn version(
    state: &AppState,
    access: &Access,
    d: &Doc,
    vid: &str,
    drafts: bool,
) -> Result<versions::Version, ApiError> {
    let mut v = stored_version(state, d, vid, drafts).await?;
    if let Some(data) = v.data.take() {
        v.data = Some(stripped(access, &d.c, data));
    }
    Ok(v)
}

fn stripped(access: &Access, c: &Collection, data: Map<String, Value>) -> Map<String, Value> {
    let mut value = Value::Object(data);
    access.strip(c, &mut value);
    match value {
        Value::Object(data) => data,
        _ => Map::new(),
    }
}

async fn get_version(
    state: &AppState,
    access: &Access,
    d: Doc,
    vid: String,
    raw: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let drafts = access.versions_of(&state.pool, &d.c, d.id).await?;
    no_params(raw.as_deref())?;
    Ok(Json(json!(version(state, access, &d, &vid, drafts).await?)))
}

/// What a restore of `version` writes over `current`: the version's value of each field
/// the caller may write with `action` (its absence too), and the current value of each
/// they may not.
fn restored(
    access: &Access,
    c: &Collection,
    action: Action,
    version: &Map<String, Value>,
    current: &Map<String, Value>,
) -> Map<String, Value> {
    let mut out = Map::new();
    for f in c.schema.data() {
        let theirs = version.get(&f.name);
        let sent = json!({ &f.name: theirs.cloned().unwrap_or(Value::Null) });
        let keep = match access.writes(c, &sent, action, Some(current)) {
            Ok(()) => theirs,
            Err(_) => current.get(&f.name),
        };
        if let Some(v) = keep {
            out.insert(f.name.clone(), v.clone());
        }
    }
    out
}

/// Writes a version back: into the draft with drafts (checked as a draft save is), else
/// into what's published. It never publishes.
async fn restore_version(
    state: &AppState,
    access: &Access,
    d: Doc,
    vid: String,
    raw: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let c = &d.c;
    let action = d.update();
    access.scope(action, &c.schema.slug)?;
    if !d.global() {
        access.may_write(action, c)?;
    }
    no_params(raw.as_deref())?;
    let drafts = access.versions_of(&state.pool, c, d.id).await?;
    let data = stored_version(state, &d, &vid, drafts)
        .await?
        .data
        .unwrap_or_default();
    let copy = access.write_copy(c)?;
    let guard = match d.global() {
        true => access.global_guard(c)?,
        false => access.write_guard(&state.pool, action, c, d.id).await?,
    };
    // As an update checks it: against the stored copy, then again against the locked one.
    let stored = documents::get(&state.pool, c.id, d.id, copy).await?.data.0;
    let targets = link_targets(state, c).await?;
    let nothing = Map::new();
    let apply = |current: &Map<String, Value>| {
        let next = restored(access, c, action, &data, current);
        // Replaces the copy whole: what the version lacks, the copy loses.
        validate_document_in(
            mode_for(copy),
            c.schema.data(),
            &Value::Object(next.clone()),
            Some(&nothing),
        )
        .map_err(ApiError::Validation)
        .and_then(|data| prepare(&targets, c, data))
        .map_err(|e| access.hide(c, e, Some(&next)))
    };
    let vouched = check_links(state, access, &apply(&stored)?, &link_ids(c, &stored)).await?;
    let actor = access.actor(action, c);
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let doc = documents::update_in(
        &mut tx,
        c.id,
        c.schema_version,
        d.id,
        copy,
        actor,
        guard.as_ref(),
        |current| {
            let next = apply(current)?;
            vouched_links(&next, &link_ids(c, current), &vouched)?;
            Ok(next)
        },
    )
    .await
    .map_err(|e| access.hide(c, unique_error(c, e), None))?;
    record(&mut tx, c, &doc, copy, actor.map(|a| a.id), false).await?;
    tx.commit().await.map_err(DbError::from)?;
    answer(state, access, &d, &doc, copy).await
}

/// One side of a diff, without the fields the caller can't read: a version (which needs
/// `readVersions`), `draft` (`readDrafts`) or `published` (`read`). Refused or missing is
/// a 404.
async fn side(
    state: &AppState,
    access: &Access,
    d: &Doc,
    which: &str,
) -> Result<Map<String, Value>, ApiError> {
    let copy = match which {
        "draft" if !d.c.schema.drafts() => {
            return Err(ApiError::BadRequest(format!(
                "`draft`: `{}` has no drafts",
                d.c.schema.slug
            )));
        }
        "draft" => DocCopy::Draft,
        "published" => DocCopy::Published,
        vid => {
            let drafts = access.versions_of(&state.pool, &d.c, d.id).await?;
            return Ok(version(state, access, d, vid, drafts)
                .await?
                .data
                .unwrap_or_default());
        }
    };
    let doc = access
        .read_copy(&state.pool, &d.c, d.id, copy)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(stripped(access, &d.c, doc.data.0))
}

async fn diff_doc(
    state: &AppState,
    access: &Access,
    d: Doc,
    raw: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let (from, to) = diff_params(raw.as_deref())?;
    let before = side(state, access, &d, &from).await?;
    let after = side(state, access, &d, &to).await?;
    let changes = diff_documents(d.c.schema.data(), &before, &after);
    Ok(Json(json!({ "from": from, "to": to, "changes": changes })))
}

pub(crate) type DocPath = Result<Path<(String, String)>, PathRejection>;
type VersionPath = Result<Path<(String, String, String)>, PathRejection>;
pub(crate) type GlobalPath = Result<Path<String>, PathRejection>;
type GlobalVersionPath = Result<Path<(String, String)>, PathRejection>;

pub async fn publish(
    State(state): State<AppState>,
    access: Access,
    path: DocPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let d = in_collection(&state, &access, Action::Publish, &slug, &id)?;
    publish_doc(&state, &access, d, raw).await
}

pub async fn unpublish(
    State(state): State<AppState>,
    access: Access,
    path: DocPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let d = in_collection(&state, &access, Action::Publish, &slug, &id)?;
    unpublish_doc(&state, &access, d, raw).await
}

pub async fn list(
    State(state): State<AppState>,
    access: Access,
    path: DocPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let d = in_collection(&state, &access, Action::ReadVersions, &slug, &id)?;
    list_versions(&state, &access, d, raw).await
}

pub async fn get(
    State(state): State<AppState>,
    access: Access,
    path: VersionPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id, vid)) = path.map_err(|_| ApiError::NotFound)?;
    let d = in_collection(&state, &access, Action::ReadVersions, &slug, &id)?;
    get_version(&state, &access, d, vid, raw).await
}

pub async fn restore(
    State(state): State<AppState>,
    access: Access,
    path: VersionPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id, vid)) = path.map_err(|_| ApiError::NotFound)?;
    let d = in_collection(&state, &access, Action::Update, &slug, &id)?;
    restore_version(&state, &access, d, vid, raw).await
}

pub async fn diff(
    State(state): State<AppState>,
    access: Access,
    path: DocPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    // Each side checks its own right; reading is the least any of them needs.
    let d = in_collection(&state, &access, Action::Read, &slug, &id)?;
    diff_doc(&state, &access, d, raw).await
}

pub async fn publish_global(
    State(state): State<AppState>,
    access: Access,
    path: GlobalPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    publish_doc(&state, &access, of_global(&state, slug)?, raw).await
}

pub async fn unpublish_global(
    State(state): State<AppState>,
    access: Access,
    path: GlobalPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    unpublish_doc(&state, &access, of_global(&state, slug)?, raw).await
}

pub async fn list_global(
    State(state): State<AppState>,
    access: Access,
    path: GlobalPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    list_versions(&state, &access, of_global(&state, slug)?, raw).await
}

pub async fn get_global(
    State(state): State<AppState>,
    access: Access,
    path: GlobalVersionPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, vid)) = path.map_err(|_| ApiError::NotFound)?;
    get_version(&state, &access, of_global(&state, slug)?, vid, raw).await
}

pub async fn restore_global(
    State(state): State<AppState>,
    access: Access,
    path: GlobalVersionPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, vid)) = path.map_err(|_| ApiError::NotFound)?;
    restore_version(&state, &access, of_global(&state, slug)?, vid, raw).await
}

pub async fn diff_global(
    State(state): State<AppState>,
    access: Access,
    path: GlobalPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    diff_doc(&state, &access, of_global(&state, slug)?, raw).await
}
