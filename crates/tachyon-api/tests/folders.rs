//! Folders: a system collection that documents of any collection with `folders` file into.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use common::{ADMIN, PASSWORD, Res, add_user, assert_problem, png, send, signed_in, upload};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_api::{Options, UploadsConfig};
use tachyon_media::storage::{StorageConfig, Storages};
use uuid::Uuid;

/// A test's spool and store, removed with everything in them when the test is done.
fn dir() -> tempfile::TempDir {
    let base = tempfile::Builder::new()
        .prefix("tachyon-folders-")
        .tempdir()
        .unwrap();
    for sub in ["spool", "store"] {
        std::fs::create_dir_all(base.path().join(sub)).unwrap();
    }
    base
}

async fn router(pool: &PgPool, d: &tempfile::TempDir) -> Router {
    let storages = Storages::open(&BTreeMap::from([(
        "default".into(),
        StorageConfig::local(d.path().join("store")),
    )]))
    .unwrap();
    let options = Options {
        uploads: UploadsConfig {
            temp_dir: d.path().join("spool"),
            storages: Arc::new(storages),
            ..UploadsConfig::default()
        },
        ..Options::default()
    };
    tachyon_api::router_with(pool.clone(), options)
        .await
        .unwrap()
}

async fn admin(pool: &PgPool, d: &tempfile::TempDir) -> Router {
    let app = router(pool, d).await;
    let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "setup: {}", res.json);
    signed_in(app, ADMIN).await
}

async fn as_role(pool: &PgPool, d: &tempfile::TempDir, email: &str, role: &str) -> Router {
    add_user(pool, email, role).await;
    signed_in(router(pool, d).await, email).await
}

/// `body` is sent unless it's `null`.
async fn call(app: &Router, method: &str, uri: &str, body: Value) -> Res {
    let text = (!body.is_null()).then(|| body.to_string());
    send(app, method, uri, text.as_deref()).await
}

async fn ok(app: &Router, method: &str, uri: &str, body: Value) -> Value {
    let res = call(app, method, uri, body).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

async fn schema(app: &Router, schema: Value) {
    ok(app, "POST", "/api/admin/collections", schema).await;
}

/// A new folder called `name`, inside `parent` when given; its ID.
async fn folder(app: &Router, name: &str, parent: Option<&str>, collections: &[&str]) -> String {
    let mut body = json!({"name": name, "collections": collections});
    if let Some(p) = parent {
        body["folder"] = p.into();
    }
    id(&ok(app, "POST", "/api/folders", body).await)
}

fn id(doc: &Value) -> String {
    doc["id"].as_str().unwrap().to_owned()
}

/// The IDs a list answers with, sorted.
async fn ids(app: &Router, uri: &str) -> Vec<String> {
    let mut out: Vec<String> = ok(app, "GET", uri, Value::Null).await["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(id)
        .collect();
    out.sort();
    out
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// The one error's path.
fn error_path(res: &Res) -> &str {
    res.json["errors"][0]["path"].as_str().unwrap_or_default()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn folders_nest_and_list(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "media", "upload": true, "folders": true, "fields": []}),
    )
    .await;
    // The folders list the collections that file into them.
    let folders = ok(&app, "GET", "/api/admin/collections/folders", Value::Null).await;
    let collections = folders["schema"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "collections")
        .unwrap()
        .clone();
    assert_eq!(
        collections["options"],
        json!(["media", "users"]),
        "{folders}"
    );

    let top = folder(&app, "Top", None, &["media"]).await;
    let inner = folder(&app, "Inner", Some(&top), &["media"]).await;
    let mut inside = Vec::new();
    for name in ["a.png", "b.png"] {
        let payload = json!({"folder": inner});
        let res = upload(
            &app,
            "POST",
            "/api/media",
            Some(&png(4, 4)),
            name,
            Some(&payload),
        )
        .await;
        assert_eq!(res.status, 201, "{}", res.json);
        assert_eq!(res.json["folder"]["id"], inner.as_str());
        inside.push(id(&res.json));
    }
    let res = upload(&app, "POST", "/api/media", Some(&png(4, 4)), "c.png", None).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let at_root = id(&res.json);

    let listed = ids(&app, &format!("/api/media?where[folder][equals]={inner}")).await;
    assert_eq!(listed, sorted(inside));
    assert_eq!(
        ids(&app, "/api/media?where[folder][exists]=false").await,
        vec![at_root]
    );
    assert_eq!(
        ids(&app, "/api/folders?where[folder][exists]=false").await,
        vec![top.clone()]
    );
    assert_eq!(
        ids(&app, &format!("/api/folders?where[folder][equals]={top}")).await,
        vec![inner.clone()]
    );
    // Through the link, by the folder's name.
    assert_eq!(
        ids(&app, "/api/media?where[folder.name][equals]=Inner")
            .await
            .len(),
        2
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn folder_cycles_are_refused(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    let a = folder(&app, "A", None, &[]).await;
    let b = folder(&app, "B", Some(&a), &[]).await;
    for parent in [&b, &a] {
        let res = call(
            &app,
            "PATCH",
            &format!("/api/folders/{a}"),
            json!({"folder": parent}),
        )
        .await;
        assert_problem(&res, 422);
        assert_eq!(error_path(&res), "folder", "{}", res.json);
        let now = ok(&app, "GET", &format!("/api/folders/{a}"), Value::Null).await;
        assert!(now.get("folder").is_none(), "A is unchanged: {now}");
    }

    // Two moves at once that would make a loop between them: the second to lock loses.
    let c = folder(&app, "C", None, &[]).await;
    let e = folder(&app, "E", None, &[]).await;
    let c_id: Uuid = c.parse().unwrap();
    let pause = tachyon_api::pause_next_update(c_id);
    let first = tokio::spawn({
        let (app, uri, e) = (app.clone(), format!("/api/folders/{c}"), e.clone());
        async move { call(&app, "PATCH", &uri, json!({"folder": e})).await }
    });
    pause.reached().await;
    ok(
        &app,
        "PATCH",
        &format!("/api/folders/{e}"),
        json!({"folder": c}),
    )
    .await;
    pause.resume();
    let res = first.await.unwrap();
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "folder", "{}", res.json);
    let now = ok(&app, "GET", &format!("/api/folders/{c}"), Value::Null).await;
    assert!(now.get("folder").is_none(), "C stays at the root: {now}");

    // Truly at once, each checked before the other commits: never both.
    for _ in 0..20 {
        let p = folder(&app, "P", None, &[]).await;
        let q = folder(&app, "Q", None, &[]).await;
        let (to_p, to_q) = (format!("/api/folders/{p}"), format!("/api/folders/{q}"));
        let (one, two) = tokio::join!(
            call(&app, "PATCH", &to_p, json!({"folder": q})),
            call(&app, "PATCH", &to_q, json!({"folder": p})),
        );
        assert!(
            !(one.status.is_success() && two.status.is_success()),
            "a loop: {} {}",
            one.json,
            two.json
        );
    }

    // 64 folders above one is as deep as they go.
    let mut parent = folder(&app, "0", None, &[]).await;
    for n in 1..=64 {
        parent = folder(&app, &n.to_string(), Some(&parent), &[]).await;
    }
    let res = call(
        &app,
        "POST",
        "/api/folders",
        json!({"name": "65", "folder": parent}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "folder", "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn folders_slug_is_the_systems(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    let res = call(
        &app,
        "POST",
        "/api/admin/collections",
        json!({"slug": "folders", "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "slug", "{}", res.json);

    // Its own fields stay; others can be added.
    let stored =
        ok(&app, "GET", "/api/admin/collections/folders", Value::Null).await["schema"].clone();
    for gone in ["name", "folder", "collections"] {
        let mut s = stored.clone();
        s["fields"]
            .as_array_mut()
            .unwrap()
            .retain(|f| f["name"] != gone);
        let res = call(
            &app,
            "PUT",
            "/api/admin/collections/folders",
            json!({"schema": s, "confirm": true}),
        )
        .await;
        assert_problem(&res, 422);
        assert!(res.json.to_string().contains(gone), "{gone}: {}", res.json);
    }
    let mut retyped = stored.clone();
    for f in retyped["fields"].as_array_mut().unwrap() {
        if f["name"] == "folder" {
            *f = json!({"name": "folder", "type": "text"});
        }
    }
    let res = call(
        &app,
        "PUT",
        "/api/admin/collections/folders",
        json!({"schema": retyped, "confirm": true}),
    )
    .await;
    assert_problem(&res, 422);
    let mut more = stored.clone();
    more["fields"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "colour", "type": "text"}));
    ok(
        &app,
        "PUT",
        "/api/admin/collections/folders",
        json!({"schema": more}),
    )
    .await;
    // Folders can't keep versions: a restored one could put a folder inside itself.
    let mut versioned = stored.clone();
    versioned["versions"] = json!(true);
    let res = call(
        &app,
        "PUT",
        "/api/admin/collections/folders",
        json!({"schema": versioned}),
    )
    .await;
    assert_problem(&res, 422);
    // Nor files: every folder would need one.
    let mut uploads = stored.clone();
    uploads["upload"] = json!(true);
    let res = call(
        &app,
        "PUT",
        "/api/admin/collections/folders",
        json!({"schema": uploads, "confirm": true}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "upload", "{}", res.json);
    // Its own fields stay at the top, not in a row.
    for inner in ["name", "folder", "collections"] {
        let mut rowed = stored.clone();
        let fields = rowed["fields"].as_array_mut().unwrap();
        let at = fields.iter().position(|f| f["name"] == inner).unwrap();
        let f = fields.remove(at);
        fields.push(json!({"type": "row", "fields": [f]}));
        let res = call(
            &app,
            "PUT",
            "/api/admin/collections/folders",
            json!({"schema": rowed, "confirm": true}),
        )
        .await;
        assert_problem(&res, 422);
        assert!(
            res.json.to_string().contains(inner),
            "{inner}: {}",
            res.json
        );
    }
    // Globals have no documents to file.
    let res = call(
        &app,
        "POST",
        "/api/admin/collections",
        json!({"slug": "site", "kind": "global", "folders": true, "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    assert_problem(&res, 422);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn deleting_a_folder(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "posts", "folders": true, "versions": {"drafts": true},
               "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let a = folder(&app, "A", None, &[]).await;
    let b = folder(&app, "B", None, &[]).await;
    let child = folder(&app, "Child", Some(&a), &[]).await;
    let mut posts = Vec::new();
    for title in ["one", "two"] {
        let p = id(&ok(
            &app,
            "POST",
            "/api/posts",
            json!({"title": title, "folder": a}),
        )
        .await);
        ok(
            &app,
            "POST",
            &format!("/api/posts/{p}/publish"),
            Value::Null,
        )
        .await;
        posts.push(p);
    }
    let versions = |p: &str| {
        let pool = pool.clone();
        let p: Uuid = p.parse().unwrap();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM versions WHERE document_id = $1")
                .bind(p)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let before = versions(&posts[0]).await;

    let res = call(&app, "DELETE", &format!("/api/folders/{a}"), Value::Null).await;
    assert_problem(&res, 409);
    for into in [&a, &child] {
        let res = call(
            &app,
            "DELETE",
            &format!("/api/folders/{a}?moveContentsTo={into}"),
            Value::Null,
        )
        .await;
        assert_problem(&res, 422);
        assert_eq!(error_path(&res), "moveContentsTo", "{}", res.json);
    }
    let res = call(
        &app,
        "DELETE",
        &format!("/api/folders/{a}?moveContentsTo={}", Uuid::now_v7()),
        Value::Null,
    )
    .await;
    assert_problem(&res, 422);

    let res = call(
        &app,
        "DELETE",
        &format!("/api/folders/{a}?moveContentsTo={b}"),
        Value::Null,
    )
    .await;
    assert_eq!(res.status, 204, "{}", res.json);
    assert_eq!(
        call(&app, "GET", &format!("/api/folders/{a}"), Value::Null)
            .await
            .status,
        404
    );
    for p in &posts {
        let published = ok(&app, "GET", &format!("/api/posts/{p}?depth=0"), Value::Null).await;
        let draft = ok(
            &app,
            "GET",
            &format!("/api/posts/{p}?draft=true&depth=0"),
            Value::Null,
        )
        .await;
        assert_eq!(
            (&published["folder"], &draft["folder"]),
            (&json!(b), &json!(b))
        );
    }
    let moved = ok(
        &app,
        "GET",
        &format!("/api/folders/{child}?depth=0"),
        Value::Null,
    )
    .await;
    assert_eq!(moved["folder"], b.as_str());
    assert_eq!(versions(&posts[0]).await, before, "versions are untouched");
    assert_eq!(
        ids(&app, &format!("/api/posts?where[folder][equals]={b}")).await,
        sorted(posts.clone()),
        "the links moved too"
    );

    let res = call(
        &app,
        "DELETE",
        &format!("/api/folders/{b}?moveContentsTo=root"),
        Value::Null,
    )
    .await;
    assert_eq!(res.status, 204, "{}", res.json);
    for p in &posts {
        let doc = ok(
            &app,
            "GET",
            &format!("/api/posts/{p}?draft=true"),
            Value::Null,
        )
        .await;
        assert!(doc.get("folder").is_none(), "{doc}");
    }
    assert_eq!(
        ids(&app, "/api/posts?where[folder][exists]=false").await,
        sorted(posts.clone())
    );
    let moved = ok(&app, "GET", &format!("/api/folders/{child}"), Value::Null).await;
    assert!(moved.get("folder").is_none(), "{moved}");

    // An Author may change only what they made: one post they can't move refuses it all.
    let author = as_role(&pool, &d, "author@example.com", "author").await;
    let x = id(&ok(&author, "POST", "/api/folders", json!({"name": "X"})).await);
    let theirs = id(&ok(
        &author,
        "POST",
        "/api/posts",
        json!({"title": "mine", "folder": x}),
    )
    .await);
    let others = id(&ok(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "not", "folder": x}),
    )
    .await);
    let res = call(
        &author,
        "DELETE",
        &format!("/api/folders/{x}?moveContentsTo=root"),
        Value::Null,
    )
    .await;
    assert_problem(&res, 403);
    for p in [&theirs, &others] {
        let doc = ok(
            &app,
            "GET",
            &format!("/api/posts/{p}?draft=true&depth=0"),
            Value::Null,
        )
        .await;
        assert_eq!(doc["folder"], x.as_str(), "nothing moved");
    }
    ok(&app, "GET", &format!("/api/folders/{x}"), Value::Null).await;
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn folders_follow_collection_reads(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "public_docs", "folders": true, "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    schema(
        &app,
        json!({"slug": "private_docs", "folders": true, "access": {"read": {"role": "editor"}},
               "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let private = folder(&app, "Private", None, &["private_docs"]).await;
    let public = folder(&app, "Public", None, &["public_docs"]).await;
    let both = folder(&app, "Both", None, &["public_docs", "private_docs"]).await;
    let any = folder(&app, "Any", None, &[]).await;
    // A child of a hidden folder is still readable by its own rules.
    let below = folder(&app, "Below", Some(&private), &["public_docs"]).await;
    let doc = id(&ok(
        &app,
        "POST",
        "/api/public_docs",
        json!({"title": "t", "folder": below}),
    )
    .await);

    let viewer = as_role(&pool, &d, "viewer@example.com", "viewer").await;
    let res = call(
        &viewer,
        "GET",
        &format!("/api/folders/{private}"),
        Value::Null,
    )
    .await;
    assert_problem(&res, 404);
    assert_eq!(
        ids(&viewer, "/api/folders").await,
        sorted(vec![public, both, any, below.clone()])
    );
    for probe in [
        "/api/folders?where[name][equals]=Private",
        "/api/folders?where[collections][contains]=private_docs&where[name][equals]=Private",
    ] {
        assert!(ids(&viewer, probe).await.is_empty(), "{probe}");
    }
    let listed = ok(
        &viewer,
        "GET",
        "/api/folders?where[name][equals]=Private&count=true",
        Value::Null,
    )
    .await;
    assert_eq!(listed["totalDocs"], 0, "{listed}");
    // Through links: no join, filter or populate reaches it.
    assert!(
        ids(
            &viewer,
            "/api/public_docs?where[folder.folder.name][equals]=Private"
        )
        .await
        .is_empty()
    );
    assert!(
        ids(
            &viewer,
            &format!("/api/folders?where[folder.name][equals]=Private&where[id][equals]={below}")
        )
        .await
        .is_empty()
    );
    assert_eq!(
        ids(
            &app,
            "/api/public_docs?where[folder.folder.name][equals]=Private"
        )
        .await,
        vec![doc]
    );
    let got = ok(
        &viewer,
        "GET",
        &format!("/api/folders/{below}?depth=1"),
        Value::Null,
    )
    .await;
    assert!(!got["folder"].is_object(), "{got}");
    let got = ok(
        &app,
        "GET",
        &format!("/api/folders/{below}?depth=1"),
        Value::Null,
    )
    .await;
    assert_eq!(got["folder"]["name"], "Private");

    let editor = as_role(&pool, &d, "editor@example.com", "editor").await;
    ok(
        &editor,
        "GET",
        &format!("/api/folders/{private}"),
        Value::Null,
    )
    .await;
}

/// The folders' `collections` options are every collection there is, so turning a
/// collection's folders off or on never rewrites a folder's list, and can't widen who
/// reads it.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn folder_options_list_every_collection(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    let options = |app: Router| async move {
        let s = ok(&app, "GET", "/api/admin/collections/folders", Value::Null).await;
        s["schema"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "collections")
            .unwrap()["options"]
            .clone()
    };
    assert_eq!(options(app.clone()).await, json!(["users"]));
    let posts =
        json!({"slug": "posts", "folders": true, "fields": [{"name": "title", "type": "text"}]});
    schema(&app, posts.clone()).await;
    schema(
        &app,
        json!({"slug": "plain", "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let private = json!({"slug": "private_docs", "folders": true,
        "access": {"read": {"role": "editor"}}, "fields": [{"name": "title", "type": "text"}]});
    schema(&app, private.clone()).await;
    assert_eq!(
        options(app.clone()).await,
        json!(["plain", "posts", "private_docs", "users"])
    );
    let f = folder(&app, "F", None, &["posts", "plain"]).await;
    let only = folder(&app, "Only", None, &["private_docs"]).await;
    let p = id(&ok(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "t", "folder": f}),
    )
    .await);

    // Turning folders off drops every post's folder, so it asks first.
    let mut off = posts.clone();
    off["folders"] = json!(false);
    let res = call(
        &app,
        "PUT",
        "/api/admin/collections/posts",
        json!({"schema": off}),
    )
    .await;
    assert_problem(&res, 409);
    ok(
        &app,
        "PUT",
        "/api/admin/collections/posts",
        json!({"schema": off, "confirm": true}),
    )
    .await;
    let doc = ok(&app, "GET", &format!("/api/posts/{p}"), Value::Null).await;
    assert!(doc.get("folder").is_none(), "{doc}");
    let mut private_off = private.clone();
    private_off["folders"] = json!(false);
    ok(
        &app,
        "PUT",
        "/api/admin/collections/private_docs",
        json!({"schema": private_off, "confirm": true}),
    )
    .await;
    assert_eq!(
        options(app.clone()).await,
        json!(["plain", "posts", "private_docs", "users"])
    );
    // The folders' own lists are left as they were.
    let kept = ok(&app, "GET", &format!("/api/folders/{f}"), Value::Null).await;
    assert_eq!(kept["collections"], json!(["posts", "plain"]));
    let viewer = as_role(&pool, &d, "viewer@example.com", "viewer").await;
    assert_problem(
        &call(&viewer, "GET", &format!("/api/folders/{only}"), Value::Null).await,
        404,
    );
    assert!(!ids(&viewer, "/api/folders").await.contains(&only));
    // A folder can be deleted now that nothing files into it.
    let res = call(&app, "DELETE", &format!("/api/folders/{f}"), Value::Null).await;
    assert_eq!(res.status, 204, "{}", res.json);
    // An empty list is stored as none.
    let none = folder(&app, "None", None, &[]).await;
    let got = ok(&app, "GET", &format!("/api/folders/{none}"), Value::Null).await;
    assert!(got.get("collections").is_none(), "{got}");

    // A save of the folders answers with what's stored once refreshed.
    let mut s =
        ok(&app, "GET", "/api/admin/collections/folders", Value::Null).await["schema"].clone();
    for f in s["fields"].as_array_mut().unwrap() {
        if f["name"] == "collections" {
            f["options"] = json!(["posts"]);
        }
    }
    let saved = ok(
        &app,
        "PUT",
        "/api/admin/collections/folders",
        json!({"schema": s, "confirm": true}),
    )
    .await;
    let listed = saved["collection"]["schema"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "collections")
        .unwrap()["options"]
        .clone();
    assert_eq!(
        listed,
        json!(["plain", "posts", "private_docs", "users"]),
        "{saved}"
    );
}

/// Signed out, folders take the same defaults as a collection with no rules, and the
/// folder filter still hides those for collections the caller can't read.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn folders_take_the_anonymous_default(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "open", "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    schema(
        &app,
        json!({"slug": "private_docs", "folders": true, "access": {"read": {"signedIn": true}},
               "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let f = folder(&app, "F", None, &[]).await;
    let private = folder(&app, "Private", None, &["private_docs"]).await;
    let o = id(&ok(&app, "POST", "/api/open", json!({"title": "t"})).await);
    let anon = router(&pool, &d).await;
    for (folders, other) in [
        ("/api/folders".to_owned(), "/api/open".to_owned()),
        (format!("/api/folders/{f}"), format!("/api/open/{o}")),
    ] {
        let (a, b) = (
            call(&anon, "GET", &folders, Value::Null).await,
            call(&anon, "GET", &other, Value::Null).await,
        );
        assert_eq!(a.status, b.status, "{folders}: {} vs {}", a.json, b.json);
        assert!(a.status.is_success(), "{}", a.json);
    }
    for (method, folders, other, body) in [
        (
            "POST",
            "/api/folders".to_owned(),
            "/api/open".to_owned(),
            json!({"name": "N", "title": "N"}),
        ),
        (
            "PATCH",
            format!("/api/folders/{f}"),
            format!("/api/open/{o}"),
            json!({"name": "N", "title": "N"}),
        ),
        (
            "DELETE",
            format!("/api/folders/{f}"),
            format!("/api/open/{o}"),
            Value::Null,
        ),
    ] {
        let a = call(&anon, method, &folders, body.clone()).await;
        let b = call(&anon, method, &other, body).await;
        assert_eq!(
            (a.status, &a.json["detail"]),
            (b.status, &b.json["detail"]),
            "{method} {folders}"
        );
    }
    let access = ok(&anon, "GET", "/api/access", Value::Null).await;
    // Listed like any collection; reads are conditional on the folder filter only.
    assert_eq!(access["collections"]["open"]["read"], true, "{access}");
    assert_eq!(
        access["collections"]["folders"]["read"], "conditional",
        "{access}"
    );
    for k in ["create", "update", "delete"] {
        assert_eq!(
            access["collections"]["folders"][k], access["collections"]["open"][k],
            "{k}"
        );
    }
    // A folder only for what they can't read is missing.
    assert_problem(
        &call(
            &anon,
            "GET",
            &format!("/api/folders/{private}"),
            Value::Null,
        )
        .await,
        404,
    );
    assert_eq!(ids(&anon, "/api/folders").await, vec![f]);
    // Profiles read like content wherever linked, signed out too, so a folder for them does.
    let people = folder(&app, "People", None, &["users"]).await;
    ok(&anon, "GET", &format!("/api/folders/{people}"), Value::Null).await;
}

/// A folder moved with what's inside it must leave its deepest folder 64 levels down at
/// most, whether it's moved itself or with a deleted folder's contents.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn moved_subtrees_stay_within_depth(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    // chain[n] has n folders above it.
    let mut chain = vec![folder(&app, "0", None, &[]).await];
    for n in 1..=62 {
        let above = chain[n - 1].clone();
        chain.push(folder(&app, &n.to_string(), Some(&above), &[]).await);
    }
    // s, with two levels below it.
    let s = folder(&app, "S", None, &[]).await;
    let s1 = folder(&app, "S1", Some(&s), &[]).await;
    folder(&app, "S2", Some(&s1), &[]).await;
    let at = |s: &str| format!("/api/folders/{s}");
    let res = call(&app, "PATCH", &at(&s), json!({"folder": chain[62]})).await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "folder", "{}", res.json);
    // Exactly 64 down.
    ok(&app, "PATCH", &at(&s), json!({"folder": chain[61]})).await;
    ok(&app, "PATCH", &at(&s), json!({"folder": null})).await;

    let y = folder(&app, "Y", None, &[]).await;
    ok(&app, "PATCH", &at(&s), json!({"folder": y})).await;
    let res = call(
        &app,
        "DELETE",
        &format!("/api/folders/{y}?moveContentsTo={}", chain[62]),
        Value::Null,
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "moveContentsTo", "{}", res.json);
    assert!(
        res.json.to_string().contains(&s),
        "names the folder: {}",
        res.json
    );
    ok(&app, "GET", &at(&y), Value::Null).await;
    let res = call(
        &app,
        "DELETE",
        &format!("/api/folders/{y}?moveContentsTo={}", chain[61]),
        Value::Null,
    )
    .await;
    assert_eq!(res.status, 204, "{}", res.json);
}

/// A move is an update of each document, validated in full: one that no longer validates
/// stops it, named by collection and ID.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_move_names_the_document_that_fails(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    let notes =
        json!({"slug": "notes", "folders": true, "fields": [{"name": "title", "type": "text"}]});
    schema(&app, notes.clone()).await;
    let f = folder(&app, "F", None, &[]).await;
    let n = id(&ok(
        &app,
        "POST",
        "/api/notes",
        json!({"title": "t", "folder": f}),
    )
    .await);
    let mut required = notes.clone();
    required["fields"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "summary", "type": "text", "required": true}));
    ok(
        &app,
        "PUT",
        "/api/admin/collections/notes",
        json!({"schema": required, "confirm": true}),
    )
    .await;
    let res = call(
        &app,
        "DELETE",
        &format!("/api/folders/{f}?moveContentsTo=root"),
        Value::Null,
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "moveContentsTo", "{}", res.json);
    let message = res.json["errors"][0]["message"].as_str().unwrap();
    assert!(
        message.contains("notes") && message.contains(&n) && message.contains("summary"),
        "{message}"
    );
    let doc = ok(&app, "GET", &format!("/api/notes/{n}?depth=0"), Value::Null).await;
    assert_eq!(doc["folder"], f.as_str(), "nothing moved");
}

/// A folder restored from the trash must still fit: the folders above it may have moved
/// down while it was there.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn restoring_a_folder_keeps_its_depth(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    let mut chain = vec![folder(&app, "0", None, &[]).await];
    for n in 1..=62 {
        let above = chain[n - 1].clone();
        chain.push(folder(&app, &n.to_string(), Some(&above), &[]).await);
    }
    // 63 above it.
    let x = folder(&app, "X", Some(&chain[62]), &[]).await;
    let res = call(&app, "DELETE", &format!("/api/folders/{x}"), Value::Null).await;
    assert_eq!(res.status, 204, "{}", res.json);
    let n1 = folder(&app, "N1", None, &[]).await;
    let n2 = folder(&app, "N2", None, &[]).await;
    ok(
        &app,
        "PATCH",
        &format!("/api/folders/{}", chain[0]),
        json!({"folder": n1}),
    )
    .await;
    ok(
        &app,
        "PATCH",
        &format!("/api/folders/{n1}"),
        json!({"folder": n2}),
    )
    .await;
    let res = call(
        &app,
        "POST",
        &format!("/api/folders/trash/{x}/restore"),
        Value::Null,
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "folder", "{}", res.json);
    assert_problem(
        &call(&app, "GET", &format!("/api/folders/{x}"), Value::Null).await,
        404,
    );
    // Moved back up, it fits again.
    ok(
        &app,
        "PATCH",
        &format!("/api/folders/{n1}"),
        json!({"folder": null}),
    )
    .await;
    ok(
        &app,
        "POST",
        &format!("/api/folders/trash/{x}/restore"),
        Value::Null,
    )
    .await;
}

/// Moving a live copy out of a folder changes what's published, so it takes the right to
/// publish, by rule and by key scope, as a publish would.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn moving_live_copies_takes_publish(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "posts", "folders": true, "versions": {"drafts": true},
               "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let author = as_role(&pool, &d, "author@example.com", "author").await;
    let g = id(&ok(&author, "POST", "/api/folders", json!({"name": "G"})).await);
    let p = id(&ok(
        &author,
        "POST",
        "/api/posts",
        json!({"title": "t", "folder": g}),
    )
    .await);
    ok(
        &app,
        "POST",
        &format!("/api/posts/{p}/publish"),
        Value::Null,
    )
    .await;
    assert_problem(
        &call(
            &author,
            "POST",
            &format!("/api/posts/{p}/publish"),
            Value::Null,
        )
        .await,
        403,
    );
    let held = |app: Router| {
        let p = p.clone();
        async move {
            let live = ok(&app, "GET", &format!("/api/posts/{p}?depth=0"), Value::Null).await;
            let draft = ok(
                &app,
                "GET",
                &format!("/api/posts/{p}?draft=true&depth=0"),
                Value::Null,
            )
            .await;
            (live["folder"].clone(), draft["folder"].clone())
        }
    };
    let res = call(
        &author,
        "DELETE",
        &format!("/api/folders/{g}?moveContentsTo=root"),
        Value::Null,
    )
    .await;
    assert_problem(&res, 403);
    assert_eq!(
        held(app.clone()).await,
        (json!(g), json!(g)),
        "nothing moved"
    );

    // A key that may write but not publish.
    let key = ok(
        &app,
        "POST",
        "/api/auth/keys",
        json!({"name": "w", "scopes": ["*:write"]}),
    )
    .await["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let keyed = common::with_key(router(&pool, &d).await, &key);
    let res = call(
        &keyed,
        "DELETE",
        &format!("/api/folders/{g}?moveContentsTo=root"),
        Value::Null,
    )
    .await;
    assert_problem(&res, 403);
    assert_eq!(
        held(app.clone()).await,
        (json!(g), json!(g)),
        "nothing moved"
    );

    let editor = as_role(&pool, &d, "editor@example.com", "editor").await;
    let res = call(
        &editor,
        "DELETE",
        &format!("/api/folders/{g}?moveContentsTo=root"),
        Value::Null,
    )
    .await;
    assert_eq!(res.status, 204, "{}", res.json);
    assert_eq!(held(app.clone()).await, (Value::Null, Value::Null));
}

/// Trashed documents move with the rest, so restoring one later works.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn trashed_contents_move_too(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "posts", "folders": true, "versions": {"drafts": true},
               "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let f = folder(&app, "F", None, &[]).await;
    let b = folder(&app, "B", None, &[]).await;
    let p = id(&ok(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "t", "folder": f}),
    )
    .await);
    ok(
        &app,
        "POST",
        &format!("/api/posts/{p}/publish"),
        Value::Null,
    )
    .await;
    let res = call(&app, "DELETE", &format!("/api/posts/{p}"), Value::Null).await;
    assert_eq!(res.status, 204, "{}", res.json);
    let res = call(
        &app,
        "DELETE",
        &format!("/api/folders/{f}?moveContentsTo={b}"),
        Value::Null,
    )
    .await;
    assert_eq!(res.status, 204, "{}", res.json);
    ok(
        &app,
        "POST",
        &format!("/api/posts/trash/{p}/restore"),
        Value::Null,
    )
    .await;
    let live = ok(&app, "GET", &format!("/api/posts/{p}?depth=0"), Value::Null).await;
    let draft = ok(
        &app,
        "GET",
        &format!("/api/posts/{p}?draft=true&depth=0"),
        Value::Null,
    )
    .await;
    assert_eq!((&live["folder"], &draft["folder"]), (&json!(b), &json!(b)));
}

/// A folder the caller can't read is no different from none, as a place to move to.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_hidden_target_is_a_missing_one(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "private_docs", "folders": true, "access": {"read": {"role": "editor"}},
               "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let hidden = folder(&app, "Hidden", None, &["private_docs"]).await;
    let author = as_role(&pool, &d, "author@example.com", "author").await;
    let x = id(&ok(&author, "POST", "/api/folders", json!({"name": "X"})).await);
    let a = call(
        &author,
        "DELETE",
        &format!("/api/folders/{x}?moveContentsTo={hidden}"),
        Value::Null,
    )
    .await;
    let b = call(
        &author,
        "DELETE",
        &format!("/api/folders/{x}?moveContentsTo={}", Uuid::now_v7()),
        Value::Null,
    )
    .await;
    assert_problem(&a, 422);
    assert_eq!((a.status, &a.json["errors"]), (b.status, &b.json["errors"]));
}

/// Accounts filed in a folder move with it, by those who manage users only.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn users_move_with_their_folder(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    let mut users =
        ok(&app, "GET", "/api/admin/collections/users", Value::Null).await["schema"].clone();
    users["folders"] = json!(true);
    ok(
        &app,
        "PUT",
        "/api/admin/collections/users",
        json!({"schema": users}),
    )
    .await;
    let me: Uuid = sqlx::query_scalar("SELECT user_id FROM accounts WHERE email = $1")
        .bind(ADMIN)
        .fetch_one(&pool)
        .await
        .unwrap();
    let f = folder(&app, "Team", None, &[]).await;
    ok(
        &app,
        "PATCH",
        &format!("/api/users/{me}"),
        json!({"folder": f}),
    )
    .await;
    let editor = as_role(&pool, &d, "editor@example.com", "editor").await;
    let res = call(
        &editor,
        "DELETE",
        &format!("/api/folders/{f}?moveContentsTo=root"),
        Value::Null,
    )
    .await;
    assert_problem(&res, 403);
    let res = call(
        &app,
        "DELETE",
        &format!("/api/folders/{f}?moveContentsTo=root"),
        Value::Null,
    )
    .await;
    assert_eq!(res.status, 204, "{}", res.json);
    let profile = ok(&app, "GET", &format!("/api/users/{me}"), Value::Null).await;
    assert!(
        profile.get("folder").is_none_or(Value::is_null),
        "{profile}"
    );
}

/// A folder that lists collections holds only those; one that lists none holds any.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn folders_hold_what_they_list(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    for slug in ["posts", "pages"] {
        schema(
            &app,
            json!({"slug": slug, "folders": true, "fields": [{"name": "title", "type": "text"}]}),
        )
        .await;
    }
    let posts_only = folder(&app, "Posts", None, &["posts"]).await;
    let any = folder(&app, "Any", None, &[]).await;
    let res = call(
        &app,
        "POST",
        "/api/pages",
        json!({"title": "t", "folder": posts_only}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "folder", "{}", res.json);
    let page = id(&ok(
        &app,
        "POST",
        "/api/pages",
        json!({"title": "t", "folder": any}),
    )
    .await);
    let res = call(
        &app,
        "PATCH",
        &format!("/api/pages/{page}"),
        json!({"folder": posts_only}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "folder", "{}", res.json);
    ok(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "t", "folder": posts_only}),
    )
    .await;
    // Moved into one that doesn't list it, the move is refused, naming the page.
    let res = call(
        &app,
        "DELETE",
        &format!("/api/folders/{any}?moveContentsTo={posts_only}"),
        Value::Null,
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "moveContentsTo", "{}", res.json);
    assert!(res.json.to_string().contains(&page), "{}", res.json);
}

/// Accounts are filed like any document: through the profile route too, into a folder
/// that lists `users` or none.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn profiles_file_by_the_folders_list(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "posts", "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let mut users =
        ok(&app, "GET", "/api/admin/collections/users", Value::Null).await["schema"].clone();
    users["folders"] = json!(true);
    ok(
        &app,
        "PUT",
        "/api/admin/collections/users",
        json!({"schema": users}),
    )
    .await;
    let me: Uuid = sqlx::query_scalar("SELECT user_id FROM accounts WHERE email = $1")
        .bind(ADMIN)
        .fetch_one(&pool)
        .await
        .unwrap();
    let posts_only = folder(&app, "Posts", None, &["posts"]).await;
    let people = folder(&app, "People", None, &["users"]).await;
    let res = call(
        &app,
        "PATCH",
        &format!("/api/users/{me}"),
        json!({"folder": posts_only}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "folder", "{}", res.json);
    ok(
        &app,
        "PATCH",
        &format!("/api/users/{me}"),
        json!({"folder": people}),
    )
    .await;
    // And when the account is made.
    let res = call(
        &app,
        "POST",
        "/api/users",
        json!({"email": "new@example.com", "password": PASSWORD, "name": "N", "folder": posts_only}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "folder", "{}", res.json);
}

/// A document restored from the trash goes back into its folder, which must still be there,
/// as for any link (Ruling T5-1). What it lists was judged when the document was filed, and
/// isn't again.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn restoring_keeps_the_folder(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "posts", "folders": true, "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    schema(
        &app,
        json!({"slug": "pages", "folders": true, "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let restore = |p: String| {
        let app = app.clone();
        async move {
            call(
                &app,
                "POST",
                &format!("/api/posts/trash/{p}/restore"),
                Value::Null,
            )
            .await
        }
    };
    // Its folder gone.
    let gone = folder(&app, "Gone", None, &["posts"]).await;
    let p = id(&ok(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "a", "folder": gone}),
    )
    .await);
    ok(&app, "DELETE", &format!("/api/posts/{p}"), Value::Null).await;
    ok(&app, "DELETE", &format!("/api/folders/{gone}"), Value::Null).await;
    let res = restore(p.clone()).await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "folder", "{}", res.json);
    assert_problem(
        &call(&app, "GET", &format!("/api/posts/{p}"), Value::Null).await,
        404,
    );
    // Its folder no longer listing posts.
    let narrowed = folder(&app, "Narrowed", None, &["posts"]).await;
    let q = id(&ok(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "b", "folder": narrowed}),
    )
    .await);
    ok(&app, "DELETE", &format!("/api/posts/{q}"), Value::Null).await;
    ok(
        &app,
        "PATCH",
        &format!("/api/folders/{narrowed}"),
        json!({"collections": ["pages"]}),
    )
    .await;
    let res = restore(q.clone()).await;
    assert_eq!(res.status, 200, "{}", res.json);
}

/// Narrowing a folder's list stops new filings only: what's already inside stays editable,
/// publishable and restorable.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn narrowing_keeps_whats_inside(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    for slug in ["posts", "pages"] {
        schema(
            &app,
            json!({"slug": slug, "folders": true, "versions": {"drafts": true},
                   "fields": [{"name": "title", "type": "text"}]}),
        )
        .await;
    }
    let n = folder(&app, "N", None, &["posts", "pages"]).await;
    let page = id(&ok(
        &app,
        "POST",
        "/api/pages",
        json!({"title": "t", "folder": n}),
    )
    .await);
    ok(
        &app,
        "PATCH",
        &format!("/api/folders/{n}"),
        json!({"collections": ["posts"]}),
    )
    .await;
    let res = call(
        &app,
        "PATCH",
        &format!("/api/pages/{page}"),
        json!({"title": "edited"}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = call(
        &app,
        "POST",
        &format!("/api/pages/{page}/publish"),
        Value::Null,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    ok(&app, "DELETE", &format!("/api/pages/{page}"), Value::Null).await;
    let res = call(
        &app,
        "POST",
        &format!("/api/pages/trash/{page}/restore"),
        Value::Null,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let live = ok(
        &app,
        "GET",
        &format!("/api/pages/{page}?depth=0"),
        Value::Null,
    )
    .await;
    assert_eq!(live["folder"], n.as_str());
    // Filing another one there is refused.
    let other = id(&ok(&app, "POST", "/api/pages", json!({"title": "o"})).await);
    let res = call(
        &app,
        "PATCH",
        &format!("/api/pages/{other}"),
        json!({"folder": n}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(error_path(&res), "folder", "{}", res.json);
}

/// Trashed documents move only for callers who may reach the trash: everything there
/// takes `delete`.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn trashed_contents_take_delete(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "posts", "folders": true, "access": {"delete": {"role": "admin"}},
               "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let e1 = folder(&app, "E1", None, &[]).await;
    let e2 = folder(&app, "E2", None, &[]).await;
    let p = id(&ok(
        &app,
        "POST",
        "/api/posts",
        json!({"title": "t", "folder": e1}),
    )
    .await);
    ok(&app, "DELETE", &format!("/api/posts/{p}"), Value::Null).await;
    let editor = as_role(&pool, &d, "editor@example.com", "editor").await;
    assert_problem(
        &call(&editor, "GET", "/api/posts/trash", Value::Null).await,
        403,
    );
    let res = call(
        &editor,
        "DELETE",
        &format!("/api/folders/{e1}?moveContentsTo={e2}"),
        Value::Null,
    )
    .await;
    assert_problem(&res, 403);
    let held: Option<String> =
        sqlx::query_scalar("SELECT published ->> 'folder' FROM documents WHERE id = $1")
            .bind(p.parse::<Uuid>().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(held.as_deref(), Some(e1.as_str()), "nothing moved");
    let res = call(
        &app,
        "DELETE",
        &format!("/api/folders/{e1}?moveContentsTo={e2}"),
        Value::Null,
    )
    .await;
    assert_eq!(res.status, 204, "{}", res.json);
}

/// Turning folders on over a field of its own called `folder` keeps the values stored, and
/// those documents stay editable: only a change of folder is judged.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn folders_on_keeps_stored_folders(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    schema(
        &app,
        json!({"slug": "posts", "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let notes = json!({"slug": "notes", "fields": [
        {"name": "title", "type": "text"},
        {"name": "folder", "type": "relationship", "relationTo": "folders"}]});
    schema(&app, notes.clone()).await;
    let posts_only = folder(&app, "Posts", None, &["posts"]).await;
    let note = id(&ok(
        &app,
        "POST",
        "/api/notes",
        json!({"title": "t", "folder": posts_only}),
    )
    .await);
    let mut on = notes.clone();
    on["folders"] = json!(true);
    ok(
        &app,
        "PUT",
        "/api/admin/collections/notes",
        json!({"schema": on, "confirm": true}),
    )
    .await;
    let res = call(
        &app,
        "PATCH",
        &format!("/api/notes/{note}"),
        json!({"title": "edited"}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = call(
        &app,
        "PATCH",
        &format!("/api/notes/{note}"),
        json!({"folder": posts_only}),
    )
    .await;
    assert_eq!(res.status, 200, "unchanged is no change: {}", res.json);
}

/// A move writes each copy holding the link, so each is judged by the rules against itself:
/// the live copy as it's live, the draft as it's drafted. Either one failing refuses the
/// whole move, for update rules and, in the trash, delete rules alike.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn each_copy_is_judged_as_itself(pool: PgPool) {
    let d = dir();
    let app = admin(&pool, &d).await;
    let ok_only = json!({"title": {"equals": "ok"}});
    schema(
        &app,
        json!({"slug": "posts", "folders": true, "versions": {"drafts": true},
               "access": {"update": ok_only},
               "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    schema(
        &app,
        json!({"slug": "notes", "folders": true, "versions": {"drafts": true},
               "access": {"delete": ok_only},
               "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    let editor = as_role(&pool, &d, "editor@example.com", "editor").await;
    // A document filed in a new folder, live as `live` and drafted as `draft`.
    let filed = |slug: &'static str, live: &'static str, draft: &'static str, trash: bool| {
        let app = app.clone();
        async move {
            let f = folder(&app, "F", None, &[]).await;
            let p = id(&ok(
                &app,
                "POST",
                &format!("/api/{slug}"),
                json!({"title": live, "folder": f}),
            )
            .await);
            ok(
                &app,
                "POST",
                &format!("/api/{slug}/{p}/publish"),
                Value::Null,
            )
            .await;
            ok(
                &app,
                "PATCH",
                &format!("/api/{slug}/{p}"),
                json!({"title": draft}),
            )
            .await;
            if trash {
                ok(&app, "DELETE", &format!("/api/{slug}/{p}"), Value::Null).await;
            }
            (f, p)
        }
    };
    let held = |p: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<String>, Option<String>)>(
                "SELECT published ->> 'folder', draft ->> 'folder' FROM documents WHERE id = $1",
            )
            .bind(p.parse::<Uuid>().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    for (slug, trash) in [("posts", false), ("notes", true)] {
        for (live, draft, moves) in [
            ("ok", "bad", false),
            ("bad", "ok", false),
            ("ok", "ok", true),
        ] {
            let (f, p) = filed(slug, live, draft, trash).await;
            let res = call(
                &editor,
                "DELETE",
                &format!("/api/folders/{f}?moveContentsTo=root"),
                Value::Null,
            )
            .await;
            let case = format!("{slug}: live {live}, draft {draft}");
            if moves {
                assert_eq!(res.status, 204, "{case}: {}", res.json);
                assert_eq!(held(p).await, (None, None), "{case}");
            } else {
                assert_problem(&res, 403);
                assert_eq!(
                    held(p).await,
                    (Some(f.clone()), Some(f)),
                    "{case}: nothing moved"
                );
            }
        }
    }
}
