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

async fn collection(admin: &Router, schema: Value) {
    ok(admin, "POST", "/api/admin/collections", Some(schema)).await;
}

async fn with_key(app: &Router, key: &str, uri: &str) -> Res {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {key}"));
    send_with(app, req, None).await
}

/// Publishes what's in the draft, as publishing will: copy and links.
async fn publish(pool: &PgPool, id: &str) {
    let id: Uuid = id.parse().unwrap();
    for sql in [
        "UPDATE documents SET published = draft, status = 'published', published_at = now()
         WHERE id = $1",
        "DELETE FROM relations WHERE from_doc = $1 AND copy = 'published'",
        "INSERT INTO relations (tenant_id, from_doc, copy, field_path, to_doc)
         SELECT tenant_id, from_doc, 'published', field_path, to_doc FROM relations
         WHERE from_doc = $1 AND copy = 'draft'",
    ] {
        sqlx::query(sql).bind(id).execute(pool).await.unwrap();
    }
}

async fn versions(pool: &PgPool, id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT kind FROM versions WHERE document_id = $1 ORDER BY id")
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_all(pool)
        .await
        .unwrap()
}

/// `posts` with drafts (title required), and `notes` without.
async fn blog(pool: &PgPool) -> Router {
    let admin = admin_app(pool.clone()).await;
    collection(
        &admin,
        json!({"slug": "posts", "versions": {"drafts": {"autosave": {"interval": 2000}}},
               "fields": [{"name": "title", "type": "text", "required": true}]}),
    )
    .await;
    collection(
        &admin,
        json!({"slug": "notes", "fields": [{"name": "title", "type": "text", "required": true}]}),
    )
    .await;
    admin
}

fn names_draft(res: &Res) {
    let detail = res.json["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("`draft`"), "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn drafts_are_private_until_published(pool: PgPool) {
    let admin = blog(&pool).await;
    let author = app_as(&pool, "author@example.com", "author").await;
    let other = app_as(&pool, "other@example.com", "author").await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();

    let created = ok(
        &author,
        "POST",
        "/api/posts",
        Some(json!({"title": "Secret"})),
    )
    .await;
    assert_eq!(
        created["title"], "Secret",
        "the draft comes back: {created}"
    );
    assert_eq!(created["status"], "draft");
    assert!(created.get("publishedAt").is_none(), "{created}");
    let id = id_of(&created);
    let one = format!("/api/posts/{id}");
    let draft_one = format!("{one}?draft=true");

    // Nothing is public yet.
    for app in [&anon, &viewer, &author, &editor] {
        assert_eq!(ok(app, "GET", "/api/posts", None).await["docs"], json!([]));
        assert_problem(&call(app, "GET", &one, None).await, 404);
    }
    // Those who may read the draft see it; an Author only their own.
    for app in [&author, &editor, &admin] {
        let list = ok(app, "GET", "/api/posts?draft=true", None).await;
        assert_eq!(list["docs"][0]["title"], "Secret", "{list}");
        assert_eq!(ok(app, "GET", &draft_one, None).await["title"], "Secret");
    }
    assert_eq!(
        ok(&other, "GET", "/api/posts?draft=true", None).await["docs"],
        json!([])
    );
    assert_problem(&call(&other, "GET", &draft_one, None).await, 404);
    // Their own draft is theirs to change; another Author's is as good as missing.
    let res = call(&other, "PATCH", &one, Some(json!({"title": "Mine"}))).await;
    assert_problem(&res, 404);
    let res = call(&other, "DELETE", &one, None).await;
    assert_problem(&res, 404);
    let edited = ok(&author, "PATCH", &one, Some(json!({"title": "Edited"}))).await;
    assert_eq!(edited["title"], "Edited");

    // No right to drafts is a 403 naming `draft`, whoever asks.
    for res in [
        call(&viewer, "GET", "/api/posts?draft=true", None).await,
        call(&viewer, "GET", &draft_one, None).await,
        call(&anon, "GET", "/api/posts?draft=true", None).await,
    ] {
        assert_problem(&res, 403);
        names_draft(&res);
    }
    let key = ok(
        &admin,
        "POST",
        "/api/auth/keys",
        Some(json!({"name": "site", "scopes": ["posts:read"]})),
    )
    .await["key"]
        .as_str()
        .unwrap()
        .to_owned();
    for uri in ["/api/posts?draft=true", draft_one.as_str()] {
        let res = with_key(&anon, &key, uri).await;
        assert_problem(&res, 403);
        names_draft(&res);
    }
    assert_eq!(with_key(&anon, &key, "/api/posts").await.status, 200);
    // A collection without drafts has none to ask for.
    let res = call(&admin, "GET", "/api/notes?draft=true", None).await;
    assert_problem(&res, 400);
    names_draft(&res);

    // Once published, the public copy is what everyone reads; the draft stays apart.
    publish(&pool, &id).await;
    ok(&author, "PATCH", &one, Some(json!({"title": "Next"}))).await;
    let public = ok(&anon, "GET", &one, None).await;
    assert_eq!(public["title"], "Edited");
    assert!(public["publishedAt"].is_string(), "{public}");
    assert_eq!(ok(&editor, "GET", &draft_one, None).await["title"], "Next");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn draft_saves_skip_required(pool: PgPool) {
    let admin = blog(&pool).await;
    let res = call(&admin, "POST", "/api/posts", Some(json!({}))).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let one = format!("/api/posts/{}", id_of(&res.json));
    ok(&admin, "PATCH", &one, Some(json!({"title": null}))).await;
    // Everything else is still checked, and drafts off still needs it.
    let res = call(&admin, "PATCH", &one, Some(json!({"title": 7}))).await;
    assert_problem(&res, 422);
    let res = call(&admin, "POST", "/api/notes", Some(json!({}))).await;
    assert_problem(&res, 422);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn autosave_skips_versions(pool: PgPool) {
    let admin = blog(&pool).await;
    let id = id_of(&ok(&admin, "POST", "/api/posts", Some(json!({"title": "A"}))).await);
    assert_eq!(versions(&pool, &id).await, ["draft"], "a create is a save");
    let one = format!("/api/posts/{id}");
    for title in ["B", "C"] {
        let uri = format!("{one}?autosave=true");
        ok(&admin, "PATCH", &uri, Some(json!({ "title": title }))).await;
    }
    ok(&admin, "PATCH", &one, Some(json!({"title": "D"}))).await;
    assert_eq!(versions(&pool, &id).await, ["draft", "draft"]);
    let res = call(&admin, "GET", &format!("{one}?draft=true"), None).await;
    assert_eq!(res.json["title"], "D");

    // Autosave is only for a collection that turns it on.
    collection(
        &admin,
        json!({"slug": "pages", "versions": {"drafts": true},
               "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let page = id_of(&ok(&admin, "POST", "/api/pages", Some(json!({}))).await);
    for (uri, body) in [
        (format!("/api/pages/{page}?autosave=true"), json!({})),
        (format!("/api/notes/{page}?autosave=true"), json!({})),
        (format!("/api/pages/{page}?autosave=yes"), json!({})),
    ] {
        let res = call(&admin, "PATCH", &uri, Some(body)).await;
        assert_problem(&res, 400);
        assert!(
            res.json["detail"].as_str().unwrap().contains("autosave"),
            "{}",
            res.json
        );
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn versions_without_drafts_record_what_is_live(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    collection(
        &admin,
        json!({"slug": "pages", "versions": true, "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let page = ok(&admin, "POST", "/api/pages", Some(json!({"title": "A"}))).await;
    assert_eq!(page["status"], "published");
    let id = id_of(&page);
    ok(
        &admin,
        "PATCH",
        &format!("/api/pages/{id}"),
        Some(json!({"title": "B"})),
    )
    .await;
    assert_eq!(versions(&pool, &id).await, ["published", "published"]);
    let res = call(&admin, "GET", &format!("/api/pages/{id}?draft=true"), None).await;
    assert_problem(&res, 400);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn rules_follow_the_copy_read(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    collection(
        &admin,
        json!({"slug": "posts", "versions": {"drafts": true},
        "access": {"read": {"visibility": "public"}},
        "fields": [
            {"name": "title", "type": "text"},
            {"name": "visibility", "type": "select", "options": ["public", "private"]}
        ]}),
    )
    .await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let post = |visibility: &str| json!({"title": "T", "visibility": visibility});
    // Published public, draft private; and the other way about.
    let public_live = id_of(&ok(&admin, "POST", "/api/posts", Some(post("public"))).await);
    publish(&pool, &public_live).await;
    let one = format!("/api/posts/{public_live}");
    ok(
        &admin,
        "PATCH",
        &one,
        Some(json!({"visibility": "private"})),
    )
    .await;
    let private_live = id_of(&ok(&admin, "POST", "/api/posts", Some(post("private"))).await);
    publish(&pool, &private_live).await;
    let other = format!("/api/posts/{private_live}");
    ok(
        &admin,
        "PATCH",
        &other,
        Some(json!({"visibility": "public"})),
    )
    .await;

    // A published read judges the published copy.
    for app in [&anon, &editor] {
        let live = ok(app, "GET", &one, None).await;
        assert_eq!(live["visibility"], "public");
        assert_problem(&call(app, "GET", &other, None).await, 404);
        let list = ok(app, "GET", "/api/posts", None).await;
        assert_eq!(list["docs"].as_array().unwrap().len(), 1, "{list}");
        assert_eq!(list["docs"][0]["id"], public_live);
    }
    // A draft read judges the draft.
    assert_problem(
        &call(&editor, "GET", &format!("{one}?draft=true"), None).await,
        404,
    );
    let draft = ok(&editor, "GET", &format!("{other}?draft=true"), None).await;
    assert_eq!(draft["visibility"], "public");
    let list = ok(&editor, "GET", "/api/posts?draft=true", None).await;
    assert_eq!(list["docs"].as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list["docs"][0]["id"], private_live);
    // A write is guarded by the draft it changes.
    let res = call(&editor, "PATCH", &one, Some(json!({"title": "U"}))).await;
    assert_problem(&res, 404);
    ok(&editor, "PATCH", &other, Some(json!({"title": "U"}))).await;
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn linked_drafts_need_their_own_right(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    collection(
        &admin,
        json!({"slug": "authors", "versions": {"drafts": true},
               "access": {"readDrafts": {"role": "admin"}},
               "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    collection(
        &admin,
        json!({"slug": "posts", "versions": {"drafts": true}, "fields": [
            {"name": "title", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "authorName", "type": "text", "virtual": "author.name"}
        ]}),
    )
    .await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;
    let ann = id_of(&ok(&admin, "POST", "/api/authors", Some(json!({"name": "Ann"}))).await);
    publish(&pool, &ann).await;
    let ann_one = format!("/api/authors/{ann}");
    ok(
        &admin,
        "PATCH",
        &ann_one,
        Some(json!({"name": "Ann (draft)"})),
    )
    .await;
    let post = ok(
        &admin,
        "POST",
        "/api/posts",
        Some(json!({"title": "P", "author": ann})),
    )
    .await;
    let draft_one = format!("/api/posts/{}?draft=true", id_of(&post));

    let read = ok(&editor, "GET", &draft_one, None).await;
    assert_eq!(read["author"]["name"], "Ann", "{read}");
    assert_eq!(read["authorName"], "Ann");
    let where_draft = format!(
        "/api/posts?draft=true&where[author.name][equals]={}",
        "Ann%20(draft)"
    );
    assert_eq!(
        ok(&editor, "GET", &where_draft, None).await["docs"],
        json!([])
    );
    let read = ok(&admin, "GET", &draft_one, None).await;
    assert_eq!(read["author"]["name"], "Ann (draft)", "{read}");
    assert_eq!(read["authorName"], "Ann (draft)");
    let list = ok(&admin, "GET", &where_draft, None).await;
    assert_eq!(list["docs"].as_array().unwrap().len(), 1, "{list}");
    // Without the right there, asking for that draft outright is refused.
    let res = call(&editor, "GET", &format!("{ann_one}?draft=true"), None).await;
    assert_problem(&res, 403);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn drafts_link_to_what_their_writer_can_read(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    collection(
        &admin,
        json!({"slug": "authors", "versions": {"drafts": true},
               "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    collection(
        &admin,
        json!({"slug": "posts", "versions": {"drafts": true}, "fields": [
            {"name": "title", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "authors"}
        ]}),
    )
    .await;
    collection(
        &admin,
        json!({"slug": "notes", "fields": [
            {"name": "author", "type": "relationship", "relationTo": "authors"}
        ]}),
    )
    .await;
    let author = app_as(&pool, "author@example.com", "author").await;
    // Never published: only a draft.
    let hidden = id_of(&ok(&admin, "POST", "/api/authors", Some(json!({"name": "H"}))).await);
    let live = id_of(&ok(&admin, "POST", "/api/authors", Some(json!({"name": "L"}))).await);
    publish(&pool, &live).await;
    let own = id_of(&ok(&author, "POST", "/api/authors", Some(json!({"name": "O"}))).await);
    let missing = Uuid::now_v7().to_string();

    // A draft may link to a draft its writer can read, and to anything published.
    let post = ok(
        &admin,
        "POST",
        "/api/posts",
        Some(json!({"title": "P", "author": hidden})),
    )
    .await;
    assert_eq!(post["author"]["name"], "H", "{post}");
    let mine = ok(
        &author,
        "POST",
        "/api/posts",
        Some(json!({"title": "P", "author": own})),
    )
    .await;
    assert_eq!(mine["author"]["name"], "O", "{mine}");
    ok(&author, "POST", "/api/posts", Some(json!({"author": live}))).await;
    // Someone else's draft is as missing as a document that isn't there.
    let refused = call(
        &author,
        "POST",
        "/api/posts",
        Some(json!({"author": hidden})),
    )
    .await;
    let absent = call(
        &author,
        "POST",
        "/api/posts",
        Some(json!({"author": missing})),
    )
    .await;
    assert_problem(&refused, 422);
    assert_eq!(refused.json["errors"], absent.json["errors"]);
    // What's live links only to what's live.
    let refused = call(
        &admin,
        "POST",
        "/api/notes",
        Some(json!({"author": hidden})),
    )
    .await;
    let absent = call(
        &admin,
        "POST",
        "/api/notes",
        Some(json!({"author": missing})),
    )
    .await;
    assert_problem(&refused, 422);
    assert_eq!(refused.json["errors"], absent.json["errors"]);
    ok(&admin, "POST", "/api/notes", Some(json!({"author": live}))).await;
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn references_show_drafts_to_those_who_may_read_them(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    collection(
        &admin,
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    collection(
        &admin,
        json!({"slug": "posts", "versions": {"drafts": true}, "fields": [
            {"name": "author", "type": "relationship", "relationTo": "authors"}
        ]}),
    )
    .await;
    let author = app_as(&pool, "author@example.com", "author").await;
    let other = app_as(&pool, "other@example.com", "author").await;
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    let ann = id_of(&ok(&admin, "POST", "/api/authors", Some(json!({"name": "Ann"}))).await);
    let by_author = ok(&author, "POST", "/api/posts", Some(json!({"author": ann}))).await;
    let refs_uri = format!("/api/authors/{ann}/references");
    let refs = |app: &Router| {
        let (app, uri) = (app.clone(), refs_uri.clone());
        async move { ok(&app, "GET", &uri, None).await["docs"].clone() }
    };

    let draft_ref = json!([{"collection": "posts", "id": id_of(&by_author),
                            "path": "author", "copy": "draft"}]);
    assert_eq!(refs(&admin).await, draft_ref, "what blocks a delete");
    assert_eq!(refs(&author).await, draft_ref);
    assert_eq!(refs(&other).await, json!([]));
    assert_eq!(refs(&viewer).await, json!([]));
    let res = call(&admin, "DELETE", &format!("/api/authors/{ann}"), None).await;
    assert_problem(&res, 409);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn global_drafts(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    collection(
        &admin,
        json!({"slug": "settings", "kind": "global", "versions": {"drafts": true},
               "fields": [{"name": "name", "type": "text", "default": "Untitled"}]}),
    )
    .await;
    let author = app_as(&pool, "author@example.com", "author").await;
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let uri = "/api/globals/settings";
    let draft = "/api/globals/settings?draft=true";
    // Never saved: defaults, either way.
    assert_eq!(ok(&anon, "GET", uri, None).await["name"], "Untitled");
    assert_eq!(ok(&admin, "GET", draft, None).await["name"], "Untitled");
    // Saved: the draft, but the public still sees the defaults until it's published.
    let saved = ok(&admin, "PATCH", uri, Some(json!({"name": "Site"}))).await;
    assert_eq!(saved["name"], "Site");
    assert_eq!(ok(&anon, "GET", uri, None).await["name"], "Untitled");
    assert_eq!(ok(&admin, "GET", draft, None).await["name"], "Site");
    // Nobody created a global, so nobody's own-only right reaches its draft.
    let res = call(&author, "GET", draft, None).await;
    assert_problem(&res, 403);
    names_draft(&res);
    publish(&pool, &versions_doc(&pool).await).await;
    ok(&admin, "PATCH", uri, Some(json!({"name": "Next"}))).await;
    assert_eq!(ok(&anon, "GET", uri, None).await["name"], "Site");
    assert_eq!(ok(&admin, "GET", draft, None).await["name"], "Next");
    assert_eq!(versions(&pool, &versions_doc(&pool).await).await.len(), 2);
}

/// The settings global's document.
async fn versions_doc(pool: &PgPool) -> String {
    let id: Uuid = sqlx::query_scalar("SELECT id FROM collections WHERE slug = 'settings'")
        .fetch_one(pool)
        .await
        .unwrap();
    id.to_string()
}
