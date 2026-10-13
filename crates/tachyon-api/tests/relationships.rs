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
    let app = common::admin_app(pool).await;
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

async fn blog(pool: PgPool, default_populate: Option<serde_json::Value>) -> (Router, String) {
    let app = common::admin_app(pool).await;
    let mut authors = json!({"slug": "authors", "fields": [
        {"name": "name", "type": "text"}, {"name": "bio", "type": "textarea"}
    ]});
    if let Some(dp) = default_populate {
        authors["defaultPopulate"] = dp;
    }
    for schema in [
        authors,
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "authors"}
        ]}),
    ] {
        let res = send(
            &app,
            "POST",
            "/api/admin/collections",
            Some(&schema.to_string()),
        )
        .await;
        assert_eq!(res.status, 201, "{}", res.json);
    }
    let ann = send(
        &app,
        "POST",
        "/api/authors",
        Some(r#"{"name": "Ann", "bio": "Writes"}"#),
    )
    .await;
    let body = json!({"title": "Hi", "author": ann.json["id"]}).to_string();
    let post = send(&app, "POST", "/api/posts", Some(&body)).await;
    (app, post.json["id"].as_str().unwrap().to_owned())
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn populate_shapes_linked_documents_per_collection(pool: PgPool) {
    let (app, post) = blog(pool, None).await;
    let res = send(
        &app,
        "GET",
        &format!("/api/posts/{post}?populate[authors][name]=true"),
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let author = res.json["author"].as_object().unwrap();
    let keys: Vec<&String> = author.keys().collect();
    assert_eq!(keys, ["id", "name"]);
    let list = send(&app, "GET", "/api/posts?populate[authors][bio]=false", None).await;
    assert!(list.json["docs"][0]["author"].get("bio").is_none());
    assert_eq!(list.json["docs"][0]["author"]["name"], "Ann");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn default_populate_applies_when_no_populate_is_given(pool: PgPool) {
    let (app, post) = blog(pool, Some(json!({"name": true}))).await;
    let res = send(&app, "GET", &format!("/api/posts/{post}"), None).await;
    let keys: Vec<&String> = res.json["author"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["id", "name"]);
    let res = send(
        &app,
        "GET",
        &format!("/api/posts/{post}?populate[authors][bio]=true"),
        None,
    )
    .await;
    let keys: Vec<&String> = res.json["author"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["bio", "id"], "populate wins over defaultPopulate");
    let direct = send(&app, "GET", "/api/authors", None).await;
    assert_eq!(
        direct.json["docs"][0]["bio"], "Writes",
        "defaultPopulate only shapes populated copies"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn bad_populate_is_a_400(pool: PgPool) {
    let (app, post) = blog(pool, None).await;
    for (q, detail) in [
        (
            "populate[nope][name]=true",
            "populate[nope]: no such collection",
        ),
        (
            "populate[authors][nope]=true",
            "populate[authors][nope]: unknown field",
        ),
        (
            "populate=authors",
            "populate: expected populate[collection][field]=true",
        ),
    ] {
        let res = send(&app, "GET", &format!("/api/posts/{post}?{q}"), None).await;
        assert_problem(&res, 400);
        assert_eq!(res.json["detail"], detail, "{q}");
    }
}

async fn capped(pool: PgPool) -> (Router, String) {
    let app = common::admin_app(pool).await;
    for schema in [
        json!({"slug": "people", "fields": [
            {"name": "name", "type": "text"},
            {"name": "mentor", "type": "relationship", "relationTo": "people"}
        ]}),
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "people", "maxDepth": 1},
            {"name": "editor", "type": "relationship", "relationTo": "people"},
            {"name": "pinned", "type": "relationship", "relationTo": "people", "maxDepth": 0}
        ]}),
    ] {
        let res = send(
            &app,
            "POST",
            "/api/admin/collections",
            Some(&schema.to_string()),
        )
        .await;
        assert_eq!(res.status, 201, "{}", res.json);
    }
    let bo = send(&app, "POST", "/api/people", Some(r#"{"name": "Bo"}"#)).await;
    let ann = send(
        &app,
        "POST",
        "/api/people",
        Some(&json!({"name": "Ann", "mentor": bo.json["id"]}).to_string()),
    )
    .await;
    let body = json!({"title": "Hi", "author": ann.json["id"], "editor": ann.json["id"], "pinned": ann.json["id"]});
    let post = send(&app, "POST", "/api/posts", Some(&body.to_string())).await;
    (app, post.json["id"].as_str().unwrap().to_owned())
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn max_depth_caps_population_per_field(pool: PgPool) {
    let (app, post) = capped(pool).await;
    let res = send(&app, "GET", &format!("/api/posts/{post}?depth=2"), None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert!(
        res.json["pinned"].is_string(),
        "maxDepth 0 never populates: {}",
        res.json
    );
    assert_eq!(res.json["author"]["name"], "Ann");
    assert!(
        res.json["author"]["mentor"].is_string(),
        "maxDepth 1 stops after one level"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_same_document_through_two_fields_gets_each_fields_depth(pool: PgPool) {
    let (app, post) = capped(pool).await;
    let res = send(&app, "GET", &format!("/api/posts/{post}?depth=2"), None).await;
    assert!(res.json["author"]["mentor"].is_string());
    assert_eq!(
        res.json["editor"]["mentor"]["name"], "Bo",
        "no cap: full depth"
    );
}

async fn with_virtuals(pool: PgPool) -> (Router, Value) {
    let app = common::admin_app(pool).await;
    for schema in [
        json!({"slug": "authors", "fields": [
            {"name": "name", "type": "text"},
            {"name": "profile", "type": "group", "fields": [{"name": "city", "type": "text"}]}
        ]}),
        json!({"slug": "guests", "fields": [{"name": "name", "type": "text"}]}),
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "authorName", "type": "text", "virtual": "author.name"},
            {"name": "authorCity", "type": "text", "virtual": "author.profile.city"},
            {"name": "people", "type": "relationship", "relationTo": ["authors", "guests"], "hasMany": true},
            {"name": "peopleNames", "type": "text", "virtual": "people.name"}
        ]}),
    ] {
        let res = send(
            &app,
            "POST",
            "/api/admin/collections",
            Some(&schema.to_string()),
        )
        .await;
        assert_eq!(res.status, 201, "{}", res.json);
    }
    let ann = send(
        &app,
        "POST",
        "/api/authors",
        Some(r#"{"name": "Ann", "profile": {"city": "Leeds"}}"#),
    )
    .await;
    let gus = send(&app, "POST", "/api/guests", Some(r#"{"name": "Gus"}"#)).await;
    let body = json!({
        "title": "Hi",
        "author": ann.json["id"],
        "people": [
            {"relationTo": "authors", "value": ann.json["id"]},
            {"relationTo": "guests", "value": gus.json["id"]}
        ]
    });
    let post = send(&app, "POST", "/api/posts", Some(&body.to_string())).await;
    assert_eq!(post.status, 201, "{}", post.json);
    (app, post.json)
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn virtual_fields_read_through_links(pool: PgPool) {
    let (app, created) = with_virtuals(pool).await;
    assert_eq!(
        created["authorName"], "Ann",
        "write responses show virtuals too"
    );
    let id = created["id"].as_str().unwrap();
    for depth in [0, 1] {
        let res = send(&app, "GET", &format!("/api/posts/{id}?depth={depth}"), None).await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(res.json["authorName"], "Ann");
        assert_eq!(res.json["authorCity"], "Leeds");
        assert_eq!(res.json["peopleNames"], json!(["Ann", "Gus"]));
    }
    let list = send(&app, "GET", "/api/posts", None).await;
    assert_eq!(list.json["docs"][0]["authorName"], "Ann");

    // No link: no value.
    let bare = send(&app, "POST", "/api/posts", Some(r#"{"title": "Alone"}"#)).await;
    assert!(bare.json.get("authorName").is_none(), "{}", bare.json);
    assert!(bare.json.get("peopleNames").is_none(), "{}", bare.json);

    let refused = send(
        &app,
        "PATCH",
        &format!("/api/posts/{id}"),
        Some(r#"{"authorName": "x"}"#),
    )
    .await;
    assert_problem(&refused, 422);
    assert_eq!(refused.json["errors"][0]["path"], "authorName");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn virtual_fields_in_populated_documents(pool: PgPool) {
    let (app, created) = with_virtuals(pool).await;
    let schema = json!({"slug": "lists", "fields": [
        {"name": "post", "type": "relationship", "relationTo": "posts"}
    ]});
    send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    let list = send(
        &app,
        "POST",
        "/api/lists",
        Some(&json!({"post": created["id"]}).to_string()),
    )
    .await;
    let id = list.json["id"].as_str().unwrap();
    let res = send(&app, "GET", &format!("/api/lists/{id}"), None).await;
    assert_eq!(res.json["post"]["authorName"], "Ann", "{}", res.json);
    let shaped = send(
        &app,
        "GET",
        &format!("/api/lists/{id}?populate[posts][authorName]=true"),
        None,
    )
    .await;
    let keys: Vec<&String> = shaped.json["post"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["authorName", "id"], "{}", shaped.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn select_picks_virtual_fields(pool: PgPool) {
    let (app, created) = with_virtuals(pool).await;
    let id = created["id"].as_str().unwrap();
    let res = send(
        &app,
        "GET",
        &format!("/api/posts/{id}?select[authorName]=true"),
        None,
    )
    .await;
    let keys: Vec<&String> = res.json.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["authorName", "id"]);
    let res = send(
        &app,
        "GET",
        &format!("/api/posts/{id}?select[authorName]=false"),
        None,
    )
    .await;
    assert!(res.json.get("authorName").is_none());
    assert_eq!(res.json["authorCity"], "Leeds");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn virtual_targets_must_have_the_field(pool: PgPool) {
    let app = common::admin_app(pool).await;
    let authors = json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]});
    send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&authors.to_string()),
    )
    .await;
    let posts = json!({"slug": "posts", "fields": [
        {"name": "author", "type": "relationship", "relationTo": "authors"},
        {"name": "v", "type": "text", "virtual": "author.nope"}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&posts.to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["message"],
        "field `v`: authors has no field `nope` to read"
    );

    let ok = json!({"slug": "posts", "fields": [
        {"name": "author", "type": "relationship", "relationTo": "authors"}
    ]});
    send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&ok.to_string()),
    )
    .await;
    let change = json!({"schema": posts});
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/posts",
        Some(&change.to_string()),
    )
    .await;
    assert_problem(&res, 422);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn virtuals_skip_deleted_documents_and_fill_inside_groups(pool: PgPool) {
    let app = common::admin_app(pool.clone()).await;
    for schema in [
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
        json!({"slug": "posts", "fields": [
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "people", "type": "relationship", "relationTo": "authors", "hasMany": true},
            {"name": "meta", "type": "group", "fields": [
                {"name": "by", "type": "text", "virtual": "author.name"}
            ]},
            {"name": "names", "type": "text", "virtual": "people.name"}
        ]}),
    ] {
        let res = send(
            &app,
            "POST",
            "/api/admin/collections",
            Some(&schema.to_string()),
        )
        .await;
        assert_eq!(res.status, 201, "{}", res.json);
    }
    let ann = send(&app, "POST", "/api/authors", Some(r#"{"name": "Ann"}"#)).await;
    let bo = send(&app, "POST", "/api/authors", Some(r#"{"name": "Bo"}"#)).await;
    let body = json!({"author": ann.json["id"], "people": [ann.json["id"], bo.json["id"]]});
    let post = send(&app, "POST", "/api/posts", Some(&body.to_string())).await;
    assert_eq!(
        post.json["meta"]["by"], "Ann",
        "a virtual inside a group: {}",
        post.json
    );

    // Deleted behind the API's back (deleting through it is refused while linked).
    sqlx::query("UPDATE documents SET deleted_at = now() WHERE id = $1")
        .bind(uuid::Uuid::parse_str(ann.json["id"].as_str().unwrap()).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let id = post.json["id"].as_str().unwrap();
    let res = send(&app, "GET", &format!("/api/posts/{id}"), None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert!(
        res.json.get("meta").and_then(|m| m.get("by")).is_none(),
        "{}",
        res.json
    );
    assert_eq!(res.json["names"], json!(["Bo"]));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn populate_without_fields_names_the_collection(pool: PgPool) {
    let (app, post) = blog(pool, None).await;
    let res = send(
        &app,
        "GET",
        &format!("/api/posts/{post}?populate[authors]=true"),
        None,
    )
    .await;
    assert_problem(&res, 400);
    assert_eq!(
        res.json["detail"],
        "populate[authors]: expected populate[authors][field]=true"
    );
}
