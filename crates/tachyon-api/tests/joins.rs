mod common;

use axum::Router;
use common::{assert_problem, send};
use serde_json::{Value, json};
use sqlx::PgPool;

async fn create(app: &Router, url: &str, body: Value) -> Value {
    let res = send(app, "POST", url, Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "{url}: {}", res.json);
    res.json
}

/// Categories that list their posts; `news` has posts a, b, c (oldest first), `guides` has g.
async fn blog(pool: PgPool) -> (Router, String, String) {
    let app = tachyon_api::router(pool).await.unwrap();
    create(
        &app,
        "/api/admin/collections",
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    create(
        &app,
        "/api/admin/collections",
        json!({"slug": "categories", "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    create(
        &app,
        "/api/admin/collections",
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "draft", "type": "boolean"},
            {"name": "category", "type": "relationship", "relationTo": "categories"},
            {"name": "author", "type": "relationship", "relationTo": "authors"}
        ]}),
    )
    .await;
    let change = json!({"schema": {"slug": "categories", "fields": [
        {"name": "name", "type": "text"},
        {"name": "posts", "type": "join", "collection": "posts", "on": "category",
         "where": {"draft": {"not_equals": "true"}}},
        {"name": "meta", "type": "group", "fields": [
            {"name": "recent", "type": "join", "collection": "posts", "on": "category", "defaultLimit": 1}
        ]}
    ]}});
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/categories",
        Some(&change.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let ann = create(&app, "/api/authors", json!({"name": "Ann"})).await;
    let news = create(&app, "/api/categories", json!({"name": "news"})).await;
    let guides = create(&app, "/api/categories", json!({"name": "guides"})).await;
    for title in ["a", "b", "c"] {
        create(
            &app,
            "/api/posts",
            json!({"title": title, "category": news["id"], "author": ann["id"]}),
        )
        .await;
    }
    create(
        &app,
        "/api/posts",
        json!({"title": "hidden", "draft": true, "category": news["id"]}),
    )
    .await;
    create(
        &app,
        "/api/posts",
        json!({"title": "g", "category": guides["id"]}),
    )
    .await;
    let id = |v: &Value| v["id"].as_str().unwrap().to_owned();
    (app, id(&news), id(&guides))
}

fn titles(join: &Value) -> Vec<&str> {
    join["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["title"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_category_lists_its_posts(pool: PgPool) {
    let (app, news, _) = blog(pool).await;
    let res = send(&app, "GET", &format!("/api/categories/{news}"), None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        titles(&res.json["posts"]),
        ["c", "b", "a"],
        "newest first, draft left out by the schema's where"
    );
    assert_eq!(res.json["posts"]["hasNextPage"], false);
    assert!(res.json["posts"].get("totalDocs").is_none());
    assert_eq!(
        titles(&res.json["meta"]["recent"]),
        ["hidden"],
        "defaultLimit inside a group; no where, so drafts count"
    );
    assert_eq!(res.json["meta"]["recent"]["hasNextPage"], true);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn joins_page_sort_filter_and_count(pool: PgPool) {
    let (app, news, guides) = blog(pool).await;
    let res = send(&app, "GET", &format!(
        "/api/categories/{news}?joins[posts][limit]=2&joins[posts][sort]=title&joins[posts][count]=true"
    ), None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(titles(&res.json["posts"]), ["a", "b"]);
    assert_eq!(res.json["posts"]["hasNextPage"], true);
    assert_eq!(res.json["posts"]["totalDocs"], 3);
    let res = send(&app, "GET", &format!(
        "/api/categories/{news}?joins[posts][limit]=2&joins[posts][sort]=title&joins[posts][page]=2"
    ), None).await;
    assert_eq!(titles(&res.json["posts"]), ["c"]);
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?joins[posts][where][title][not_equals]=b"),
        None,
    )
    .await;
    assert_eq!(titles(&res.json["posts"]), ["c", "a"]);

    let list = send(&app, "GET", "/api/categories?sort=name", None).await;
    assert_eq!(list.status, 200, "{}", list.json);
    assert_eq!(list.json["docs"][0]["id"], guides.as_str());
    assert_eq!(titles(&list.json["docs"][0]["posts"]), ["g"]);
    assert_eq!(titles(&list.json["docs"][1]["posts"]), ["c", "b", "a"]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn schema_where_cant_be_widened(pool: PgPool) {
    let (app, news, _) = blog(pool).await;
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?joins[posts][where][draft][equals]=true"),
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert!(
        titles(&res.json["posts"]).is_empty(),
        "the schema keeps drafts out whatever the request asks"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn depth_zero_gives_ids_and_depth_two_populates_inside(pool: PgPool) {
    let (app, news, _) = blog(pool).await;
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?depth=0"),
        None,
    )
    .await;
    assert!(res.json["posts"]["docs"][0].is_string(), "{}", res.json);
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?depth=1"),
        None,
    )
    .await;
    assert!(
        res.json["posts"]["docs"][0]["author"].is_string(),
        "depth 1: joined docs, links as IDs"
    );
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?depth=2"),
        None,
    )
    .await;
    assert_eq!(res.json["posts"]["docs"][0]["author"]["name"], "Ann");
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?populate[posts][title]=true"),
        None,
    )
    .await;
    let keys: Vec<&String> = res.json["posts"]["docs"][0]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(keys, ["id", "title"], "populate shapes joined docs");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn joins_false_and_select_skip_joins(pool: PgPool) {
    let (app, news, _) = blog(pool).await;
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?joins=false"),
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert!(res.json.get("posts").is_none());
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?select[name]=true"),
        None,
    )
    .await;
    assert!(res.json.get("posts").is_none());
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?select[posts]=true"),
        None,
    )
    .await;
    assert_eq!(titles(&res.json["posts"]).len(), 3);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn bad_joins_parameters_are_400s(pool: PgPool) {
    let (app, news, _) = blog(pool).await;
    for (q, detail) in [
        ("joins[nope][limit]=1", "joins[nope]: no such join field"),
        (
            "joins[posts][limit]=0",
            "joins[posts][limit]: must be between 1 and 100",
        ),
        (
            "joins[posts][page]=0",
            "joins[posts][page]: must be at least 1",
        ),
        (
            "joins[posts][sort]=nope",
            "joins[posts][sort]: unknown field `nope`",
        ),
        (
            "joins[posts][where][nope][equals]=1",
            "joins[posts][where][nope][equals]: unknown field `nope`",
        ),
        (
            "joins[posts][bogus]=1",
            "joins[posts][bogus]: unknown option; use limit, page, sort, where or count",
        ),
        (
            "joins=maybe",
            "joins: use joins=false, or joins[field][option]",
        ),
    ] {
        let res = send(&app, "GET", &format!("/api/categories/{news}?{q}"), None).await;
        assert_problem(&res, 400);
        assert_eq!(res.json["detail"], detail, "{q}");
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn joins_cant_be_written(pool: PgPool) {
    let (app, news, _) = blog(pool).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/api/categories/{news}"),
        Some(r#"{"posts": []}"#),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["message"],
        "is filled from posts.category; it can't be set"
    );
    let created = create(&app, "/api/categories", json!({"name": "fresh"})).await;
    assert_eq!(
        created["posts"]["docs"],
        json!([]),
        "write responses fill joins too"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn create_refuses_a_join_to_a_field_that_doesnt_link_here(pool: PgPool) {
    let (app, _, _) = blog(pool).await;
    let schema = json!({"slug": "tags", "fields": [
        {"name": "posts", "type": "join", "collection": "posts", "on": "category"}
    ]});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["message"],
        "field `posts`: posts.category links to categories, not tags"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn stale_join_targets_return_empty(pool: PgPool) {
    let (app, news, _) = blog(pool.clone()).await;
    // posts loses the field the join reads, behind the join's back.
    sqlx::query(
        "UPDATE collections SET schema = jsonb_set(schema, '{fields}', \
         (SELECT jsonb_agg(f) FROM jsonb_array_elements(schema->'fields') f WHERE f->>'name' <> 'category')) \
         WHERE slug = 'posts'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let app = tachyon_api::router(pool).await.unwrap_or(app);
    let res = send(&app, "GET", &format!("/api/categories/{news}"), None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["posts"], json!({"docs": [], "hasNextPage": false}));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn changes_that_break_another_collections_join_are_refused(pool: PgPool) {
    let (app, _, _) = blog(pool).await;
    let drop_category = json!({"schema": {"slug": "posts", "fields": [
        {"name": "title", "type": "text"},
        {"name": "draft", "type": "boolean"},
        {"name": "author", "type": "relationship", "relationTo": "authors"}
    ]}, "confirm": true});
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/posts",
        Some(&drop_category.to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["message"],
        "this change would break categories: field `posts`: posts has no field `category`"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn nested_joins_take_either_path_form(pool: PgPool) {
    let (app, news, _) = blog(pool).await;
    for q in [
        "joins[meta.recent][limit]=2",
        "joins[meta][recent][limit]=2",
    ] {
        let res = send(&app, "GET", &format!("/api/categories/{news}?{q}"), None).await;
        assert_eq!(res.status, 200, "{q}: {}", res.json);
        assert_eq!(titles(&res.json["meta"]["recent"]).len(), 2, "{q}");
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn join_where_hints_and_near_are_explained(pool: PgPool) {
    let (app, news, _) = blog(pool).await;
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?joins[posts][where]=x"),
        None,
    )
    .await;
    assert_problem(&res, 400);
    assert_eq!(
        res.json["detail"],
        "joins[posts][where]: expected conditions like joins[posts][where][title][equals]=Hello"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn joins_through_lists_self_links_and_several_targets(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    create(
        &app,
        "/api/admin/collections",
        json!({"slug": "topics", "fields": [
            {"name": "name", "type": "text"},
            {"name": "parent", "type": "relationship", "relationTo": "topics"}
        ]}),
    )
    .await;
    create(
        &app,
        "/api/admin/collections",
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "tags", "type": "relationship", "relationTo": "topics", "hasMany": true},
            {"name": "links", "type": "array", "fields": [
                {"name": "target", "type": "relationship", "relationTo": ["topics", "posts"]}
            ]}
        ]}),
    )
    .await;
    let change = json!({"schema": {"slug": "topics", "fields": [
        {"name": "name", "type": "text"},
        {"name": "parent", "type": "relationship", "relationTo": "topics"},
        {"name": "children", "type": "join", "collection": "topics", "on": "parent"},
        {"name": "tagged", "type": "join", "collection": "posts", "on": "tags"},
        {"name": "linked", "type": "join", "collection": "posts", "on": "links.target"}
    ]}});
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/topics",
        Some(&change.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let rust = create(&app, "/api/topics", json!({"name": "rust"})).await;
    create(
        &app,
        "/api/topics",
        json!({"name": "axum", "parent": rust["id"]}),
    )
    .await;
    let link = json!({"relationTo": "topics", "value": rust["id"]});
    create(&app, "/api/posts", json!({"title": "p", "tags": [rust["id"], rust["id"]], "links": [{"target": link}, {"target": link}]})).await;
    let id = rust["id"].as_str().unwrap();
    let res = send(&app, "GET", &format!("/api/topics/{id}"), None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let names: Vec<&str> = res.json["children"]["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["axum"]);
    assert_eq!(
        titles(&res.json["tagged"]),
        ["p"],
        "linked twice through a list, listed once"
    );
    assert_eq!(
        titles(&res.json["linked"]),
        ["p"],
        "through list rows with several targets"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn join_where_can_reach_through_relationships(pool: PgPool) {
    let (app, news, _) = blog(pool).await;
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?joins[posts][where][author.name][equals]=Ann"),
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(titles(&res.json["posts"]), ["c", "b", "a"]);
    let res = send(
        &app,
        "GET",
        &format!("/api/categories/{news}?joins[posts][where][author.name][equals]=Nobody"),
        None,
    )
    .await;
    assert!(titles(&res.json["posts"]).is_empty());
    // A join's schema where can reach through too.
    let change = json!({"schema": {"slug": "categories", "fields": [
        {"name": "name", "type": "text"},
        {"name": "posts", "type": "join", "collection": "posts", "on": "category",
         "where": {"author.name": {"equals": "Ann"}}}
    ]}, "confirm": true});
    let res = send(
        &app,
        "PUT",
        "/api/admin/collections/categories",
        Some(&change.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = send(&app, "GET", &format!("/api/categories/{news}"), None).await;
    assert_eq!(titles(&res.json["posts"]), ["c", "b", "a"]);
}
