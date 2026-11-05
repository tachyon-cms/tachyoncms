mod common;

use axum::Router;
use axum::http::Request;
use common::{Res, admin_app, app_as, assert_problem, send, send_with};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const MISSING: &str = "doesn't match any document in the related collection";

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Res {
    send(app, method, uri, body.map(|b| b.to_string()).as_deref()).await
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let res = call(app, method, uri, body).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

async fn create(app: &Router, slug: &str, body: Value) -> String {
    ok(app, "POST", &format!("/api/{slug}"), Some(body)).await["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn trash(app: &Router, slug: &str, id: &str) {
    let res = call(app, "DELETE", &format!("/api/{slug}/{id}"), None).await;
    assert_eq!(res.status, 204, "{}", res.json);
}

async fn restore(app: &Router, slug: &str, id: &str) -> Res {
    call(
        app,
        "POST",
        &format!("/api/{slug}/trash/{id}/restore"),
        None,
    )
    .await
}

fn ids(list: &Value) -> Vec<&str> {
    list["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect()
}

fn errors(res: &Res, path: &str, message: &str) {
    assert_problem(res, 422);
    assert_eq!(
        res.json["errors"],
        json!([{"path": path, "message": message}])
    );
}

async fn count(pool: &PgPool, sql: &'static str, id: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

/// `posts` with drafts, a unique title and links to `categories` and other posts;
/// `categories` read only while `visible`; `notes` without drafts.
async fn blog(pool: &PgPool) -> Router {
    let admin = admin_app(pool.clone()).await;
    for schema in [
        json!({"slug": "categories", "access": {"read": {"visible": true}},
               "fields": [{"name": "name", "type": "text"},
                          {"name": "visible", "type": "boolean", "default": true}]}),
        json!({"slug": "posts", "versions": {"drafts": true},
               "fields": [{"name": "title", "type": "text", "unique": true},
                          {"name": "category", "type": "relationship", "relationTo": "categories"},
                          {"name": "related", "type": "relationship", "relationTo": "posts"}]}),
        json!({"slug": "notes", "fields": [{"name": "title", "type": "text"}]}),
    ] {
        ok(&admin, "POST", "/api/admin/collections", Some(schema)).await;
    }
    admin
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn trash_round_trip(pool: PgPool) {
    let admin = blog(&pool).await;
    let cat = create(&admin, "categories", json!({"name": "News"})).await;
    let post = create(
        &admin,
        "posts",
        json!({"title": "Hello", "category": cat, "related": null}),
    )
    .await;
    ok(&admin, "POST", &format!("/api/posts/{post}/publish"), None).await;
    let one = format!("/api/posts/{post}");
    ok(&admin, "PATCH", &one, Some(json!({"title": "Hello again"}))).await;

    trash(&admin, "posts", &post).await;
    for uri in ["/api/posts", "/api/posts?draft=true"] {
        assert!(ids(&ok(&admin, "GET", uri, None).await).is_empty());
    }
    assert_problem(&call(&admin, "GET", &one, None).await, 404);
    let listed = ok(&admin, "GET", "/api/posts/trash", None).await;
    assert_eq!(ids(&listed), [post.as_str()]);
    let doc = &listed["docs"][0];
    // The draft, which a delete judges, at depth 0.
    assert_eq!(doc["title"], "Hello again");
    assert_eq!(doc["category"], cat.as_str());
    assert!(doc["deletedAt"].is_string(), "{doc}");
    assert_eq!(listed["page"], 1);
    assert_eq!(listed["hasNextPage"], false);
    // Trashing the category is allowed now: only a trashed post links to it.
    trash(&admin, "categories", &cat).await;
    errors(&restore(&admin, "posts", &post).await, "category", MISSING);
    assert_eq!(
        restore(&admin, "categories", &cat).await.status,
        200,
        "the category comes back first"
    );

    let res = restore(&admin, "posts", &post).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["title"], "Hello again");
    assert_eq!(res.json["category"]["name"], "News", "populated again");
    assert!(res.json.get("deletedAt").is_none());
    let live = ok(&admin, "GET", &one, None).await;
    assert_eq!(live["title"], "Hello");
    assert_eq!(live["category"]["name"], "News");
    assert!(live.get("deletedAt").is_none());
    let draft = ok(&admin, "GET", &format!("{one}?draft=true"), None).await;
    assert_eq!(draft["title"], "Hello again");
    assert!(ids(&ok(&admin, "GET", "/api/posts/trash", None).await).is_empty());
    // Both copies' links are back, so the category is held again.
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM relations WHERE from_doc = $1",
            &post
        )
        .await,
        2
    );
    assert_problem(
        &call(&admin, "DELETE", &format!("/api/categories/{cat}"), None).await,
        409,
    );
    // Restoring what's live is a 404: only the trash restores.
    assert_problem(&restore(&admin, "posts", &post).await, 404);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn restore_refuses_broken_links_and_clashes(pool: PgPool) {
    let admin = blog(&pool).await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;

    // A target that's now in the trash.
    let cat = create(&admin, "categories", json!({"name": "News"})).await;
    let linked = create(&admin, "posts", json!({"title": "Linked", "category": cat})).await;
    ok(
        &admin,
        "POST",
        &format!("/api/posts/{linked}/publish"),
        None,
    )
    .await;
    trash(&admin, "posts", &linked).await;
    trash(&admin, "categories", &cat).await;
    errors(
        &restore(&editor, "posts", &linked).await,
        "category",
        MISSING,
    );

    // One the caller can no longer read is told the same.
    ok(
        &admin,
        "POST",
        &format!("/api/categories/trash/{cat}/restore"),
        None,
    )
    .await;
    ok(
        &admin,
        "PATCH",
        &format!("/api/categories/{cat}"),
        Some(json!({"visible": false})),
    )
    .await;
    errors(
        &restore(&editor, "posts", &linked).await,
        "category",
        MISSING,
    );

    // A unique value a live document took meanwhile: 422 at the field (Ruling T5-1).
    let first = create(&admin, "posts", json!({"title": "Same"})).await;
    ok(&admin, "POST", &format!("/api/posts/{first}/publish"), None).await;
    trash(&admin, "posts", &first).await;
    let second = create(&admin, "posts", json!({"title": "Same"})).await;
    ok(
        &admin,
        "POST",
        &format!("/api/posts/{second}/publish"),
        None,
    )
    .await;
    errors(
        &restore(&editor, "posts", &first).await,
        "title",
        "must be unique; another document already uses this value",
    );
    assert_eq!(
        ids(&ok(&admin, "GET", "/api/posts/trash", None).await).len(),
        2,
        "nothing came back"
    );
    // Once the clash is gone, it restores.
    trash(&admin, "posts", &second).await;
    assert_eq!(restore(&editor, "posts", &first).await.status, 200);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn restore_validates_current_schema(pool: PgPool) {
    let admin = blog(&pool).await;
    let note = create(&admin, "notes", json!({"title": "Hi"})).await;
    let live = create(&admin, "posts", json!({"title": "Live"})).await;
    ok(&admin, "POST", &format!("/api/posts/{live}/publish"), None).await;
    let unpublished = create(&admin, "posts", json!({"title": "Draft only"})).await;
    trash(&admin, "notes", &note).await;
    trash(&admin, "posts", &live).await;
    trash(&admin, "posts", &unpublished).await;

    let summary = json!({"name": "summary", "type": "text", "required": true});
    for (slug, mut schema) in [
        (
            "notes",
            json!({"slug": "notes", "fields": [{"name": "title", "type": "text"}]}),
        ),
        (
            "posts",
            json!({"slug": "posts", "versions": {"drafts": true},
                   "fields": [{"name": "title", "type": "text", "unique": true},
                              {"name": "category", "type": "relationship", "relationTo": "categories"},
                              {"name": "related", "type": "relationship", "relationTo": "posts"}]}),
        ),
    ] {
        schema["fields"]
            .as_array_mut()
            .unwrap()
            .push(summary.clone());
        let res = call(
            &admin,
            "PUT",
            &format!("/api/admin/collections/{slug}"),
            Some(json!({"schema": schema})),
        )
        .await;
        assert_eq!(res.status, 200, "{}", res.json);
    }

    let required = "is required";
    let res = restore(&admin, "notes", &note).await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "summary", "{}", res.json);
    assert!(
        res.json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains(required),
        "{}",
        res.json
    );
    // What's published is held to the schema in full ...
    let res = restore(&admin, "posts", &live).await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "summary", "{}", res.json);
    // ... and a draft as a draft save is.
    let res = restore(&admin, "posts", &unpublished).await;
    assert_eq!(res.status, 200, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn empty_trash_deletes_only_mine(pool: PgPool) {
    let admin = blog(&pool).await;
    let author = app_as(&pool, "author@example.com", "author").await;
    let other = app_as(&pool, "other@example.com", "author").await;

    let target = create(&author, "posts", json!({"title": "Target"})).await;
    let linking = create(
        &author,
        "posts",
        json!({"title": "Linking", "related": target}),
    )
    .await;
    // Someone else's trashed post links to it too, and stays.
    let theirs = create(
        &admin,
        "posts",
        json!({"title": "Theirs", "related": target}),
    )
    .await;
    let others = create(&other, "posts", json!({"title": "Other's"})).await;
    for (app, id) in [
        (&author, &linking),
        (&admin, &theirs),
        (&author, &target),
        (&other, &others),
    ] {
        trash(app, "posts", id).await;
    }

    let mine = ok(&author, "GET", "/api/posts/trash", None).await;
    assert_eq!(
        ids(&mine),
        [target.as_str(), linking.as_str()],
        "newest first"
    );
    let res = ok(&author, "DELETE", "/api/posts/trash", None).await;
    assert_eq!(res, json!({"deleted": 2}));

    for id in [&target, &linking] {
        for sql in [
            "SELECT count(*) FROM documents WHERE id = $1",
            "SELECT count(*) FROM versions WHERE document_id = $1",
        ] {
            assert_eq!(count(&pool, sql, id).await, 0, "{sql}");
        }
    }
    let left = ok(&admin, "GET", "/api/posts/trash", None).await;
    assert_eq!(ids(&left), [others.as_str(), theirs.as_str()]);
    // The link to what's gone went with it, so restoring says so.
    errors(&restore(&admin, "posts", &theirs).await, "related", MISSING);

    // One at a time, too; then it's gone.
    let one = format!("/api/posts/trash/{others}");
    assert_eq!(call(&other, "DELETE", &one, None).await.status, 204);
    assert_problem(&call(&other, "DELETE", &one, None).await, 404);
    assert_eq!(
        ok(&admin, "DELETE", "/api/posts/trash", None).await,
        json!({"deleted": 1})
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn trash_is_guarded(pool: PgPool) {
    let admin = blog(&pool).await;
    let author = app_as(&pool, "author@example.com", "author").await;
    let other = app_as(&pool, "other@example.com", "author").await;
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "settings", "kind": "global",
                    "fields": [{"name": "title", "type": "text"}]})),
    )
    .await;

    let post = create(&author, "posts", json!({"title": "Mine"})).await;
    trash(&author, "posts", &post).await;
    let restore_uri = format!("/api/posts/trash/{post}/restore");
    let purge_uri = format!("/api/posts/trash/{post}");

    for (method, uri) in [
        ("GET", "/api/posts/trash"),
        ("POST", restore_uri.as_str()),
        ("DELETE", purge_uri.as_str()),
        ("DELETE", "/api/posts/trash"),
    ] {
        assert_problem(&call(&viewer, method, uri, None).await, 403);
        assert_problem(&call(&anon, method, uri, None).await, 401);
    }
    // A `:read` key may not; a `:write` key may, as its owner.
    for (scope, status) in [("posts:read", 403), ("posts:write", 200)] {
        let key = ok(
            &admin,
            "POST",
            "/api/auth/keys",
            Some(json!({"name": scope, "scopes": [scope]})),
        )
        .await["key"]
            .as_str()
            .unwrap()
            .to_owned();
        let req = Request::builder()
            .method("GET")
            .uri("/api/posts/trash")
            .header("authorization", format!("Bearer {key}"));
        let res = send_with(&anon, req, None).await;
        assert_eq!(res.status, status, "{scope}: {}", res.json);
        if status == 200 {
            assert_eq!(ids(&res.json), [post.as_str()]);
        }
    }

    // Another Author's trashed post isn't theirs to see, restore or delete.
    assert!(ids(&ok(&other, "GET", "/api/posts/trash", None).await).is_empty());
    assert_problem(&call(&other, "POST", &restore_uri, None).await, 404);
    assert_problem(&call(&other, "DELETE", &purge_uri, None).await, 404);
    assert_eq!(
        ok(&other, "DELETE", "/api/posts/trash", None).await,
        json!({"deleted": 0})
    );

    // No trash for users or globals, and the list takes only its own parameters.
    for uri in ["/api/users/trash", "/api/settings/trash", "/api/nope/trash"] {
        assert_problem(&call(&admin, "GET", uri, None).await, 404);
    }
    assert_problem(
        &call(&author, "GET", "/api/posts/trash?depth=1", None).await,
        400,
    );
    assert_problem(
        &call(&author, "POST", &format!("{restore_uri}?x=1"), None).await,
        400,
    );

    let res = call(&author, "POST", &restore_uri, None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["title"], "Mine");
}
