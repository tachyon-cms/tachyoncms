mod common;

use axum::http::Request;
use common::{assert_problem, send, send_with};
use sqlx::PgPool;

const POSTS: &str =
    r#"{"slug": "posts", "fields": [{"name": "title", "type": "text", "required": true}]}"#;

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn create_and_list_collections(pool: PgPool) {
    let app = tachyon_api::router(pool);
    let res = send(&app, "POST", "/api/admin/collections", Some(POSTS)).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["schema"]["slug"], "posts");
    assert_eq!(res.json["schemaVersion"], 1);

    let res = send(&app, "GET", "/api/admin/collections", None).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json["docs"][0]["schema"]["slug"], "posts");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn duplicate_collection_is_409(pool: PgPool) {
    let app = tachyon_api::router(pool);
    send(&app, "POST", "/api/admin/collections", Some(POSTS)).await;
    let res = send(&app, "POST", "/api/admin/collections", Some(POSTS)).await;
    assert_problem(&res, 409);
    assert_eq!(res.json["detail"], "collection `posts` already exists");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn invalid_schemas_are_422(pool: PgPool) {
    let app = tachyon_api::router(pool);
    for body in [
        r#"{"slug": "Posts", "fields": []}"#,
        r#"{"slug": "admin", "fields": []}"#,
    ] {
        let res = send(&app, "POST", "/api/admin/collections", Some(body)).await;
        assert_problem(&res, 422);
        assert_eq!(res.json["errors"][0]["path"], "");
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unknown_field_options_are_422(pool: PgPool) {
    let app = tachyon_api::router(pool);
    let body = r#"{"slug": "posts", "fields": [{"name": "t", "type": "text", "unique": true}]}"#;
    let res = send(&app, "POST", "/api/admin/collections", Some(body)).await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["message"],
        "field `t`: unknown option `unique`"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn malformed_bodies_are_400(pool: PgPool) {
    let app = tachyon_api::router(pool);
    for body in [
        r#"{"slug": "#,
        r#"{"slug": "posts"}"#,
        r#"{"slug": "posts", "fields": [{"name": "x", "type": "nope"}]}"#,
    ] {
        let res = send(&app, "POST", "/api/admin/collections", Some(body)).await;
        assert_problem(&res, 400);
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unknown_routes_are_problem_404(pool: PgPool) {
    let app = tachyon_api::router(pool);
    assert_problem(&send(&app, "GET", "/nope", None).await, 404);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn request_ids_are_echoed_or_generated(pool: PgPool) {
    let app = tachyon_api::router(pool);
    let ok = send_with(
        &app,
        Request::builder()
            .uri("/nope")
            .header("x-request-id", "abc-123_X"),
        None,
    )
    .await;
    assert_eq!(ok.headers["x-request-id"], "abc-123_X");
    assert_eq!(ok.json["requestId"], "abc-123_X");

    // Untrusted IDs that could forge log lines are replaced.
    let bad = send_with(
        &app,
        Request::builder()
            .uri("/nope")
            .header("x-request-id", "a b\tc"),
        None,
    )
    .await;
    let id = bad.headers["x-request-id"].to_str().unwrap();
    assert_ne!(id, "a b\tc");
    assert_eq!(id.len(), 36, "expected a generated UUID, got {id}");
}
