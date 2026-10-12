mod common;

use common::{assert_problem, send};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn globals_hold_one_document(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    let schema = json!({"slug": "settings", "kind": "global", "fields": [
        {"name": "siteName", "type": "text", "required": true},
        {"name": "tagline", "type": "text", "default": "Just launched"}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    assert_eq!(res.status, 201);

    let empty = send(&app, "GET", "/api/globals/settings", None).await;
    assert_eq!(empty.status, 200);
    assert_eq!(empty.json, json!({"tagline": "Just launched"}));

    assert_problem(
        &send(&app, "PATCH", "/api/globals/settings", Some("{}")).await,
        422,
    );
    let saved = send(
        &app,
        "PATCH",
        "/api/globals/settings",
        Some(r#"{"siteName": "Tachyon"}"#),
    )
    .await;
    assert_eq!(saved.status, 200, "{}", saved.json);
    assert_eq!(saved.json["tagline"], "Just launched");

    let again = send(
        &app,
        "PATCH",
        "/api/globals/settings",
        Some(r#"{"tagline": "Fast"}"#),
    )
    .await;
    assert_eq!(
        (
            again.json["siteName"].as_str(),
            again.json["tagline"].as_str()
        ),
        (Some("Tachyon"), Some("Fast"))
    );
    assert_eq!(
        again.json["id"], saved.json["id"],
        "always the same document"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn globals_and_collections_dont_mix(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    let global = r#"{"slug": "settings", "kind": "global", "fields": []}"#;
    send(&app, "POST", "/api/admin/collections", Some(global)).await;
    send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(r#"{"slug": "posts", "fields": []}"#),
    )
    .await;
    assert_problem(&send(&app, "GET", "/api/settings", None).await, 404);
    assert_problem(&send(&app, "GET", "/api/globals/posts", None).await, 404);
    assert_problem(&send(&app, "GET", "/api/globals/nope", None).await, 404);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn globals_show_nested_defaults_before_first_save(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    let schema = json!({"slug": "settings", "kind": "global", "fields": [
        {"name": "seo", "type": "group", "fields": [
            {"name": "index", "type": "boolean", "default": true}
        ]},
        {"type": "tabs", "tabs": [
            {"name": "footer", "label": "Footer", "fields": [
                {"name": "note", "type": "text", "default": "Made with care"}
            ]}
        ]}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let empty = send(&app, "GET", "/api/globals/settings", None).await;
    assert_eq!(
        empty.json,
        json!({"seo": {"index": true}, "footer": {"note": "Made with care"}})
    );
}
