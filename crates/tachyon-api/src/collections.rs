//! `/api/admin/collections`. Unauthenticated until M3, which is why the server binds to localhost by default.
//!
//! A change or preview too big to finish inside its request (over `INLINE_ROWS`) runs on a
//! job worker instead: the route answers 202 with the job, which `GET /api/admin/jobs/{id}`
//! reports on until it holds what the route would have answered.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tachyon_core::access;
use tachyon_core::auth::Action;
use tachyon_core::diff::Renames;
use tachyon_core::{
    CollectionKind, CollectionSchema, FieldError, SchemaError, explain_parse_error,
};
use tachyon_db::DbError;
use tachyon_db::collections::{self, Collection};
use tachyon_db::indexes::reconcile_indexes;
use tachyon_db::jobs::{self, Job, NewJob};
use tachyon_db::schema_change::{self, ChangeError};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::authorize;
use crate::auth::principal::Principal;
use crate::documents::json_body;
use crate::jobs::{JobError, LEASE, keep_leased};
use crate::{ApiError, AppState};

/// The most rows (documents, trashed ones included, and their versions) a schema change or
/// preview scans inside its request; a bigger one runs on a job worker.
// shortcut: a constant, not a setting; make it one under `[jobs]` if anyone asks.
pub(crate) const INLINE_ROWS: i64 = 2_000;

/// The jobs a change and a preview too big for their request become.
pub(crate) const SCHEMA_CHANGE: &str = "schema_change";
pub(crate) const SCHEMA_PREVIEW: &str = "schema_preview";

fn to_json(c: &Collection) -> Value {
    json!({ "id": c.id, "schemaVersion": c.schema_version, "schema": c.schema.to_compact_json() })
}

fn schema_error(e: SchemaError) -> ApiError {
    ApiError::Validation(vec![FieldError {
        path: String::new(),
        message: e.to_string(),
    }])
}

/// Parses a schema, refusing unknown keys. Check it with `saved`, once its shared blocks are in place.
fn parse_schema(value: &Value) -> Result<CollectionSchema, ApiError> {
    CollectionSchema::check_option_types(value).map_err(schema_error)?;
    let schema = CollectionSchema::deserialize(value)
        .map_err(|e| ApiError::BadRequest(explain_parse_error(value, &e)))?;
    schema.reject_unknown_options(value).map_err(schema_error)?;
    Ok(schema)
}

/// Every schema as it would be with `schema` saved (see `Schemas::with_saved`), after
/// checking `schema` with its shared blocks in place, since a select or condition may
/// reach into one. Unknown keys were refused first: a typo such as `nmae` should be named,
/// not reported as a bad name.
fn saved(state: &AppState, schema: &CollectionSchema) -> Result<Vec<CollectionSchema>, ApiError> {
    // Slug and block rules first, so a bad slug isn't reported as a missing reference.
    if schema.block_refs().is_empty() {
        schema.check().map_err(schema_error)?;
    }
    let world = state.schemas.with_saved(schema).map_err(schema_error)?;
    world[0].check().map_err(schema_error)?;
    Ok(world)
}

/// Logged here so the cause survives even if the reload after it fails too.
fn log_index_failure(indexed: &Result<(), tachyon_db::DbError>, slug: &str) {
    if let Err(error) = indexed {
        tracing::warn!(%error, collection = slug, "couldn't build indexes; startup will retry");
    }
}

pub async fn list(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Value>, ApiError> {
    principal.require()?;
    let docs: Vec<Value> = collections::list(&state.pool)
        .await?
        .iter()
        .map(to_json)
        .collect();
    Ok(Json(json!({ "docs": docs })))
}

pub async fn create(
    State(state): State<AppState>,
    principal: Principal,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    authorize(&principal, Action::Schemas, "")?;
    let body = json_body(body)?;
    let schema = parse_schema(&body)?;
    if schema.slug == tachyon_db::auth::USERS {
        return Err(ApiError::Conflict(
            "users is built in; change its fields with PUT /api/admin/collections/users".into(),
        ));
    }
    let world = saved(&state, &schema)?;
    for target in world[0].relation_targets() {
        let ok = (target == schema.slug && schema.kind != CollectionKind::Block)
            || world
                .iter()
                .any(|c| c.slug == target && c.kind == CollectionKind::Collection);
        if !ok {
            return Err(schema_error(SchemaError::InvalidChange(format!(
                "relationTo `{target}` doesn't match an existing collection"
            ))));
        }
    }
    check_virtuals(&world)?;
    check_rules(&state, &world).await?;
    let created = schema_change::create(&state.pool, &schema)
        .await
        .map_err(change_error)?;
    // The schema is saved either way, so serve it even if its indexes failed to build.
    let indexed = reconcile_indexes(&state.pool, created.id).await;
    log_index_failure(&indexed, &schema.slug);
    state.schemas.reload(&state.pool).await?;
    indexed?;
    Ok((StatusCode::CREATED, Json(to_json(&created))))
}

pub async fn get_one(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    principal.require()?;
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    // As stored, shared blocks included and references kept.
    let c = collections::get_by_slug(&state.pool, &slug).await?;
    Ok(Json(to_json(&c)))
}

/// Deletes a shared block that no schema names. Collections and globals can't be deleted yet.
pub async fn delete(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    authorize(&principal, Action::Schemas, "")?;
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    collections::delete_block(&state.pool, &slug).await?;
    state.schemas.reload(&state.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ChangeRequest {
    schema: Value,
    #[serde(default)]
    renames: Renames,
    #[serde(default)]
    confirm: bool,
}

/// A schema in `world` (see `Schemas::with_saved`) by slug.
fn find<'w>(world: &'w [CollectionSchema]) -> impl Fn(&str) -> Option<&'w CollectionSchema> {
    move |slug| world.iter().find(|c| c.slug == slug)
}

/// Every virtual field of the saved schema, `world[0]`, must read a field its targets have,
/// and every join a field that links here.
fn check_virtuals(world: &[CollectionSchema]) -> Result<(), ApiError> {
    let lookup = find(world);
    world[0]
        .check_virtual_targets(&lookup)
        .and_then(|()| world[0].check_join_targets(&lookup))
        .map_err(schema_error)
}

/// A change must not break the virtual or join fields other collections read from it, such
/// as a join on a relationship this change removes, or on a field of a shared block.
fn check_dependents(world: &[CollectionSchema]) -> Result<(), ApiError> {
    let lookup = find(world);
    for other in &world[1..] {
        other
            .check_virtual_targets(&lookup)
            .and_then(|()| other.check_join_targets(&lookup))
            .map_err(|e| {
                schema_error(SchemaError::InvalidChange(format!(
                    "this change would break {}: {e}",
                    other.slug
                )))
            })?;
    }
    Ok(())
}

/// Every collection's access rules must hold with `schema` saved: its own, and the others',
/// which may filter through it or, when it's `users`, read its fields as `$user` values.
/// A role deleted after this check is caught where the schema is stored, under the lock
/// role deletes share (`tachyon_db::auth::check_rule_roles`).
// ponytail: a rule stored before these checks existed, which they'd now refuse, blocks
// saving every other collection until it's fixed. Fine before the first release, since no
// deployment holds rules yet; add a migration that reports them if that changes.
async fn check_rules(state: &AppState, world: &[CollectionSchema]) -> Result<(), ApiError> {
    let roles = tachyon_db::auth::roles_list(&state.pool).await?;
    let roles: Vec<&str> = roles.iter().map(|r| r.slug.as_str()).collect();
    let lookup = |slug: &str| find(world)(slug).cloned();
    let users = lookup(tachyon_db::auth::USERS)
        .map(|u| u.fields)
        .unwrap_or_default();
    access::check_rules(&world[0], &users, &roles, &lookup).map_err(schema_error)?;
    for other in &world[1..] {
        access::check_rules(other, &users, &roles, &lookup).map_err(|e| {
            schema_error(SchemaError::InvalidChange(format!(
                "this change would break {}: {e}",
                other.slug
            )))
        })?;
    }
    Ok(())
}

fn change_request(
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(String, ChangeRequest, CollectionSchema), ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let req: ChangeRequest = serde_json::from_value(json_body(body)?)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let schema = parse_schema(&req.schema)?;
    Ok((slug, req, schema))
}

fn change_error(e: ChangeError) -> ApiError {
    match e {
        ChangeError::NotFound => ApiError::NotFound,
        ChangeError::Invalid(e) => schema_error(e),
        ChangeError::Duplicates(p) => ApiError::Validation(
            p.duplicates
                .iter()
                .map(|f| FieldError {
                    path: f.clone(),
                    message: "has duplicate values; make them unique first".into(),
                })
                .collect(),
        ),
        ChangeError::NeedsConfirmation(p) => ApiError::NeedsConfirmation {
            detail: p.confirmation_scope().what(),
            preview: json!(*p),
        },
        ChangeError::PendingDrafts(message) => ApiError::Conflict(message),
        ChangeError::Db(e) => e.into(),
    }
}

pub async fn preview(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Response, ApiError> {
    authorize(&principal, Action::Schemas, "")?;
    let (slug, req, schema) = change_request(path, body)?;
    let world = saved(&state, &schema)?;
    check_virtuals(&world)?;
    check_dependents(&world)?;
    check_rules(&state, &world).await?;
    if let Some(queued) = queue(&state, &principal, SCHEMA_PREVIEW, &slug, &req).await? {
        return Ok(queued);
    }
    let p = schema_change::preview(&state.pool, &slug, &schema, &req.renames)
        .await
        .map_err(change_error)?;
    Ok(Json(json!(p)).into_response())
}

pub async fn update(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Response, ApiError> {
    authorize(&principal, Action::Schemas, "")?;
    let (slug, req, schema) = change_request(path, body)?;
    let world = saved(&state, &schema)?;
    check_virtuals(&world)?;
    check_dependents(&world)?;
    check_rules(&state, &world).await?;
    if let Some(queued) = queue(&state, &principal, SCHEMA_CHANGE, &slug, &req).await? {
        return Ok(queued);
    }
    let (c, p) = schema_change::apply(&state.pool, &slug, &schema, &req.renames, req.confirm)
        .await
        .map_err(change_error)?;
    let indexed = reconcile_indexes(&state.pool, c.id).await;
    log_index_failure(&indexed, &slug);
    state.schemas.reload(&state.pool).await?;
    indexed?;
    Ok(Json(json!({ "collection": to_json(&c), "preview": p })).into_response())
}

/// Queues a change or preview too big to run inside its request (over
/// `AppState::inline_rows`) as a `kind` job, and answers 202 with it; `None` when it's small
/// enough to run here. A schema has one such job at a time. A second is a 409, never a
/// replacement: dropping an admin's change unseen is worse than asking them to wait.
async fn queue(
    state: &AppState,
    principal: &Principal,
    kind: &str,
    slug: &str,
    req: &ChangeRequest,
) -> Result<Option<Response>, ApiError> {
    let key = format!("schema:{slug}");
    if schema_change::rows(&state.pool, slug).await? <= state.inline_rows {
        // Small enough to run here, but not ahead of a change already waiting (Ruling P1-3).
        return match jobs::pending_key(&state.pool, &key).await? {
            Some(id) => Err(already_queued(slug, id)),
            None => Ok(None),
        };
    }
    let job = NewJob {
        kind,
        document_id: None,
        collection_id: None,
        payload: json!({ "slug": slug, "request": req }),
        run_at: OffsetDateTime::now_utc(),
        dedupe_key: Some(key.clone()),
        created_by: principal.identity().map(|i| i.user_id),
        key_id: match principal {
            Principal::Key { key, .. } => Some(*key),
            _ => None,
        },
    };
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    let id = match jobs::add(&mut conn, &job).await {
        Ok(id) => id,
        Err(DbError::UniqueViolation(c)) if c == "jobs_one_pending" => {
            return Err(match jobs::pending_key(&state.pool, &key).await? {
                Some(id) => already_queued(slug, id),
                None => {
                    ApiError::Conflict(format!("a change to `{slug}` just finished; try again"))
                }
            });
        }
        Err(e) => return Err(e.into()),
    };
    let status = jobs::status(&state.pool, id)
        .await?
        .ok_or_else(|| ApiError::Internal(format!("job {id} went missing")))?;
    let location = format!("/api/admin/jobs/{id}");
    let body = json!({ "job": job_json(&status)? });
    Ok(Some(
        (
            StatusCode::ACCEPTED,
            [(header::LOCATION, location)],
            Json(body),
        )
            .into_response(),
    ))
}

fn already_queued(slug: &str, job: Uuid) -> ApiError {
    ApiError::Conflict(format!(
        "a change to `{slug}` is already queued; wait for job {job}"
    ))
}

/// A job as its route shows it. One that failed with no answer kept, such as one whose
/// lease ran out on its last attempt, still gets one, so a client never sees a failure it
/// can't read.
fn job_json(s: &jobs::Status) -> Result<Value, ApiError> {
    let at = |t: OffsetDateTime| {
        t.format(&Rfc3339)
            .map_err(|e| ApiError::Internal(e.to_string()))
    };
    Ok(json!({
        "id": s.id,
        "kind": s.kind,
        "state": s.state,
        "attempts": s.attempts,
        "runAt": at(s.run_at)?,
        "finishedAt": s.finished_at.map(at).transpose()?,
        "result": match (&s.result, s.state.as_str()) {
            (None, "failed") => {
                let detail = match s.attempts >= s.max_attempts {
                    true => "the job ran out of attempts; queue the change again",
                    false => "the job couldn't run; queue the change again",
                };
                json!({ "status": 500, "problem": {
                    "type": "about:blank",
                    "title": "Internal server error",
                    "status": 500,
                    "detail": detail,
                }})
            }
            (result, _) => json!(result),
        },
    }))
}

/// `GET /api/admin/jobs/{id}`: a schema change or preview a request queued, for whoever may
/// change schemas, as they're the ones who queue them. Any other job is missing. Its
/// `last_error` is never shown; `result` says what went wrong.
pub async fn job(
    State(state): State<AppState>,
    principal: Principal,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    authorize(&principal, Action::Schemas, "")?;
    let Path(id) = path.map_err(|_| ApiError::NotFound)?;
    let id: Uuid = id.parse().map_err(|_| ApiError::NotFound)?;
    match jobs::status(&state.pool, id).await? {
        Some(s) if s.kind == SCHEMA_CHANGE || s.kind == SCHEMA_PREVIEW => Ok(Json(job_json(&s)?)),
        _ => Err(ApiError::NotFound),
    }
}

/// A fault worth running again: ours, or the database's, a deadlock or serialization
/// failure included, which `ApiError` reports as a 409 asking to try again.
fn fault(e: &ApiError) -> bool {
    match e {
        ApiError::Internal(_) => true,
        ApiError::Conflict(d) => d == crate::error::TRY_AGAIN,
        _ => false,
    }
}

/// Why a run failed. A database fault (`retry`) runs again while attempts last; anything
/// else ends the job with what the route would have answered: its status and problem,
/// without a request ID, there being no request.
fn failed(job: &Job, e: ApiError, retry: bool) -> JobError {
    let error = format!("{e:?}");
    if retry && job.attempts < job.max_attempts {
        return JobError::Retry(error);
    }
    let (status, mut problem) = e.problem();
    if let Some(p) = problem.as_object_mut() {
        p.remove("requestId");
    }
    JobError::Failed {
        error,
        result: json!({ "status": status.as_u16(), "problem": problem }),
    }
}

/// Runs a queued schema change or preview as whoever queued it is now, making every check
/// the route made again, against the schemas as they are now. A long handler (see
/// `crate::jobs`): it keeps its lease while it works and marks itself done in the change's
/// own transaction, so a run that lost the job can't commit it. Its result is the route's
/// 200 body.
pub(crate) async fn run_job(state: &AppState, job: &Job) -> Result<(), JobError> {
    let (Some(slug), Ok(req)) = (
        job.payload["slug"].as_str(),
        ChangeRequest::deserialize(&job.payload["request"]),
    ) else {
        return Err(JobError::Permanent(
            "the job's payload isn't a schema change".into(),
        ));
    };
    let leased = keep_leased(state, job, LEASE).await?;
    // Already taken over: the checks below could only fail into a fenced `fail`.
    if leased.lost() {
        return Ok(());
    }
    let api = |e: ApiError| {
        let retry = fault(&e);
        failed(job, e, retry)
    };
    let change = |e: ChangeError| {
        let retry = matches!(e, ChangeError::Db(_));
        failed(job, change_error(e), retry)
    };
    let db = |e: DbError| failed(job, e.into(), true);
    let user = job
        .created_by
        .ok_or_else(|| JobError::Permanent("nobody queued it".into()))?;
    let principal = Principal::for_job(state, user, job.key_id)
        .await
        .map_err(api)?
        .ok_or_else(|| {
            let why = "whoever queued this change was removed, or the API key they used was revoked or has expired";
            failed(job, ApiError::Forbidden(why.into()), false)
        })?;
    authorize(&principal, Action::Schemas, "").map_err(api)?;
    let schema = parse_schema(&req.schema).map_err(api)?;
    let world = saved(state, &schema).map_err(api)?;
    check_virtuals(&world).map_err(api)?;
    check_dependents(&world).map_err(api)?;
    check_rules(state, &world).await.map_err(api)?;

    // A run that lost the job stops (see `crate::jobs`): the run that took it is the one
    // that counts, and this one couldn't complete anyway.
    if leased.lost() {
        return Ok(());
    }
    let (mut tx, result, changed) = if job.kind == SCHEMA_PREVIEW {
        let p = schema_change::preview(&state.pool, slug, &schema, &req.renames)
            .await
            .map_err(change)?;
        if leased.lost() {
            return Ok(());
        }
        let tx = state.pool.begin().await.map_err(|e| db(e.into()))?;
        (tx, json!(p), None)
    } else {
        let mut tx = state.pool.begin().await.map_err(|e| db(e.into()))?;
        let (c, p) = schema_change::apply_in(&mut tx, slug, &schema, &req.renames, req.confirm)
            .await
            .map_err(change)?;
        let result = json!({ "collection": to_json(&c), "preview": p });
        (tx, result, Some(c.id))
    };
    drop(leased);
    if !jobs::complete_with(&mut tx, job, Some(&result))
        .await
        .map_err(db)?
    {
        // Another worker took the job over, and its run is the one that counts. Rolled
        // back now, not when the connection is next used, so the takeover's wait on these
        // locks ends here.
        tx.rollback().await.map_err(|e| db(e.into()))?;
        return Ok(());
    }
    tx.commit().await.map_err(|e| db(e.into()))?;
    if let Some(id) = changed {
        // As the route does, though the change is saved and its result kept already.
        let indexed = reconcile_indexes(&state.pool, id).await;
        log_index_failure(&indexed, slug);
        if let Err(error) = state.schemas.reload(&state.pool).await {
            tracing::warn!(
                ?error,
                collection = slug,
                "couldn't reload schemas after a change"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use sqlx::PgPool;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use tower::ServiceExt;

    use super::*;
    use crate::schemas::Schemas;

    /// An Admin, `posts` with a title, and a queued rename of it to `headline`.
    async fn queued(pool: &PgPool) -> (AppState, Uuid) {
        let app = crate::router(pool.clone()).await.unwrap();
        let setup =
            json!({"email": "a@example.com", "name": "A", "password": "a long enough passphrase"});
        let res = app
            .oneshot(
                Request::post("/api/setup")
                    .header("content-type", "application/json")
                    .body(Body::from(setup.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let user: Uuid = sqlx::query_scalar("SELECT user_id FROM accounts")
            .fetch_one(pool)
            .await
            .unwrap();
        let posts: CollectionSchema = serde_json::from_value(
            json!({"slug": "posts", "fields": [{"name": "title", "type": "text"}]}),
        )
        .unwrap();
        schema_change::create(pool, &posts).await.unwrap();
        let state = AppState {
            pool: pool.clone(),
            schemas: std::sync::Arc::new(Schemas::load(pool).await.unwrap()),
            auth: std::sync::Arc::new(crate::auth::AuthState::new(Default::default()).unwrap()),
            inline_rows: INLINE_ROWS,
        };
        let request = json!({
            "schema": {"slug": "posts", "fields": [{"name": "headline", "type": "text"}]},
            "renames": {"title": "headline"},
        });
        let new = NewJob {
            kind: SCHEMA_CHANGE,
            document_id: None,
            collection_id: None,
            payload: json!({"slug": "posts", "request": request}),
            run_at: OffsetDateTime::now_utc(),
            dedupe_key: Some("schema:posts".into()),
            created_by: Some(user),
            key_id: None,
        };
        let id = jobs::add(&mut pool.acquire().await.unwrap(), &new)
            .await
            .unwrap();
        (state, id)
    }

    async fn version(pool: &PgPool) -> i32 {
        sqlx::query_scalar("SELECT schema_version FROM collections WHERE schema->>'slug' = 'posts'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Ends job `id`'s lease now, as if its worker had stalled past it.
    async fn expire(pool: &PgPool, id: Uuid) {
        sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 second' WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    /// Waits until `n` of this database's sessions are waiting on a lock.
    async fn waiting(pool: &PgPool, n: i64) {
        for _ in 0..200 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{n} sessions never waited on a lock");
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_run_that_lost_its_job_cant_commit_the_change(pool: PgPool) {
        let (state, id) = queued(&pool).await;
        // Worker A claims, then stalls past its lease; worker B takes the job over.
        let a = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
        expire(&pool, id).await;
        let b = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
        assert_eq!((b.id, a.attempts, b.attempts), (id, 1, 2));

        // A runs first, and changes nothing.
        assert!(run_job(&state, &a).await.is_ok());
        assert_eq!(version(&pool).await, 1, "A's change rolled back");
        assert!(run_job(&state, &b).await.is_ok());
        assert_eq!(version(&pool).await, 2, "B's change went in");
        let done = jobs::status(&pool, id).await.unwrap().unwrap();
        assert_eq!((done.state.as_str(), done.attempts), ("done", 2));
        assert_eq!(done.result.unwrap()["collection"]["schemaVersion"], 2);
        // Run again, A changes nothing more.
        let _ = run_job(&state, &a).await;
        assert_eq!(version(&pool).await, 2, "the change went in once");
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_deadlock_while_checking_runs_again(pool: PgPool) {
        sqlx::raw_sql("CREATE TABLE t (id int PRIMARY KEY); INSERT INTO t VALUES (1), (2)")
            .execute(&pool)
            .await
            .unwrap();
        let lock = |id: i32| sqlx::query("SELECT id FROM t WHERE id = $1 FOR UPDATE").bind(id);
        let (mut a, mut b) = (pool.begin().await.unwrap(), pool.begin().await.unwrap());
        lock(1).execute(&mut *a).await.unwrap();
        lock(2).execute(&mut *b).await.unwrap();
        let (x, y) = tokio::join!(lock(2).execute(&mut *a), lock(1).execute(&mut *b));
        let deadlock = ApiError::from(DbError::from(x.err().or(y.err()).unwrap()));
        assert!(fault(&deadlock), "{deadlock:?}");
        assert!(fault(&ApiError::Internal("down".into())));
        assert!(!fault(&ApiError::Conflict("`posts` already exists".into())));
        assert!(!fault(&ApiError::Forbidden("no".into())));
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_run_that_lost_its_lease_stops_before_the_work(pool: PgPool) {
        let (state, id) = queued(&pool).await;
        let a = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
        expire(&pool, id).await;
        let b = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
        // Every schema save waits on this lock, so a run that starts the change hangs here.
        let mut held = pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(7311002)")
            .execute(&mut *held)
            .await
            .unwrap();
        let stale = tokio::time::timeout(Duration::from_secs(5), run_job(&state, &a)).await;
        assert!(
            matches!(stale, Ok(Ok(()))),
            "A stopped without waiting: {stale:?}"
        );
        held.rollback().await.unwrap();
        assert_eq!(version(&pool).await, 1);
        assert!(run_job(&state, &b).await.is_ok());
        assert_eq!(version(&pool).await, 2);
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_takeover_waits_for_the_stale_run_and_commits_once(
        opts: PgPoolOptions,
        connect: PgConnectOptions,
    ) {
        // Each run holds a connection for its heartbeats and one for its change.
        let pool = opts
            .max_connections(10)
            .connect_with(connect)
            .await
            .unwrap();
        let (state, id) = queued(&pool).await;
        let a = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
        // Hold the collection's row, so A stops midway with its transaction open.
        let mut held = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM collections WHERE schema->>'slug' = 'posts' FOR UPDATE")
            .execute(&mut *held)
            .await
            .unwrap();
        let run_a = {
            let (state, a) = (state.clone(), a.clone());
            tokio::spawn(async move { run_job(&state, &a).await })
        };
        waiting(&pool, 1).await;

        // A's lease runs out mid-run; B takes the job over and waits on A's locks.
        expire(&pool, id).await;
        let b = jobs::claim(&pool, LEASE).await.unwrap().unwrap();
        assert_eq!((a.attempts, b.attempts), (1, 2));
        let run_b = {
            let state = state.clone();
            tokio::spawn(async move { run_job(&state, &b).await })
        };
        waiting(&pool, 2).await;

        held.rollback().await.unwrap();
        assert!(run_a.await.unwrap().is_ok(), "A loses at the fence");
        assert!(run_b.await.unwrap().is_ok());
        assert_eq!(version(&pool).await, 2, "the change went in once");
        let done = jobs::status(&pool, id).await.unwrap().unwrap();
        assert_eq!((done.state.as_str(), done.attempts), ("done", 2));
    }
}
