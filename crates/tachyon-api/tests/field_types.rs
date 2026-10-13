mod common;

use common::{assert_problem, send};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn new_field_types_work_through_the_api(pool: PgPool) {
    let app = common::admin_app(pool).await;
    let schema = json!({"slug": "recipes", "fields": [
        {"name": "title", "type": "text", "admin": {"description": "What's it called?"}},
        {"name": "course", "type": "radio", "options": [{"label": "Starter", "value": "starter"}, {"label": "Main", "value": "main"}]},
        {"name": "vegan", "type": "checkbox"},
        {"name": "steps", "type": "text", "hasMany": true, "minRows": 1},
        {"name": "temps", "type": "number", "hasMany": true},
        {"name": "embed", "type": "code", "language": "html"}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["schema"]["fields"][2]["type"], "boolean");

    let body = json!({"title": "Soup", "course": "starter", "vegan": true, "steps": ["Chop", "Boil"], "temps": [180, 200], "embed": "<p>hi</p>"});
    let created = send(&app, "POST", "/api/recipes", Some(&body.to_string())).await;
    assert_eq!(created.status, 201, "{}", created.json);

    let bad = send(
        &app,
        "POST",
        "/api/recipes",
        Some(&json!({"course": "Starter", "steps": []}).to_string()),
    )
    .await;
    assert_problem(&bad, 422);
    assert_eq!(
        bad.json["errors"],
        json!([
            {"path": "course", "message": "expected one of: starter, main"},
            {"path": "steps", "message": "must have at least 1 row"}
        ])
    );

    let found = send(
        &app,
        "GET",
        "/api/recipes?where[temps][contains]=200&where[course][equals]=starter",
        None,
    )
    .await;
    assert_eq!(found.json["docs"][0]["title"], "Soup");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn layout_fields_end_to_end(pool: PgPool) {
    let app = common::admin_app(pool).await;
    let flat = json!({"slug": "pages", "fields": [{"name": "title", "type": "text"}, {"name": "rank", "type": "number"}]});
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/admin/collections",
            Some(&flat.to_string())
        )
        .await
        .status,
        201
    );
    let doc = send(
        &app,
        "POST",
        "/api/pages",
        Some(r#"{"title": "Home", "rank": 1}"#),
    )
    .await;
    let url = format!("/api/pages/{}", doc.json["id"].as_str().unwrap());

    let arranged = json!({"schema": {"slug": "pages", "fields": [
        {"type": "tabs", "tabs": [
            {"label": "Content", "fields": [{"type": "row", "fields": [{"name": "title", "type": "text"}]}]},
            {"label": "Settings", "fields": [{"type": "collapsible", "label": "Order", "fields": [{"name": "rank", "type": "number"}]}]}
        ]}
    ]}});
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/pages",
        Some(&arranged.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        res.json["preview"]["diff"]["changes"],
        json!([{"type": "layoutChanged"}])
    );
    assert_eq!(res.json["collection"]["schemaVersion"], 2);

    let got = send(&app, "GET", &url, None).await.json;
    assert_eq!(
        (got["title"].as_str(), got["rank"].as_i64()),
        (Some("Home"), Some(1)),
        "data is untouched"
    );
    let found = send(&app, "GET", "/api/pages?where[rank][equals]=1", None)
        .await
        .json;
    assert_eq!(found["docs"][0]["title"], "Home");
    let bad = send(&app, "PATCH", &url, Some(r#"{"rank": "first"}"#)).await;
    assert_problem(&bad, 422);
    assert_eq!(bad.json["errors"][0]["path"], "rank");
}
