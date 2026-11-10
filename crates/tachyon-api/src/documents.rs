//! `/api/{collection}` and `/api/{collection}/{id}`.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::StatusCode;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tachyon_core::auth::Action;
use tachyon_core::query::value_kind;
use tachyon_core::select::{Select, project};
use tachyon_core::validate::Mode;
use tachyon_core::{CollectionKind, FieldError, merged, validate_document_in};
use tachyon_db::DbError;
use tachyon_db::DocCopy;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Document, FileChange};
use tachyon_db::indexes::{IndexKind, index_name};
use tachyon_db::jobs;
use tachyon_db::query::{self, ListQuery, Readable, get_many_readable};
use tachyon_db::versions::record_version;
use uuid::Uuid;

use crate::access::{Access, both};
use crate::joins::fill_joins;
use crate::media_jobs::queue_sizes;
use crate::params::{
    Format, Joins, create_params, list_params, no_params, read_params, update_params,
};
use crate::populate::{
    Shapes, check_links, fill_virtuals, from_markdown, link_ids, link_targets, populate, prepare,
    to_value, vouched_links,
};
use crate::uploads::{
    Incoming, UploadBody, check_and_process, file_limit, focal_of, persist, require_file, scope_of,
    server_fields, settle_server_set, taken_or_abandoned, with_fields,
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

/// How strictly a save to `copy` is validated: a draft may leave out what's required.
pub(crate) fn mode_for(copy: DocCopy) -> Mode {
    match copy {
        DocCopy::Draft => Mode::Draft,
        DocCopy::Published => Mode::Full,
    }
}

/// Records `doc`, just saved to `copy` of `c`, as a version when `c` keeps them, unless
/// it's an autosave.
pub(crate) async fn record(
    tx: &mut sqlx::PgConnection,
    c: &Collection,
    doc: &Document,
    copy: DocCopy,
    by: Option<Uuid>,
    autosave: bool,
) -> Result<(), ApiError> {
    if c.schema.versioned() && !autosave {
        record_version(tx, doc.id, c, copy, &doc.data.0, by).await?;
    }
    Ok(())
}

/// A malformed ID can't match any document, so it's a 404 rather than a 400.
pub(crate) fn parse_id(id: &str) -> Result<Uuid, ApiError> {
    id.parse().map_err(|_| ApiError::NotFound)
}

/// The most a JSON body, or a `_payload`, may hold.
pub(crate) const JSON_LIMIT: usize = 10 * 1024 * 1024;

pub(crate) fn json_body(body: Result<Json<Value>, JsonRejection>) -> Result<Value, ApiError> {
    body.map(|Json(v)| v).map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::BodyTooLarge(format!(
                "the request body is over {JSON_LIMIT} bytes (10 MiB)"
            ))
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
    access.permit(Action::Read, &c)?;
    let p = list_params(raw.as_deref(), &c, access)?;
    let read = access.readable_of(&c).ok_or_else(|| access.refuse())?;
    let filter = both(p.filter, read.filter);
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
        copy: read.copy,
        owner: read.owner,
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
        p.format,
    )
    .await?;
    let docs = docs
        .into_iter()
        .map(|(v, c)| (shaped(v, p.select.as_ref()), c))
        .collect();
    let docs = populate(state, access, docs, p.depth, &p.populate, p.format).await?;
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

/// The document just written, as the `copy` written, in JSON with its relationships
/// populated to `depth` as far as the caller may read them; only its ID when they can't
/// read that copy.
pub(crate) async fn respond(
    state: &AppState,
    access: &Access,
    c: Arc<Collection>,
    doc: &Document,
    copy: DocCopy,
    depth: u8,
) -> Result<Json<Value>, ApiError> {
    if !access.can_read(&state.pool, &c, doc.id, copy).await? {
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
        Format::Json,
    )
    .await?;
    Ok(Json(
        populate(state, access, items, depth, &Shapes::new(), Format::Json)
            .await?
            .remove(0),
    ))
}

pub async fn create(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<String>, PathRejection>,
    RawQuery(raw): RawQuery,
    req: Request,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let c = collection(&state, &slug)?;
    access.scope(Action::Create, &c.schema.slug)?;
    if !access.may_create(&c) {
        return Err(access.refuse());
    }
    let wait = create_params(raw.as_deref())?;
    let deadline = deadline(&req);
    let copy = access.write_copy(&c)?;
    // Read only now, so a caller who may not create never gets a byte spooled.
    let body = UploadBody::read(req, file_limit(&state, &c), &state.media.temp_dir).await?;
    let (body, file) = match body {
        UploadBody::Json(v) => (v, None),
        UploadBody::Upload { payload, file } => (payload, Some(file)),
    };
    let hide = |e, body: &Value| access.hide(&c, e, Some(&merged(c.schema.data(), body, None)));
    let mut body = settle_server_set(&c, &body, None).map_err(|e| hide(e, &body))?;
    require_file(&c, file.is_some())?;
    from_markdown(&state, &access, &c, &mut body, None).await?;
    access.writes(&c, &body, Action::Create, None)?;
    // Decoded only once the write is allowed.
    let checked = match (&c.schema.upload, file) {
        (Some(u), Some(Incoming::Spooled(spool))) => {
            Some((u, check_and_process(&state, u, spool).await?))
        }
        _ => None,
    };
    let targets = link_targets(&state, &c).await?;
    // Checked as the document will be, server-set fields and all, before anything is stored.
    let would = checked
        .as_ref()
        .map(|(u, f)| server_fields(u, f, &f.spool.name));
    let input = with_fields(&body, would.as_ref());
    let mut prepared = validate_document_in(mode_for(copy), c.schema.data(), &input, None)
        .map_err(ApiError::Validation)
        .and_then(|data| prepare(&targets, &c, data))
        .map_err(|e| hide(e, &input))?;
    check_links(&state, &access, &prepared, &[]).await?;
    let stored = match &checked {
        Some((u, f)) => {
            let (scope, focal) = (scope_of(u, &prepared.data), focal_of(&prepared.data));
            Some(persist(&state, &c, u, f, &scope, focal, None).await?)
        }
        None => None,
    };
    if let Some(s) = &stored {
        prepared.data.extend(s.fields.clone());
        prepared.file = FileChange::Set(s.file.id);
    }
    let by = access.actor(Action::Create, &c).map(|a| a.id);
    let (doc, sizes) = taken_or_abandoned(&state, stored.as_ref(), async {
        let mut tx = state.pool.begin().await.map_err(DbError::from)?;
        let doc = documents::insert_in(&mut tx, c.id, c.schema_version, None, copy, &prepared, by)
            .await
            .map_err(|e| access.hide(&c, unique_error(&c, e.into()), None))?;
        record(&mut tx, &c, &doc, copy, by, false).await?;
        let sizes = match &stored {
            Some(s) => queue_sizes(&mut tx, &c, s.file.id).await?,
            None => None,
        };
        tx.commit().await.map_err(DbError::from)?;
        Ok((doc, sizes))
    })
    .await?;
    let doc = waited(&state, &c, doc, copy, sizes.filter(|_| wait), deadline).await;
    Ok((
        StatusCode::CREATED,
        respond(&state, &access, c, &doc, copy, 1).await?,
    ))
}

/// When the request's time is up, if a time limit is set on it.
fn deadline(req: &Request) -> Option<tokio::time::Instant> {
    req.extensions().get::<crate::Deadline>().map(|d| d.0)
}

/// `doc` read again once its file's sizes job `sizes` has finished, or once `deadline` is
/// a second away, whichever comes first (call 5); `doc` itself with no job to wait for.
/// Polled, so it works whichever server runs the job.
async fn waited(
    state: &AppState,
    c: &Collection,
    doc: Document,
    copy: DocCopy,
    sizes: Option<Uuid>,
    deadline: Option<tokio::time::Instant>,
) -> Document {
    const EVERY: std::time::Duration = std::time::Duration::from_millis(200);
    let Some(job) = sizes else { return doc };
    // Without a time limit on the request (a router built without one), 30 s at most.
    let deadline = deadline
        .unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(30));
    let until = Some(deadline - std::time::Duration::from_secs(1));
    loop {
        match jobs::status(&state.pool, job).await {
            Ok(Some(s)) if !matches!(s.state.as_str(), "queued" | "running") => break,
            Ok(Some(_)) => {}
            // Gone, or unreadable: there's nothing more to wait for.
            Ok(None) | Err(_) => break,
        }
        let next = tokio::time::Instant::now() + EVERY;
        if until.is_some_and(|u| next > u) {
            break;
        }
        tokio::time::sleep(EVERY).await;
    }
    documents::get(&state.pool, c.id, doc.id, copy)
        .await
        .unwrap_or(doc)
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
        p.format,
    )
    .await?;
    let (value, c) = items.remove(0);
    let value = shaped(value, p.select.as_ref());
    Ok(Json(
        populate(
            &state,
            &access,
            vec![(value, c)],
            p.depth,
            &p.populate,
            p.format,
        )
        .await?
        .remove(0),
    ))
}

pub async fn update(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<(String, String)>, PathRejection>,
    RawQuery(raw): RawQuery,
    req: Request,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    // Checked again later; here so a key's 403 comes before a malformed ID's 404.
    access.scope(Action::Update, &slug)?;
    let id = parse_id(&id)?;
    let c = collection(&state, &slug)?;
    access.may_write(Action::Update, &c)?;
    let (autosave, wait) = update_params(raw.as_deref(), &c)?;
    let deadline = deadline(&req);
    let copy = access.write_copy(&c)?;
    let guard = access
        .write_guard(&state.pool, Action::Update, &c, id)
        .await?;
    let actor = access.actor(Action::Update, &c);
    // The update rule and ownership, judged on the stored document before a byte of the body
    // is read, so a refused caller never has a file spooled or stored. The lock judges again.
    let own = actor.filter(|a| a.own_only).map(|a| a.id);
    if guard.is_some() || own.is_some() {
        let may = Readable {
            filter: guard.clone(),
            copy,
            owner: own,
            otherwise: None,
        };
        let one = HashMap::from([(c.id, may)]);
        if get_many_readable(&state.pool, &[id], &one)
            .await?
            .is_empty()
        {
            return Err(match (own, &guard) {
                (Some(_), None) => DbError::NotOwner,
                _ => DbError::Forbidden,
            }
            .into());
        }
    }
    // Read only now, so a caller who may not change this document never gets a byte spooled.
    let body = UploadBody::read(req, file_limit(&state, &c), &state.media.temp_dir).await?;
    let (sent, file) = match body {
        UploadBody::Json(v) => (v, None),
        UploadBody::Upload { payload, file } => (payload, Some(file)),
    };
    // Which fields the patch writes depends on the block rows it replaces, so it's checked
    // against the stored document, then again against the locked one. `write_guard` has
    // seen the caller may read this copy of it.
    let stored = documents::get(&state.pool, c.id, id, copy).await?.data.0;
    // Server-set fields are only compared, here and again against the locked document, and
    // never written from the patch: the stored ones, or the new file's, stay.
    let settle = |current: &Map<String, Value>| {
        settle_server_set(&c, &sent, Some(current))
            .map_err(|e| access.hide(&c, e, Some(&merged(c.schema.data(), &sent, Some(current)))))
    };
    let mut patch = settle(&stored)?;
    from_markdown(&state, &access, &c, &mut patch, Some(&stored)).await?;
    access.writes(&c, &patch, Action::Update, Some(&stored))?;
    // Decoded only once the write is allowed.
    let checked = match (&c.schema.upload, file) {
        (Some(u), Some(Incoming::Spooled(spool))) => {
            Some((u, check_and_process(&state, u, spool).await?))
        }
        _ => None,
    };
    let targets = link_targets(&state, &c).await?;
    // Validated with the server-set fields the file brings; those are written in after, as
    // they are, `null`s included.
    let apply = |current: &Map<String, Value>, fields: Option<&Map<String, Value>>| {
        settle(current)?;
        access.writes(&c, &patch, Action::Update, Some(current))?;
        let input = with_fields(&patch, fields);
        let mut next = validate_document_in(mode_for(copy), c.schema.data(), &input, Some(current))
            .map_err(ApiError::Validation)
            .and_then(|data| prepare(&targets, &c, data))
            .map_err(|e| {
                let doc = merged(c.schema.data(), &input, Some(current));
                access.hide(&c, e, Some(&doc))
            })?;
        next.data.extend(fields.cloned().unwrap_or_default());
        Ok::<_, ApiError>(next)
    };
    let would = checked
        .as_ref()
        .map(|(u, f)| server_fields(u, f, &f.spool.name));
    // Links the patch adds are checked against the stored document, then again against the
    // locked one, in case it changed in between.
    let dry = apply(&stored, would.as_ref())?;
    let vouched = check_links(&state, &access, &dry, &link_ids(&c, &stored)).await?;
    let saved = match &checked {
        Some((u, f)) => {
            let (scope, focal) = (scope_of(u, &dry.data), focal_of(&dry.data));
            Some(persist(&state, &c, u, f, &scope, focal, Some(id)).await?)
        }
        None => None,
    };
    let (doc, sizes) = taken_or_abandoned(&state, saved.as_ref(), async {
        let mut tx = state.pool.begin().await.map_err(DbError::from)?;
        let doc = documents::update_in(
            &mut tx,
            c.id,
            c.schema_version,
            id,
            copy,
            actor,
            guard.as_ref(),
            |current| {
                let mut next = apply(current, saved.as_ref().map(|s| &s.fields))?;
                vouched_links(&next, &link_ids(&c, current), &vouched)?;
                if let Some(s) = &saved {
                    next.file = FileChange::Set(s.file.id);
                }
                Ok(next)
            },
        )
        .await
        .map_err(|e| access.hide(&c, unique_error(&c, e), None))?;
        record(&mut tx, &c, &doc, copy, actor.map(|a| a.id), autosave).await?;
        let sizes = match &saved {
            Some(s) => queue_sizes(&mut tx, &c, s.file.id).await?,
            None => None,
        };
        tx.commit().await.map_err(DbError::from)?;
        Ok((doc, sizes))
    })
    .await?;
    let doc = waited(&state, &c, doc, copy, sizes.filter(|_| wait), deadline).await;
    respond(&state, &access, c, &doc, copy, 1).await
}

pub async fn delete(
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
    access.may_write(Action::Delete, &c)?;
    no_params(raw.as_deref())?;
    let copy = access.write_copy(&c)?;
    let guard = access
        .write_guard(&state.pool, Action::Delete, &c, id)
        .await?;
    documents::delete_as(
        &state.pool,
        c.id,
        id,
        access.actor(Action::Delete, &c),
        guard.as_ref(),
        copy,
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
    if !access.sees(&state.pool, &c, id).await? {
        return Err(ApiError::NotFound);
    }
    let mut refs =
        documents::references(&state.pool, id, &[DocCopy::Published, DocCopy::Draft]).await?;
    if !refs.is_empty() {
        let ids: Vec<Uuid> = refs.iter().map(|r| r.id).collect();
        // Each link is seen only by who may read the copy holding it: a draft's, by who
        // may read that draft.
        let mut readable: HashSet<(Uuid, DocCopy)> = HashSet::new();
        for drafts in [false, true] {
            let mut read = access.readable_with_globals(drafts);
            read.retain(|_, r| (r.copy == DocCopy::Draft) == drafts);
            let copy = if drafts {
                DocCopy::Draft
            } else {
                DocCopy::Published
            };
            for (_, doc) in get_many_readable(&state.pool, &ids, &read).await? {
                readable.insert((doc.id, copy));
            }
        }
        // A link held in a field the caller can't read isn't theirs to see either. Admins
        // read every field, and see every link, so they can always tell what blocks a delete.
        let admin = access.caller().admin;
        refs.retain(|r| {
            readable.contains(&(r.id, r.copy))
                && (admin
                    || state
                        .schemas
                        .get(&r.collection)
                        .is_some_and(|from| access.reads(&from, &r.path)))
        });
    }
    Ok(Json(json!({ "docs": refs })))
}
