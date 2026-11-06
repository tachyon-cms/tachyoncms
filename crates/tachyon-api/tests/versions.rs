mod common;

use axum::Router;
use axum::http::Request;
use common::{Res, admin_app, app_as, assert_problem, send, send_with};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Res {
    send(app, method, uri, body.map(|b| b.to_string()).as_deref()).await
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let res = call(app, method, uri, body).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

fn id_of(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

async fn with_key(app: &Router, key: &str, method: &str, uri: &str) -> Res {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"));
    send_with(app, req, None).await
}

async fn key(app: &Router, scopes: &[&str]) -> String {
    ok(
        app,
        "POST",
        "/api/auth/keys",
        Some(json!({"name": "k", "scopes": scopes})),
    )
    .await["key"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn kinds(pool: &PgPool, id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT kind FROM versions WHERE document_id = $1 ORDER BY id")
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_all(pool)
        .await
        .unwrap()
}

/// `authors` and `posts`, both with drafts; posts keep 3 versions, and hold a `note` only
/// Editors (and Admins) may read or change.
async fn blog(pool: &PgPool) -> Router {
    let admin = admin_app(pool.clone()).await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "authors", "versions": {"drafts": true},
                    "fields": [{"name": "name", "type": "text"}]})),
    )
    .await;
    let ed = json!({"read": {"role": "editor"}, "update": {"role": "editor"}});
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(
            json!({"slug": "posts", "versions": {"drafts": true, "maxVersions": 3},
            "fields": [
                {"name": "title", "type": "text", "required": true},
                {"name": "slug", "type": "slug"},
                {"name": "author", "type": "relationship", "relationTo": "authors"},
                {"name": "note", "type": "text", "access": ed},
                {"name": "links", "type": "array", "fields": [{"name": "url", "type": "text"}]}
            ]}),
        ),
    )
    .await;
    admin
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn publish_round_trip(pool: PgPool) {
    let admin = blog(&pool).await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let id = id_of(&ok(&admin, "POST", "/api/posts", Some(json!({"title": "One"}))).await);
    let one = format!("/api/posts/{id}");
    assert_problem(&call(&anon, "GET", &one, None).await, 404);

    let published = ok(&admin, "POST", &format!("{one}/publish"), None).await;
    assert_eq!(published["title"], "One");
    assert_eq!(published["status"], "published");
    assert!(published["publishedAt"].is_string(), "{published}");
    assert_eq!(ok(&anon, "GET", &one, None).await["title"], "One");
    assert_eq!(
        ok(&anon, "GET", "/api/posts", None).await["docs"][0]["title"],
        "One"
    );

    // Editing the draft leaves what's live alone, until it's published again.
    ok(&admin, "PATCH", &one, Some(json!({"title": "Two"}))).await;
    assert_eq!(ok(&anon, "GET", &one, None).await["title"], "One");
    ok(&admin, "POST", &format!("{one}/publish"), None).await;
    assert_eq!(ok(&anon, "GET", &one, None).await["title"], "Two");
    // Each publish records a version; three are kept.
    assert_eq!(kinds(&pool, &id).await, ["published", "draft", "published"]);

    // Unpublished: gone from the site, the draft as it was.
    let draft = ok(&admin, "POST", &format!("{one}/unpublish"), None).await;
    assert_eq!(draft["title"], "Two");
    assert_eq!(draft["status"], "draft");
    assert!(draft.get("publishedAt").is_none(), "{draft}");
    assert_problem(&call(&anon, "GET", &one, None).await, 404);
    assert_eq!(
        ok(&anon, "GET", "/api/posts", None).await["docs"],
        json!([])
    );
    let draft = ok(&admin, "GET", &format!("{one}?draft=true"), None).await;
    assert_eq!(draft["title"], "Two");

    // Nothing to ask of either but the document.
    let res = call(&admin, "POST", &format!("{one}/publish?draft=true"), None).await;
    assert_problem(&res, 400);
    let res = call(
        &admin,
        "POST",
        &format!("/api/posts/{}/publish", Uuid::now_v7()),
        None,
    )
    .await;
    assert_problem(&res, 404);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn publish_validates_and_checks_unique(pool: PgPool) {
    let admin = blog(&pool).await;
    // Saved without its required title, as a draft may be.
    let id = id_of(&ok(&admin, "POST", "/api/posts", Some(json!({"slug": "a"}))).await);
    let res = call(&admin, "POST", &format!("/api/posts/{id}/publish"), None).await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "title", "{}", res.json);
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    assert_problem(
        &call(&anon, "GET", &format!("/api/posts/{id}"), None).await,
        404,
    );

    // Drafts may share a slug; published copies may not.
    ok(
        &admin,
        "PATCH",
        &format!("/api/posts/{id}"),
        Some(json!({"title": "A"})),
    )
    .await;
    ok(&admin, "POST", &format!("/api/posts/{id}/publish"), None).await;
    let twin = id_of(
        &ok(
            &admin,
            "POST",
            "/api/posts",
            Some(json!({"title": "B", "slug": "a"})),
        )
        .await,
    );
    let res = call(&admin, "POST", &format!("/api/posts/{twin}/publish"), None).await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0],
        json!({"path": "slug", "message": "must be unique; another document already uses this value"})
    );
    assert_eq!(kinds(&pool, &twin).await, ["draft"], "nothing was recorded");

    // A draft may link to a draft; what's published may link only to what's published and
    // readable. A hidden author and a missing one read the same.
    let draft_only = id_of(&ok(&admin, "POST", "/api/authors", Some(json!({"name": "D"}))).await);
    let post = ok(
        &admin,
        "POST",
        "/api/posts",
        Some(json!({"title": "C", "slug": "c", "author": draft_only})),
    )
    .await;
    let publish = format!("/api/posts/{}/publish", id_of(&post));
    let res = call(&admin, "POST", &publish, None).await;
    assert_problem(&res, 422);
    let missing = res.json["errors"].clone();
    assert_eq!(missing[0]["path"], "author", "{}", res.json);
    // Published, but under a read rule the Editor fails; the Admin may publish it.
    ok(
        &admin,
        "POST",
        &format!("/api/authors/{draft_only}/publish"),
        None,
    )
    .await;
    let mut schema =
        ok(&admin, "GET", "/api/admin/collections/authors", None).await["schema"].clone();
    schema["access"] = json!({"read": {"name": "visible"}});
    let change = json!({"schema": schema});
    ok(
        &admin,
        "PUT",
        "/api/admin/collections/authors",
        Some(change),
    )
    .await;
    // Signed in after the change, so its schemas have it.
    let editor = app_as(&pool, "editor@example.com", "editor").await;
    let res = call(&editor, "POST", &publish, None).await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"], missing, "hidden reads as missing");
    let published = ok(&admin, "POST", &publish, None).await;
    assert_eq!(published["author"]["name"], "D", "{published}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn author_cannot_publish(pool: PgPool) {
    let admin = blog(&pool).await;
    let author = app_as(&pool, "author@example.com", "author").await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let id = id_of(
        &ok(
            &author,
            "POST",
            "/api/posts",
            Some(json!({"title": "Mine"})),
        )
        .await,
    );
    let publish = format!("/api/posts/{id}/publish");
    assert_problem(&call(&author, "POST", &publish, None).await, 403);
    assert_problem(
        &call(&author, "POST", &format!("/api/posts/{id}/unpublish"), None).await,
        403,
    );
    assert_problem(&call(&anon, "POST", &publish, None).await, 401);

    // `:write` doesn't publish; `:publish` alone does, and reads nothing back.
    let write = key(&editor, &["posts:write"]).await;
    assert_problem(&with_key(&anon, &write, "POST", &publish).await, 403);
    let publisher = key(&editor, &["posts:publish"]).await;
    let res = with_key(&anon, &publisher, "POST", &publish).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json, json!({"id": id}));
    assert_eq!(
        ok(&anon, "GET", &format!("/api/posts/{id}"), None).await["title"],
        "Mine"
    );
    // Only publishing: no restore, no reading versions.
    let vid = ok(&admin, "GET", &format!("/api/posts/{id}/versions"), None).await["docs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let restore = format!("/api/posts/{id}/versions/{vid}/restore");
    assert_problem(&with_key(&anon, &publisher, "POST", &restore).await, 403);
    let versions = format!("/api/posts/{id}/versions");
    assert_problem(&with_key(&anon, &publisher, "GET", &versions).await, 403);
    let res = with_key(
        &anon,
        &publisher,
        "POST",
        &format!("/api/posts/{id}/unpublish"),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_problem(
        &call(&anon, "GET", &format!("/api/posts/{id}"), None).await,
        404,
    );
    // Another collection's key publishes nothing here.
    let other = key(&editor, &["authors:publish"]).await;
    assert_problem(&with_key(&anon, &other, "POST", &publish).await, 403);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn versions_list_read_restore(pool: PgPool) {
    let admin = blog(&pool).await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let id = id_of(&ok(&admin, "POST", "/api/posts", Some(json!({"title": "v1"}))).await);
    let one = format!("/api/posts/{id}");
    ok(&admin, "POST", &format!("{one}/publish"), None).await;
    ok(
        &admin,
        "PATCH",
        &one,
        Some(json!({"title": "v2", "links": [{"url": "x"}]})),
    )
    .await;

    let list = ok(&admin, "GET", &format!("{one}/versions"), None).await;
    let docs = list["docs"].as_array().unwrap();
    assert_eq!(docs.len(), 3, "{list}");
    assert_eq!(list["page"], 1);
    assert_eq!(list["hasNextPage"], false);
    assert_eq!(docs[0]["kind"], "draft", "newest first");
    assert!(docs[0].get("data").is_none(), "{list}");
    assert!(docs[0]["createdAt"].is_string() && docs[0]["createdBy"].is_string());
    let first = docs[2]["id"].as_str().unwrap().to_owned();
    let page = ok(
        &admin,
        "GET",
        &format!("{one}/versions?limit=2&page=2"),
        None,
    )
    .await;
    assert_eq!(page["docs"].as_array().unwrap().len(), 1, "{page}");
    assert_problem(
        &call(&admin, "GET", &format!("{one}/versions?sort=id"), None).await,
        400,
    );

    let v = ok(&admin, "GET", &format!("{one}/versions/{first}"), None).await;
    assert_eq!(v["data"]["title"], "v1");
    assert_eq!(v["kind"], "draft");
    assert_problem(
        &call(
            &admin,
            "GET",
            &format!("{one}/versions/{}", Uuid::now_v7()),
            None,
        )
        .await,
        404,
    );

    // Restoring writes the draft, records a version, and leaves what's live alone. What
    // the version lacks, the draft loses.
    let restored = ok(
        &admin,
        "POST",
        &format!("{one}/versions/{first}/restore"),
        None,
    )
    .await;
    assert_eq!(restored["title"], "v1");
    assert!(restored.get("links").is_none(), "{restored}");
    assert_eq!(ok(&anon, "GET", &one, None).await["title"], "v1");
    let draft = ok(&admin, "GET", &format!("{one}?draft=true"), None).await;
    assert_eq!(draft["title"], "v1");
    // Three kept, the newest the restore's.
    let list = ok(&admin, "GET", &format!("{one}/versions"), None).await;
    let docs = list["docs"].as_array().unwrap();
    assert_eq!(docs.len(), 3, "maxVersions: 3 keeps 3: {list}");
    assert_eq!(docs[0]["kind"], "draft");
    assert!(
        docs.iter().all(|d| d["id"] != first.as_str()),
        "the oldest went"
    );

    // A diff between any two of a version, the draft and what's published.
    let v2 = docs[1]["id"].as_str().unwrap();
    let diff = ok(
        &admin,
        "GET",
        &format!("{one}/diff?from={v2}&to=draft"),
        None,
    )
    .await;
    assert_eq!(diff["from"], v2);
    assert_eq!(diff["to"], "draft");
    let changes = diff["changes"].as_array().unwrap();
    assert_eq!(
        changes[0],
        json!({"path": "title", "op": "changed", "before": "v2",
                                  "after": "v1", "rowId": null})
    );
    assert_eq!(changes[1]["path"], "links.0");
    assert_eq!(changes[1]["op"], "removed");
    let same = ok(
        &admin,
        "GET",
        &format!("{one}/diff?from=published&to=draft"),
        None,
    )
    .await;
    assert_eq!(same["changes"], json!([]));
    assert_problem(
        &call(&admin, "GET", &format!("{one}/diff?from=draft"), None).await,
        400,
    );

    // A document in the trash has no history to show.
    ok(&admin, "DELETE", &one, None).await;
    for uri in [
        format!("{one}/versions"),
        format!("{one}/versions/{v2}"),
        format!("{one}/diff?from={v2}&to=draft"),
    ] {
        assert_problem(&call(&admin, "GET", &uri, None).await, 404);
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn history_respects_field_rules(pool: PgPool) {
    let admin = blog(&pool).await;
    let author = app_as(&pool, "author@example.com", "author").await;
    let id = id_of(&ok(&author, "POST", "/api/posts", Some(json!({"title": "t1"}))).await);
    let one = format!("/api/posts/{id}");
    ok(&admin, "PATCH", &one, Some(json!({"note": "secret one"}))).await;
    ok(
        &admin,
        "PATCH",
        &one,
        Some(json!({"title": "t2", "note": "secret two"})),
    )
    .await;
    let list = ok(&author, "GET", &format!("{one}/versions"), None).await;
    assert!(!list.to_string().contains("secret"), "{list}");
    let docs = list["docs"].as_array().unwrap();
    let (newest, oldest) = (
        docs[0]["id"].as_str().unwrap(),
        docs[2]["id"].as_str().unwrap(),
    );
    let middle = docs[1]["id"].as_str().unwrap();

    let v = ok(&author, "GET", &format!("{one}/versions/{middle}"), None).await;
    assert_eq!(v["data"]["title"], "t1");
    assert!(v["data"].get("note").is_none(), "{v}");
    let v = ok(&admin, "GET", &format!("{one}/versions/{middle}"), None).await;
    assert_eq!(v["data"]["note"], "secret one", "an Admin sees it");

    // Not even a path: the note changed, and only the title shows.
    let diff = ok(
        &author,
        "GET",
        &format!("{one}/diff?from={middle}&to={newest}"),
        None,
    )
    .await;
    assert_eq!(diff["changes"].as_array().unwrap().len(), 1, "{diff}");
    assert_eq!(diff["changes"][0]["path"], "title");
    let diff = ok(
        &admin,
        "GET",
        &format!("{one}/diff?from={middle}&to={newest}"),
        None,
    )
    .await;
    assert_eq!(diff["changes"].as_array().unwrap().len(), 2, "{diff}");

    // The Author restores what they may write; the note they can't touch stays.
    let restored = ok(
        &author,
        "POST",
        &format!("{one}/versions/{oldest}/restore"),
        None,
    )
    .await;
    assert_eq!(restored["title"], "t1");
    assert!(!restored.to_string().contains("secret"), "{restored}");
    let draft = ok(&admin, "GET", &format!("{one}?draft=true"), None).await;
    assert_eq!(draft["note"], "secret two", "{draft}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn history_hides_unreadable_documents(pool: PgPool) {
    let admin = blog(&pool).await;
    let author = app_as(&pool, "author@example.com", "author").await;
    let other = app_as(&pool, "other@example.com", "author").await;
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let id = id_of(
        &ok(
            &author,
            "POST",
            "/api/posts",
            Some(json!({"title": "Mine"})),
        )
        .await,
    );
    ok(&admin, "POST", &format!("/api/posts/{id}/publish"), None).await;
    let vid = ok(&author, "GET", &format!("/api/posts/{id}/versions"), None).await["docs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let random = Uuid::now_v7();
    let probes = |doc: String| {
        vec![
            ("GET", format!("/api/posts/{doc}/versions")),
            ("GET", format!("/api/posts/{doc}/versions/{vid}")),
            ("POST", format!("/api/posts/{doc}/versions/{vid}/restore")),
            (
                "GET",
                format!("/api/posts/{doc}/diff?from={vid}&to=published"),
            ),
            (
                "GET",
                format!("/api/posts/{doc}/diff?from=draft&to=published"),
            ),
        ]
    };
    // Each answer is the one a document that isn't there gets: a 404, except where the
    // caller may restore nothing in `posts` at all (a Viewer's 403).
    for (app, who) in [(&other, "another Author"), (&viewer, "a Viewer")] {
        let probed = probes(id.clone())
            .into_iter()
            .zip(probes(random.to_string()));
        for ((method, uri), (_, missing)) in probed {
            let res = call(app, method, &uri, None).await;
            let gone = call(app, method, &missing, None).await;
            let want = match (who, method) {
                ("a Viewer", "POST") => 403,
                _ => 404,
            };
            assert_eq!(res.status, want, "{who}: {method} {uri}: {}", res.json);
            assert_eq!(gone.status, want, "{who}: {missing}");
            assert_eq!(res.json["detail"], gone.json["detail"]);
        }
    }
    // The public may diff what's published only.
    let uri = format!("/api/posts/{id}/diff?from=published&to=published");
    assert_eq!(ok(&anon, "GET", &uri, None).await["changes"], json!([]));
    for uri in [
        format!("/api/posts/{id}/diff?from=published&to=draft"),
        format!("/api/posts/{id}/diff?from={vid}&to=published"),
    ] {
        assert_problem(&call(&anon, "GET", &uri, None).await, 404);
    }
    // A `:read` key reads only what's published, too.
    let read = key(&admin, &["posts:read"]).await;
    assert_problem(
        &with_key(&anon, &read, "GET", &format!("/api/posts/{id}/versions")).await,
        403,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unpublishing_keeps_links_counted(pool: PgPool) {
    let admin = blog(&pool).await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let author = id_of(&ok(&admin, "POST", "/api/authors", Some(json!({"name": "A"}))).await);
    ok(
        &admin,
        "POST",
        &format!("/api/authors/{author}/publish"),
        None,
    )
    .await;
    let post = id_of(
        &ok(
            &admin,
            "POST",
            "/api/posts",
            Some(json!({"title": "P", "author": author})),
        )
        .await,
    );
    ok(&admin, "POST", &format!("/api/posts/{post}/publish"), None).await;
    let refs = format!("/api/authors/{author}/references");

    // The author goes off the site; the live post still links to it, which the public
    // sees as a bare ID, and still keeps it from being deleted.
    ok(
        &admin,
        "POST",
        &format!("/api/authors/{author}/unpublish"),
        None,
    )
    .await;
    let live = ok(&anon, "GET", &format!("/api/posts/{post}"), None).await;
    assert_eq!(live["author"], author.as_str(), "{live}");
    let found = ok(&admin, "GET", &refs, None).await["docs"].clone();
    assert_eq!(
        found.as_array().unwrap().len(),
        2,
        "both copies link: {found}"
    );
    assert_problem(
        &call(&admin, "DELETE", &format!("/api/authors/{author}"), None).await,
        409,
    );
    // Taking the post down drops its live link; its draft still holds one.
    ok(
        &admin,
        "POST",
        &format!("/api/posts/{post}/unpublish"),
        None,
    )
    .await;
    let found = ok(&admin, "GET", &refs, None).await["docs"].clone();
    assert_eq!(
        found,
        json!([{"collection": "posts", "id": post, "path": "author", "copy": "draft"}])
    );
    assert_problem(
        &call(&admin, "DELETE", &format!("/api/authors/{author}"), None).await,
        409,
    );
    ok(
        &admin,
        "PATCH",
        &format!("/api/posts/{post}"),
        Some(json!({"author": null})),
    )
    .await;
    assert_eq!(ok(&admin, "GET", &refs, None).await["docs"], json!([]));
    let res = call(&admin, "DELETE", &format!("/api/authors/{author}"), None).await;
    assert_eq!(res.status, 204, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn global_history(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(
            json!({"slug": "site", "kind": "global", "versions": {"drafts": true},
                    "fields": [{"name": "name", "type": "text", "required": true}]}),
        ),
    )
    .await;
    let author = app_as(&pool, "author@example.com", "author").await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    ok(
        &admin,
        "PATCH",
        "/api/globals/site",
        Some(json!({"name": "One"})),
    )
    .await;
    assert!(
        ok(&anon, "GET", "/api/globals/site", None)
            .await
            .get("name")
            .is_none()
    );
    let published = ok(&admin, "POST", "/api/globals/site/publish", None).await;
    assert_eq!(published["name"], "One");
    assert_eq!(
        ok(&anon, "GET", "/api/globals/site", None).await["name"],
        "One"
    );
    ok(
        &admin,
        "PATCH",
        "/api/globals/site",
        Some(json!({"name": "Two"})),
    )
    .await;

    let list = ok(&admin, "GET", "/api/globals/site/versions", None).await;
    let docs = list["docs"].as_array().unwrap();
    assert_eq!(docs.len(), 3, "{list}");
    let first = docs[2]["id"].as_str().unwrap();
    let v = ok(
        &admin,
        "GET",
        &format!("/api/globals/site/versions/{first}"),
        None,
    )
    .await;
    assert_eq!(v["data"]["name"], "One");
    let diff = ok(
        &admin,
        "GET",
        "/api/globals/site/diff?from=published&to=draft",
        None,
    )
    .await;
    assert_eq!(diff["changes"][0]["after"], "Two");
    let restored = ok(
        &admin,
        "POST",
        &format!("/api/globals/site/versions/{first}/restore"),
        None,
    )
    .await;
    assert_eq!(restored["name"], "One");
    // Nobody created a global, so an own-only right reaches none of its history.
    // Refused as a global is, not hidden (Ruling T5-5).
    for uri in [
        "/api/globals/site/versions".to_owned(),
        format!("/api/globals/site/versions/{first}"),
        format!("/api/globals/site/diff?from={first}&to=published"),
    ] {
        assert_problem(&call(&author, "GET", &uri, None).await, 403);
    }
    assert_problem(
        &call(&author, "POST", "/api/globals/site/publish", None).await,
        403,
    );
    let draft = ok(&admin, "POST", "/api/globals/site/unpublish", None).await;
    assert_eq!(draft["name"], "One");
    assert!(
        ok(&anon, "GET", "/api/globals/site", None)
            .await
            .get("name")
            .is_none()
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn draft_versions_need_read_drafts(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "pages", "versions": {"drafts": true},
                    "access": {"readVersions": true, "readDrafts": false},
                    "fields": [{"name": "title", "type": "text"}]})),
    )
    .await;
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    let id = id_of(
        &ok(
            &admin,
            "POST",
            "/api/pages",
            Some(json!({"title": "Secret"})),
        )
        .await,
    );
    let one = format!("/api/pages/{id}");
    ok(&admin, "PATCH", &one, Some(json!({"title": "Live"}))).await;
    ok(&admin, "POST", &format!("{one}/publish"), None).await;
    ok(&admin, "PATCH", &one, Some(json!({"title": "Next"}))).await;
    let all = ok(&admin, "GET", &format!("{one}/versions"), None).await["docs"].clone();
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 4);
    let draft = all[0]["id"].as_str().unwrap();
    let published = all[1]["id"].as_str().unwrap();

    // Only what was published, and pages that count only that.
    let list = ok(&viewer, "GET", &format!("{one}/versions"), None).await;
    assert_eq!(list["docs"].as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list["docs"][0]["id"], published);
    let page = ok(&viewer, "GET", &format!("{one}/versions?limit=1"), None).await;
    assert_eq!(page["hasNextPage"], false, "{page}");
    let v = ok(&viewer, "GET", &format!("{one}/versions/{published}"), None).await;
    assert_eq!(v["data"]["title"], "Live");
    // A draft's version reads as one that isn't there.
    let random = Uuid::now_v7();
    for vid in [draft.to_owned(), random.to_string()] {
        for uri in [
            format!("{one}/versions/{vid}"),
            format!("{one}/diff?from={vid}&to=published"),
        ] {
            let res = call(&viewer, "GET", &uri, None).await;
            assert_problem(&res, 404);
            assert!(!res.json.to_string().contains("Next"), "{}", res.json);
        }
    }
    // A page never published is one the viewer can't read, so its history isn't there
    // either: no empty list to say it exists.
    let unseen = id_of(
        &ok(
            &admin,
            "POST",
            "/api/pages",
            Some(json!({"title": "Draft"})),
        )
        .await,
    );
    let unseen = format!("/api/pages/{unseen}");
    ok(&admin, "PATCH", &unseen, Some(json!({"title": "Again"}))).await;
    assert_problem(&call(&viewer, "GET", &unseen, None).await, 404);
    assert_problem(
        &call(&viewer, "GET", &format!("{unseen}/versions"), None).await,
        404,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn history_follows_the_copy_the_caller_reads(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "posts", "versions": {"drafts": true},
        "access": {"read": {"visibility": "public"}, "readVersions": true,
                   "readDrafts": {"role": "editor"}},
        "fields": [
            {"name": "title", "type": "text"},
            {"name": "visibility", "type": "select", "options": ["public", "private"]}
        ]})),
    )
    .await;
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    // Published as `from`, then a draft that turns it to `to`.
    let post = async |from: &str, to: &str| {
        let id = id_of(
            &ok(
                &admin,
                "POST",
                "/api/posts",
                Some(json!({"title": "T", "visibility": from})),
            )
            .await,
        );
        let one = format!("/api/posts/{id}");
        ok(&admin, "POST", &format!("{one}/publish"), None).await;
        ok(&admin, "PATCH", &one, Some(json!({"visibility": to}))).await;
        let all = ok(&admin, "GET", &format!("{one}/versions"), None).await;
        let published = all["docs"][1]["id"].as_str().unwrap().to_owned();
        (one, published)
    };
    // What's live is private and the draft public: the viewer can't read the post, nor its
    // history.
    let (hidden, vid) = post("private", "public").await;
    assert_problem(&call(&viewer, "GET", &hidden, None).await, 404);
    for uri in [
        format!("{hidden}/versions"),
        format!("{hidden}/versions/{vid}"),
        format!("{hidden}/diff?from={vid}&to={vid}"),
    ] {
        assert_problem(&call(&viewer, "GET", &uri, None).await, 404);
    }
    // The other way about: the post reads, and so does what was published of it.
    let (shown, vid) = post("public", "private").await;
    ok(&viewer, "GET", &shown, None).await;
    let list = ok(&viewer, "GET", &format!("{shown}/versions"), None).await;
    assert_eq!(list["docs"].as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list["docs"][0]["id"], vid);
    let v = ok(&viewer, "GET", &format!("{shown}/versions/{vid}"), None).await;
    assert_eq!(v["data"]["visibility"], "public");
    let diff = ok(
        &viewer,
        "GET",
        &format!("{shown}/diff?from={vid}&to={vid}"),
        None,
    )
    .await;
    assert_eq!(diff["changes"], json!([]), "{diff}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unpublishing_what_isnt_published_is_refused(pool: PgPool) {
    let admin = blog(&pool).await;
    let id = id_of(&ok(&admin, "POST", "/api/posts", Some(json!({"title": "New"}))).await);
    let one = format!("/api/posts/{id}");
    let before = ok(&admin, "GET", &format!("{one}?draft=true"), None).await;
    let res = call(&admin, "POST", &format!("{one}/unpublish"), None).await;
    assert_problem(&res, 409);
    assert_eq!(res.json["detail"], format!("`{id}` isn't published"));
    let after = ok(&admin, "GET", &format!("{one}?draft=true"), None).await;
    assert_eq!(
        before["updatedAt"], after["updatedAt"],
        "nothing was written"
    );
    // Once published and taken down, a second time is refused too.
    ok(&admin, "POST", &format!("{one}/publish"), None).await;
    ok(&admin, "POST", &format!("{one}/unpublish"), None).await;
    assert_problem(
        &call(&admin, "POST", &format!("{one}/unpublish"), None).await,
        409,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn restoring_a_draft_version_needs_read_drafts(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    // Editors may change pages and read their history, but no draft matches `readDrafts`.
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(json!({"slug": "pages", "versions": {"drafts": true},
                    "access": {"readVersions": true, "readDrafts": {"title": "never"}},
                    "fields": [{"name": "title", "type": "text"}]})),
    )
    .await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;
    let id = id_of(
        &ok(
            &admin,
            "POST",
            "/api/pages",
            Some(json!({"title": "Secret"})),
        )
        .await,
    );
    let one = format!("/api/pages/{id}");
    ok(&admin, "PATCH", &one, Some(json!({"title": "Live"}))).await;
    ok(&admin, "POST", &format!("{one}/publish"), None).await;
    let all = ok(&admin, "GET", &format!("{one}/versions"), None).await["docs"].clone();
    let draft = all[2]["id"].as_str().unwrap().to_owned();
    assert_eq!(all[2]["kind"], "draft");

    let random = Uuid::now_v7().to_string();
    let mut answers = Vec::new();
    for vid in [draft, random] {
        let res = call(
            &editor,
            "POST",
            &format!("{one}/versions/{vid}/restore"),
            None,
        )
        .await;
        assert_problem(&res, 404);
        assert!(!res.json.to_string().contains("Secret"), "{}", res.json);
        answers.push((res.json["title"].clone(), res.json.get("detail").cloned()));
    }
    assert_eq!(
        answers[0], answers[1],
        "a draft's version reads as a random one"
    );
    let draft = ok(&admin, "GET", &format!("{one}?draft=true"), None).await;
    assert_eq!(draft["title"], "Live", "nothing was restored");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_global_refuses_history_with_a_403(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(
            json!({"slug": "site", "kind": "global", "versions": {"drafts": true},
                    "access": {"readVersions": {"name": "Open"}},
                    "fields": [{"name": "name", "type": "text"}]}),
        ),
    )
    .await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;
    let uri = "/api/globals/site/versions";
    // Never saved, so there's nothing for the rule to match: refused, as when it fails.
    assert_problem(&call(&editor, "GET", uri, None).await, 403);
    ok(
        &admin,
        "PATCH",
        "/api/globals/site",
        Some(json!({"name": "Shut"})),
    )
    .await;
    assert_problem(&call(&editor, "GET", uri, None).await, 403);
    ok(
        &admin,
        "PATCH",
        "/api/globals/site",
        Some(json!({"name": "Open"})),
    )
    .await;
    ok(&editor, "GET", uri, None).await;
}
