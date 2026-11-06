//! Schema changes too big to run inside a request, run on a job worker instead.

mod common;

use std::time::Duration;

use axum::Router;
use common::{ADMIN, PASSWORD, Res, add_user, assert_problem, send, signed_in};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_db::jobs::{self, NewJob};
use time::OffsetDateTime;
use uuid::Uuid;

/// Changes scanning more rows than this run on a worker, in these tests.
const ROWS: i64 = 2;

fn options(inline_rows: Option<i64>, workers: usize) -> tachyon_api::Options {
    tachyon_api::Options {
        jobs: tachyon_api::JobsConfig {
            workers,
            ..Default::default()
        },
        schema_inline_rows: inline_rows,
        ..Default::default()
    }
}

/// The API, set up and signed in as its first Admin, with no workers. Changes over `ROWS`
/// rows go to a worker.
async fn admin(pool: &PgPool) -> Router {
    let app = tachyon_api::router_with(pool.clone(), options(Some(ROWS), 0))
        .await
        .unwrap();
    let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "setup: {}", res.json);
    signed_in(app, ADMIN).await
}

/// Signed in as `email`, with the same low threshold.
async fn app_as(pool: &PgPool, email: &str) -> Router {
    let app = tachyon_api::router_with(pool.clone(), options(Some(ROWS), 0))
        .await
        .unwrap();
    signed_in(app, email).await
}

/// One job worker, which runs whatever is queued.
async fn worker(pool: &PgPool) -> Router {
    tachyon_api::router_with(pool.clone(), options(None, 1))
        .await
        .unwrap()
}

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> Res {
    send(app, method, uri, Some(&body.to_string())).await
}

/// Collection `slug` with a title and a summary, holding `docs` documents.
async fn collection(app: &Router, slug: &str, docs: usize) {
    let schema = json!({"slug": slug, "fields": [
        {"name": "title", "type": "text"}, {"name": "summary", "type": "textarea"}
    ]});
    let res = call(app, "POST", "/api/admin/collections", schema).await;
    assert_eq!(res.status, 201, "{}", res.json);
    for i in 0..docs {
        let doc = json!({"title": format!("Post {i}"), "summary": "S"});
        let res = call(app, "POST", &format!("/api/{slug}"), doc).await;
        assert_eq!(res.status, 201, "{}", res.json);
    }
}

/// `title` renamed to `headline`.
fn rename(slug: &str) -> Value {
    json!({
        "schema": {"slug": slug, "fields": [
            {"name": "headline", "type": "text"}, {"name": "summary", "type": "textarea"}
        ]},
        "renames": {"title": "headline"},
    })
}

/// A 202 for a queued job: its `Location`, and the job.
fn queued(res: &Res) -> (String, Value) {
    assert_eq!(res.status, 202, "{}", res.json);
    let job = res.json["job"].clone();
    let location = res.headers["location"].to_str().unwrap().to_owned();
    assert_eq!(
        location,
        format!("/api/admin/jobs/{}", job["id"].as_str().unwrap())
    );
    assert_eq!(job["state"], "queued");
    assert_eq!(job["result"], Value::Null);
    (location, job)
}

/// Polls the job at `location` until it's finished, and returns it.
async fn finished(app: &Router, location: &str) -> Value {
    for _ in 0..200 {
        let res = send(app, "GET", location, None).await;
        assert_eq!(res.status, 200, "{}", res.json);
        if matches!(res.json["state"].as_str(), Some("done" | "failed")) {
            return res.json;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the job at {location} never finished");
}

/// The answer with each collection's own ID and slug taken out, to compare twins.
fn twin(mut v: Value, slug: &str) -> Value {
    if let Some(c) = v.get_mut("collection") {
        c["id"] = Value::Null;
        c["schema"]["slug"] = Value::Null;
    }
    let text = v.to_string().replace(&format!("\"{slug}\""), "\"twin\"");
    serde_json::from_str(&text).unwrap()
}

async fn schema_version(app: &Router, slug: &str) -> Value {
    send(app, "GET", &format!("/api/admin/collections/{slug}"), None)
        .await
        .json["schemaVersion"]
        .clone()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_change_at_the_threshold_runs_inline(pool: PgPool) {
    let app = admin(&pool).await;
    collection(&app, "posts", ROWS as usize).await;
    let res = call(
        &app,
        "POST",
        "/api/admin/collections/posts/preview",
        rename("posts"),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = call(&app, "PUT", "/api/admin/collections/posts", rename("posts")).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["collection"]["schemaVersion"], 2);
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_large_change_runs_on_a_worker(pool: PgPool) {
    let app = admin(&pool).await;
    collection(&app, "posts", 3).await;
    collection(&app, "twins", 3).await;

    let res = call(&app, "PUT", "/api/admin/collections/posts", rename("posts")).await;
    let (location, job) = queued(&res);
    assert_eq!(job["kind"], "schema_change");
    assert_eq!(schema_version(&app, "posts").await, 1, "nothing ran yet");
    let waiting = send(&app, "GET", &location, None).await;
    assert_eq!(waiting.status, 200, "{}", waiting.json);
    assert_eq!(waiting.json["state"], "queued");

    let _worker = worker(&pool).await;
    let done = finished(&app, &location).await;
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["attempts"], 1);
    assert!(done["finishedAt"].is_string(), "{done}");
    let docs = send(&app, "GET", "/api/posts", None).await.json;
    assert!(docs["docs"].as_array().unwrap().iter().all(|d| {
        d["headline"]
            .as_str()
            .is_some_and(|h| h.starts_with("Post"))
    }));

    // The same change on a twin small enough to run inline answers the same.
    let inline = tachyon_api::router_with(pool.clone(), options(None, 0))
        .await
        .unwrap();
    let inline = signed_in(inline, ADMIN).await;
    let res = call(
        &inline,
        "PUT",
        "/api/admin/collections/twins",
        rename("twins"),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        twin(done["result"].clone(), "posts"),
        twin(res.json, "twins")
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_large_preview_runs_on_a_worker(pool: PgPool) {
    let app = admin(&pool).await;
    collection(&app, "posts", 3).await;
    collection(&app, "twins", 3).await;
    // Dropping `summary` without confirming: the preview says it needs confirmation.
    let drop = |slug: &str| json!({"schema": {"slug": slug, "fields": [{"name": "title", "type": "text"}]}});

    let res = call(
        &app,
        "POST",
        "/api/admin/collections/posts/preview",
        drop("posts"),
    )
    .await;
    let (location, job) = queued(&res);
    assert_eq!(job["kind"], "schema_preview");
    let _worker = worker(&pool).await;
    let done = finished(&app, &location).await;
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["result"]["needsConfirmation"], true, "{done}");
    assert_eq!(done["result"]["documents"], 3);
    assert_eq!(
        schema_version(&app, "posts").await,
        1,
        "a preview changes nothing"
    );

    let inline = tachyon_api::router_with(pool.clone(), options(None, 0))
        .await
        .unwrap();
    let inline = signed_in(inline, ADMIN).await;
    let res = call(
        &inline,
        "POST",
        "/api/admin/collections/twins/preview",
        drop("twins"),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        twin(done["result"].clone(), "posts"),
        twin(res.json, "twins")
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_change_that_breaks_before_it_runs_fails(pool: PgPool) {
    let app = admin(&pool).await;
    collection(&app, "posts", 3).await;
    let unique = json!({"schema": {"slug": "posts", "fields": [
        {"name": "title", "type": "text", "unique": true}, {"name": "summary", "type": "textarea"}
    ]}});
    let res = call(&app, "PUT", "/api/admin/collections/posts", unique).await;
    let (location, _) = queued(&res);
    // Unique when it was queued; not by the time it runs.
    let res = call(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "Post 0", "summary": "again"}),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);

    let _worker = worker(&pool).await;
    let done = finished(&app, &location).await;
    assert_eq!(done["state"], "failed", "{done}");
    assert_eq!(done["attempts"], 1, "a refused change isn't retried");
    let result = &done["result"];
    assert_eq!(result["status"], 422, "{done}");
    assert_eq!(result["problem"]["status"], 422);
    assert_eq!(result["problem"]["title"], "Validation failed");
    assert_eq!(result["problem"]["errors"][0]["path"], "title");
    assert!(done.get("lastError").is_none());
    assert_eq!(schema_version(&app, "posts").await, 1);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_change_that_needs_confirming_when_it_runs_fails_with_the_preview(pool: PgPool) {
    let app = admin(&pool).await;
    collection(&app, "posts", 3).await;
    let drop = json!({"schema": {"slug": "posts", "fields": [{"name": "title", "type": "text"}]}});
    let res = call(&app, "PUT", "/api/admin/collections/posts", drop).await;
    let (location, _) = queued(&res);

    let _worker = worker(&pool).await;
    let done = finished(&app, &location).await;
    assert_eq!(done["state"], "failed", "{done}");
    let result = &done["result"];
    assert_eq!(result["status"], 409, "{done}");
    assert_eq!(result["problem"]["title"], "Confirmation required");
    assert_eq!(result["problem"]["preview"]["needsConfirmation"], true);
    assert_eq!(result["problem"]["preview"]["documents"], 3);
    assert_eq!(schema_version(&app, "posts").await, 1);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_second_large_change_waits_for_the_first(pool: PgPool) {
    let app = admin(&pool).await;
    collection(&app, "posts", 3).await;
    let res = call(&app, "PUT", "/api/admin/collections/posts", rename("posts")).await;
    let (location, job) = queued(&res);
    let id = job["id"].as_str().unwrap();

    for (method, uri) in [
        ("PUT", "/api/admin/collections/posts"),
        ("POST", "/api/admin/collections/posts/preview"),
    ] {
        let res = call(&app, method, uri, rename("posts")).await;
        assert_problem(&res, 409);
        let detail = res.json["detail"].as_str().unwrap();
        assert_eq!(
            detail,
            format!("a change to `posts` is already queued; wait for job {id}")
        );
    }

    let _worker = worker(&pool).await;
    let done = finished(&app, &location).await;
    assert_eq!(done["state"], "done", "the first still ran: {done}");
    assert_eq!(schema_version(&app, "posts").await, 2);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_change_whose_creator_lost_schemas_fails(pool: PgPool) {
    let app = admin(&pool).await;
    collection(&app, "posts", 3).await;
    add_user(&pool, "second@example.com", "admin").await;
    let second = app_as(&pool, "second@example.com").await;
    let res = call(
        &second,
        "PUT",
        "/api/admin/collections/posts",
        rename("posts"),
    )
    .await;
    let (location, _) = queued(&res);
    sqlx::query("UPDATE accounts SET role = 'editor' WHERE email = 'second@example.com'")
        .execute(&pool)
        .await
        .unwrap();

    let _worker = worker(&pool).await;
    let done = finished(&app, &location).await;
    assert_eq!(done["state"], "failed", "{done}");
    assert_eq!(done["result"]["status"], 403, "{done}");
    assert_eq!(done["result"]["problem"]["title"], "Forbidden");
    assert_eq!(
        schema_version(&app, "posts").await,
        1,
        "the schema is unchanged"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_job_route_shows_only_schema_jobs_to_schema_admins(pool: PgPool) {
    let app = admin(&pool).await;
    collection(&app, "posts", 3).await;
    let res = call(&app, "PUT", "/api/admin/collections/posts", rename("posts")).await;
    let (location, _) = queued(&res);

    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    assert_problem(&send(&anon, "GET", &location, None).await, 401);
    add_user(&pool, "editor@example.com", "editor").await;
    let editor = app_as(&pool, "editor@example.com").await;
    assert_problem(&send(&editor, "GET", &location, None).await, 403);

    let schedule = NewJob {
        kind: "publish",
        document_id: None,
        collection_id: None,
        payload: json!({}),
        run_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
        dedupe_key: None,
        created_by: None,
        key_id: None,
    };
    let other = jobs::enqueue(&mut pool.acquire().await.unwrap(), &schedule)
        .await
        .unwrap();
    for uri in [
        format!("/api/admin/jobs/{other}"),
        format!("/api/admin/jobs/{}", Uuid::now_v7()),
        "/api/admin/jobs/not-an-id".to_owned(),
    ] {
        assert_problem(&send(&app, "GET", &uri, None).await, 404);
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_shared_block_counts_the_documents_it_reaches(pool: PgPool) {
    let app = admin(&pool).await;
    let cta = |fields: Value| json!({"slug": "cta", "kind": "block", "fields": fields});
    let res = call(
        &app,
        "POST",
        "/api/admin/collections",
        cta(json!([{"name": "label", "type": "text"}])),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let pages = json!({"slug": "pages", "fields": [
        {"name": "layout", "type": "blocks", "blocks": [{"blockRef": "cta"}]}
    ]});
    assert_eq!(
        call(&app, "POST", "/api/admin/collections", pages)
            .await
            .status,
        201
    );
    let change = json!({"schema": cta(json!([
        {"name": "label", "type": "text"}, {"name": "note", "type": "text"}
    ]))});
    // Up to two pages it runs inline; a third tips it onto a worker.
    for i in 0..4 {
        let res = call(
            &app,
            "POST",
            "/api/admin/collections/cta/preview",
            change.clone(),
        )
        .await;
        assert_eq!(
            res.status,
            if i <= ROWS { 200 } else { 202 },
            "{}",
            res.json
        );
        let res = call(&app, "POST", "/api/pages", json!({"layout": []})).await;
        assert_eq!(res.status, 201, "{}", res.json);
    }
}
