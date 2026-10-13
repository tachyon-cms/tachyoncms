mod common;

use axum::Router;
use common::{assert_problem, send};
use serde_json::{Value, json};
use sqlx::PgPool;

async fn app(pool: PgPool) -> (Router, String) {
    let app = common::admin_app(pool).await;
    let schema = json!({"slug": "posts", "fields": [
        {"name": "title", "type": "text"}, {"name": "summary", "type": "textarea"}
    ]});
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/admin/collections",
            Some(&schema.to_string())
        )
        .await
        .status,
        201
    );
    let doc = send(
        &app,
        "POST",
        "/api/posts",
        Some(r#"{"title": "Hello", "summary": "S"}"#),
    )
    .await;
    let url = format!("/api/posts/{}", doc.json["id"].as_str().unwrap());
    (app, url)
}

fn change(fields: Value, extra: Value) -> String {
    let mut body = json!({"schema": {"slug": "posts", "fields": fields}});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    body.to_string()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn reads_one_collection(pool: PgPool) {
    let (app, _) = app(pool).await;
    let res = send(&app, "GET", "/api/admin/collections/posts", None).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json["schema"]["slug"], "posts");
    assert_problem(
        &send(&app, "GET", "/api/admin/collections/nope", None).await,
        404,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn renames_carry_data_across(pool: PgPool) {
    let (app, url) = app(pool).await;
    let body = change(
        json!([{"name": "headline", "type": "text"}, {"name": "summary", "type": "textarea"}]),
        json!({"renames": {"title": "headline"}}),
    );
    let res = send(&app, "PUT", "/api/admin/collections/posts", Some(&body)).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["collection"]["schemaVersion"], 2);
    assert_eq!(
        res.json["preview"]["diff"]["changes"],
        json!([{"type": "renamed", "from": "title", "to": "headline"}])
    );
    let doc = send(&app, "GET", &url, None).await.json;
    assert_eq!(doc["headline"], "Hello");
    assert!(doc.get("title").is_none());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn destructive_changes_need_confirmation(pool: PgPool) {
    let (app, url) = app(pool).await;
    let fields = json!([{"name": "title", "type": "text"}]);
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/posts",
        Some(&change(fields.clone(), json!({}))),
    )
    .await;
    assert_problem(&res, 409);
    assert_eq!(res.json["preview"]["needsConfirmation"], true);
    assert_eq!(
        res.json["preview"]["diff"]["changes"][0],
        json!({"type": "removed", "path": "summary", "kind": "textarea"})
    );
    assert_eq!(send(&app, "GET", &url, None).await.json["summary"], "S");

    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/posts",
        Some(&change(fields, json!({"confirm": true}))),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert!(
        send(&app, "GET", &url, None)
            .await
            .json
            .get("summary")
            .is_none()
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn preview_counts_documents_that_wont_validate(pool: PgPool) {
    let (app, _) = app(pool).await;
    let body = change(
        json!([
            {"name": "title", "type": "text"}, {"name": "summary", "type": "textarea"},
            {"name": "byline", "type": "text", "required": true}
        ]),
        json!({}),
    );
    let res = send(
        &app,
        "POST",
        "/api/admin/collections/posts/preview",
        Some(&body),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        (res.json["documents"].as_i64(), res.json["invalid"].as_i64()),
        (Some(1), Some(1))
    );
    assert_eq!(
        res.json["examples"][0]["errors"][0],
        json!({"path": "byline", "message": "is required"})
    );
    assert_eq!(res.json["needsConfirmation"], true);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unique_over_duplicates_is_422(pool: PgPool) {
    let (app, _) = app(pool).await;
    send(&app, "POST", "/api/posts", Some(r#"{"title": "Hello"}"#)).await;
    let body = change(
        json!([{"name": "title", "type": "text", "unique": true}, {"name": "summary", "type": "textarea"}]),
        json!({"confirm": true}),
    );
    let res = send(&app, "PUT", "/api/admin/collections/posts", Some(&body)).await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0],
        json!({"path": "title", "message": "has duplicate values; make them unique first"})
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unsupported_or_malformed_changes_are_rejected(pool: PgPool) {
    let (app, _) = app(pool).await;
    let renamed_collection = json!({"schema": {"slug": "articles", "fields": []}}).to_string();
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/posts",
        Some(&renamed_collection),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["message"],
        "renaming a collection isn't supported yet"
    );

    let typo = change(
        json!([{"name": "title", "type": "text"}]),
        json!({"confrim": true}),
    );
    assert_problem(
        &send(&app, "PUT", "/api/admin/collections/posts", Some(&typo)).await,
        400,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_failed_index_build_still_reloads_the_schema_and_startup_heals_it(pool: PgPool) {
    let (app, _) = app(pool.clone()).await;
    let posts = tachyon_db::collections::get_by_slug(&pool, "posts")
        .await
        .unwrap();
    let name = tachyon_db::indexes::index_name(
        posts.id,
        "rank",
        tachyon_db::indexes::IndexKind::Filter,
        tachyon_core::query::ValueKind::Number,
    );
    // Something already holds the index's name, so building it fails.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE \"{name}\" ()")))
        .execute(&pool)
        .await
        .unwrap();
    let fields = json!([
        {"name": "title", "type": "text"}, {"name": "summary", "type": "textarea"},
        {"name": "rank", "type": "number", "filterable": true}
    ]);
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/posts",
        Some(&change(fields, json!({}))),
    )
    .await;
    assert_problem(&res, 500);
    let got = send(&app, "GET", "/api/admin/collections/posts", None).await;
    assert_eq!(
        got.json["schema"]["fields"][2]["name"], "rank",
        "the saved schema is served even though its index failed"
    );
    let res = send(&app, "POST", "/api/posts", Some(r#"{"rank": 3}"#)).await;
    assert_eq!(res.status, 201, "{}", res.json);

    sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE \"{name}\"")))
        .execute(&pool)
        .await
        .unwrap();
    let _restarted = tachyon_api::router(pool.clone()).await.unwrap();
    let mut built = false;
    for _ in 0..100 {
        built = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE indexname = $1)")
            .bind(&name)
            .fetch_one(&pool)
            .await
            .unwrap();
        if built {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(built, "startup builds indexes a failed change left missing");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn startup_doesnt_wait_for_another_servers_index_build(pool: PgPool) {
    let (_app, _) = app(pool.clone()).await;
    let posts = tachyon_db::collections::get_by_slug(&pool, "posts")
        .await
        .unwrap();
    // Another server is mid-reconcile for this collection.
    let mut other = pool.acquire().await.unwrap();
    let key = i64::from_le_bytes(posts.id.as_bytes()[..8].try_into().unwrap());
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(key)
        .execute(&mut *other)
        .await
        .unwrap();
    let started = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tachyon_api::router(pool.clone()),
    )
    .await;
    assert!(started.is_ok(), "startup waited on the lock");
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut *other)
        .await
        .unwrap();
}
