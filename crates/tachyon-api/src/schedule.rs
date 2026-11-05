//! Scheduled publishing: `PUT`, `GET` and `DELETE /api/{collection}/{id}/schedule` and
//! `/api/globals/{slug}/schedule`, and the `publish` and `unpublish` jobs they queue.
//!
//! A scheduled publish ships the draft as it was when it was scheduled, held as a `draft`
//! version, so edits made since wait for the next publish. A job acts as whoever scheduled
//! it is when it runs: their role then, and their key's scopes then. One they can no longer
//! publish with fails, and nothing is published.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgConnection;
use tachyon_core::FieldError;
use tachyon_core::auth::Action;
use tachyon_core::validate::Mode;
use tachyon_core::validate_document_in;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Prepared};
use tachyon_db::jobs::{self, Job, NewJob};
use tachyon_db::versions::{self, record_version};
use tachyon_db::{DbError, DocCopy};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::access::{Access, Reach};
use crate::auth::principal::Principal;
use crate::documents::{json_body, unique_error};
use crate::jobs::JobError;
use crate::params::no_params;
use crate::populate::{link_targets, prepare, published_links};
use crate::versions::{Doc, DocPath, GlobalPath, in_collection, of_global};
use crate::{ApiError, AppState};

const PUBLISH: &str = "publish";
const UNPUBLISH: &str = "unpublish";

/// What a job is for, so a new one replaces it: `publish:<document id>`.
fn dedupe_key(kind: &str, id: Uuid) -> String {
    format!("{kind}:{id}")
}

/// One side of a schedule as sent: left out leaves it, `null` cancels it, a time sets it.
type Side = Option<Option<OffsetDateTime>>;

/// The body of a `PUT`: `publishAt` and `unpublishAt`, each an RFC 3339 time within the
/// next 10 years, `null`, or left out.
fn sides(body: &Value) -> Result<(Side, Side), ApiError> {
    const BAD: &str = "must be an RFC 3339 date and time, or null";
    let Value::Object(body) = body else {
        return Err(ApiError::BadRequest(
            "send an object with publishAt, unpublishAt or both".into(),
        ));
    };
    if let Some(k) = body
        .keys()
        .find(|k| *k != "publishAt" && *k != "unpublishAt")
    {
        return Err(ApiError::BadRequest(format!(
            "unknown key `{k}`; use publishAt or unpublishAt"
        )));
    }
    let now = OffsetDateTime::now_utc();
    // Only 29 February has no date 10 years on; the day before it does.
    let latest = now
        .replace_year(now.year() + 10)
        .unwrap_or(now + time::Duration::days(3652));
    let mut errors = Vec::new();
    let mut side = |key: &str| -> Side {
        let mut refuse = |message: &str| {
            errors.push(FieldError {
                path: key.into(),
                message: message.into(),
            });
            None
        };
        match body.get(key)? {
            Value::Null => Some(None),
            Value::String(s) => match OffsetDateTime::parse(s, &Rfc3339) {
                Ok(t) if t <= now => refuse("must be in the future"),
                Ok(t) if t > latest => refuse("must be at most 10 years from now"),
                Ok(t) => Some(Some(t)),
                Err(_) => refuse(BAD),
            },
            _ => refuse(BAD),
        }
    };
    let publish = side("publishAt");
    let unpublish = side("unpublishAt");
    match errors.is_empty() {
        true => Ok((publish, unpublish)),
        false => Err(ApiError::Validation(errors)),
    }
}

/// The document's pending publish and unpublish times, or nulls.
async fn pending(state: &AppState, id: Uuid) -> Result<Value, ApiError> {
    let mut out = json!({ "publishAt": null, "unpublishAt": null });
    for job in jobs::pending(&state.pool, id).await? {
        let key = match job.kind.as_str() {
            PUBLISH => "publishAt",
            UNPUBLISH => "unpublishAt",
            _ => continue,
        };
        let at = job
            .run_at
            .format(&Rfc3339)
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        out[key] = at.into();
    }
    Ok(out)
}

/// Replaces the pending job of `kind` with one at `job`'s time and payload, or cancels it.
async fn set(
    tx: &mut PgConnection,
    access: &Access,
    d: &Doc,
    kind: &str,
    job: Option<(OffsetDateTime, Value)>,
) -> Result<(), ApiError> {
    let key = dedupe_key(kind, d.id);
    let Some((run_at, payload)) = job else {
        jobs::cancel(tx, &key).await?;
        return Ok(());
    };
    let job = NewJob {
        kind,
        document_id: Some(d.id),
        collection_id: Some(d.c.id),
        payload,
        run_at,
        dedupe_key: Some(key),
        created_by: access.user_id(),
        key_id: access.key_id(),
    };
    match jobs::enqueue(tx, &job).await {
        Ok(_) => Ok(()),
        // Only when more callers than enqueue retries for schedule this at once.
        Err(DbError::UniqueViolation(c)) if c == "jobs_one_pending" => Err(ApiError::Conflict(
            "the schedule changed at the same time; try again".into(),
        )),
        Err(e) => Err(e.into()),
    }
}

/// The schedule, to a caller who may read the draft; only the ID to one who can't, such
/// as a publish-only key (Ruling T5-2).
async fn respond(state: &AppState, access: &Access, d: &Doc) -> Result<Json<Value>, ApiError> {
    if !access
        .can_read(&state.pool, &d.c, d.id, DocCopy::Draft)
        .await?
    {
        return Ok(Json(json!({ "id": d.id })));
    }
    Ok(Json(pending(state, d.id).await?))
}

/// Sets either side or both, as `publish` and `unpublish` would be checked now. A publish
/// takes its snapshot of the draft now, which must pass full validation.
async fn put_schedule(
    state: &AppState,
    access: &Access,
    d: Doc,
    raw: Option<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let c = &d.c;
    // A collection's handler checked it before reading the ID.
    if d.global() {
        access.scope(Action::Publish, &c.schema.slug)?;
    }
    no_params(raw.as_deref())?;
    let (publish, unpublish) = sides(&json_body(body)?)?;
    let guard = access.publish_guard(&state.pool, c, d.id).await?;
    let targets = link_targets(state, c).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    // Locked as a publish locks it, so the rights are judged on the draft snapshotted, and
    // a trash waits for this to commit, then cancels what it queued.
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
    if let Some(at) = publish {
        let job = match at {
            None => None,
            Some(at) => {
                validate_document_in(Mode::Full, c.schema.data(), &json!({}), Some(&draft))
                    .map_err(ApiError::Validation)
                    .and_then(|data| prepare(&targets, c, data))
                    .map_err(|e| access.hide(c, e, Some(&draft)))?;
                let version =
                    record_version(&mut tx, d.id, c, DocCopy::Draft, &draft, access.user_id())
                        .await?;
                Some((at, json!({ "version": version })))
            }
        };
        set(&mut tx, access, &d, PUBLISH, job).await?;
    }
    if let Some(at) = unpublish {
        set(&mut tx, access, &d, UNPUBLISH, at.map(|at| (at, json!({})))).await?;
    }
    tx.commit().await.map_err(DbError::from)?;
    respond(state, access, &d).await
}

/// What's scheduled, for a caller who may read the draft.
async fn get_schedule(
    state: &AppState,
    access: &Access,
    d: Doc,
    raw: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let c = &d.c;
    if d.global() {
        access.scope(Action::ReadDrafts, &c.schema.slug)?;
    }
    no_params(raw.as_deref())?;
    access.copy_for(c, true)?;
    if !access
        .can_read(&state.pool, c, d.id, DocCopy::Draft)
        .await?
    {
        // A global's existence isn't secret; one never saved has nothing scheduled.
        let known = d.global()
            && documents::get(&state.pool, c.id, d.id, DocCopy::Draft)
                .await
                .is_ok();
        return Err(match known {
            true => access.refuse(),
            false => ApiError::NotFound,
        });
    }
    Ok(Json(pending(state, d.id).await?))
}

/// Cancels both sides.
async fn delete_schedule(
    state: &AppState,
    access: &Access,
    d: Doc,
    raw: Option<String>,
) -> Result<StatusCode, ApiError> {
    let c = &d.c;
    if d.global() {
        access.scope(Action::Publish, &c.schema.slug)?;
    }
    no_params(raw.as_deref())?;
    let guard = access.publish_guard(&state.pool, c, d.id).await?;
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
    for kind in [PUBLISH, UNPUBLISH] {
        set(&mut tx, access, &d, kind, None).await?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Why a run failed, from an API error: only a fault of ours, or of the database, is worth
/// running again.
fn why(e: ApiError) -> JobError {
    JobError::Permanent(match e {
        ApiError::Internal(cause) => return JobError::Retry(cause),
        ApiError::Validation(errors) => errors
            .iter()
            .map(|e| format!("{}: {}", e.path, e.message))
            .collect::<Vec<_>>()
            .join("; "),
        ApiError::NotFound => "document deleted".into(),
        ApiError::Unique(constraint) => format!("a value must be unique ({constraint})"),
        ApiError::Forbidden(d)
        | ApiError::Unauthorized(d)
        | ApiError::BadRequest(d)
        | ApiError::Conflict(d) => d,
        other => format!("{other:?}"),
    })
}

/// `why`, for a database error: a schema change or a database fault runs again.
fn db(e: DbError) -> JobError {
    match e {
        DbError::SchemaChanged | DbError::Sqlx(_) => JobError::Retry(e.to_string()),
        e => why(e.into()),
    }
}

/// Why the scheduler's rights refuse the run now.
fn refused(e: ApiError) -> JobError {
    match why(e) {
        JobError::Permanent(d) => {
            JobError::Permanent(format!("whoever scheduled it can't publish it now: {d}"))
        }
        retry => retry,
    }
}

/// The snapshot a publish job ships, checked as a publish checks the draft: full
/// validation, and links only to published documents the scheduler may read.
async fn snapshot(
    state: &AppState,
    access: &Access,
    c: &Collection,
    id: Uuid,
    job: &Job,
) -> Result<Prepared, JobError> {
    let version = job
        .payload
        .get("version")
        .and_then(Value::as_str)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| JobError::Permanent("the job names no version".into()))?;
    let data = match versions::get(&state.pool, id, version).await {
        Ok(v) => v.data.unwrap_or_default(),
        Err(DbError::NotFound) => {
            return Err(JobError::Permanent("the scheduled version is gone".into()));
        }
        Err(e) => return Err(db(e)),
    };
    let targets = link_targets(state, c).await.map_err(why)?;
    let prepared = validate_document_in(Mode::Full, c.schema.data(), &json!({}), Some(&data))
        .map_err(ApiError::Validation)
        .and_then(|data| prepare(&targets, c, data))
        .map_err(why)?;
    published_links(state, access, &prepared)
        .await
        .map_err(why)?;
    Ok(prepared)
}

/// Runs a `publish` or `unpublish` job, as whoever scheduled it is now, in one transaction
/// with `complete_in`.
pub(crate) async fn run(state: &AppState, job: &Job) -> Result<(), JobError> {
    let gone = || JobError::Permanent("document deleted".into());
    let (Some(id), Some(collection)) = (job.document_id, job.collection_id) else {
        return Err(JobError::Permanent("the job names no document".into()));
    };
    // Deleting a collection deletes its jobs, so this one's is still loading.
    let c = state
        .schemas
        .get_by_id(collection)
        .ok_or_else(|| JobError::Retry("its collection isn't loaded on this server yet".into()))?;
    let user = job
        .created_by
        .ok_or_else(|| JobError::Permanent("nobody scheduled it".into()))?;
    let principal = Principal::for_job(state, user, job.key_id)
        .await
        .map_err(why)?
        .ok_or_else(|| {
            JobError::Permanent(
                match job.key_id {
                    Some(_) => "the API key that scheduled it was revoked or has expired, or its owner was removed",
                    None => "the user who scheduled it was removed",
                }
                .into(),
            )
        })?;
    let access = Access::new(state, principal, Reach::From(&c))
        .await
        .map_err(why)?;
    match documents::get(&state.pool, c.id, id, DocCopy::Draft).await {
        Ok(_) => {}
        Err(DbError::NotFound) => return Err(gone()),
        Err(e) => return Err(db(e)),
    }
    access
        .scope(Action::Publish, &c.schema.slug)
        .map_err(refused)?;
    let guard = access
        .publish_guard(&state.pool, &c, id)
        .await
        .map_err(refused)?;
    let publish = match job.kind == PUBLISH {
        true => Some(snapshot(state, &access, &c, id, job).await?),
        false => None,
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(DbError::from)
        .map_err(db)?;
    documents::lock_in(
        &mut tx,
        c.id,
        c.schema_version,
        id,
        DocCopy::Draft,
        guard.filter.as_ref(),
        guard.owner,
    )
    .await
    .map_err(|e| match e {
        DbError::NotFound => gone(),
        DbError::Forbidden => JobError::Permanent(
            "whoever scheduled it can't publish it now: an access rule refuses it".into(),
        ),
        e => db(e),
    })?;
    // After the document's lock, which every write takes first, and before the work: from
    // here no other worker can take the job over.
    if !jobs::complete_in(&mut tx, job).await.map_err(db)? {
        return Ok(());
    }
    match &publish {
        Some(p) => documents::publish_in(&mut tx, &c, id, Some(p), Some(user))
            .await
            .map(drop)
            .map_err(|e| why(unique_error(&c, e.into())))?,
        None => documents::unpublish_in(&mut tx, &c, id, Some(user))
            .await
            .map(drop)
            .map_err(db)?,
    }
    tx.commit().await.map_err(DbError::from).map_err(db)?;
    Ok(())
}

type Body = Result<Json<Value>, JsonRejection>;

pub async fn put(
    State(state): State<AppState>,
    access: Access,
    path: DocPath,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let d = in_collection(&state, &access, Action::Publish, &slug, &id)?;
    put_schedule(&state, &access, d, raw, body).await
}

pub async fn get(
    State(state): State<AppState>,
    access: Access,
    path: DocPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let d = in_collection(&state, &access, Action::ReadDrafts, &slug, &id)?;
    get_schedule(&state, &access, d, raw).await
}

pub async fn delete(
    State(state): State<AppState>,
    access: Access,
    path: DocPath,
    RawQuery(raw): RawQuery,
) -> Result<StatusCode, ApiError> {
    let Path((slug, id)) = path.map_err(|_| ApiError::NotFound)?;
    let d = in_collection(&state, &access, Action::Publish, &slug, &id)?;
    delete_schedule(&state, &access, d, raw).await
}

pub async fn put_global(
    State(state): State<AppState>,
    access: Access,
    path: GlobalPath,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    put_schedule(&state, &access, of_global(&state, slug)?, raw, body).await
}

pub async fn get_global(
    State(state): State<AppState>,
    access: Access,
    path: GlobalPath,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    get_schedule(&state, &access, of_global(&state, slug)?, raw).await
}

pub async fn delete_global(
    State(state): State<AppState>,
    access: Access,
    path: GlobalPath,
    RawQuery(raw): RawQuery,
) -> Result<StatusCode, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    delete_schedule(&state, &access, of_global(&state, slug)?, raw).await
}
