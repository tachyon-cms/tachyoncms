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

/// The same error about both copies of a drafts document, each saying its copy.
fn both_copies(res: &Res, path: &str, message: &str) {
    assert_problem(res, 422);
    assert_eq!(
        res.json["errors"],
        json!([{"path": path, "message": format!("the draft: {message}")},
               {"path": path, "message": format!("the published copy: {message}")}])
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
    both_copies(&restore(&admin, "posts", &post).await, "category", MISSING);
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
    both_copies(
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
    both_copies(
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
        "the published copy: must be unique; another document already uses this value",
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
    assert_eq!(
        res.json["errors"].as_array().unwrap().len(),
        1,
        "{}",
        res.json
    );
    assert_eq!(res.json["errors"][0]["path"], "summary", "{}", res.json);
    assert!(
        res.json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .starts_with("the published copy: "),
        "{}",
        res.json
    );
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
    errors(
        &restore(&admin, "posts", &theirs).await,
        "related",
        &format!("the draft: {MISSING}"),
    );

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

async fn kinds(pool: &PgPool, id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT kind FROM versions WHERE document_id = $1 ORDER BY created_at, id")
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_all(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn restore_is_a_write(pool: PgPool) {
    let admin = blog(&pool).await;
    let cat = create(&admin, "categories", json!({"name": "News"})).await;
    let post = create(&admin, "posts", json!({"title": "Hello", "category": cat})).await;
    ok(&admin, "POST", &format!("/api/posts/{post}/publish"), None).await;
    let plain = create(&admin, "posts", json!({"title": "Plain"})).await;
    for id in [&post, &plain] {
        trash(&admin, "posts", id).await;
    }
    // The draft stores its link in a form validation rewrites (an upper-case ID).
    sqlx::query("UPDATE documents SET draft = jsonb_set(draft, '{category}', to_jsonb(upper($2))) WHERE id = $1")
        .bind(post.parse::<Uuid>().unwrap())
        .bind(&cat)
        .execute(&pool)
        .await
        .unwrap();
    let before_kinds = kinds(&pool, &post).await;
    let before = ok(&admin, "GET", "/api/posts/trash", None).await["docs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == post.as_str())
        .unwrap()["updatedAt"]
        .clone();

    let res = restore(&admin, "posts", &post).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["category"]["id"], cat.as_str());
    assert_ne!(res.json["updatedAt"], before, "a restore is a write");
    // Only the copy validation changed is recorded, as its own kind.
    let mut expected = before_kinds;
    expected.push("draft".into());
    assert_eq!(kinds(&pool, &post).await, expected);

    let unchanged = kinds(&pool, &plain).await;
    assert_eq!(restore(&admin, "posts", &plain).await.status, 200);
    assert_eq!(kinds(&pool, &plain).await, unchanged, "nothing changed");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn restore_reports_both_copies(pool: PgPool) {
    let admin = blog(&pool).await;
    let post = create(&admin, "posts", json!({"title": "Hello"})).await;
    ok(&admin, "POST", &format!("/api/posts/{post}/publish"), None).await;
    trash(&admin, "posts", &post).await;
    // The draft breaks a type, and the published copy lacks a field now required.
    sqlx::query("UPDATE documents SET draft = '{\"title\": 5}' WHERE id = $1")
        .bind(post.parse::<Uuid>().unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let res = call(
        &admin,
        "PUT",
        "/api/admin/collections/posts",
        Some(
            json!({"schema": {"slug": "posts", "versions": {"drafts": true},
            "fields": [{"name": "title", "type": "text", "unique": true},
                       {"name": "category", "type": "relationship", "relationTo": "categories"},
                       {"name": "related", "type": "relationship", "relationTo": "posts"},
                       {"name": "summary", "type": "text", "required": true}]}}),
        ),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);

    let res = restore(&admin, "posts", &post).await;
    assert_problem(&res, 422);
    let errors = res.json["errors"].as_array().unwrap();
    let said: Vec<(&str, &str)> = errors
        .iter()
        .map(|e| (e["path"].as_str().unwrap(), e["message"].as_str().unwrap()))
        .collect();
    assert_eq!(said.len(), 2, "{}", res.json);
    assert!(
        said[0].0 == "title" && said[0].1.starts_with("the draft: "),
        "{said:?}"
    );
    assert!(
        said[1].0 == "summary" && said[1].1.starts_with("the published copy: "),
        "{said:?}"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn trash_lists_like_a_list(pool: PgPool) {
    let admin = blog(&pool).await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(
            json!({"slug": "memos", "access": {"read": {"role": "admin"}},
                    "fields": [{"name": "title", "type": "text"}]}),
        ),
    )
    .await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "files", "fields": [
            {"name": "title", "type": "text"},
            {"name": "secret", "type": "text", "access": {"read": {"role": "admin"}}}]})),
    )
    .await;
    // Made after the collections, so its schema cache already holds them.
    let editor = app_as(&pool, "editor@example.com", "editor").await;
    // A collection they can't read at all is a 403, as a plain list says (Ruling T8-3).
    for uri in ["/api/memos", "/api/memos/trash"] {
        assert_problem(&call(&editor, "GET", uri, None).await, 403);
    }
    // A field they can't read can't be filtered or sorted by, as in a plain list.
    for q in ["where[secret][equals]=x", "sort=secret"] {
        let plain = call(&editor, "GET", &format!("/api/files?{q}"), None).await;
        let trash = call(&editor, "GET", &format!("/api/files/trash?{q}"), None).await;
        assert_problem(&trash, plain.status.as_u16());
        assert_eq!(trash.json["detail"], plain.json["detail"], "{q}");
    }

    // `count`, and sorting by when each went (Ruling T8-4); cursors stay out.
    let mut trashed = Vec::new();
    for title in ["a", "b", "c"] {
        let id = create(&admin, "notes", json!({"title": title})).await;
        trash(&admin, "notes", &id).await;
        trashed.push(id);
    }
    let newest: Vec<&str> = trashed.iter().rev().map(String::as_str).collect();
    let oldest: Vec<&str> = trashed.iter().map(String::as_str).collect();
    let res = ok(&admin, "GET", "/api/notes/trash?count=true&limit=2", None).await;
    assert_eq!(res["totalDocs"], 3);
    assert_eq!(ids(&res), newest[..2]);
    assert_eq!(res["hasNextPage"], true);
    let res = ok(&admin, "GET", "/api/notes/trash?sort=-deletedAt", None).await;
    assert_eq!(ids(&res), newest);
    let res = ok(&admin, "GET", "/api/notes/trash?sort=deletedAt", None).await;
    assert_eq!(ids(&res), oldest);
    assert!(res.get("totalDocs").is_none());
    assert_problem(
        &call(&admin, "GET", "/api/notes/trash?after=x", None).await,
        400,
    );
    // Plain lists don't know the column.
    assert_problem(
        &call(&admin, "GET", "/api/notes?sort=deletedAt", None).await,
        400,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn purge_takes_jobs_and_spares_the_living(pool: PgPool) {
    let admin = blog(&pool).await;
    let cat = create(&admin, "categories", json!({"name": "News"})).await;
    let live = create(&admin, "posts", json!({"title": "Live", "category": cat})).await;
    ok(&admin, "POST", &format!("/api/posts/{live}/publish"), None).await;
    let gone = create(&admin, "posts", json!({"title": "Gone", "related": live})).await;
    let when = (time::OffsetDateTime::now_utc() + time::Duration::hours(1))
        .replace_nanosecond(0)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    ok(
        &admin,
        "PUT",
        &format!("/api/posts/{gone}/schedule"),
        Some(json!({"publishAt": when})),
    )
    .await;
    let jobs = "SELECT count(*) FROM jobs WHERE document_id = $1";
    assert_eq!(count(&pool, jobs, &gone).await, 1);
    trash(&admin, "posts", &gone).await;

    let res = call(&admin, "DELETE", &format!("/api/posts/trash/{gone}"), None).await;
    assert_eq!(res.status, 204, "{}", res.json);
    assert_eq!(count(&pool, jobs, &gone).await, 0, "its jobs went with it");
    let rows = "SELECT count(*) FROM relations WHERE from_doc = $1";
    assert_eq!(count(&pool, rows, &gone).await, 0);
    // What it linked lives on, its own links intact, and nothing holds it now.
    assert_eq!(count(&pool, rows, &live).await, 2);
    let res = ok(&admin, "GET", &format!("/api/posts/{live}"), None).await;
    assert_eq!(res["category"]["name"], "News");
    trash(&admin, "posts", &live).await;
}
