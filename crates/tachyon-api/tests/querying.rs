mod common;

use axum::Router;
use common::{assert_problem, send};
use serde_json::json;
use sqlx::PgPool;

async fn app(pool: PgPool) -> Router {
    let app = tachyon_api::router(pool).await.unwrap();
    let schema = json!({"slug": "items", "fields": [
        {"name": "title", "type": "text"},
        {"name": "price", "type": "number", "filterable": true},
        {"name": "live", "type": "boolean"}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    assert_eq!(res.status, 201);
    for (title, price, live) in [("a", 1, true), ("b", 2, false), ("c", 3, true)] {
        let body = json!({"title": title, "price": price, "live": live}).to_string();
        assert_eq!(
            send(&app, "POST", "/api/items", Some(&body)).await.status,
            201
        );
    }
    app
}

fn titles(json: &serde_json::Value) -> Vec<&str> {
    json["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["title"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn filters_and_sorts(pool: PgPool) {
    let app = app(pool).await;
    let res = send(
        &app,
        "GET",
        "/api/items?where[live][equals]=true&sort=-price",
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(titles(&res.json), ["c", "a"]);
    let res = send(
        &app,
        "GET",
        "/api/items?where[or][0][price][lt]=2&where[or][1][title][equals]=c&sort=title",
        None,
    )
    .await;
    assert_eq!(titles(&res.json), ["a", "c"]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn cursor_pagination(pool: PgPool) {
    let app = app(pool).await;
    let first = send(&app, "GET", "/api/items?sort=price&limit=2", None).await;
    assert_eq!(titles(&first.json), ["a", "b"]);
    assert_eq!(first.json["page"], 1);
    let cursor = first.json["nextCursor"].as_str().unwrap().to_owned();
    let second = send(
        &app,
        "GET",
        &format!("/api/items?sort=price&limit=2&after={cursor}"),
        None,
    )
    .await;
    assert_eq!(titles(&second.json), ["c"]);
    assert!(second.json.get("page").is_none());
    assert!(second.json.get("nextCursor").is_none());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn bad_parameters_are_400(pool: PgPool) {
    let app = app(pool).await;
    let number_cursor = {
        let first = send(&app, "GET", "/api/items?sort=price&limit=1", None).await;
        first.json["nextCursor"].as_str().unwrap().to_owned()
    };
    let cases: Vec<(String, &str)> = vec![
        (
            "where[price][gt]=abc".into(),
            "where[price][gt]: expected a number",
        ),
        (
            "where[live][equals]=yes".into(),
            "where[live][equals]: expected true or false",
        ),
        (
            "where[nope][equals]=1".into(),
            "where[nope][equals]: unknown field `nope`",
        ),
        (
            "sort=price,title".into(),
            "sort: sorting by more than one field isn't supported yet",
        ),
        (
            "wehre[price][gt]=1".into(),
            "unknown query parameter `wehre`",
        ),
        ("after=zz".into(), "after isn't a valid cursor"),
        (
            format!("sort=title&after={number_cursor}"),
            "after doesn't match this sort; start again without it",
        ),
        (
            format!("page=2&after={number_cursor}"),
            "use either page or after, not both",
        ),
        ("depth=6".into(), "depth must be between 0 and 5"),
        ("limit[x]=1".into(), "limit must be a single value"),
    ];
    for (qs, detail) in cases {
        let res = send(&app, "GET", &format!("/api/items?{qs}"), None).await;
        assert_problem(&res, 400);
        assert_eq!(res.json["detail"], detail, "{qs}");
    }
    let one = send(&app, "GET", "/api/items?limit=1", None).await;
    let id = one.json["docs"][0]["id"].as_str().unwrap();
    assert_problem(
        &send(&app, "GET", &format!("/api/items/{id}?sort=price"), None).await,
        400,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn hostile_cursors_dates_and_nuls_are_400s(pool: PgPool) {
    use tachyon_core::query::{Cursor, Scalar};
    let app = app(pool).await;
    let garbage = Cursor {
        value: Some(Scalar::Text("garbage".into())),
        id: uuid::Uuid::nil(),
    }
    .encode();
    for qs in [
        format!("sort=id&after={garbage}"),
        format!("sort=createdAt&after={garbage}"),
        "where[createdAt][gt]=9999-12-31T23:00:00-02:00".to_owned(),
        "where[title][equals]=a%00".to_owned(),
        "where[title][like]=a%00".to_owned(),
    ] {
        assert_problem(
            &send(&app, "GET", &format!("/api/items?{qs}"), None).await,
            400,
        );
    }

    let schema = r#"{"slug": "events", "fields": [{"name": "at", "type": "date"}]}"#;
    assert_eq!(
        send(&app, "POST", "/api/admin/collections", Some(schema))
            .await
            .status,
        201
    );
    let res = send(
        &app,
        "POST",
        "/api/events",
        Some(r#"{"at": "9999-12-31T23:00:00-02:00"}"#),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "at");
}
