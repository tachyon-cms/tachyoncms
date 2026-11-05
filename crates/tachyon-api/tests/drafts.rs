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
    // A write is guarded by the draft it changes: refused when only what's live can be
    // read, and hidden when neither can.
    let neither = id_of(&ok(&admin, "POST", "/api/posts", Some(post("private"))).await);
    publish(&pool, &neither).await;
    let neither = format!("/api/posts/{neither}");
    for (method, body) in [("PATCH", Some(json!({"title": "U"}))), ("DELETE", None)] {
        assert_problem(&call(&editor, method, &one, body.clone()).await, 403);
        assert_problem(&call(&editor, method, &neither, body).await, 404);
    }
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
    // A `:read` key sees published content only; an `admin` key sees what an Admin does.
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    for (scopes, want) in [
        (json!(["authors:read", "posts:read"]), json!([])),
        (json!(["admin"]), draft_ref.clone()),
    ] {
        let key = ok(
            &admin,
            "POST",
            "/api/auth/keys",
            Some(json!({"name": "k", "scopes": scopes})),
        )
        .await["key"]
            .as_str()
            .unwrap()
            .to_owned();
        let res = with_key(&anon, &key, &refs_uri).await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(res.json["docs"], want, "{scopes}");
    }
    let res = call(&admin, "DELETE", &format!("/api/authors/{ann}"), None).await;
    assert_problem(&res, 409);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn writes_take_only_their_own_parameters(pool: PgPool) {
    let admin = blog(&pool).await;
    let id = id_of(&ok(&admin, "POST", "/api/posts", Some(json!({"title": "A"}))).await);
    for (method, uri, body) in [
        ("POST", "/api/posts?draft=true".to_owned(), Some(json!({}))),
        (
            "POST",
            "/api/posts?autosave=true".to_owned(),
            Some(json!({})),
        ),
        (
            "POST",
            "/api/notes?depth=0".to_owned(),
            Some(json!({"title": "N"})),
        ),
        ("DELETE", format!("/api/posts/{id}?draft=true"), None),
        (
            "PATCH",
            format!("/api/posts/{id}?draft=true"),
            Some(json!({})),
        ),
    ] {
        let res = call(&admin, method, &uri, body).await;
        assert_problem(&res, 400);
        assert!(
            res.json["detail"]
                .as_str()
                .unwrap()
                .contains("unknown query parameter"),
            "{method} {uri}: {}",
            res.json
        );
    }
    ok(&admin, "GET", &format!("/api/posts/{id}?draft=true"), None).await;
}

/// The user who created document `id`.
async fn creator(pool: &PgPool, id: &str) -> Uuid {
    sqlx::query_scalar("SELECT created_by FROM documents WHERE id = $1")
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Makes `by` the creator of document `id`.
async fn hand_to(pool: &PgPool, id: &str, by: Uuid) {
    sqlx::query("UPDATE documents SET created_by = $2 WHERE id = $1")
        .bind(id.parse::<Uuid>().unwrap())
        .bind(by)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn own_only_draft_reads_agree_everywhere(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let categories = |join: bool| {
        let mut fields =
            json!([{"name": "name", "type": "text"}, {"name": "slug", "type": "slug"}]);
        if join {
            fields.as_array_mut().unwrap().push(
                json!({"name": "posts", "type": "join", "collection": "posts", "on": "category"}),
            );
        }
        json!({"slug": "categories", "versions": {"drafts": true}, "url": "/c/{slug}",
               "fields": fields})
    };
    collection(&admin, categories(false)).await;
    collection(
        &admin,
        json!({"slug": "posts", "versions": {"drafts": true}, "fields": [
            {"name": "title", "type": "text"},
            {"name": "category", "type": "relationship", "relationTo": "categories"},
            {"name": "categoryName", "type": "text", "virtual": "category.name"},
            {"name": "body", "type": "richText", "relationTo": ["categories"]}
        ]}),
    )
    .await;
    let schema = json!({"schema": categories(true)});
    ok(
        &admin,
        "PUT",
        "/api/admin/collections/categories",
        Some(schema),
    )
    .await;
    let me = app_as(&pool, "me@example.com", "author").await;
    let them = app_as(&pool, "them@example.com", "author").await;
    let category = |app: &Router, name: &str, slug: &str| {
        let (app, body) = (app.clone(), json!({"name": name, "slug": slug}));
        async move { id_of(&ok(&app, "POST", "/api/categories", Some(body)).await) }
    };
    // (a) my own draft, (b) their published one with a draft of its own, and (c) their
    // draft, never published.
    let own = category(&me, "Own", "own").await;
    let live = category(&them, "Live", "live").await;
    publish(&pool, &live).await;
    ok(
        &them,
        "PATCH",
        &format!("/api/categories/{live}"),
        Some(json!({"name": "Live draft", "slug": "live-draft"})),
    )
    .await;
    let hidden = category(&them, "Hidden", "hidden").await;
    let (my_id, their_id) = (creator(&pool, &own).await, creator(&pool, &live).await);

    let post = |app: &Router, body: Value| {
        let app = app.clone();
        async move { id_of(&ok(&app, "POST", "/api/posts", Some(body)).await) }
    };
    let to_own = post(&me, json!({"title": "to-own", "category": own})).await;
    let to_live = post(&me, json!({"title": "to-live", "category": live})).await;
    // Mine too, though I couldn't have linked them myself.
    let to_hidden = post(&admin, json!({"title": "to-hidden", "category": hidden})).await;
    hand_to(&pool, &to_hidden, my_id).await;
    let link = |id: &str| {
        json!({"type": "paragraph", "content": [{"type": "text", "text": "see", "marks": [
            {"type": "link", "attrs": {"doc": {"relationTo": "categories", "value": id}}}
        ]}]})
    };
    let body = json!({"type": "doc", "content": [link(&own), link(&live), link(&hidden)]});
    let links = post(&admin, json!({"title": "links", "body": body})).await;
    hand_to(&pool, &links, my_id).await;
    // Theirs, linking my category: one published, one never.
    let their_live = post(&admin, json!({"title": "their-live", "category": own})).await;
    hand_to(&pool, &their_live, their_id).await;
    publish(&pool, &their_live).await;
    let their_draft = post(&admin, json!({"title": "their-draft", "category": own})).await;
    hand_to(&pool, &their_draft, their_id).await;

    // Filters through the link.
    let titles = |res: Value| -> Vec<String> {
        let mut t: Vec<String> = res["docs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["title"].as_str().unwrap().to_owned())
            .collect();
        t.sort();
        t
    };
    for (name, want) in [
        ("Own", vec!["to-own"]),
        ("Live", vec!["to-live"]),
        ("Live%20draft", vec![]),
        ("Hidden", vec![]),
        ("Nothing", vec![]),
    ] {
        let uri = format!("/api/posts?draft=true&where[category.name][equals]={name}");
        assert_eq!(titles(ok(&me, "GET", &uri, None).await), want, "{name}");
    }
    // Populate and virtuals.
    for (id, want) in [
        (&to_own, Some("Own")),
        (&to_live, Some("Live")),
        (&to_hidden, None),
    ] {
        let read = ok(&me, "GET", &format!("/api/posts/{id}?draft=true"), None).await;
        let cat = &read["category"];
        assert_eq!(cat["name"].as_str(), want, "{read}");
        assert_eq!(read["categoryName"].as_str(), want, "{read}");
        if want.is_none() {
            assert!(cat.is_string(), "a link I can't read stays an ID: {read}");
            assert!(!read.to_string().contains("Hidden"), "{read}");
        }
    }
    // Rendered links.
    let html = ok(
        &me,
        "GET",
        &format!("/api/posts/{links}?draft=true&format=html"),
        None,
    )
    .await;
    assert_eq!(
        html["body"],
        concat!(
            "<p><a href=\"/c/own\">see</a></p>",
            "<p><a href=\"/c/live\">see</a></p>",
            "<p>see</p>"
        )
    );
    // Joins: my category lists my draft and their published post, never their draft.
    let read = ok(
        &me,
        "GET",
        &format!("/api/categories/{own}?draft=true"),
        None,
    )
    .await;
    assert_eq!(
        titles(read["posts"].clone()),
        ["their-live", "to-own"],
        "{read}"
    );
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

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_slug_links_one_document_or_says_why_not(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    collection(
        &admin,
        json!({"slug": "categories", "versions": {"drafts": true},
               "fields": [{"name": "name", "type": "text"}, {"name": "slug", "type": "slug"}]}),
    )
    .await;
    collection(
        &admin,
        json!({"slug": "posts", "versions": {"drafts": true},
               "fields": [{"name": "body", "type": "richText", "relationTo": ["categories"]}]}),
    )
    .await;
    // Drafts needn't have unique slugs: two say `a`, made last so they'd come first.
    for (name, slug) in [("B", "b"), ("A", "a"), ("A again", "a")] {
        let body = json!({"name": name, "slug": slug});
        ok(&admin, "POST", "/api/categories", Some(body)).await;
    }
    let md = |src: &str| json!({"body": {"format": "markdown", "value": src}});
    let res = call(
        &admin,
        "POST",
        "/api/posts",
        Some(md("[x](doc:categories/a)\n\n[y](doc:categories/b)")),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"],
        json!([{"path": "body.content.0.content.0.marks.0.attrs.doc",
                "message": "line 1: `a` matches more than one document; link it by id"}]),
        "b is found, and a is never a silent pick"
    );
    let res = call(
        &admin,
        "POST",
        "/api/posts",
        Some(md("[y](doc:categories/b)")),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn refusals_come_before_bad_parameters(pool: PgPool) {
    let admin = blog(&pool).await;
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    let id = id_of(&ok(&admin, "POST", "/api/posts", Some(json!({"title": "A"}))).await);
    publish(&pool, &id).await;
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
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let one = format!("/api/posts/{id}");
    for (method, uri, body) in [
        ("POST", "/api/posts?stray=1".to_owned(), Some(json!({}))),
        ("PATCH", format!("{one}?autosave=maybe"), Some(json!({}))),
        ("DELETE", format!("{one}?stray=1"), None),
    ] {
        let body = body.map(|b| b.to_string());
        assert_problem(&send(&viewer, method, &uri, body.as_deref()).await, 403);
        let req = Request::builder()
            .method(method)
            .uri(&uri)
            .header("authorization", format!("Bearer {key}"));
        assert_problem(&send_with(&anon, req, body.as_deref()).await, 403);
    }
}
