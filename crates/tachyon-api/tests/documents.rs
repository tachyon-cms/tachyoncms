mod common;

use axum::Router;
use common::{assert_problem, send};
use serde_json::json;
use sqlx::PgPool;

async fn app_with_posts(pool: PgPool) -> Router {
    let app = tachyon_api::router(pool).await.unwrap();
    let schema = json!({"slug": "posts", "fields": [
        {"name": "title", "type": "text", "required": true, "maxLength": 100},
        {"name": "summary", "type": "textarea"}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    app
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn crud_round_trip(pool: PgPool) {
    let app = app_with_posts(pool).await;

    let created = send(
        &app,
        "POST",
        "/api/posts",
        Some(r#"{"title": "Hello", "summary": "First"}"#),
    )
    .await;
    assert_eq!(created.status, 201, "{}", created.json);
    assert_eq!(created.json["title"], "Hello");
    assert_eq!(created.json["status"], "published");
    let id = created.json["id"].as_str().unwrap().to_owned();

    let got = send(&app, "GET", &format!("/api/posts/{id}"), None).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json["summary"], "First");

    let patched = send(
        &app,
        "PATCH",
        &format!("/api/posts/{id}"),
        Some(r#"{"title": "Hi"}"#),
    )
    .await;
    assert_eq!(patched.status, 200, "{}", patched.json);
    assert_eq!(patched.json["title"], "Hi");
    assert_eq!(patched.json["summary"], "First");

    let listed = send(&app, "GET", "/api/posts", None).await;
    assert_eq!(listed.status, 200);
    assert_eq!(listed.json["docs"][0]["id"], id.as_str());
    assert_eq!(listed.json["page"], 1);
    assert_eq!(listed.json["limit"], 10);
    assert!(listed.json.get("totalDocs").is_none());

    let deleted = send(&app, "DELETE", &format!("/api/posts/{id}"), None).await;
    assert_eq!(deleted.status, 204);
    assert_problem(
        &send(&app, "GET", &format!("/api/posts/{id}"), None).await,
        404,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn validation_errors_are_422_with_paths(pool: PgPool) {
    let app = app_with_posts(pool).await;
    let res = send(
        &app,
        "POST",
        "/api/posts",
        Some(r#"{"summary": "no title", "extra": 1}"#),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"],
        json!([
            {"path": "extra", "message": "unknown field"},
            {"path": "title", "message": "is required"}
        ])
    );

    let res = send(&app, "POST", "/api/posts", Some("[1, 2]")).await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unsupported_methods_are_problem_405(pool: PgPool) {
    let app = app_with_posts(pool).await;
    assert_problem(&send(&app, "PUT", "/api/posts", Some("{}")).await, 405);
    assert_problem(
        &send(&app, "DELETE", "/api/admin/collections", None).await,
        405,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn nul_characters_are_422_not_500(pool: PgPool) {
    let app = app_with_posts(pool).await;
    let res = send(&app, "POST", "/api/posts", Some(r#"{"title": "a\u0000b"}"#)).await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "title");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn malformed_json_is_400(pool: PgPool) {
    let app = app_with_posts(pool).await;
    assert_problem(
        &send(&app, "POST", "/api/posts", Some(r#"{"title": "#)).await,
        400,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn bad_identifiers_are_404(pool: PgPool) {
    let app = app_with_posts(pool).await;
    let missing = "/api/posts/0199d5c4-0000-7000-8000-000000000000";
    assert_problem(&send(&app, "GET", "/api/nope", None).await, 404);
    assert_problem(
        &send(&app, "POST", "/api/nope", Some(r#"{"title": "x"}"#)).await,
        404,
    );
    assert_problem(&send(&app, "GET", "/api/posts/not-a-uuid", None).await, 404);
    assert_problem(&send(&app, "GET", "/api/%FF", None).await, 404);
    assert_problem(&send(&app, "GET", "/api/posts/%FF", None).await, 404);
    assert_problem(
        &send(&app, "DELETE", "/api/posts/not-a-uuid", None).await,
        404,
    );
    assert_problem(&send(&app, "GET", missing, None).await, 404);
    assert_problem(&send(&app, "PATCH", missing, Some("{}")).await, 404);
    assert_problem(&send(&app, "DELETE", missing, None).await, 404);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn pagination_is_validated_and_clamped(pool: PgPool) {
    let app = app_with_posts(pool).await;
    for i in 0..3 {
        send(
            &app,
            "POST",
            "/api/posts",
            Some(&json!({"title": format!("Post {i}")}).to_string()),
        )
        .await;
    }
    let res = send(&app, "GET", "/api/posts?limit=2&page=2&count=true", None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["docs"].as_array().unwrap().len(), 1);
    assert_eq!(res.json["totalDocs"], 3);

    assert_eq!(
        send(&app, "GET", "/api/posts?limit=500", None).await.json["limit"],
        100
    );
    for q in [
        "limit=0",
        "limit=abc",
        "page=0",
        "page=-1",
        "page=9223372036854775807",
    ] {
        assert_problem(
            &send(&app, "GET", &format!("/api/posts?{q}"), None).await,
            400,
        );
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn patch_null_removes_optional_but_not_required(pool: PgPool) {
    let app = app_with_posts(pool).await;
    let created = send(
        &app,
        "POST",
        "/api/posts",
        Some(r#"{"title": "T", "summary": "S"}"#),
    )
    .await;
    let url = format!("/api/posts/{}", created.json["id"].as_str().unwrap());

    let res = send(&app, "PATCH", &url, Some(r#"{"summary": null}"#)).await;
    assert_eq!(res.status, 200);
    assert!(res.json.get("summary").is_none(), "{}", res.json);

    let res = send(&app, "PATCH", &url, Some(r#"{"title": null}"#)).await;
    assert_problem(&res, 422);
    assert_eq!(send(&app, "GET", &url, None).await.json["title"], "T");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unique_values_are_field_errors(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    let schema = r#"{"slug": "pages", "fields": [{"name": "title", "type": "text"}, {"name": "slug", "type": "slug", "from": "title"}]}"#;
    assert_eq!(
        send(&app, "POST", "/api/admin/collections", Some(schema))
            .await
            .status,
        201
    );

    let first = send(&app, "POST", "/api/pages", Some(r#"{"title": "About us"}"#)).await;
    assert_eq!(first.json["slug"], "about-us");
    let dup = send(
        &app,
        "POST",
        "/api/pages",
        Some(r#"{"title": "About Us!"}"#),
    )
    .await;
    assert_problem(&dup, 422);
    assert_eq!(
        dup.json["errors"][0],
        json!({"path": "slug", "message": "must be unique; another document already uses this value"})
    );

    let other = send(&app, "POST", "/api/pages", Some(r#"{"title": "Team"}"#)).await;
    let url = format!("/api/pages/{}", other.json["id"].as_str().unwrap());
    assert_problem(
        &send(&app, "PATCH", &url, Some(r#"{"slug": "about-us"}"#)).await,
        422,
    );
}
