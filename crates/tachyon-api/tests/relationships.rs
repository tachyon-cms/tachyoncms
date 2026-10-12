mod common;

use axum::Router;
use common::{assert_problem, send};
use serde_json::{Value, json};
use sqlx::PgPool;

async fn create(app: &Router, url: &str, body: Value) -> Value {
    let res = send(app, "POST", url, Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "{}", res.json);
    res.json
}

async fn app(pool: PgPool) -> Router {
    let app = tachyon_api::router(pool).await.unwrap();
    create(
        &app,
        "/api/admin/collections",
        json!({"slug": "authors", "fields": [
            {"name": "name", "type": "text"},
            {"name": "mentor", "type": "relationship", "relationTo": "authors"}
        ]}),
    )
    .await;
    create(
        &app,
        "/api/admin/collections",
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "related", "type": "relationship", "relationTo": "posts", "hasMany": true}
        ]}),
    )
    .await;
    app
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn populates_to_the_requested_depth(pool: PgPool) {
    let app = app(pool).await;
    let mentor = create(&app, "/api/authors", json!({"name": "Mo"})).await;
    let ann = create(
        &app,
        "/api/authors",
        json!({"name": "Ann", "mentor": mentor["id"]}),
    )
    .await;
    let post = create(
        &app,
        "/api/posts",
        json!({"title": "Hi", "author": ann["id"]}),
    )
    .await;
    let url = format!("/api/posts/{}", post["id"].as_str().unwrap());

    let d0 = send(&app, "GET", &format!("{url}?depth=0"), None)
        .await
        .json;
    assert_eq!(d0["author"], ann["id"]);
    let d1 = send(&app, "GET", &url, None).await.json;
    assert_eq!(d1["author"]["name"], "Ann");
    assert_eq!(
        d1["author"]["mentor"], mentor["id"],
        "depth 1 stops after one hop"
    );
    let d2 = send(&app, "GET", &format!("{url}?depth=2"), None)
        .await
        .json;
    assert_eq!(d2["author"]["mentor"]["name"], "Mo");

    let list = send(&app, "GET", "/api/posts", None).await.json;
    assert_eq!(list["docs"][0]["author"]["name"], "Ann");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn links_must_point_at_live_documents_of_the_right_collection(pool: PgPool) {
    let app = app(pool).await;
    let other_post = create(&app, "/api/posts", json!({"title": "x"})).await;
    for author in [
        other_post["id"].clone(),
        json!("0199d5c4-0000-7000-8000-000000000000"),
    ] {
        let body = json!({"title": "t", "author": author}).to_string();
        let res = send(&app, "POST", "/api/posts", Some(&body)).await;
        assert_problem(&res, 422);
        assert_eq!(res.json["errors"][0]["path"], "author");
    }
    let me = create(&app, "/api/posts", json!({"title": "me"})).await;
    let url = format!("/api/posts/{}", me["id"].as_str().unwrap());
    let res = send(
        &app,
        "PATCH",
        &url,
        Some(&json!({"related": [me["id"]]}).to_string()),
    )
    .await;
    assert_eq!(
        res.status, 200,
        "a document may link to itself: {}",
        res.json
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn linked_documents_are_protected_and_listed(pool: PgPool) {
    let app = app(pool).await;
    let ann = create(&app, "/api/authors", json!({"name": "Ann"})).await;
    let ann_url = format!("/api/authors/{}", ann["id"].as_str().unwrap());
    let post = create(
        &app,
        "/api/posts",
        json!({"title": "Hi", "author": ann["id"]}),
    )
    .await;

    let res = send(&app, "DELETE", &ann_url, None).await;
    assert_problem(&res, 409);
    assert_eq!(
        res.json["detail"],
        "1 other document links to this one; remove that link first"
    );

    let refs = send(&app, "GET", &format!("{ann_url}/references"), None)
        .await
        .json;
    assert_eq!(
        refs["docs"],
        json!([{"collection": "posts", "id": post["id"], "path": "author"}])
    );

    let post_url = format!("/api/posts/{}", post["id"].as_str().unwrap());
    assert_eq!(send(&app, "DELETE", &post_url, None).await.status, 204);
    assert_eq!(send(&app, "DELETE", &ann_url, None).await.status, 204);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn polymorphic_links_store_populate_and_filter(pool: PgPool) {
    let app = app(pool).await;
    create(&app, "/api/admin/collections", json!({"slug": "menus", "fields": [
        {"name": "label", "type": "text"},
        {"name": "target", "type": "relationship", "relationTo": ["posts", "authors"]},
        {"name": "extras", "type": "relationship", "relationTo": ["posts", "authors"], "hasMany": true}
    ]}))
    .await;
    let ann = create(&app, "/api/authors", json!({"name": "Ann"})).await;
    let post = create(&app, "/api/posts", json!({"title": "Hi"})).await;
    let menu = create(
        &app,
        "/api/menus",
        json!({
            "label": "Top",
            "target": {"relationTo": "authors", "value": ann["id"]},
            "extras": [{"relationTo": "posts", "value": post["id"]}]
        }),
    )
    .await;
    assert_eq!(menu["target"]["relationTo"], "authors");
    assert_eq!(menu["target"]["value"]["name"], "Ann", "populated in place");
    assert_eq!(menu["extras"][0]["value"]["title"], "Hi");

    let wrong = send(
        &app,
        "POST",
        "/api/menus",
        Some(&json!({"target": {"relationTo": "posts", "value": ann["id"]}}).to_string()),
    )
    .await;
    assert_problem(&wrong, 422);
    assert_eq!(wrong.json["errors"][0]["path"], "target");

    let by_collection = send(
        &app,
        "GET",
        "/api/menus?where[target.relationTo][equals]=authors&depth=0",
        None,
    )
    .await
    .json;
    assert_eq!(by_collection["docs"][0]["target"]["value"], ann["id"]);
    let by_link = send(
        &app,
        "GET",
        &format!(
            "/api/menus?where[extras][contains]={}",
            post["id"].as_str().unwrap()
        ),
        None,
    )
    .await
    .json;
    assert_eq!(by_link["docs"][0]["label"], "Top");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_link_cannot_borrow_another_links_collection(pool: PgPool) {
    let app = app(pool).await;
    create(&app, "/api/admin/collections", json!({"slug": "menus", "fields": [
        {"name": "target", "type": "relationship", "relationTo": ["posts", "authors"]},
        {"name": "extras", "type": "relationship", "relationTo": ["posts", "authors"], "hasMany": true}
    ]}))
    .await;
    let ann = create(&app, "/api/authors", json!({"name": "Ann"})).await;
    // The first link is right; the second claims Ann is a post.
    let body = json!({
        "target": {"relationTo": "authors", "value": ann["id"]},
        "extras": [{"relationTo": "posts", "value": ann["id"]}]
    });
    let res = send(&app, "POST", "/api/menus", Some(&body.to_string())).await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "extras.0");
}
