mod common;

use axum::http::Request;
use common::{assert_problem, send, send_with};
use serde_json::json;
use sqlx::PgPool;

const POSTS: &str =
    r#"{"slug": "posts", "fields": [{"name": "title", "type": "text", "required": true}]}"#;

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn create_and_list_collections(pool: PgPool) {
    let app = common::admin_app(pool).await;
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
    let app = common::admin_app(pool).await;
    send(&app, "POST", "/api/admin/collections", Some(POSTS)).await;
    let res = send(&app, "POST", "/api/admin/collections", Some(POSTS)).await;
    assert_problem(&res, 409);
    assert_eq!(res.json["detail"], "collection `posts` already exists");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn invalid_schemas_are_422(pool: PgPool) {
    let app = common::admin_app(pool).await;
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
    let app = common::admin_app(pool).await;
    let body =
        r#"{"slug": "posts", "fields": [{"name": "t", "type": "text", "searchable": true}]}"#;
    let res = send(&app, "POST", "/api/admin/collections", Some(body)).await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["message"],
        "field `t`: unknown option `searchable`"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn malformed_bodies_are_400(pool: PgPool) {
    let app = common::admin_app(pool).await;
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
    let app = common::admin_app(pool).await;
    assert_problem(&send(&app, "GET", "/nope", None).await, 404);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn request_ids_are_echoed_or_generated(pool: PgPool) {
    let app = common::admin_app(pool).await;
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

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn relationships_must_target_existing_collections(pool: PgPool) {
    let app = common::admin_app(pool).await;
    let to_nowhere = r#"{"slug": "posts", "fields": [{"name": "a", "type": "relationship", "relationTo": "authors"}]}"#;
    let res = send(&app, "POST", "/api/admin/collections", Some(to_nowhere)).await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["message"],
        "relationTo `authors` doesn't match an existing collection"
    );

    let to_self = r#"{"slug": "pages", "fields": [{"name": "parent", "type": "relationship", "relationTo": "pages"}]}"#;
    assert_eq!(
        send(&app, "POST", "/api/admin/collections", Some(to_self))
            .await
            .status,
        201
    );

    let global = r#"{"slug": "settings", "kind": "global", "fields": []}"#;
    assert_eq!(
        send(&app, "POST", "/api/admin/collections", Some(global))
            .await
            .status,
        201
    );
    let to_global = r#"{"slug": "notes", "fields": [{"name": "s", "type": "relationship", "relationTo": "settings"}]}"#;
    assert_problem(
        &send(&app, "POST", "/api/admin/collections", Some(to_global)).await,
        422,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn misspelt_keys_are_named_before_other_schema_errors(pool: PgPool) {
    let app = common::admin_app(pool).await;
    let body = r#"{"slug": "posts", "fields": [{"nmae": "title", "type": "text"}]}"#;
    let res = send(&app, "POST", "/api/admin/collections", Some(body)).await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["message"],
        "field `fields.0`: unknown option `nmae`"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn bad_options_name_the_field(pool: PgPool) {
    let app = common::admin_app(pool).await;
    let schema = json!({"slug": "posts", "fields": [
        {"name": "kind", "type": "select", "options": [1, 2]}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    assert_problem(&res, 400);
    assert_eq!(
        res.json["detail"],
        "field `kind`: each option must be text, or {\"label\": \"…\", \"value\": \"…\"}"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn schemas_come_back_without_default_options(pool: PgPool) {
    let app = common::admin_app(pool).await;
    let schema = json!({"slug": "posts", "fields": [
        {"type": "row", "fields": [{"name": "title", "type": "text", "required": true}]}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    assert_eq!(
        res.json["schema"],
        json!({"slug": "posts", "kind": "collection", "fields": [
            {"type": "row", "fields": [{"name": "title", "type": "text", "required": true}]}
        ]})
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn schema_names_a_configured_storage(pool: PgPool) {
    let app = common::admin_app(pool).await;
    let images = |upload: serde_json::Value| {
        json!({"slug": "images", "upload": upload, "fields": []}).to_string()
    };
    for (upload, path, message) in [
        (
            json!({"storage": "nope"}),
            "upload.storage",
            "no storage named `nope` is configured; this server has default",
        ),
        (
            json!({"disableLocalStorage": true}),
            "upload.storage",
            "disableLocalStorage is set, but `default` is local storage; name a bucket",
        ),
        (
            json!({"constructorOptions": {"limitInputPixels": 50_000_001}}),
            "upload.constructorOptions.limitInputPixels",
            "50000001 is above this server's limit of 50000000 (`[uploads] max_pixels`)",
        ),
    ] {
        let res = send(
            &app,
            "POST",
            "/api/admin/collections",
            Some(&images(upload.clone())),
        )
        .await;
        assert_problem(&res, 422);
        assert_eq!(res.json["errors"][0]["path"], path, "{upload}");
        assert_eq!(res.json["errors"][0]["message"], message, "{upload}");
    }
    for dir in ["../x", "/etc", "a//b", "./x", "x/", "a\\b", "", "."] {
        let res = send(
            &app,
            "POST",
            "/api/admin/collections",
            Some(&images(json!({ "staticDir": dir }))),
        )
        .await;
        assert_problem(&res, 422);
        assert_eq!(res.json["errors"][0]["path"], "upload.staticDir", "{dir:?}");
        assert_eq!(
            res.json["errors"][0]["message"],
            format!(
                "`{dir}` has to be a relative path of plain names, like media/images: no empty, `.` or `..` parts, no leading or trailing /, and no backslash"
            ),
            "{dir:?}"
        );
    }
    let ok = images(json!({
        "storage": "default", "staticDir": "media/images",
        "constructorOptions": {"limitInputPixels": 50_000_000}
    }));
    let res = send(&app, "POST", "/api/admin/collections", Some(&ok)).await;
    assert_eq!(res.status, 201, "{}", res.json);
    // A change is checked the same way.
    let change = json!({"schema": {"slug": "images", "upload": {"storage": "nope"}, "fields": []}});
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/images",
        Some(&change.to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "upload.storage");
}
