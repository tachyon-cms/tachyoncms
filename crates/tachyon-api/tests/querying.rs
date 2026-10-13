mod common;

use axum::Router;
use common::{assert_problem, send};
use serde_json::json;
use sqlx::PgPool;

async fn app(pool: PgPool) -> Router {
    let app = common::admin_app(pool).await;
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

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn near_pages_by_number_not_cursor(pool: PgPool) {
    let app = common::admin_app(pool).await;
    let schema = json!({"slug": "places", "fields": [{"name": "at", "type": "point"}]});
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
    let cursor = tachyon_core::query::Cursor {
        value: None,
        id: uuid::Uuid::nil(),
    }
    .encode();
    let res = send(
        &app,
        "GET",
        &format!("/api/places?where[at][near]=0,0&after={cursor}"),
        None,
    )
    .await;
    assert_problem(&res, 400);
    assert_eq!(
        res.json["detail"],
        "results near a point are ordered by distance; page through them with page, not after"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn lists_say_whether_another_page_follows(pool: PgPool) {
    let app = app(pool).await;
    let first = send(&app, "GET", "/api/items?limit=2", None).await.json;
    assert_eq!(first["hasNextPage"], true);
    let last = send(&app, "GET", "/api/items?limit=2&page=2", None)
        .await
        .json;
    assert_eq!(last["hasNextPage"], false);
    let exact = send(&app, "GET", "/api/items?limit=3", None).await.json;
    assert_eq!(exact["hasNextPage"], false);
    assert!(
        exact.get("nextCursor").is_none(),
        "no cursor to an empty page"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn select_shapes_lists_and_single_reads(pool: PgPool) {
    let app = app(pool).await;
    let res = send(
        &app,
        "GET",
        "/api/items?select[title]=true&sort=title",
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let first = &res.json["docs"][0];
    assert_eq!(first["title"], "a");
    let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["id", "title"]);

    let id = first["id"].as_str().unwrap();
    let one = send(
        &app,
        "GET",
        &format!("/api/items/{id}?select[price]=false"),
        None,
    )
    .await;
    assert_eq!(one.status, 200, "{}", one.json);
    assert!(one.json.get("price").is_none());
    assert_eq!(one.json["title"], "a");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn select_skips_unselected_relationships(pool: PgPool) {
    let app = common::admin_app(pool).await;
    for schema in [
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "authors"}
        ]}),
    ] {
        send(
            &app,
            "POST",
            "/api/admin/collections",
            Some(&schema.to_string()),
        )
        .await;
    }
    let ann = send(&app, "POST", "/api/authors", Some(r#"{"name": "Ann"}"#)).await;
    let body = json!({"title": "Hi", "author": ann.json["id"]}).to_string();
    send(&app, "POST", "/api/posts", Some(&body)).await;

    let res = send(&app, "GET", "/api/posts?select[title]=true&depth=2", None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert!(res.json["docs"][0].get("author").is_none());
    let res = send(&app, "GET", "/api/posts?select[author]=true", None).await;
    assert_eq!(
        res.json["docs"][0]["author"]["name"], "Ann",
        "a selected link still populates"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn select_on_globals(pool: PgPool) {
    let app = common::admin_app(pool).await;
    let schema = json!({"slug": "settings", "kind": "global", "fields": [
        {"name": "siteName", "type": "text"}, {"name": "tagline", "type": "text"}
    ]});
    send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    send(
        &app,
        "PATCH",
        "/api/globals/settings",
        Some(r#"{"siteName": "T", "tagline": "x"}"#),
    )
    .await;
    let res = send(
        &app,
        "GET",
        "/api/globals/settings?select[tagline]=true",
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["tagline"], "x");
    assert!(res.json.get("siteName").is_none());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn bad_select_is_a_400_naming_the_parameter(pool: PgPool) {
    let app = app(pool).await;
    for (uri, detail) in [
        (
            "/api/items?select[nope]=true",
            "select[nope]: unknown field",
        ),
        (
            "/api/items?select[title]=true&select[price]=false",
            "select: use either true or false throughout, not both",
        ),
    ] {
        let res = send(&app, "GET", uri, None).await;
        assert_problem(&res, 400);
        assert_eq!(res.json["detail"], detail);
    }
}

async fn linked(pool: PgPool) -> Router {
    let app = common::admin_app(pool).await;
    for schema in [
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "authorName", "type": "text", "virtual": "author.name"},
            {"name": "links", "type": "array", "fields": [{"name": "url", "type": "url"}]}
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
    for (title, author, url) in [("a", &ann, "https://a.test"), ("b", &bo, "https://b.test")] {
        let body = json!({"title": title, "author": author.json["id"], "links": [{"url": url}]});
        send(&app, "POST", "/api/posts", Some(&body.to_string())).await;
    }
    app
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn where_reaches_through_relationships_and_into_lists(pool: PgPool) {
    let app = linked(pool).await;
    let res = send(
        &app,
        "GET",
        "/api/posts?where[author.name][equals]=Ann",
        None,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(titles(&res.json), ["a"]);
    let res = send(
        &app,
        "GET",
        "/api/posts?where[links.url][like]=b.test",
        None,
    )
    .await;
    assert_eq!(titles(&res.json), ["b"]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn virtual_fields_can_be_filtered(pool: PgPool) {
    let app = linked(pool).await;
    let res = send(&app, "GET", "/api/posts?where[authorName][equals]=Bo", None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(titles(&res.json), ["b"]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn bad_cross_paths_are_400s(pool: PgPool) {
    let app = linked(pool).await;
    for (uri, detail) in [
        (
            "/api/posts?where[author.nope][equals]=x",
            "where[author.nope][equals]: `author.nope`: authors has no field `nope`",
        ),
        (
            "/api/posts?sort=author.name",
            "sort: `author.name` goes through a relationship and can't be sorted yet",
        ),
        (
            "/api/posts?where[author.id][like]=a",
            "where[author.id][like]: like can't be used on this field",
        ),
    ] {
        let res = send(&app, "GET", uri, None).await;
        assert_problem(&res, 400);
        assert_eq!(res.json["detail"], detail);
    }
}
