mod common;

use std::time::Duration;

use axum::Router;
use axum::http::Request;
use common::{Res, add_user, admin_app, assert_problem, send, send_with, signed_in};
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Res {
    send(app, method, uri, body.map(|b| b.to_string()).as_deref()).await
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let res = call(app, method, uri, body).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

async fn with_key(app: &Router, key: &str, method: &str, uri: &str, body: Value) -> Res {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"));
    send_with(app, req, Some(&body.to_string())).await
}

/// `hours` from now, to the second, as RFC 3339.
fn at(hours: i64) -> String {
    (OffsetDateTime::now_utc() + time::Duration::hours(hours))
        .replace_nanosecond(0)
        .unwrap()
        .format(&Rfc3339)
        .unwrap()
}

/// `authors`, and `posts` linking to them, both with drafts; posts keep 2 versions.
async fn blog(pool: &PgPool) -> Router {
    let admin = admin_app(pool.clone()).await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "authors", "versions": {"drafts": true},
                    "fields": [{"name": "name", "type": "text"}]})),
    )
    .await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(
            json!({"slug": "posts", "versions": {"drafts": true, "maxVersions": 2},
            "fields": [
                {"name": "title", "type": "text", "required": true},
                {"name": "slug", "type": "slug"},
                {"name": "author", "type": "relationship", "relationTo": "authors"}
            ]}),
        ),
    )
    .await;
    admin
}

async fn post(admin: &Router, title: &str) -> String {
    let doc = ok(admin, "POST", "/api/posts", Some(json!({"title": title}))).await;
    doc["id"].as_str().unwrap().to_owned()
}

/// An API with one job worker, for running what's due.
async fn worker(pool: &PgPool) -> Router {
    let options = tachyon_api::Options {
        jobs: tachyon_api::JobsConfig {
            workers: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    tachyon_api::router_with(pool.clone(), options)
        .await
        .unwrap()
}

/// Makes every queued job due now, then waits until none is queued or running.
async fn run_due(pool: &PgPool) {
    sqlx::query("UPDATE jobs SET run_at = now() WHERE state = 'queued'")
        .execute(pool)
        .await
        .unwrap();
    for _ in 0..200 {
        // A run that's to try again later counts as due now too.
        sqlx::query("UPDATE jobs SET run_at = now() WHERE state = 'queued'")
            .execute(pool)
            .await
            .unwrap();
        let pending: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE state IN ('queued', 'running')")
                .fetch_one(pool)
                .await
                .unwrap();
        if pending == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("jobs still pending");
}

/// Document `id`'s jobs, oldest first: kind, state, last error.
async fn jobs(pool: &PgPool, id: &str) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as(
        "SELECT kind, state, last_error FROM jobs WHERE document_id = $1 ORDER BY created_at, id",
    )
    .bind(id.parse::<Uuid>().unwrap())
    .fetch_all(pool)
    .await
    .unwrap()
}

fn job(kind: &str, state: &str) -> (String, String, Option<String>) {
    (kind.to_owned(), state.to_owned(), None)
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn scheduled_publish_ships_the_snapshot(pool: PgPool) {
    let admin = blog(&pool).await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let _worker = worker(&pool).await;
    let id = post(&admin, "Snapshot").await;
    let one = format!("/api/posts/{id}");
    let when = at(1);
    let res = ok(
        &admin,
        "PUT",
        &format!("{one}/schedule"),
        Some(json!({"publishAt": when})),
    )
    .await;
    assert_eq!(res, json!({"publishAt": when, "unpublishAt": null}));
    assert_eq!(
        ok(&admin, "GET", &format!("{one}/schedule"), None).await,
        json!({"publishAt": when, "unpublishAt": null})
    );
    // Edited after scheduling, more times than versions are kept: the snapshot stays.
    for title in ["Edit 1", "Edit 2", "Edit 3"] {
        ok(&admin, "PATCH", &one, Some(json!({"title": title}))).await;
    }
    assert_problem(&call(&anon, "GET", &one, None).await, 404);

    run_due(&pool).await;
    assert_eq!(jobs(&pool, &id).await, [job("publish", "done")]);
    let live = ok(&anon, "GET", &one, None).await;
    assert_eq!(live["title"], "Snapshot", "the snapshot, not the edit");
    assert_eq!(live["status"], "published");
    let draft = ok(&admin, "GET", &format!("{one}?draft=true"), None).await;
    assert_eq!(draft["title"], "Edit 3", "the draft is left alone");
    assert_eq!(
        ok(&admin, "GET", &format!("{one}/schedule"), None).await,
        json!({"publishAt": null, "unpublishAt": null})
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn rescheduling_cancels_the_earlier_job(pool: PgPool) {
    let admin = blog(&pool).await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let id = post(&admin, "First").await;
    let one = format!("/api/posts/{id}");
    let schedule = format!("{one}/schedule");
    ok(&admin, "PUT", &schedule, Some(json!({"publishAt": at(1)}))).await;
    ok(&admin, "PATCH", &one, Some(json!({"title": "Second"}))).await;
    let later = at(2);
    ok(&admin, "PUT", &schedule, Some(json!({"publishAt": later}))).await;
    assert_eq!(
        jobs(&pool, &id).await,
        [job("publish", "cancelled"), job("publish", "queued")]
    );
    // A missing key leaves its side alone; null cancels it.
    let off = at(3);
    let both = ok(&admin, "PUT", &schedule, Some(json!({"unpublishAt": off}))).await;
    assert_eq!(both, json!({"publishAt": later, "unpublishAt": off}));
    let res = ok(&admin, "PUT", &schedule, Some(json!({"unpublishAt": null}))).await;
    assert_eq!(res, json!({"publishAt": later, "unpublishAt": null}));

    let _worker = worker(&pool).await;
    run_due(&pool).await;
    assert_eq!(
        ok(&anon, "GET", &one, None).await["title"],
        "Second",
        "the later snapshot"
    );

    // DELETE cancels both sides.
    ok(
        &admin,
        "PUT",
        &schedule,
        Some(json!({"publishAt": at(1), "unpublishAt": at(2)})),
    )
    .await;
    let res = call(&admin, "DELETE", &schedule, None).await;
    assert_eq!(res.status, 204, "{}", res.json);
    assert_eq!(
        ok(&admin, "GET", &schedule, None).await,
        json!({"publishAt": null, "unpublishAt": null})
    );
    let states = jobs(&pool, &id).await;
    assert!(states.iter().all(|(_, s, _)| s != "queued"), "{states:?}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn scheduled_unpublish(pool: PgPool) {
    let admin = blog(&pool).await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let _worker = worker(&pool).await;
    let id = post(&admin, "Live").await;
    let one = format!("/api/posts/{id}");
    ok(&admin, "POST", &format!("{one}/publish"), None).await;
    ok(
        &admin,
        "PUT",
        &format!("{one}/schedule"),
        Some(json!({"unpublishAt": at(1)})),
    )
    .await;
    assert_eq!(ok(&anon, "GET", &one, None).await["title"], "Live");
    run_due(&pool).await;
    assert_eq!(jobs(&pool, &id).await, [job("unpublish", "done")]);
    assert_problem(&call(&anon, "GET", &one, None).await, 404);
    let draft = ok(&admin, "GET", &format!("{one}?draft=true"), None).await;
    assert_eq!(draft["title"], "Live");
    assert_eq!(draft["status"], "draft");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_global_can_be_scheduled(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(
            json!({"slug": "site", "kind": "global", "versions": {"drafts": true},
                    "fields": [{"name": "name", "type": "text"}]}),
        ),
    )
    .await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let _worker = worker(&pool).await;
    ok(
        &admin,
        "PATCH",
        "/api/globals/site",
        Some(json!({"name": "Soon"})),
    )
    .await;
    let when = at(1);
    ok(
        &admin,
        "PUT",
        "/api/globals/site/schedule",
        Some(json!({"publishAt": when})),
    )
    .await;
    assert_eq!(
        ok(&admin, "GET", "/api/globals/site/schedule", None).await["publishAt"],
        when
    );
    run_due(&pool).await;
    assert_eq!(
        ok(&anon, "GET", "/api/globals/site", None).await["name"],
        "Soon"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn demoted_scheduler_job_fails(pool: PgPool) {
    let admin = blog(&pool).await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    add_user(&pool, "editor@example.com", "editor").await;
    let editor = signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "editor@example.com",
    )
    .await;
    let _worker = worker(&pool).await;

    // Scheduled by an Editor who is an Author by the time it runs.
    let id = post(&admin, "Demoted").await;
    let one = format!("/api/posts/{id}");
    ok(
        &editor,
        "PUT",
        &format!("{one}/schedule"),
        Some(json!({"publishAt": at(1)})),
    )
    .await;
    sqlx::query("UPDATE accounts SET role = 'author' WHERE email = 'editor@example.com'")
        .execute(&pool)
        .await
        .unwrap();
    run_due(&pool).await;
    let ran = jobs(&pool, &id).await;
    assert_eq!(
        (ran[0].0.as_str(), ran[0].1.as_str()),
        ("publish", "failed")
    );
    assert!(
        ran[0].2.as_deref().unwrap().contains("permission"),
        "{ran:?}"
    );
    assert_problem(&call(&anon, "GET", &one, None).await, 404);

    // Scheduled with a key that's revoked by the time it runs.
    sqlx::query("UPDATE accounts SET role = 'editor' WHERE email = 'editor@example.com'")
        .execute(&pool)
        .await
        .unwrap();
    let key = ok(
        &editor,
        "POST",
        "/api/auth/keys",
        Some(json!({"name": "k", "scopes": ["posts:publish"]})),
    )
    .await["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = post(&admin, "Revoked").await;
    let one = format!("/api/posts/{id}");
    let res = with_key(
        &editor,
        &key,
        "PUT",
        &format!("{one}/schedule"),
        json!({"publishAt": at(1)}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        res.json,
        json!({"id": id}),
        "a publish-only key reads nothing back"
    );
    sqlx::query("UPDATE api_keys SET revoked_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    run_due(&pool).await;
    let ran = jobs(&pool, &id).await;
    assert_eq!(
        (ran[0].0.as_str(), ran[0].1.as_str()),
        ("publish", "failed")
    );
    assert!(ran[0].2.as_deref().unwrap().contains("revoked"), "{ran:?}");
    assert_problem(&call(&anon, "GET", &one, None).await, 404);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_draft_that_no_longer_validates_fails_its_job(pool: PgPool) {
    let admin = blog(&pool).await;
    let _worker = worker(&pool).await;
    let id = post(&admin, "Fine").await;
    let one = format!("/api/posts/{id}");
    ok(
        &admin,
        "PUT",
        &format!("{one}/schedule"),
        Some(json!({"publishAt": at(1)})),
    )
    .await;
    // The held snapshot loses its required title, as a schema change could leave it.
    sqlx::query("UPDATE versions SET data = data - 'title' WHERE document_id = $1")
        .bind(id.parse::<Uuid>().unwrap())
        .execute(&pool)
        .await
        .unwrap();
    run_due(&pool).await;
    let ran = jobs(&pool, &id).await;
    assert_eq!(ran[0].1, "failed");
    assert!(ran[0].2.as_deref().unwrap().contains("title"), "{ran:?}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn schedule_needs_publish_right(pool: PgPool) {
    let admin = blog(&pool).await;
    add_user(&pool, "author@example.com", "author").await;
    let author = signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "author@example.com",
    )
    .await;
    let id = ok(
        &author,
        "POST",
        "/api/posts",
        Some(json!({"title": "Mine"})),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let schedule = format!("/api/posts/{id}/schedule");
    let body = json!({"publishAt": at(1)});
    assert_problem(&call(&author, "PUT", &schedule, Some(body)).await, 403);
    // Refused before anything about the body.
    let bad = json!({"publishAt": "whenever", "other": 1});
    assert_problem(&call(&author, "PUT", &schedule, Some(bad)).await, 403);
    assert_problem(&call(&author, "DELETE", &schedule, None).await, 403);
    // Reading it takes readDrafts, which an Author has for their own.
    assert_eq!(
        ok(&author, "GET", &schedule, None).await,
        json!({"publishAt": null, "unpublishAt": null})
    );

    for (body, key) in [
        (json!({"publishAt": at(-1)}), "publishAt"),
        (
            json!({"unpublishAt": at(24 * 366 * 10 + 48)}),
            "unpublishAt",
        ),
        (json!({"publishAt": "tomorrow"}), "publishAt"),
        (json!({"unpublishAt": 5}), "unpublishAt"),
    ] {
        let res = call(&admin, "PUT", &schedule, Some(body.clone())).await;
        assert_problem(&res, 422);
        assert_eq!(res.json["errors"][0]["path"], key, "{body}: {}", res.json);
    }
    let res = call(&admin, "PUT", &schedule, Some(json!({"publish": at(1)}))).await;
    assert_problem(&res, 400);
    // A draft that wouldn't validate can't be scheduled.
    let empty = ok(&admin, "POST", "/api/posts", Some(json!({}))).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let res = call(
        &admin,
        "PUT",
        &format!("/api/posts/{empty}/schedule"),
        Some(json!({"publishAt": at(1)})),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "title", "{}", res.json);
    assert!(jobs(&pool, &empty).await.is_empty());
    assert!(jobs(&pool, &id).await.is_empty());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn trashing_cancels_schedule(pool: PgPool) {
    let admin = blog(&pool).await;
    let id = post(&admin, "Doomed").await;
    let one = format!("/api/posts/{id}");
    ok(&admin, "POST", &format!("{one}/publish"), None).await;
    ok(
        &admin,
        "PUT",
        &format!("{one}/schedule"),
        Some(json!({"publishAt": at(1), "unpublishAt": at(2)})),
    )
    .await;
    let res = call(&admin, "DELETE", &one, None).await;
    assert_eq!(res.status, 204, "{}", res.json);
    assert_eq!(
        jobs(&pool, &id).await,
        [job("publish", "cancelled"), job("unpublish", "cancelled")]
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn turning_drafts_off_cancels_schedule(pool: PgPool) {
    let admin = blog(&pool).await;
    let id = post(&admin, "Live").await;
    let one = format!("/api/posts/{id}");
    ok(&admin, "POST", &format!("{one}/publish"), None).await;
    ok(
        &admin,
        "PUT",
        &format!("{one}/schedule"),
        Some(json!({"publishAt": at(1), "unpublishAt": at(2)})),
    )
    .await;
    // Nothing pending in the draft, so drafts may go, and with them what's scheduled.
    let schema = json!({"slug": "posts", "versions": {"maxVersions": 2}, "fields": [
        {"name": "title", "type": "text", "required": true},
        {"name": "slug", "type": "slug"},
        {"name": "author", "type": "relationship", "relationTo": "authors"}
    ]});
    ok(
        &admin,
        "PUT",
        "/api/admin/collections/posts",
        Some(json!({"schema": schema})),
    )
    .await;
    assert_eq!(
        jobs(&pool, &id).await,
        [job("publish", "cancelled"), job("unpublish", "cancelled")]
    );
}

/// `id`'s one job failed, with a reason containing `why`.
async fn failed_with(pool: &PgPool, id: &str, why: &str) {
    let ran = jobs(pool, id).await;
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert_eq!(ran[0].1, "failed", "{ran:?}");
    assert!(ran[0].2.as_deref().unwrap().contains(why), "{ran:?}");
}

async fn new_key(app: &Router, scopes: &[&str]) -> (String, String) {
    let key = ok(
        app,
        "POST",
        "/api/auth/keys",
        Some(json!({"name": "k", "scopes": scopes})),
    )
    .await;
    (
        key["id"].as_str().unwrap().to_owned(),
        key["key"].as_str().unwrap().to_owned(),
    )
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_scheduler_who_lost_their_rights_fails_the_job(pool: PgPool) {
    let admin = blog(&pool).await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let editor_id = add_user(&pool, "editor@example.com", "editor").await;
    let editor = signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "editor@example.com",
    )
    .await;
    let _worker = worker(&pool).await;
    let schedule = async |key: &str, id: &str| {
        let res = with_key(
            &editor,
            key,
            "PUT",
            &format!("/api/posts/{id}/schedule"),
            json!({"publishAt": at(1)}),
        )
        .await;
        assert_eq!(res.status, 200, "{}", res.json);
    };

    // A key whose scopes no longer reach publishing.
    let (_, rescoped) = new_key(&editor, &["posts:publish"]).await;
    let a = post(&admin, "Rescoped").await;
    schedule(&rescoped, &a).await;
    sqlx::query("UPDATE api_keys SET scopes = '{posts:read}'")
        .execute(&pool)
        .await
        .unwrap();
    // A key rotated since: the old one is revoked (Ruling T7-1).
    let (old, rotated) = new_key(&editor, &["posts:publish"]).await;
    let b = post(&admin, "Rotated").await;
    schedule(&rotated, &b).await;
    ok(
        &editor,
        "POST",
        &format!("/api/auth/keys/{old}/rotate"),
        None,
    )
    .await;
    run_due(&pool).await;
    failed_with(&pool, &a, "can't publish it now").await;
    failed_with(&pool, &b, "revoked").await;

    // A user removed since.
    let c = post(&admin, "Removed").await;
    ok(
        &editor,
        "PUT",
        &format!("/api/posts/{c}/schedule"),
        Some(json!({"publishAt": at(1)})),
    )
    .await;
    let res = call(&admin, "DELETE", &format!("/api/users/{editor_id}"), None).await;
    assert_eq!(res.status, 204, "{}", res.json);
    run_due(&pool).await;
    failed_with(&pool, &c, "was removed").await;
    for id in [a, b, c] {
        assert_problem(
            &call(&anon, "GET", &format!("/api/posts/{id}"), None).await,
            404,
        );
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_hidden_document_and_a_deleted_one_fail_for_their_own_reasons(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "notes", "versions": {"drafts": true},
                    "access": {"read": {"title": {"not_equals": "Hidden"}}},
                    "fields": [{"name": "title", "type": "text"}]})),
    )
    .await;
    add_user(&pool, "editor@example.com", "editor").await;
    let editor = signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "editor@example.com",
    )
    .await;
    let _worker = worker(&pool).await;
    let mut ids = Vec::new();
    for title in ["Fine", "Gone"] {
        let id = ok(&admin, "POST", "/api/notes", Some(json!({"title": title}))).await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        ok(
            &editor,
            "PUT",
            &format!("/api/notes/{id}/schedule"),
            Some(json!({"publishAt": at(1)})),
        )
        .await;
        ids.push(id);
    }
    // The Editor can no longer read the first; the second is gone without its jobs being
    // cancelled, as a trash would.
    let first = format!("/api/notes/{}", ids[0]);
    ok(&admin, "PATCH", &first, Some(json!({"title": "Hidden"}))).await;
    sqlx::query("UPDATE documents SET deleted_at = now() WHERE id = $1")
        .bind(ids[1].parse::<Uuid>().unwrap())
        .execute(&pool)
        .await
        .unwrap();
    run_due(&pool).await;
    failed_with(&pool, &ids[0], "can no longer read it").await;
    failed_with(&pool, &ids[1], "document deleted").await;
    let draft = ok(&admin, "GET", &format!("{first}?draft=true"), None).await;
    assert_eq!(draft["status"], "draft", "nothing was published");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_snapshot_checks_links_and_unique_values_when_it_runs(pool: PgPool) {
    let admin = blog(&pool).await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let _worker = worker(&pool).await;
    // Links to an author who's only a draft when it runs.
    let author = ok(&admin, "POST", "/api/authors", Some(json!({"name": "D"}))).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let linked = ok(
        &admin,
        "POST",
        "/api/posts",
        Some(json!({"title": "Linked", "author": author})),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Shares a slug with a post published since it was scheduled.
    let twin = ok(
        &admin,
        "POST",
        "/api/posts",
        Some(json!({"title": "Twin", "slug": "same"})),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for id in [&linked, &twin] {
        ok(
            &admin,
            "PUT",
            &format!("/api/posts/{id}/schedule"),
            Some(json!({"publishAt": at(1)})),
        )
        .await;
    }
    let first = ok(
        &admin,
        "POST",
        "/api/posts",
        Some(json!({"title": "First", "slug": "same"})),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(&admin, "POST", &format!("/api/posts/{first}/publish"), None).await;
    run_due(&pool).await;
    failed_with(&pool, &linked, "author").await;
    failed_with(&pool, &twin, "slug: must be unique").await;
    for id in [linked, twin] {
        assert_problem(
            &call(&anon, "GET", &format!("/api/posts/{id}"), None).await,
            404,
        );
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_clash_enqueue_cant_retry_past_is_a_409(pool: PgPool) {
    let admin = blog(&pool).await;
    let id = post(&admin, "Busy").await;
    // Every insert clashes, as if more schedulers than enqueue retries for got in first.
    sqlx::raw_sql(
        "CREATE FUNCTION clash() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
             RAISE unique_violation USING CONSTRAINT = 'jobs_one_pending';
         END $$;
         CREATE TRIGGER clash BEFORE INSERT ON jobs FOR EACH ROW EXECUTE FUNCTION clash();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let res = call(
        &admin,
        "PUT",
        &format!("/api/posts/{id}/schedule"),
        Some(json!({"publishAt": at(1)})),
    )
    .await;
    assert_problem(&res, 409);
    assert_eq!(
        res.json["detail"],
        "the schedule changed at the same time; try again"
    );
    let versions: i64 = sqlx::query_scalar("SELECT count(*) FROM versions WHERE document_id = $1")
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(versions, 1, "the snapshot rolled back with it");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn another_authors_schedule_reads_as_missing(pool: PgPool) {
    let _admin = blog(&pool).await;
    let mut authors = Vec::new();
    for email in ["one@example.com", "two@example.com"] {
        add_user(&pool, email, "author").await;
        authors.push(signed_in(tachyon_api::router(pool.clone()).await.unwrap(), email).await);
    }
    let theirs = ok(
        &authors[0],
        "POST",
        "/api/posts",
        Some(json!({"title": "Mine"})),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut answers = Vec::new();
    for id in [theirs, Uuid::now_v7().to_string()] {
        let res = call(
            &authors[1],
            "GET",
            &format!("/api/posts/{id}/schedule"),
            None,
        )
        .await;
        assert_problem(&res, 404);
        answers.push((res.json["title"].clone(), res.json.get("detail").cloned()));
    }
    assert_eq!(answers[0], answers[1]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_global_refuses_its_schedule_with_a_403(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(
            json!({"slug": "site", "kind": "global", "versions": {"drafts": true},
                    "access": {"readDrafts": {"name": "Open"}},
                    "fields": [{"name": "name", "type": "text"}]}),
        ),
    )
    .await;
    add_user(&pool, "editor@example.com", "editor").await;
    let editor = signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "editor@example.com",
    )
    .await;
    let uri = "/api/globals/site/schedule";
    // Never saved: refused, as when the rule fails.
    assert_problem(&call(&editor, "GET", uri, None).await, 403);
    ok(
        &admin,
        "PATCH",
        "/api/globals/site",
        Some(json!({"name": "Shut"})),
    )
    .await;
    assert_problem(&call(&editor, "GET", uri, None).await, 403);
    ok(
        &admin,
        "PATCH",
        "/api/globals/site",
        Some(json!({"name": "Open"})),
    )
    .await;
    ok(&editor, "GET", uri, None).await;
}
