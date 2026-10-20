//! Access rules on collections and globals, on every read path and write route.

mod common;

use axum::Router;
use axum::http::Request;
use common::{Res, add_user, admin_app, app_as, assert_problem, send, send_with, signed_in};
use serde_json::{Value, json};
use sqlx::PgPool;

async fn create(app: &Router, url: &str, body: Value) -> Value {
    let res = send(app, "POST", url, Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "{url}: {}", res.json);
    res.json
}

async fn get(app: &Router, url: &str) -> Res {
    send(app, "GET", url, None).await
}

async fn patch(app: &Router, url: &str, body: Value) -> Res {
    send(app, "PATCH", url, Some(&body.to_string())).await
}

/// The API with nobody signed in. Build it after the schemas are made, so its cache has them.
async fn visitor(pool: &PgPool) -> Router {
    tachyon_api::router(pool.clone()).await.unwrap()
}

/// The `title`s of a list response, sorted.
fn titles(res: &Res) -> Vec<String> {
    assert_eq!(res.status, 200, "{}", res.json);
    let mut out: Vec<String> = res.json["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["title"].as_str().unwrap_or_default().to_owned())
        .collect();
    out.sort();
    out
}

fn id(doc: &Value) -> String {
    doc["id"].as_str().unwrap().to_owned()
}

struct Blog {
    admin: Router,
    ann: String,
    hid: String,
    pub1: String,
    pub2: String,
    draft1: String,
}

/// Authors and posts that only show what's published. Ann is published, Hid isn't.
/// `Pub one` (by Ann) links to `Pub two`; `Draft one` (by Ann) links to `Pub one`;
/// `Pub two` (by Hid) links to `Draft one`, and names Hid and `Draft one` in its lists.
async fn blog(pool: &PgPool) -> Blog {
    let admin = admin_app(pool.clone()).await;
    let published = json!({"read": {"stage": "published"}});
    let stage = json!({"name": "stage", "type": "select", "options": ["draft", "published"]});
    let author_fields = json!([{"name": "name", "type": "text"}, stage]);
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "authors", "access": published, "fields": author_fields}),
    )
    .await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "posts", "access": published, "fields": [
            {"name": "title", "type": "text"},
            stage,
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "authorName", "type": "text", "virtual": "author.name"},
            {"name": "related", "type": "relationship", "relationTo": "posts"},
            {"name": "people", "type": "relationship", "relationTo": ["authors", "posts"], "hasMany": true},
            {"name": "credits", "type": "array", "fields": [
                {"name": "who", "type": "relationship", "relationTo": "authors"}
            ]}
        ]}),
    )
    .await;
    let mut with_join = author_fields.clone();
    with_join
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "posts", "type": "join", "collection": "posts", "on": "author"}));
    let change = json!({"schema": {"slug": "authors", "access": published, "fields": with_join}});
    let res = send(
        &admin,
        "PUT",
        "/api/admin/collections/authors",
        Some(&change.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "settings", "kind": "global", "access": {"read": {"signedIn": true}},
               "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;

    let ann = id(&create(
        &admin,
        "/api/authors",
        json!({"name": "Ann", "stage": "published"}),
    )
    .await);
    let hid = id(&create(
        &admin,
        "/api/authors",
        json!({"name": "Hid", "stage": "draft"}),
    )
    .await);
    let post = |title: &str, stage: &str, author: &str| json!({"title": title, "stage": stage, "author": author});
    let pub1 = id(&create(&admin, "/api/posts", post("Pub one", "published", &ann)).await);
    let mut d = post("Draft one", "draft", &ann);
    d["related"] = pub1.clone().into();
    let draft1 = id(&create(&admin, "/api/posts", d).await);
    let mut p = post("Pub two", "published", &hid);
    p["related"] = draft1.clone().into();
    p["people"] = json!([
        {"relationTo": "authors", "value": hid},
        {"relationTo": "posts", "value": draft1}
    ]);
    p["credits"] = json!([{"who": hid}]);
    let pub2 = id(&create(&admin, "/api/posts", p).await);
    let res = patch(
        &admin,
        &format!("/api/posts/{pub1}"),
        json!({"related": pub2}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    Blog {
        admin,
        ann,
        hid,
        pub1,
        pub2,
        draft1,
    }
}

/// Filters through links: `(query, what an Admin sees)`. A visitor sees none of them,
/// because each match is only through a hidden document.
const PROBES: &[(&str, &str)] = &[
    ("where[author.name][equals]=Hid", "Pub two"),
    ("where[author.id][equals]={hid}", "Pub two"),
    ("where[authorName][equals]=Hid", "Pub two"),
    ("where[related.title][equals]=Draft%20one", "Pub two"),
    ("where[related.stage][equals]=draft", "Pub two"),
    ("where[related.author.name][equals]=Hid", "Pub one"),
    (
        "where[related.related.title][equals]=Draft%20one",
        "Pub one",
    ),
    ("where[people.stage][equals]=draft", "Pub two"),
    ("where[credits.who.name][equals]=Hid", "Pub two"),
    (
        "where[or][0][author.name][equals]=Hid&where[or][1][title][equals]=nope",
        "Pub two",
    ),
];

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn published_only_reads_on_every_path(pool: PgPool) {
    let b = blog(&pool).await;
    let anon = visitor(&pool).await;
    let (pub1, pub2, draft1, ann, hid) = (&b.pub1, &b.pub2, &b.draft1, &b.ann, &b.hid);

    // Lists, with and without a filter of the reader's own.
    assert_eq!(
        titles(&get(&anon, "/api/posts").await),
        ["Pub one", "Pub two"]
    );
    let counted = get(&anon, "/api/posts?count=true").await;
    assert_eq!(counted.json["totalDocs"], 2);
    assert!(titles(&get(&anon, "/api/posts?where[stage][equals]=draft").await).is_empty());
    assert_eq!(
        titles(&get(&b.admin, "/api/posts").await),
        ["Draft one", "Pub one", "Pub two"]
    );
    let authors = get(&anon, "/api/authors").await;
    assert_eq!(authors.json["docs"].as_array().unwrap().len(), 1);
    assert_eq!(authors.json["docs"][0]["name"], "Ann");

    // Single reads: a hidden document is a 404.
    assert_eq!(get(&anon, &format!("/api/posts/{pub1}")).await.status, 200);
    assert_problem(&get(&anon, &format!("/api/posts/{draft1}")).await, 404);
    assert_problem(&get(&anon, &format!("/api/authors/{hid}")).await, 404);

    // References only list what the reader may read, and a hidden document has none.
    let refs = |doc: &str| format!("/api/posts/{doc}/references");
    assert_eq!(
        get(&b.admin, &refs(pub1)).await.json["docs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(get(&anon, &refs(pub1)).await.json["docs"], json!([]));
    let seen = get(&anon, &refs(pub2)).await;
    assert_eq!(seen.json["docs"][0]["id"], pub1.as_str(), "{}", seen.json);
    assert_problem(&get(&anon, &refs(draft1)).await, 404);

    // Populated links: a hidden one stays an ID, at every depth and in every kind of link.
    let two = get(&anon, &format!("/api/posts/{pub2}?depth=2")).await.json;
    assert_eq!(two["author"], hid.as_str());
    assert_eq!(two["related"], draft1.as_str());
    assert_eq!(two["people"][0]["value"], hid.as_str());
    assert_eq!(two["people"][1]["value"], draft1.as_str());
    assert_eq!(two["credits"][0]["who"], hid.as_str());
    let one = get(&anon, &format!("/api/posts/{pub1}?depth=2")).await.json;
    assert_eq!(one["author"]["name"], "Ann");
    assert_eq!(one["related"]["title"], "Pub two");
    assert_eq!(
        one["related"]["author"],
        hid.as_str(),
        "depth 2 is guarded too"
    );
    let admin_two = get(&b.admin, &format!("/api/posts/{pub2}")).await.json;
    assert_eq!(admin_two["author"]["name"], "Hid");

    // Virtual fields read nothing from a hidden document.
    assert!(two.get("authorName").is_none(), "{two}");
    assert_eq!(one["authorName"], "Ann");
    assert_eq!(one["related"]["authorName"], Value::Null);
    assert_eq!(admin_two["authorName"], "Hid");

    // Joins only hold what the reader may read, and count only that.
    let ann_doc = get(
        &anon,
        &format!("/api/authors/{ann}?joins[posts][count]=true"),
    )
    .await;
    assert_eq!(ann_doc.status, 200, "{}", ann_doc.json);
    let joined: Vec<&str> = ann_doc.json["posts"]["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["title"].as_str().unwrap())
        .collect();
    assert_eq!(joined, ["Pub one"]);
    assert_eq!(ann_doc.json["posts"]["totalDocs"], 1);
    let filtered = get(
        &anon,
        &format!("/api/authors/{ann}?joins[posts][where][related.title][equals]=Draft%20one"),
    )
    .await;
    assert_eq!(
        filtered.json["posts"]["docs"],
        json!([]),
        "{}",
        filtered.json
    );
    let admin_ann = get(&b.admin, &format!("/api/authors/{ann}")).await;
    assert_eq!(admin_ann.json["posts"]["docs"].as_array().unwrap().len(), 2);
    let listed = get(&anon, "/api/authors?joins[posts][count]=true").await;
    assert_eq!(listed.json["docs"][0]["posts"]["totalDocs"], 1);

    // Filters through relationships can't find a document by what a hidden one holds.
    for (query, admin_sees) in PROBES {
        let url = format!("/api/posts?{}", query.replace("{hid}", hid));
        assert_eq!(titles(&get(&b.admin, &url).await), [*admin_sees], "{url}");
        assert!(titles(&get(&anon, &url).await).is_empty(), "{url}");
    }

    // Select and populate shaping don't bring a hidden document back.
    let shaped = get(
        &anon,
        "/api/posts?select[title]=true&select[author]=true&select[authorName]=true&populate[authors][name]=true",
    )
    .await;
    for doc in shaped.json["docs"].as_array().unwrap() {
        match doc["title"].as_str().unwrap() {
            "Pub one" => {
                assert_eq!(doc["author"]["name"], "Ann");
                assert_eq!(doc["authorName"], "Ann");
            }
            _ => {
                assert_eq!(doc["author"], hid.as_str());
                assert!(doc.get("authorName").is_none(), "{doc}");
            }
        }
    }
    let shaped_one = get(
        &anon,
        &format!("/api/posts/{pub2}?select[related]=true&populate[posts][title]=true"),
    )
    .await;
    assert_eq!(shaped_one.json["related"], draft1.as_str());

    // A global's read rule.
    assert_problem(&get(&anon, "/api/globals/settings").await, 401);
    let viewer = app_as(&pool, "viv@example.com", "viewer").await;
    assert_eq!(get(&viewer, "/api/globals/settings").await.status, 200);

    // A rule on a profile field: each reader sees their team's. Someone with no team, and a
    // visitor, are refused rather than shown everything (or an error).
    let users = json!({"schema": {"slug": "users", "fields": [
        {"name": "name", "type": "text", "required": true},
        {"name": "team", "type": "text"},
        {"name": "fav", "type": "relationship", "relationTo": "posts"}
    ]}});
    let res = send(
        &b.admin,
        "PUT",
        "/api/admin/collections/users",
        Some(&users.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    create(
        &b.admin,
        "/api/admin/collections",
        json!({"slug": "boards", "access": {"read": {"team": "$user.team"}},
               "fields": [{"name": "title", "type": "text"}, {"name": "team", "type": "text"}]}),
    )
    .await;
    for team in ["red", "blue"] {
        create(
            &b.admin,
            "/api/boards",
            json!({"title": format!("{team} board"), "team": team}),
        )
        .await;
    }
    let rex = add_user(&pool, "rex@example.com", "viewer").await;
    let res = patch(
        &b.admin,
        &format!("/api/users/{rex}"),
        json!({"team": "red"}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let rex = signed_in(visitor(&pool).await, "rex@example.com").await;
    assert_eq!(titles(&get(&rex, "/api/boards").await), ["red board"]);
    let teamless = signed_in(visitor(&pool).await, "viv@example.com").await;
    assert_problem(&get(&teamless, "/api/boards").await, 403);
    assert_problem(&get(&visitor(&pool).await, "/api/boards").await, 401);

    // Linking to a hidden document is refused exactly like linking to none, on create and
    // on change, in every kind of link; only the request ID differs.
    for global in [
        json!({"slug": "front", "kind": "global",
               "fields": [{"name": "feature", "type": "relationship", "relationTo": "posts"}]}),
        json!({"slug": "notice", "kind": "global", "access": {"read": {"stage": "published"}},
               "fields": [{"name": "stage", "type": "select", "options": ["draft", "published"]}]}),
    ] {
        create(&b.admin, "/api/admin/collections", global).await;
    }
    let res = patch(
        &b.admin,
        "/api/globals/notice",
        json!({"stage": "published"}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let wes = add_user(&pool, "wes@example.com", "author").await;
    let writer = signed_in(visitor(&pool).await, "wes@example.com").await;
    let random = uuid::Uuid::now_v7().to_string();
    let without_id = |r: &Res| {
        let mut json = r.json.clone();
        json.as_object_mut().unwrap().remove("requestId");
        json
    };
    let mine = id(&create(
        &writer,
        "/api/posts",
        json!({"title": "w", "stage": "published"}),
    )
    .await);
    for (hidden, missing) in [
        (json!({"author": hid}), json!({"author": random})),
        (json!({"related": draft1}), json!({"related": random})),
        (
            json!({"people": [{"relationTo": "authors", "value": ann}, {"relationTo": "posts", "value": draft1}]}),
            json!({"people": [{"relationTo": "authors", "value": ann}, {"relationTo": "posts", "value": random}]}),
        ),
        (
            json!({"credits": [{"who": hid}]}),
            json!({"credits": [{"who": random}]}),
        ),
    ] {
        let a = send(&writer, "POST", "/api/posts", Some(&hidden.to_string())).await;
        let b = send(&writer, "POST", "/api/posts", Some(&missing.to_string())).await;
        assert_problem(&a, 422);
        assert_eq!(without_id(&a), without_id(&b), "{hidden}");
        let a = patch(&writer, &format!("/api/posts/{mine}"), hidden.clone()).await;
        let b = patch(&writer, &format!("/api/posts/{mine}"), missing).await;
        assert_problem(&a, 422);
        assert_eq!(without_id(&a), without_id(&b), "{hidden}");
    }
    // Links already on a document stay: an Editor can retitle Pub two, which holds hidden
    // ones, and keep them.
    let editor = app_as(&pool, "eve@example.com", "editor").await;
    let res = patch(
        &editor,
        &format!("/api/posts/{pub2}"),
        json!({"title": "Pub 2"}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["author"], hid.as_str());

    // The same on a global's link, and on a profile's.
    let same = |a: Res, b: Res| {
        assert_problem(&a, 422);
        assert_eq!(without_id(&a), without_id(&b));
    };
    same(
        patch(&editor, "/api/globals/front", json!({"feature": draft1})).await,
        patch(&editor, "/api/globals/front", json!({"feature": random})).await,
    );
    let me = format!("/api/users/{wes}");
    same(
        patch(&writer, &me, json!({"fav": draft1})).await,
        patch(&writer, &me, json!({"fav": random})).await,
    );
    // A refused profile leaves the account alone too: the new email isn't kept.
    let res = patch(
        &writer,
        &me,
        json!({"email": "new@example.com", "currentPassword": common::PASSWORD, "fav": draft1}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(get(&b.admin, &me).await.json["email"], "wes@example.com");

    // A change that leaves a global unreadable to its writer answers with its ID only.
    let res = patch(&editor, "/api/globals/notice", json!({"stage": "draft"})).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json.as_object().unwrap().len(), 1, "{}", res.json);
    assert!(res.json["id"].is_string());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn update_rules_on_own_documents(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let own = json!({"author": "$user.id"});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "articles",
        "access": {"read": {"stage": "published"}, "update": own, "delete": own},
        "fields": [
            {"name": "title", "type": "text"},
            {"name": "stage", "type": "select", "options": ["draft", "published"]},
            {"name": "author", "type": "relationship", "relationTo": "users"},
            {"name": "see", "type": "relationship", "relationTo": "articles"}
        ]}),
    )
    .await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "settings", "kind": "global", "access": {"update": {"role": "author"}},
               "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    let amy_id = add_user(&pool, "amy@example.com", "author").await;
    let bob_id = add_user(&pool, "bob@example.com", "author").await;
    let viv_id = add_user(&pool, "viv@example.com", "viewer").await;
    add_user(&pool, "ed@example.com", "editor").await;
    let article = |title: &str, stage: &str, author: uuid::Uuid| json!({"title": title, "stage": stage, "author": author});
    // All made by the Admin, so the Author role's own-only default would refuse them all.
    let a1 = id(&create(&admin, "/api/articles", article("a1", "published", amy_id)).await);
    let b1 = id(&create(&admin, "/api/articles", article("b1", "published", bob_id)).await);
    let bdraft = id(&create(&admin, "/api/articles", article("bd", "draft", bob_id)).await);
    let v1 = id(&create(&admin, "/api/articles", article("v1", "published", viv_id)).await);

    let app = || tachyon_api::router(pool.clone());
    let amy = signed_in(app().await.unwrap(), "amy@example.com").await;
    let viv = signed_in(app().await.unwrap(), "viv@example.com").await;
    let ed = signed_in(app().await.unwrap(), "ed@example.com").await;
    let url = |id: &str| format!("/api/articles/{id}");
    let rename = json!({"title": "renamed"});

    // The rule replaces the role default, for every role it covers.
    let res = patch(&amy, &url(&a1), rename.clone()).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["title"], "renamed");
    assert_eq!(patch(&viv, &url(&v1), rename.clone()).await.status, 200);
    assert_problem(&patch(&ed, &url(&b1), rename.clone()).await, 403);
    // Someone else's document: readable, so a 403; hidden from the reader, a 404.
    assert_problem(&patch(&amy, &url(&b1), rename.clone()).await, 403);
    assert_problem(&send(&amy, "DELETE", &url(&b1), None).await, 403);
    assert_problem(&patch(&amy, &url(&bdraft), rename.clone()).await, 404);
    assert_problem(&send(&amy, "DELETE", &url(&bdraft), None).await, 404);
    let stored = get(&admin, &url(&b1)).await;
    assert_eq!(stored.json["title"], "b1", "refused writes change nothing");
    // Giving a document away is a change to your own (any existing user may be linked, but
    // not a made-up one); after that it isn't yours.
    let nobody = json!({"author": uuid::Uuid::now_v7()});
    assert_problem(&patch(&amy, &url(&a1), nobody).await, 422);
    let res = patch(&amy, &url(&a1), json!({"author": bob_id})).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_problem(&patch(&amy, &url(&a1), rename.clone()).await, 403);
    assert_eq!(send(&viv, "DELETE", &url(&v1), None).await.status, 204);
    // No create rule: the role default stands.
    assert_eq!(
        send(&amy, "POST", "/api/articles", Some(r#"{"title": "new"}"#))
            .await
            .status,
        201
    );
    assert_problem(
        &send(&viv, "POST", "/api/articles", Some(r#"{"title": "new"}"#)).await,
        403,
    );

    // The author changes while Amy's write waits on the row: her write judges the row it
    // locked, so it's refused.
    let mine = id(&create(
        &admin,
        "/api/articles",
        article("mine", "published", amy_id),
    )
    .await);
    let mut other = pool.begin().await.unwrap();
    sqlx::query("UPDATE documents SET published = jsonb_set(published, '{author}', to_jsonb($2::text)) WHERE id = $1::uuid")
        .bind(&mine)
        .bind(bob_id.to_string())
        .execute(&mut *other)
        .await
        .unwrap();
    let write = {
        let (amy, url) = (amy.clone(), url(&mine));
        tokio::spawn(async move { patch(&amy, &url, json!({"title": "stolen"})).await })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        assert!(!write.is_finished(), "the write never waited for the lock");
        assert!(
            std::time::Instant::now() < deadline,
            "the write didn't reach the lock in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    other.commit().await.unwrap();
    assert_problem(&write.await.unwrap(), 403);
    assert_eq!(get(&admin, &url(&mine)).await.json["title"], "mine");

    // A change that leaves the document unreadable answers with its ID only.
    let gone = id(&create(
        &admin,
        "/api/articles",
        article("gone", "published", amy_id),
    )
    .await);
    let res = patch(&amy, &url(&gone), json!({"stage": "draft"})).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json, json!({"id": gone}));

    // A refused delete doesn't count links from documents the caller can't see; an Admin's
    // does.
    let linked = id(&create(
        &admin,
        "/api/articles",
        article("linked", "published", amy_id),
    )
    .await);
    let mut hidden = article("hidden", "draft", bob_id);
    hidden["see"] = linked.clone().into();
    create(&admin, "/api/articles", hidden).await;
    let res = send(&amy, "DELETE", &url(&linked), None).await;
    assert_problem(&res, 409);
    let detail = res.json["detail"].as_str().unwrap();
    assert!(!detail.chars().any(|c| c.is_ascii_digit()), "{detail}");
    let res = send(&admin, "DELETE", &url(&linked), None).await;
    assert_problem(&res, 409);
    assert!(
        res.json["detail"]
            .as_str()
            .unwrap()
            .starts_with("1 other document"),
        "{}",
        res.json
    );
    // An Admin's key narrowed by its scopes isn't told the count either.
    let key = create(
        &admin,
        "/api/auth/keys",
        json!({"name": "k", "scopes": ["articles:write"]}),
    )
    .await;
    let req = Request::delete(url(&linked)).header(
        "authorization",
        format!("Bearer {}", key["key"].as_str().unwrap()),
    );
    let res = send_with(&admin, req, None).await;
    assert_problem(&res, 409);
    let detail = res.json["detail"].as_str().unwrap();
    assert!(!detail.chars().any(|c| c.is_ascii_digit()), "{detail}");

    // A global's update rule replaces the default too.
    let name = json!({"name": "x"});
    assert_eq!(
        patch(&amy, "/api/globals/settings", name.clone())
            .await
            .status,
        200
    );
    assert_problem(&patch(&ed, "/api/globals/settings", name).await, 403);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn public_create_rule_lets_visitors_post(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let body = json!([{"name": "body", "type": "text"}]);
    for (slug, access) in [
        ("messages", json!({"create": true, "read": false})),
        ("members", json!({"create": {"signedIn": true}})),
        ("memos", json!({"create": {"role": "editor"}})),
        ("closed", json!({})),
    ] {
        create(
            &admin,
            "/api/admin/collections",
            json!({"slug": slug, "access": access, "fields": body}),
        )
        .await;
    }
    let anon = visitor(&pool).await;
    let viewer = app_as(&pool, "viv@example.com", "viewer").await;
    let author = app_as(&pool, "amy@example.com", "author").await;
    let post = |app: &Router, slug: &str| {
        let (app, url) = (app.clone(), format!("/api/{slug}"));
        async move { send(&app, "POST", &url, Some(r#"{"body": "hello"}"#)).await }
    };

    let res = post(&anon, "messages").await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert!(res.json["id"].is_string(), "{}", res.json);
    assert_eq!(
        res.json,
        json!({"id": res.json["id"]}),
        "visitors can't read messages, so they get only the ID back"
    );
    assert_eq!(post(&viewer, "messages").await.status, 201);
    // Nobody but an Admin may read them back.
    assert_problem(&get(&anon, "/api/messages").await, 401);
    assert_problem(&get(&viewer, "/api/messages").await, 403);
    assert_problem(
        &get(&anon, &format!("/api/messages/{}", id(&res.json))).await,
        404,
    );
    assert_eq!(
        get(&admin, "/api/messages").await.json["docs"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    assert_problem(&post(&anon, "members").await, 401);
    let res = post(&viewer, "members").await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["body"], "hello", "readable, so shown in full");
    assert_problem(&post(&author, "memos").await, 403);
    // Without a rule, visitors still can't post.
    assert_problem(&post(&anon, "closed").await, 401);
    assert_eq!(post(&author, "closed").await.status, 201);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn admins_pass_every_rule(pool: PgPool) {
    let b = blog(&pool).await;
    let no = json!({"read": false, "create": false, "update": false, "delete": false});
    create(
        &b.admin,
        "/api/admin/collections",
        json!({"slug": "locked", "access": no, "fields": [{"name": "title", "type": "text"}]}),
    )
    .await;
    create(
        &b.admin,
        "/api/admin/collections",
        json!({"slug": "frozen", "kind": "global", "access": {"read": false, "update": false},
               "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    let admin = &b.admin;
    let doc = create(admin, "/api/locked", json!({"title": "x"})).await;
    let url = format!("/api/locked/{}", id(&doc));
    assert_eq!(
        get(admin, "/api/locked").await.json["docs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(get(admin, &url).await.status, 200);
    assert_eq!(patch(admin, &url, json!({"title": "y"})).await.status, 200);
    assert_eq!(send(admin, "DELETE", &url, None).await.status, 204);
    assert_eq!(
        patch(admin, "/api/globals/frozen", json!({"name": "n"}))
            .await
            .status,
        200
    );
    assert_eq!(get(admin, "/api/globals/frozen").await.json["name"], "n");

    // Everything the published-only rule hides from others.
    assert_eq!(
        get(admin, &format!("/api/posts/{}", b.draft1)).await.status,
        200
    );
    assert_eq!(
        get(admin, &format!("/api/authors/{}", b.hid)).await.status,
        200
    );
    // References include globals that link here.
    create(
        admin,
        "/api/admin/collections",
        json!({"slug": "home", "kind": "global",
               "fields": [{"name": "feature", "type": "relationship", "relationTo": "posts"}]}),
    )
    .await;
    let res = patch(admin, "/api/globals/home", json!({"feature": b.draft1})).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let refs = get(admin, &format!("/api/posts/{}/references", b.draft1)).await;
    let mut from: Vec<&str> = refs.json["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["collection"].as_str().unwrap())
        .collect();
    from.sort();
    from.dedup();
    assert_eq!(from, ["home", "posts"], "{}", refs.json);
    assert!(
        refs.json["docs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == b.pub2.as_str())
    );
    for (query, admin_sees) in PROBES {
        let url = format!("/api/posts?{}", query.replace("{hid}", &b.hid));
        assert_eq!(titles(&get(admin, &url).await), [*admin_sees], "{url}");
    }

    // An Admin's key, scoped to everything, passes the rules too.
    let key = create(
        admin,
        "/api/auth/keys",
        json!({"name": "all", "scopes": ["*:write"]}),
    )
    .await;
    let key = key["key"].as_str().unwrap();
    let req = Request::get(format!("/api/posts/{}", b.draft1))
        .header("authorization", format!("Bearer {key}"));
    assert_eq!(
        send_with(&visitor(&pool).await, req, None).await.status,
        200
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn keys_stay_narrowed_by_scopes(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let open = json!({"read": true, "create": true, "update": true, "delete": true});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "authors", "access": open, "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    for slug in ["posts", "pages"] {
        create(
            &admin,
            "/api/admin/collections",
            json!({"slug": slug, "access": open, "fields": [
                {"name": "title", "type": "text"},
                {"name": "author", "type": "relationship", "relationTo": "authors"},
                {"name": "authorName", "type": "text", "virtual": "author.name"}
            ]}),
        )
        .await;
    }
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "settings", "kind": "global", "access": {"read": true, "update": true},
               "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    let ann = id(&create(&admin, "/api/authors", json!({"name": "Ann"})).await);
    let post = id(&create(&admin, "/api/posts", json!({"title": "p", "author": ann})).await);
    let page = id(&create(&admin, "/api/pages", json!({"title": "q"})).await);
    let key = |scopes: Value| {
        let admin = admin.clone();
        async move {
            let made = create(
                &admin,
                "/api/auth/keys",
                json!({"name": "k", "scopes": scopes}),
            )
            .await;
            made["key"].as_str().unwrap().to_owned()
        }
    };
    let read_posts = key(json!(["posts:read"])).await;
    let read_all = key(json!(["*:read"])).await;
    let write_posts = key(json!(["posts:write"])).await;
    let app = visitor(&pool).await;
    let call = |key: &str, method: &str, url: &str, body: Option<Value>| {
        let (app, key, method, url) = (
            app.clone(),
            key.to_owned(),
            method.to_owned(),
            url.to_owned(),
        );
        async move {
            let req = Request::builder()
                .method(method.as_str())
                .uri(url)
                .header("authorization", format!("Bearer {key}"));
            send_with(&app, req, body.map(|b| b.to_string()).as_deref()).await
        }
    };
    let post_url = format!("/api/posts/{post}");
    let title = || Some(json!({"title": "x"}));

    // Rules open to everyone, yet a key reads and writes only what its scopes name.
    let res = call(&read_posts, "GET", &post_url, None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        res.json["author"],
        ann.as_str(),
        "authors aren't in its scopes"
    );
    assert!(res.json.get("authorName").is_none(), "{}", res.json);
    let found = call(
        &read_posts,
        "GET",
        "/api/posts?where[author.name][equals]=Ann",
        None,
    )
    .await;
    assert_eq!(found.json["docs"], json!([]));
    assert_problem(&call(&read_posts, "GET", "/api/pages", None).await, 403);
    assert_problem(
        &call(&read_posts, "GET", &format!("/api/pages/{page}"), None).await,
        403,
    );
    assert_problem(
        &call(&read_posts, "GET", "/api/globals/settings", None).await,
        403,
    );
    assert_problem(&call(&read_posts, "POST", "/api/posts", title()).await, 403);
    assert_problem(&call(&read_posts, "PATCH", &post_url, title()).await, 403);
    assert_problem(&call(&read_posts, "DELETE", &post_url, None).await, 403);
    assert_problem(
        &call(&read_posts, "PATCH", "/api/globals/settings", title()).await,
        403,
    );

    let res = call(&read_all, "GET", &post_url, None).await;
    assert_eq!(res.json["author"]["name"], "Ann");
    assert_eq!(res.json["authorName"], "Ann");
    let found = call(
        &read_all,
        "GET",
        "/api/posts?where[author.name][equals]=Ann",
        None,
    )
    .await;
    assert_eq!(found.json["docs"].as_array().unwrap().len(), 1);
    assert_problem(&call(&read_all, "PATCH", &post_url, title()).await, 403);

    assert_eq!(
        call(&write_posts, "PATCH", &post_url, title()).await.status,
        200
    );
    assert_problem(
        &call(
            &write_posts,
            "PATCH",
            &format!("/api/pages/{page}"),
            title(),
        )
        .await,
        403,
    );
}

/// Every page of a list, following `nextCursor`: the titles per page, and the total.
async fn pages(app: &Router, query: &str) -> (Vec<Vec<String>>, Value) {
    let mut out = Vec::new();
    let mut after = String::new();
    loop {
        let res = get(
            app,
            &format!("/api/posts?{query}&limit=2&count=true{after}"),
        )
        .await;
        let total = res.json["totalDocs"].clone();
        let page: Vec<String> = res.json["docs"]
            .as_array()
            .unwrap_or_else(|| panic!("{query}: {}", res.json))
            .iter()
            .map(|d| d["title"].as_str().unwrap().to_owned())
            .collect();
        out.push(page);
        match res.json["nextCursor"].as_str() {
            Some(c) => after = format!("&after={c}"),
            None => return (out, total),
        }
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn filters_through_links_respect_the_linked_read_rule(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let published = json!({"read": {"stage": "published"}});
    let stage = json!({"name": "stage", "type": "select", "options": ["draft", "published"]});
    let link =
        |name: &str, to: &str| json!({"name": name, "type": "relationship", "relationTo": to});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "authors", "access": published, "fields": [
            {"name": "name", "type": "text"}, stage,
            link("mentor", "authors"),
            {"name": "age", "type": "number"},
            {"name": "tags", "type": "text", "hasMany": true},
            {"name": "friends", "type": "relationship", "relationTo": "authors", "hasMany": true}
        ]}),
    )
    .await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "posts", "access": {"read": true}, "fields": [
            {"name": "title", "type": "text"},
            link("author", "authors"),
            {"name": "writers", "type": "relationship", "relationTo": "authors", "hasMany": true},
            {"name": "credits", "type": "array", "fields": [link("who", "authors")]}
        ]}),
    )
    .await;
    let author = |name: &str,
                  age: u32,
                  stage: &str,
                  mentor: Option<&str>,
                  tags: &[&str],
                  friends: &[&str]| {
        json!({"name": name, "age": age, "stage": stage, "mentor": mentor, "tags": tags, "friends": friends})
    };
    let ann = id(&create(
        &admin,
        "/api/authors",
        author("Ann", 30, "published", None, &["y"], &[]),
    )
    .await);
    // Hid is a hidden author with a mentor of his own, so Mia -> Hid -> Ann has a hidden middle.
    let hid = id(&create(
        &admin,
        "/api/authors",
        author("Hid", 50, "draft", Some(&ann), &["x"], &[&ann]),
    )
    .await);
    let mia = id(&create(
        &admin,
        "/api/authors",
        author("Mia", 20, "published", Some(&hid), &["z"], &[]),
    )
    .await);
    let bo = id(&create(
        &admin,
        "/api/authors",
        author("Bo", 40, "published", Some(&ann), &["x", "w"], &[&ann]),
    )
    .await);
    let (ann, hid, mia, bo) = (ann.as_str(), hid.as_str(), mia.as_str(), bo.as_str());
    let posts = [
        ("p1", Some(ann), vec![ann], vec![]),
        ("p2", Some(hid), vec![hid], vec![]),
        ("p3", Some(ann), vec![ann, hid], vec![hid]),
        ("p4", None, vec![], vec![]),
        ("p5", Some(mia), vec![mia], vec![hid]),
        ("p6", Some(bo), vec![bo], vec![ann]),
        ("p7", Some(hid), vec![], vec![hid, ann]),
    ];
    // A post's JSON, leaving out every link to `skip`.
    let body = |title: &str, a: Option<&str>, writers: &[&str], credits: &[&str], skip: &str| {
        let credits: Vec<Value> = credits
            .iter()
            .filter(|w| **w != skip)
            .map(|w| json!({"who": w}))
            .collect();
        let writers: Vec<&&str> = writers.iter().filter(|w| **w != skip).collect();
        json!({"title": title, "author": a.filter(|a| *a != skip), "writers": writers, "credits": credits})
    };
    let mut made = Vec::new();
    for (title, a, writers, credits) in &posts {
        let doc = create(&admin, "/api/posts", body(title, *a, writers, credits, "")).await;
        made.push(id(&doc));
    }
    let anon = visitor(&pool).await;

    // `(query, differs)`: whether an admin sees something different from a visitor, which
    // is true when a hidden author could satisfy or fail the filter.
    let queries: &[(&str, bool)] = &[
        // Single link, has-many, a link inside a list row, and two hops.
        ("where[author.name][equals]=Hid", true),
        ("where[author.id][equals]={hid}", true),
        ("where[author.stage][equals]=draft", true),
        ("where[writers.name][equals]=Hid", true),
        ("where[writers.id][in]={hid}", true),
        ("where[credits.who.name][equals]=Hid", true),
        ("where[author.mentor.name][equals]=Hid", true),
        ("where[author.mentor.stage][equals]=draft", true),
        // A hidden document in the middle of a chain (Mia -> Hid -> Ann), and a visible
        // chain beside it (Bo -> Ann).
        ("where[author.mentor.name][equals]=Ann", true),
        ("where[author.mentor.stage][equals]=published", true),
        ("where[author.mentor.mentor.name][equals]=Ann", true),
        ("where[author.mentor.mentor.id][equals]={ann}", true),
        ("where[author.mentor.mentor.name][not_equals]=Ann", true),
        // Other paths to the same document, and ranges, patterns and membership.
        ("where[credits.who.id][equals]={hid}", true),
        ("where[credits.who.id][in]={hid},{ann}", true),
        ("where[credits.who.age][gt]=45", true),
        ("where[author.age][gt]=45", true),
        ("where[author.age][gte]=50", true),
        ("where[author.age][lt]=55", true),
        ("where[author.age][lte]=50", true),
        ("where[writers.age][gt]=45", true),
        ("where[author.mentor.age][gt]=45", true),
        ("where[author.name][like]=Hi", true),
        ("where[author.age][not_in]=50", true),
        // `contains` on a list of the linked document: its tags and its own links.
        ("where[author.tags][contains]=x", true),
        ("where[writers.tags][contains]=x", true),
        ("where[credits.who.tags][contains]=x", true),
        ("where[author.mentor.tags][contains]=x", true),
        ("where[author.mentor.mentor.tags][contains]=y", true),
        ("where[author.friends][contains]={ann}", true),
        ("where[writers.friends][contains]={ann}", true),
        ("where[credits.who.friends][contains]={ann}", true),
        ("where[author.mentor.friends][contains]={ann}", true),
        // Inside `or` and `and`.
        (
            "where[or][0][author.name][equals]=Hid&where[or][1][title][equals]=p1",
            true,
        ),
        (
            "where[or][0][writers.name][equals]=Hid&where[or][1][title][equals]=p4",
            true,
        ),
        (
            "where[and][0][title][like]=p&where[and][1][author.name][equals]=Hid",
            true,
        ),
        // The negative operators count a hidden author as missing.
        ("where[author.name][not_equals]=Ann", false),
        ("where[author.name][not_equals]=Hid", true),
        ("where[author.id][not_in]={hid}", true),
        ("where[author.name][not_in]=Hid,Ann", true),
        ("where[writers.name][not_equals]=Hid", true),
        ("where[credits.who.name][not_equals]=Hid", true),
        ("where[author.mentor.name][not_equals]=Hid", true),
        ("where[author.name][exists]=false", true),
        ("where[writers.name][exists]=false", true),
        ("where[author.mentor.name][exists]=false", true),
        // Nothing hidden is involved, so a visitor sees what anyone would.
        ("where[author.name][equals]=Ann", false),
        ("where[writers.name][equals]=Ann", false),
    ];

    // Each answer for a visitor, whole pages and total included.
    let ask = |app: Router| async move {
        let mut seen = Vec::new();
        for (q, _) in queries {
            let q = q.replace("{hid}", hid).replace("{ann}", ann);
            let all = get(&app, &format!("/api/posts?{q}&sort=title")).await;
            seen.push((titles(&all), pages(&app, &format!("{q}&sort=title")).await));
        }
        seen
    };
    let before = ask(anon.clone()).await;
    let admin_before = ask(admin.clone()).await;
    for (i, (q, differs)) in queries.iter().enumerate() {
        assert_eq!(before[i].0 != admin_before[i].0, *differs, "{q}");
    }

    // The same questions with Hid truly gone must answer the same, page by page.
    // (A link to nothing reads the same as no link, and a document can't be deleted while
    // linked, so Hid's links come out first.)
    for ((title, a, writers, credits), doc) in posts.iter().zip(&made) {
        let res = patch(
            &admin,
            &format!("/api/posts/{doc}"),
            body(title, *a, writers, credits, hid),
        )
        .await;
        assert_eq!(res.status, 200, "{}", res.json);
    }
    let res = patch(
        &admin,
        &format!("/api/authors/{mia}"),
        json!({"mentor": null}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = send(&admin, "DELETE", &format!("/api/authors/{hid}"), None).await;
    assert_eq!(res.status, 204, "{}", res.json);
    let after = ask(anon.clone()).await;
    for (i, (q, _)) in queries.iter().enumerate() {
        assert_eq!(before[i], after[i], "{q}");
    }

    // A few answers worth reading outright.
    let url = "/api/posts?where[author.name][equals]=Hid";
    assert!(titles(&get(&anon, url).await).is_empty());
    let url = "/api/posts?where[author.name][not_equals]=Ann";
    assert_eq!(
        titles(&get(&anon, url).await),
        ["p2", "p4", "p5", "p6", "p7"]
    );

    // Sort doesn't cross a link, so there's no order to read a hidden document from.
    for sort in ["author.name", "-author.name"] {
        let res = get(&anon, &format!("/api/posts?sort={sort}")).await;
        assert_eq!(res.status, 400, "sort={sort}: {}", res.json);
    }
    // Sorting by the link field itself orders by the stored ID, which is the post's own data.
    // By ID, so Ann's posts, then Mia's, then Bo's (names would put Bo before Mia), then
    // the posts left with no author.
    let by_link = get(&anon, "/api/posts?sort=author").await;
    let order: Vec<&str> = by_link.json["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["title"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["p3", "p1", "p5", "p6", "p7", "p4", "p2"]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn polymorphic_links_use_each_collections_rule(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let stage = json!({"name": "stage", "type": "select", "options": ["draft", "published"]});
    let fields = json!([{"name": "title", "type": "text"}, stage]);
    // Notes anyone reads, memos nobody, pages only when published.
    for (slug, read) in [
        ("notes", json!(true)),
        ("memos", json!(false)),
        ("pages", json!({"stage": "published"})),
    ] {
        create(
            &admin,
            "/api/admin/collections",
            json!({"slug": slug, "access": {"read": read}, "fields": fields}),
        )
        .await;
    }
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "items", "access": {"read": true}, "fields": [
            {"name": "name", "type": "text"},
            {"name": "ref", "type": "relationship", "relationTo": ["notes", "memos"]},
            {"name": "mix", "type": "relationship", "relationTo": ["notes", "pages"], "hasMany": true}
        ]}),
    )
    .await;
    let doc = |slug: &str, title: &str, stage: &str| {
        let admin = admin.clone();
        let (slug, body) = (slug.to_owned(), json!({"title": title, "stage": stage}));
        async move { id(&create(&admin, &format!("/api/{slug}"), body).await) }
    };
    let note = doc("notes", "T", "draft").await;
    let memo = doc("memos", "T", "published").await;
    let live = doc("pages", "T", "published").await;
    let draft = doc("pages", "T", "draft").await;
    let to = |slug: &str, id: &str| json!({"relationTo": slug, "value": id});
    let items = [
        ("i-note", Some(to("notes", &note)), vec![to("notes", &note)]),
        ("i-memo", Some(to("memos", &memo)), vec![to("notes", &note)]),
        ("i-live", None, vec![to("pages", &live)]),
        ("i-draft", None, vec![to("pages", &draft)]),
        (
            "i-both",
            None,
            vec![to("pages", &draft), to("notes", &note)],
        ),
    ];
    for (name, r, mix) in items {
        create(
            &admin,
            "/api/items",
            json!({"name": name, "ref": r, "mix": mix}),
        )
        .await;
    }
    let anon = visitor(&pool).await;
    let names = |res: &Res| {
        assert_eq!(res.status, 200, "{}", res.json);
        let mut v: Vec<String> = res.json["docs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_owned())
            .collect();
        v.sort();
        v
    };
    let ask = |app: &Router, q: &str| {
        let (app, url) = (app.clone(), format!("/api/items?{q}"));
        async move { get(&app, &url).await }
    };

    // Memos deny everything, and that leaves notes alone in the same link.
    let q = "where[ref.title][equals]=T";
    assert_eq!(names(&ask(&anon, q).await), ["i-note"]);
    assert_eq!(names(&ask(&admin, q).await), ["i-memo", "i-note"]);
    let q = "where[ref.id][equals]=";
    assert!(names(&ask(&anon, &format!("{q}{memo}")).await).is_empty());
    assert_eq!(names(&ask(&admin, &format!("{q}{memo}")).await), ["i-memo"]);
    // A hidden memo counts as no link at all for the negative operators.
    let q = "where[ref.title][not_equals]=T";
    assert_eq!(
        names(&ask(&anon, q).await),
        ["i-both", "i-draft", "i-live", "i-memo"]
    );
    assert_eq!(
        names(&ask(&admin, q).await),
        ["i-both", "i-draft", "i-live"]
    );

    // The pair's own parts are the item's data: a hidden link stays in the body as an ID,
    // so filtering on it tells the reader nothing the body doesn't (it isn't a walk
    // into the memo, which `ref.id` and `ref.title` above are).
    for q in [
        "where[ref.relationTo][equals]=memos".to_owned(),
        format!("where[ref.value][equals]={memo}"),
    ] {
        let res = ask(&anon, &q).await;
        assert_eq!(names(&res), ["i-memo"], "{q}");
        assert_eq!(res.json["docs"][0]["ref"]["value"], memo.as_str());
    }

    // Notes are open and pages are published-only: each link keeps its own collection's rule.
    let q = "where[mix.title][equals]=T";
    assert_eq!(
        names(&ask(&anon, q).await),
        ["i-both", "i-live", "i-memo", "i-note"]
    );
    assert_eq!(
        names(&ask(&admin, q).await),
        ["i-both", "i-draft", "i-live", "i-memo", "i-note"]
    );
    let q = "where[mix.stage][equals]=draft";
    // The note is a draft too, and open; only the draft page stays out of reach.
    assert_eq!(names(&ask(&anon, q).await), ["i-both", "i-memo", "i-note"]);
    assert_eq!(
        names(&ask(&admin, q).await),
        ["i-both", "i-draft", "i-memo", "i-note"]
    );
    let q = format!("where[mix.id][equals]={draft}");
    assert!(names(&ask(&anon, &q).await).is_empty());
    assert_eq!(names(&ask(&admin, &q).await), ["i-both", "i-draft"]);
    let q = format!("where[mix.id][in]={draft},{live}");
    assert_eq!(names(&ask(&anon, &q).await), ["i-live"]);
    assert_eq!(
        names(&ask(&admin, &q).await),
        ["i-both", "i-draft", "i-live"]
    );
    // Negative operators on the has-many link: the draft page counts as no link.
    for q in [
        "where[mix.title][not_equals]=T",
        "where[mix.title][exists]=false",
    ] {
        assert_eq!(names(&ask(&anon, q).await), ["i-draft"], "{q}");
        assert!(names(&ask(&admin, q).await).is_empty(), "{q}");
    }
    let q = format!("where[mix.id][not_in]={draft}");
    assert_eq!(
        names(&ask(&anon, &q).await),
        ["i-both", "i-draft", "i-live", "i-memo", "i-note"]
    );
    assert_eq!(
        names(&ask(&admin, &q).await),
        ["i-live", "i-memo", "i-note"]
    );
    let q = format!("where[mix.id][equals]={note}");
    assert_eq!(names(&ask(&anon, &q).await), ["i-both", "i-memo", "i-note"]);
}

// Field rules. Editors (and Admins) can read every field below; Viewers and Authors can't
// read the ones marked `ed`, and only Editors may write `secret`, `links.url` and so on.

struct Staff {
    admin: Router,
    editor: Router,
    viewer: Router,
    author: Router,
    viewer_id: String,
    ann: String,
    /// Admin's post: by Ann, hidden-linked to Ann, every field set.
    p1: String,
    /// Links to `p1` through `related`.
    p2: String,
}

async fn staff(pool: &PgPool) -> Staff {
    let admin = admin_app(pool.clone()).await;
    let ed = json!({"read": {"role": "editor"}});
    let ed_all = json!({"read": {"role": "editor"}, "create": {"role": "editor"}, "update": {"role": "editor"}});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "people", "fields": [
            {"name": "name", "type": "text"},
            {"name": "salary", "type": "number", "access": ed},
            {"name": "contact", "type": "group", "fields": [
                {"name": "email", "type": "text"},
                {"name": "phone", "type": "text", "access": ed}
            ]}
        ]}),
    )
    .await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "secret", "type": "text", "default": "s3", "access": ed_all},
            {"name": "rank", "type": "number", "access": ed},
            {"name": "author", "type": "relationship", "relationTo": "people"},
            {"name": "authorName", "type": "text", "virtual": "author.name"},
            {"name": "authorSalary", "type": "number", "virtual": "author.salary"},
            {"name": "authorNameEd", "type": "text", "virtual": "author.name", "access": ed},
            {"name": "hiddenAuthor", "type": "relationship", "relationTo": "people", "access": ed},
            {"name": "hiddenName", "type": "text", "virtual": "hiddenAuthor.name"},
            {"name": "related", "type": "relationship", "relationTo": "posts"},
            {"name": "meta", "type": "group", "fields": [
                {"name": "topic", "type": "text"},
                {"name": "note", "type": "text", "access": {"read": {"role": "editor"}, "update": {"role": "editor"}}}
            ]},
            {"name": "links", "type": "array", "fields": [
                {"name": "label", "type": "text"},
                {"name": "url", "type": "text", "access": ed_all}
            ]},
            {"type": "tabs", "tabs": [{"name": "extra", "label": "Extra", "fields": [
                {"name": "code", "type": "text", "access": ed}
            ]}]},
            {"type": "row", "fields": [{"name": "inRow", "type": "text", "access": ed}]},
            // Hidden two and three levels down, beside fields that aren't.
            {"name": "deep", "type": "group", "fields": [
                {"name": "inner", "type": "group", "fields": [
                    {"name": "x", "type": "text", "access": ed},
                    {"name": "y", "type": "text"}
                ]},
                {"name": "rows", "type": "array", "fields": [
                    {"name": "sub", "type": "group", "fields": [
                        {"name": "x", "type": "text", "access": ed},
                        {"name": "y", "type": "text"}
                    ]}
                ]}
            ]}
        ]}),
    )
    .await;
    let change = json!({"schema": {"slug": "people", "fields": [
        {"name": "name", "type": "text"},
        {"name": "salary", "type": "number", "access": ed},
        {"name": "contact", "type": "group", "fields": [
            {"name": "email", "type": "text"},
            {"name": "phone", "type": "text", "access": ed}
        ]},
        {"name": "posts", "type": "join", "collection": "posts", "on": "author", "defaultSort": "rank"},
        {"name": "hiddenPosts", "type": "join", "collection": "posts", "on": "hiddenAuthor"},
        {"name": "edPosts", "type": "join", "collection": "posts", "on": "author", "access": ed}
    ]}});
    let res = send(
        &admin,
        "PUT",
        "/api/admin/collections/people",
        Some(&change.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "site", "kind": "global", "fields": [
            {"name": "name", "type": "text"},
            {"name": "apiKey", "type": "text", "access": {"read": {"role": "editor"}, "update": {"role": "admin"}}},
            {"name": "token", "type": "number", "access": {"read": {"role": "admin"}}}
        ]}),
    )
    .await;
    // Never saved, so reads show its defaults.
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "prefs", "kind": "global", "fields": [
            {"name": "title", "type": "text", "default": "p"},
            {"name": "hint", "type": "text", "default": "h", "access": ed}
        ]}),
    )
    .await;
    let users = json!({"schema": {"slug": "users", "fields": [
        {"name": "name", "type": "text", "required": true},
        {"name": "pay", "type": "number", "access": ed_all}
    ]}});
    let res = send(
        &admin,
        "PUT",
        "/api/admin/collections/users",
        Some(&users.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);

    let ann = id(&create(
        &admin,
        "/api/people",
        json!({"name": "Ann", "salary": 100, "contact": {"email": "a@x", "phone": "555"}}),
    )
    .await);
    let p1 = id(&create(
        &admin,
        "/api/posts",
        json!({"title": "One", "secret": "s1", "rank": 1, "author": ann, "hiddenAuthor": ann,
               "meta": {"topic": "t", "note": "n"}, "links": [{"label": "l", "url": "u"}],
               "extra": {"code": "c"}, "inRow": "r",
               "deep": {"inner": {"x": "x", "y": "y"}, "rows": [{"sub": {"x": "x", "y": "y"}}]}}),
    )
    .await);
    let p2 = id(&create(
        &admin,
        "/api/posts",
        json!({"title": "Two", "rank": 2, "author": ann, "related": p1}),
    )
    .await);
    let res = patch(
        &admin,
        "/api/globals/site",
        json!({"name": "Site", "apiKey": "k"}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);

    let viewer_id = add_user(pool, "vic@example.com", "viewer")
        .await
        .to_string();
    let res = patch(
        &admin,
        &format!("/api/users/{viewer_id}"),
        json!({"pay": 9}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let viewer = signed_in(visitor(pool).await, "vic@example.com").await;
    Staff {
        editor: app_as(pool, "eve@example.com", "editor").await,
        author: app_as(pool, "al@example.com", "author").await,
        admin,
        viewer,
        viewer_id,
        ann,
        p1,
        p2,
    }
}

/// The keys a post shows that only Editors may read, wherever they'd be.
fn hidden_in_post(doc: &Value) -> Vec<&'static str> {
    let mut out = Vec::new();
    for key in [
        "secret",
        "rank",
        "authorSalary",
        "authorNameEd",
        "hiddenAuthor",
        "hiddenName",
        "inRow",
    ] {
        if doc.get(key).is_some() {
            out.push(key);
        }
    }
    if doc["meta"].get("note").is_some() {
        out.push("meta.note");
    }
    if doc["links"][0].get("url").is_some() {
        out.push("links.url");
    }
    if doc["extra"].get("code").is_some() {
        out.push("extra.code");
    }
    if doc["deep"]["inner"].get("x").is_some() {
        out.push("deep.inner.x");
    }
    if doc["deep"]["rows"][0]["sub"].get("x").is_some() {
        out.push("deep.rows.sub.x");
    }
    out
}

fn hidden_in_person(doc: &Value) -> Vec<&'static str> {
    let mut out = Vec::new();
    for key in ["salary", "edPosts"] {
        if doc.get(key).is_some() {
            out.push(key);
        }
    }
    if doc["contact"].get("phone").is_some() {
        out.push("contact.phone");
    }
    out
}

const ALL_POST: [&str; 12] = [
    "secret",
    "rank",
    "authorSalary",
    "authorNameEd",
    "hiddenAuthor",
    "hiddenName",
    "inRow",
    "meta.note",
    "links.url",
    "extra.code",
    "deep.inner.x",
    "deep.rows.sub.x",
];

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unreadable_fields_are_removed_everywhere(pool: PgPool) {
    let s = staff(&pool).await;
    let one = format!("/api/posts/{}", s.p1);

    // Lists and single reads, with links populated.
    for (app, hidden) in [(&s.viewer, true), (&s.editor, false), (&s.admin, false)] {
        let list = get(app, "/api/posts?where[title][equals]=One").await;
        assert_eq!(list.status, 200, "{}", list.json);
        let single = get(app, &one).await;
        assert_eq!(single.status, 200, "{}", single.json);
        for doc in [&list.json["docs"][0], &single.json] {
            assert_eq!(doc["title"], "One");
            assert_eq!(doc["meta"]["topic"], "t");
            assert_eq!(doc["links"][0]["label"], "l");
            assert_eq!(doc["authorName"], "Ann");
            if hidden {
                assert!(hidden_in_post(doc).is_empty(), "{doc}");
                assert!(hidden_in_person(&doc["author"]).is_empty(), "{doc}");
                assert_eq!(doc["author"]["contact"]["email"], "a@x");
            } else {
                assert_eq!(hidden_in_post(doc), ALL_POST, "{doc}");
                assert_eq!(
                    hidden_in_person(&doc["author"]),
                    ["salary", "contact.phone"]
                );
            }
        }
    }

    // Populated at every depth: p2 -> p1 -> Ann.
    let deep = get(&s.viewer, &format!("/api/posts/{}?depth=2", s.p2)).await;
    assert_eq!(deep.json["related"]["title"], "One", "{}", deep.json);
    assert!(
        hidden_in_post(&deep.json["related"]).is_empty(),
        "{}",
        deep.json
    );
    assert_eq!(deep.json["related"]["author"]["name"], "Ann");
    assert!(hidden_in_person(&deep.json["related"]["author"]).is_empty());
    let deep = get(&s.editor, &format!("/api/posts/{}?depth=2", s.p2)).await;
    assert_eq!(hidden_in_post(&deep.json["related"]), ALL_POST);
    assert_eq!(deep.json["related"]["author"]["salary"], 100);

    // `select` asking for a hidden field gets nothing for it, not an error.
    let picked = get(
        &s.viewer,
        &format!("{one}?select[secret]=true&select[title]=true"),
    )
    .await;
    assert_eq!(picked.status, 200, "{}", picked.json);
    assert_eq!(picked.json["title"], "One");
    assert!(picked.json.get("secret").is_none(), "{}", picked.json);
    let picked = get(&s.editor, &format!("{one}?select[secret]=true")).await;
    assert_eq!(picked.json["secret"], "s1");

    // Joined documents, and joins that would show a hidden link or are hidden themselves.
    let person = format!("/api/people/{}", s.ann);
    let viewed = get(&s.viewer, &person).await;
    assert_eq!(viewed.status, 200, "{}", viewed.json);
    assert!(hidden_in_person(&viewed.json).is_empty(), "{}", viewed.json);
    let joined = viewed.json["posts"]["docs"].as_array().unwrap();
    assert_eq!(joined.len(), 2, "{}", viewed.json);
    for doc in joined {
        assert!(hidden_in_post(doc).is_empty(), "{doc}");
    }
    assert_eq!(viewed.json["hiddenPosts"]["docs"], json!([]));
    // Its default sort is by `rank`, which a Viewer can't read, so they get the usual order
    // (newest first) rather than one that ranks what they can't see.
    let order = |res: &Res| -> Vec<String> {
        res.json["posts"]["docs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["title"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(order(&viewed), ["Two", "One"]);
    let listed = get(&s.viewer, "/api/people").await;
    assert!(hidden_in_person(&listed.json["docs"][0]).is_empty());
    let viewed = get(&s.editor, &person).await;
    assert_eq!(
        hidden_in_person(&viewed.json),
        ["salary", "edPosts", "contact.phone"]
    );
    assert_eq!(viewed.json["hiddenPosts"]["docs"][0]["id"], s.p1.as_str());
    assert_eq!(order(&viewed), ["One", "Two"]);

    // References don't show links held in hidden fields.
    let refs = |res: Res| -> Vec<String> {
        assert_eq!(res.status, 200, "{}", res.json);
        let mut out: Vec<String> = res.json["docs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["path"].as_str().unwrap().to_owned())
            .collect();
        out.sort();
        out
    };
    let url = format!("{person}/references");
    assert_eq!(refs(get(&s.viewer, &url).await), ["author", "author"]);
    assert_eq!(
        refs(get(&s.editor, &url).await),
        ["author", "author", "hiddenAuthor"]
    );

    // Globals.
    let site = get(&s.viewer, "/api/globals/site").await;
    assert_eq!(site.json["name"], "Site", "{}", site.json);
    assert!(site.json.get("apiKey").is_none(), "{}", site.json);
    assert_eq!(
        get(&s.editor, "/api/globals/site").await.json["apiKey"],
        "k"
    );
    let prefs = get(&s.viewer, "/api/globals/prefs").await;
    assert_eq!(prefs.json, json!({"title": "p"}));
    let prefs = get(&s.editor, "/api/globals/prefs").await;
    assert_eq!(prefs.json, json!({"title": "p", "hint": "h"}));

    // What a write sends back: `secret` gets its default, which the Author can't see.
    let made = create(&s.author, "/api/posts", json!({"title": "Mine"})).await;
    assert!(made.get("secret").is_none(), "{made}");
    let mine = format!("/api/posts/{}", id(&made));
    assert_eq!(get(&s.admin, &mine).await.json["secret"], "s3");
    let changed = patch(&s.author, &mine, json!({"title": "Mine 2"})).await;
    assert_eq!(changed.status, 200, "{}", changed.json);
    assert_eq!(changed.json["title"], "Mine 2");
    assert!(changed.json.get("secret").is_none(), "{}", changed.json);

    // Profiles: a Viewer can't see their own pay; an Admin can.
    let me = format!("/api/users/{}", s.viewer_id);
    for res in [
        get(&s.viewer, &me).await,
        get(&s.viewer, "/api/auth/me").await,
    ] {
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(res.json["name"], "viewer");
        assert!(res.json.get("pay").is_none(), "{}", res.json);
    }
    assert_eq!(get(&s.admin, &me).await.json["pay"], 9);
    let login = json!({"email": "vic@example.com", "password": common::PASSWORD});
    let res = send(
        &visitor(&pool).await,
        "POST",
        "/api/auth/login",
        Some(&login.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["user"]["name"], "viewer");
    assert!(res.json["user"].get("pay").is_none(), "{}", res.json);
    let listed = get(&s.admin, "/api/users").await;
    assert!(
        listed.json["docs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["pay"] == 9)
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unreadable_fields_cant_be_filtered_or_sorted(pool: PgPool) {
    let s = staff(&pool).await;
    let probes = [
        (
            "/api/posts?where[secret][equals]=s1",
            "where[secret][equals]",
        ),
        ("/api/posts?where[rank][gt]=0", "where[rank][gt]"),
        (
            "/api/posts?where[meta.note][exists]=true",
            "where[meta.note][exists]",
        ),
        (
            "/api/posts?where[links.url][equals]=u",
            "where[links.url][equals]",
        ),
        (
            "/api/posts?where[extra.code][like]=c",
            "where[extra.code][like]",
        ),
        ("/api/posts?where[inRow][equals]=r", "where[inRow][equals]"),
        (
            "/api/posts?where[or][0][title][equals]=x&where[or][1][secret][equals]=s1",
            "where[or][1][secret][equals]",
        ),
        (
            "/api/posts?where[and][0][author.salary][gt]=50",
            "where[and][0][author.salary][gt]",
        ),
        (
            "/api/posts?where[author.contact.phone][equals]=555",
            "where[author.contact.phone][equals]",
        ),
        (
            "/api/posts?where[hiddenAuthor.name][equals]=Ann",
            "where[hiddenAuthor.name][equals]",
        ),
        (
            "/api/posts?where[hiddenAuthor][exists]=true",
            "where[hiddenAuthor][exists]",
        ),
        (
            "/api/posts?where[hiddenName][equals]=Ann",
            "where[hiddenName][equals]",
        ),
        (
            "/api/posts?where[authorSalary][equals]=100",
            "where[authorSalary][equals]",
        ),
        (
            "/api/posts?where[authorNameEd][equals]=Ann",
            "where[authorNameEd][equals]",
        ),
        (
            "/api/posts?where[related.secret][equals]=s1",
            "where[related.secret][equals]",
        ),
        ("/api/posts?sort=rank", "sort"),
        ("/api/posts?sort=-secret", "sort"),
        ("/api/people?where[salary][gt]=0", "where[salary][gt]"),
        (
            "/api/people?joins[posts][where][secret][equals]=s1",
            "joins[posts][where][secret][equals]",
        ),
        ("/api/people?joins[posts][sort]=rank", "joins[posts][sort]"),
        (
            "/api/people?joins[posts][where][author.salary][gt]=0",
            "joins[posts][where][author.salary][gt]",
        ),
    ];
    for (url, param) in probes {
        let res = get(&s.viewer, url).await;
        assert_problem(&res, 403);
        let detail = res.json["detail"].as_str().unwrap();
        assert!(detail.starts_with(param), "{url}: {detail}");
        // Editors can use every one of them.
        let res = get(&s.editor, url).await;
        assert_eq!(res.status, 200, "{url}: {}", res.json);
    }
    // They find what's there.
    assert_eq!(
        titles(&get(&s.editor, "/api/posts?where[secret][equals]=s1").await),
        ["One"]
    );
    assert_eq!(
        titles(&get(&s.editor, "/api/posts?where[author.salary][gt]=50").await).len(),
        2
    );
    // A field that doesn't exist is still a 400, and readable ones work as before.
    assert_problem(
        &get(&s.viewer, "/api/posts?where[nope][equals]=x").await,
        400,
    );
    assert_problem(
        &get(&s.viewer, "/api/posts?where[rank][gt]=nope").await,
        403,
    );
    assert_eq!(
        titles(
            &get(
                &s.viewer,
                "/api/posts?where[author.name][equals]=Ann&sort=title"
            )
            .await
        ),
        ["One", "Two"]
    );
    assert_eq!(
        titles(&get(&s.viewer, "/api/posts?where[meta.topic][equals]=t").await),
        ["One"]
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unwritable_fields_are_refused_by_name(pool: PgPool) {
    let s = staff(&pool).await;
    let refused = |res: Res, field: &str| {
        assert_problem(&res, 403);
        let detail = res.json["detail"].as_str().unwrap();
        assert!(detail.contains(&format!("`{field}`")), "{field}: {detail}");
    };
    let post = |app: &Router, body: Value| {
        let app = app.clone();
        async move { send(&app, "POST", "/api/posts", Some(&body.to_string())).await }
    };

    // Create: `secret` and `links.url` need Editors; `meta.note` has no create rule.
    refused(
        post(&s.author, json!({"title": "A", "secret": "x"})).await,
        "secret",
    );
    refused(
        post(&s.author, json!({"title": "A", "secret": null})).await,
        "secret",
    );
    refused(
        post(
            &s.author,
            json!({"title": "A", "links": [{"label": "a"}, {"url": "u"}]}),
        )
        .await,
        "links.url",
    );
    let mine = id(&create(
        &s.author,
        "/api/posts",
        json!({"title": "A", "links": [{"label": "a"}], "meta": {"topic": "t", "note": "n"}}),
    )
    .await);
    let mine = format!("/api/posts/{mine}");
    assert_eq!(
        post(&s.editor, json!({"title": "E", "secret": "x"}))
            .await
            .status,
        201
    );
    assert_eq!(
        post(&s.admin, json!({"title": "Ad", "secret": "x"}))
            .await
            .status,
        201
    );

    // Update: sending the field at all counts, even with the value it already has.
    refused(
        patch(&s.author, &mine, json!({"secret": "x"})).await,
        "secret",
    );
    refused(
        patch(&s.author, &mine, json!({"secret": "s3"})).await,
        "secret",
    );
    refused(
        patch(&s.author, &mine, json!({"meta": {"note": "x"}})).await,
        "meta.note",
    );
    // Clearing the group, or replacing the list, would change what they can't.
    refused(
        patch(&s.author, &mine, json!({"meta": null})).await,
        "meta.note",
    );
    refused(
        patch(&s.author, &mine, json!({"links": [{"label": "b"}]})).await,
        "links.url",
    );
    refused(
        patch(&s.author, &mine, json!({"links": null})).await,
        "links.url",
    );
    // Nothing changed, and what they may change still can be.
    let ok = patch(
        &s.author,
        &mine,
        json!({"title": "B", "meta": {"topic": "u"}}),
    )
    .await;
    assert_eq!(ok.status, 200, "{}", ok.json);
    let stored = get(&s.admin, &mine).await.json;
    assert_eq!(stored["secret"], "s3");
    assert_eq!(stored["meta"], json!({"topic": "u", "note": "n"}));
    assert_eq!(stored["links"][0]["label"], "a");
    let ok = patch(
        &s.editor,
        &mine,
        json!({"secret": "y", "links": [{"label": "c", "url": "v"}]}),
    )
    .await;
    assert_eq!(ok.status, 200, "{}", ok.json);
    assert_eq!(ok.json["secret"], "y");

    // Globals: only Admins may set the key.
    refused(
        patch(&s.editor, "/api/globals/site", json!({"apiKey": "x"})).await,
        "apiKey",
    );
    assert_eq!(
        patch(&s.editor, "/api/globals/site", json!({"name": "S2"}))
            .await
            .status,
        200
    );
    assert_eq!(
        patch(&s.admin, "/api/globals/site", json!({"apiKey": "z"}))
            .await
            .status,
        200
    );
    assert_eq!(
        get(&s.editor, "/api/globals/site").await.json["apiKey"],
        "z"
    );

    // Profiles: a Viewer can't give themselves a raise.
    let me = format!("/api/users/{}", s.viewer_id);
    refused(patch(&s.viewer, &me, json!({"pay": 1000})).await, "pay");
    assert_eq!(
        patch(&s.viewer, &me, json!({"name": "Vic"})).await.status,
        200
    );
    assert_eq!(get(&s.admin, &me).await.json["pay"], 9);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn virtual_fields_dont_read_hidden_fields(pool: PgPool) {
    let s = staff(&pool).await;
    // Through a hidden field at the far end, a hidden link, or a rule of its own: no value.
    let mut seen = Vec::new();
    for app in [&s.viewer, &s.editor] {
        let one = get(app, &format!("/api/posts/{}", s.p1)).await.json;
        let two = get(app, &format!("/api/posts/{}?depth=1", s.p2)).await.json;
        let person = get(app, &format!("/api/people/{}", s.ann)).await.json;
        let joined = person["posts"]["docs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["title"] == "One")
            .unwrap()
            .clone();
        for doc in [&one, &two["related"], &joined] {
            assert_eq!(doc["authorName"], "Ann", "{doc}");
            seen.push((
                doc.get("authorSalary").cloned(),
                doc.get("hiddenName").cloned(),
                doc.get("authorNameEd").cloned(),
            ));
        }
    }
    let none = (None, None, None);
    let all = (Some(json!(100)), Some(json!("Ann")), Some(json!("Ann")));
    assert_eq!(
        seen,
        [
            none.clone(),
            none.clone(),
            none,
            all.clone(),
            all.clone(),
            all
        ]
    );
    // Asking for them by name changes nothing.
    let picked = get(
        &s.viewer,
        &format!(
            "/api/posts/{}?select[authorSalary]=true&select[hiddenName]=true",
            s.p1
        ),
    )
    .await;
    assert_eq!(picked.status, 200, "{}", picked.json);
    assert!(picked.json.get("authorSalary").is_none(), "{}", picked.json);
    assert!(picked.json.get("hiddenName").is_none(), "{}", picked.json);
}

/// Stores `value` at `key` in document `id` behind the API's back, as bad data would be.
async fn corrupt(pool: &PgPool, id: &str, key: &str, value: Value) {
    sqlx::query("UPDATE documents SET published = published || $1 WHERE id = $2")
        .bind(json!({ key: value }))
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn hidden_fields_stay_out_of_validation_errors(pool: PgPool) {
    let s = staff(&pool).await;
    let generic = json!({"path": "", "message": "a field you can't see needs fixing; ask someone who can see it"});
    // Only the generic entry, and nothing that names or describes the field.
    let hidden = |res: &Res, field: &str| {
        assert_problem(res, 422);
        assert_eq!(res.json["errors"], json!([generic]), "{}", res.json);
        let body = res.json.to_string();
        assert!(!body.contains(field) && !body.contains("number"), "{body}");
    };
    let named = |res: &Res, field: &str| {
        assert_problem(res, 422);
        assert_eq!(res.json["errors"][0]["path"], field, "{}", res.json);
    };

    // Update: a bad stored value in a field the Author can't read.
    let mine = id(&create(&s.author, "/api/posts", json!({"title": "Mine"})).await);
    corrupt(&pool, &mine, "rank", json!("oops")).await;
    let url = format!("/api/posts/{mine}");
    hidden(
        &patch(&s.author, &url, json!({"title": "M2"})).await,
        "rank",
    );
    named(
        &patch(&s.editor, &url, json!({"title": "M2"})).await,
        "rank",
    );
    // What the caller can see is still named, beside the generic entry.
    let res = patch(&s.author, &url, json!({"title": 5})).await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"],
        json!([{"path": "title", "message": "expected text"}, generic])
    );

    // Create: a required field the Author can't read, with no default.
    create(
        &s.admin,
        "/api/admin/collections",
        json!({"slug": "tickets", "fields": [
            {"name": "title", "type": "text"},
            {"name": "code", "type": "text", "required": true, "access": {"read": {"role": "editor"}}}
        ]}),
    )
    .await;
    let author = signed_in(visitor(&pool).await, "al@example.com").await;
    let editor = signed_in(visitor(&pool).await, "eve@example.com").await;
    let body = json!({"title": "T"}).to_string();
    hidden(
        &send(&author, "POST", "/api/tickets", Some(&body)).await,
        "code",
    );
    named(
        &send(&editor, "POST", "/api/tickets", Some(&body)).await,
        "code",
    );

    // Globals: only Admins can read `token`.
    let site: String = sqlx::query_scalar("SELECT id::text FROM collections WHERE slug = 'site'")
        .fetch_one(&pool)
        .await
        .unwrap();
    corrupt(&pool, &site, "token", json!("bad")).await;
    hidden(
        &patch(&s.editor, "/api/globals/site", json!({"name": "S"})).await,
        "token",
    );
    named(
        &patch(&s.admin, "/api/globals/site", json!({"name": "S"})).await,
        "token",
    );

    // Profiles: the Viewer's own pay, which only Editors can read.
    corrupt(&pool, &s.viewer_id, "pay", json!("lots")).await;
    let me = format!("/api/users/{}", s.viewer_id);
    hidden(&patch(&s.viewer, &me, json!({"name": "Vic"})).await, "pay");
    named(&patch(&s.admin, &me, json!({"name": "Vic"})).await, "pay");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn admins_see_links_whatever_their_path(pool: PgPool) {
    let s = staff(&pool).await;
    // A link row whose path names no field. Schema changes re-sync every live document's
    // rows, so this stands in for whatever else might leave one behind.
    sqlx::query(
        "UPDATE relations SET field_path = 'gone' WHERE from_doc = $1 AND field_path = 'author'",
    )
    .bind(uuid::Uuid::parse_str(&s.p2).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    let url = format!("/api/people/{}/references", s.ann);
    let paths = |res: Res| -> Vec<String> {
        assert_eq!(res.status, 200, "{}", res.json);
        let mut out: Vec<String> = res.json["docs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["path"].as_str().unwrap().to_owned())
            .collect();
        out.sort();
        out
    };
    assert_eq!(
        paths(get(&s.admin, &url).await),
        ["author", "gone", "hiddenAuthor"]
    );
    assert_eq!(
        paths(get(&s.editor, &url).await),
        ["author", "hiddenAuthor"]
    );
    assert_eq!(paths(get(&s.viewer, &url).await), ["author"]);
}

/// Whether a request went through (2xx) or was refused: 401, 403, or 404 for what the
/// caller can't see. Anything else means the probe itself is wrong.
fn went_through(res: &Res) -> bool {
    match res.status.as_u16() {
        200..=299 => true,
        401 | 403 | 404 => false,
        s => panic!("unexpected {s}: {}", res.json),
    }
}

/// `went_through` for a write of one field, where a join or virtual field's "can't be set"
/// 422 is a refusal too.
fn wrote(res: &Res) -> bool {
    let unsettable = res.status == 422
        && res.json["errors"].as_array().is_some_and(|errors| {
            errors.iter().all(|e| {
                let m = e["message"].as_str().unwrap_or_default();
                m.ends_with("it can't be set") || m.ends_with("set that instead")
            })
        });
    !unsettable && went_through(res)
}

/// Holds `answer` against what the requests behind `outcomes` did: `true` when every one
/// went through, `false` when none did, "conditional" when only some did.
fn agrees(what: &str, answer: &Value, outcomes: &[bool]) {
    assert!(!outcomes.is_empty(), "{what}: nothing was tried");
    let expected = match (outcomes.iter().all(|o| *o), outcomes.iter().any(|o| *o)) {
        (true, _) => json!(true),
        (false, false) => json!(false),
        _ => json!("conditional"),
    };
    assert_eq!(answer, &expected, "{what}: {outcomes:?}");
}

/// Holds a field's read answer against the documents the caller could read, each of which
/// holds every field: shown in all of them, or in none. With none readable, it's false.
fn field_read_agrees(what: &str, answer: &Value, readable: &[Value], path: &str) {
    let seen: Vec<bool> = readable.iter().map(|doc| has(doc, path)).collect();
    agrees(what, answer, if seen.is_empty() { &[false] } else { &seen });
}

/// Whether `doc` holds the field at dotted `path`, through groups and every row of a list.
fn has(doc: &Value, path: &str) -> bool {
    let (head, rest) = path
        .split_once('.')
        .map_or((path, None), |(h, r)| (h, Some(r)));
    match (doc.get(head), rest) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(Value::Array(rows)), Some(rest)) => {
            !rows.is_empty() && rows.iter().all(|r| has(r, rest))
        }
        (Some(v), Some(rest)) => has(v, rest),
    }
}

const LISTS: &[&str] = &["links"];
const GROUPS: &[&str] = &["seo", "box"];

/// Every collection and global in `access_answers_match_what_the_api_allows`, with its fields.
const KNOWN: &[(&str, &str, &[&str])] = &[
    (
        "collections",
        "posts",
        &[
            "box",
            "box.y",
            "inRow",
            "links",
            "links.label",
            "links.url",
            "note",
            "noteBody",
            "owner",
            "peek",
            "refs",
            "related",
            "secret",
            "seo",
            "seo.note",
            "seo.title",
            "stage",
            "title",
        ],
    ),
    ("collections", "notes", &["body"]),
    ("collections", "inbox", &["body"]),
    ("collections", "users", &["name"]),
    ("globals", "home", &["banner", "live", "memo"]),
];

/// Holds `/api/access` to listing only what the caller can do something with: no collection,
/// global or field whose answers are all false, and nothing that isn't in `KNOWN`.
fn lists_only_what_it_allows(who: &str, a: &Value) {
    for section in ["collections", "globals"] {
        for (slug, entry) in a[section].as_object().unwrap() {
            let known = KNOWN.iter().find(|k| k.0 == section && k.1 == slug);
            let known = known.unwrap_or_else(|| panic!("{who}: unexpected {slug}"));
            let entry = entry.as_object().unwrap();
            let actions = entry.iter().filter(|(k, _)| *k != "fields");
            assert!(
                actions.clone().any(|(_, v)| v != &json!(false)),
                "{who}: {slug} {entry:?}"
            );
            for (path, f) in entry["fields"].as_object().unwrap() {
                assert!(
                    known.2.contains(&path.as_str()),
                    "{who}: unexpected {slug} {path}"
                );
                let any = f.as_object().unwrap().values().any(|v| v == &json!(true));
                assert!(any, "{who}: {slug} field {path} {f}");
            }
        }
    }
}

/// `a` with what it leaves out put back as all false: a missing collection, global or field
/// means no access.
fn filled(a: &Value) -> Value {
    let none = |actions: &[&str]| -> Value {
        actions
            .iter()
            .map(|k| ((*k).to_owned(), json!(false)))
            .collect()
    };
    let mut a = a.clone();
    for (section, slug, fields) in KNOWN {
        let global = *section == "globals";
        let (actions, field_actions): (&[&str], &[&str]) = if global {
            (&["read", "update"], &["read", "update"])
        } else {
            (
                &["read", "create", "update", "delete"],
                &["read", "create", "update"],
            )
        };
        let entry = &mut a[*section][*slug];
        if entry.is_null() {
            *entry = none(actions);
            entry["fields"] = json!({});
        }
        for f in *fields {
            if entry["fields"][*f].is_null() {
                entry["fields"][*f] = none(field_actions);
            }
        }
    }
    a
}

/// A body that writes only the field at dotted `path`: `leaf` for a value, an empty group
/// or list for a container, wrapped in its groups and one list row.
fn body_at(path: &str, leaf: Value) -> Value {
    let mut segments: Vec<&str> = path.split('.').collect();
    let last = segments.pop().unwrap();
    let value = match last {
        l if LISTS.contains(&l) => json!([]),
        l if GROUPS.contains(&l) => json!({}),
        _ => leaf,
    };
    let mut out = json!({ last: value });
    for s in segments.into_iter().rev() {
        out = json!({ s: if LISTS.contains(&s) { json!([out]) } else { out } });
    }
    out
}

/// One collection's answers for one caller, and what to probe them with.
struct Probe<'a, L> {
    who: &'a str,
    app: &'a Router,
    admin: &'a Router,
    slug: &'a str,
    /// The caller's answers for this collection, with what was left out filled back in.
    answers: &'a Value,
    /// A text field set to `who` on every document made here, so a list can find them.
    mark: &'a str,
    /// What the Admin makes. Each holds every field, so reads show which are left out.
    seeds: Vec<Value>,
    /// What the caller tries to create.
    own: Value,
    /// A value that's valid for the field at a path.
    leaf: L,
}

/// Holds one collection's answers against what the caller can actually do: the Admin makes
/// the seeds; the caller tries to create one, reads each singly and in a list, checks each
/// field is shown or left out as answered, writes each field alone, then updates and
/// deletes each document.
async fn bears_out<L: Fn(&str) -> Value>(p: Probe<'_, L>) {
    let Probe {
        who,
        app,
        admin,
        slug,
        answers,
        mark,
        seeds,
        own,
        leaf,
    } = p;
    let what = |x: &str| format!("{who}: {slug} {x}");
    let url = |d: &str| format!("/api/{slug}/{d}");
    let marked = |mut v: Value| {
        v[mark] = who.into();
        v
    };
    let mut docs = Vec::new();
    for seed in seeds {
        docs.push(id(
            &create(admin, &format!("/api/{slug}"), marked(seed)).await
        ));
    }
    let seeded = docs.len();
    let mine = marked(own).to_string();
    let made = send(app, "POST", &format!("/api/{slug}"), Some(&mine)).await;
    agrees(&what("create"), &answers["create"], &[went_through(&made)]);
    if went_through(&made) {
        docs.push(id(&made.json));
    }

    let mut reads = Vec::new();
    let mut readable = Vec::new();
    for (i, d) in docs.iter().enumerate() {
        let res = get(app, &url(d)).await;
        reads.push(went_through(&res));
        if went_through(&res) && i < seeded {
            readable.push(res.json);
        }
    }
    agrees(&what("read"), &answers["read"], &reads);
    // A list shows exactly what single reads do, and is refused only when reads are.
    let list = get(
        app,
        &format!("/api/{slug}?limit=100&where[{mark}][equals]={who}"),
    )
    .await;
    assert_eq!(
        went_through(&list),
        answers["read"] != json!(false),
        "{}",
        what("list")
    );
    let mut listed: Vec<String> = if went_through(&list) {
        list.json["docs"]
            .as_array()
            .unwrap()
            .iter()
            .map(id)
            .collect()
    } else {
        Vec::new()
    };
    let mut shown: Vec<String> = docs
        .iter()
        .zip(&reads)
        .filter(|(_, r)| **r)
        .map(|(d, _)| d.clone())
        .collect();
    listed.sort();
    shown.sort();
    assert_eq!(listed, shown, "{}", what("list"));

    let fields = answers["fields"].as_object().unwrap();
    for (path, f) in fields {
        let what = what(&format!("field {path} read"));
        field_read_agrees(&what, &f["read"], &readable, path);
    }

    let mut updates = Vec::new();
    for d in &docs {
        updates.push(went_through(
            &patch(app, &url(d), json!({ mark: who })).await,
        ));
    }
    agrees(&what("update"), &answers["update"], &updates);
    // A document the caller could change, or (when there's none) any: that must refuse.
    let target = docs
        .iter()
        .zip(&updates)
        .find(|(_, u)| **u)
        .map_or(&docs[0], |(d, _)| d)
        .clone();
    for (path, f) in fields {
        let body = body_at(path, leaf(path));
        let res = send(
            app,
            "POST",
            &format!("/api/{slug}"),
            Some(&body.to_string()),
        )
        .await;
        let created = what(&format!("field {path} create"));
        agrees(&created, &f["create"], &[wrote(&res)]);
        let res = patch(app, &url(&target), body).await;
        let updated = what(&format!("field {path} update"));
        agrees(&updated, &f["update"], &[wrote(&res)]);
    }

    let mut deletes = Vec::new();
    for d in &docs {
        deletes.push(went_through(&send(app, "DELETE", &url(d), None).await));
    }
    agrees(&what("delete"), &answers["delete"], &deletes);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn access_answers_match_what_the_api_allows(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let editor = json!({"role": "editor"});
    let writers = json!({"role": ["author", "editor"]});
    // Allow and Deny outright, by role, and an own-only default (Authors' deletes) on a
    // collection everyone reads.
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "notes",
        "access": {"read": true, "create": writers, "update": false},
        "fields": [{"name": "body", "type": "text"}]}),
    )
    .await;
    // A filter (`or` with a role, so Editors pass outright), a `$user` filter, and role
    // defaults for create and delete; field rules on containers and inside them; a join
    // and a virtual field, which are never written.
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "posts",
        "access": {"read": {"or": [editor, {"stage": "published"}]}, "update": {"owner": "$user.id"}},
        "fields": [
            {"name": "title", "type": "text"},
            {"name": "stage", "type": "select", "options": ["draft", "published"]},
            {"name": "owner", "type": "relationship", "relationTo": "users"},
            {"name": "secret", "type": "text",
             "access": {"read": editor, "create": editor, "update": editor}},
            {"name": "peek", "type": "text", "access": {"read": editor}},
            {"name": "seo", "type": "group", "access": {"update": editor}, "fields": [
                {"name": "title", "type": "text"},
                {"name": "note", "type": "text", "access": {"create": editor}}
            ]},
            {"name": "links", "type": "array", "fields": [
                {"name": "url", "type": "text"},
                {"name": "label", "type": "text", "access": {"update": editor}}
            ]},
            {"type": "row", "fields": [{"name": "inRow", "type": "text"}]},
            {"name": "box", "type": "group", "access": {"read": editor}, "fields": [
                {"name": "y", "type": "text"}
            ]},
            {"name": "note", "type": "relationship", "relationTo": "notes"},
            {"name": "noteBody", "type": "text", "virtual": "note.body"},
            {"name": "related", "type": "relationship", "relationTo": "posts"},
            {"name": "refs", "type": "join", "collection": "posts", "on": "related"}
        ]}),
    )
    .await;
    // Anyone may write, but only Editors read: changing what you can't read is refused.
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "inbox",
        "access": {"read": editor, "create": true, "update": true, "delete": true},
        "fields": [{"name": "body", "type": "text"}]}),
    )
    .await;
    let home = create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "home", "kind": "global",
        "access": {"read": {"live": true}, "update": writers},
        "fields": [
            {"name": "live", "type": "checkbox"},
            {"name": "banner", "type": "text"},
            {"name": "memo", "type": "text", "access": {"read": editor}}
        ]}),
    )
    .await;
    let home_id: uuid::Uuid = id(&home).parse().unwrap();
    // Every reader can read these, so links to them show for everyone.
    let shared_note = id(&create(&admin, "/api/notes", json!({"body": "shared"})).await);
    let anchor = id(&create(
        &admin,
        "/api/posts",
        json!({"title": "anchor", "stage": "published"}),
    )
    .await);
    let me = get(&admin, "/api/auth/me").await;
    let admin_id: uuid::Uuid = me.json["id"].as_str().unwrap().parse().unwrap();
    let made = create(
        &admin,
        "/api/auth/keys",
        json!({"name": "k", "scopes": ["posts:read", "notes:write"]}),
    )
    .await;
    let bearer = format!("Bearer {}", made["key"].as_str().unwrap());
    let key = visitor(&pool).await.layer(axum::middleware::map_request(
        move |mut req: Request<axum::body::Body>| {
            let bearer = bearer.clone();
            async move {
                let value = axum::http::HeaderValue::from_str(&bearer).unwrap();
                req.headers_mut().insert("authorization", value);
                req
            }
        },
    ));
    let mut callers = vec![("anonymous", visitor(&pool).await, None)];
    for role in ["viewer", "author", "editor"] {
        let email = format!("{role}@example.com");
        let id = add_user(&pool, &email, role).await;
        callers.push((
            role,
            signed_in(visitor(&pool).await, &email).await,
            Some(id),
        ));
    }
    callers.push(("admin", admin.clone(), Some(admin_id)));
    callers.push(("key", key, Some(admin_id)));

    let mut answers = Vec::new();
    for (who, app, me) in &callers {
        let res = get(app, "/api/access").await;
        assert_eq!(res.status, 200, "{who}: {}", res.json);
        let raw = res.json;
        let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        // Only what the caller can do something with is listed.
        lists_only_what_it_allows(who, &raw);
        let (collections, globals): (&[&str], &[&str]) = match *who {
            "anonymous" => (&["inbox", "notes", "posts"], &["home"]),
            "key" => (&["notes", "posts"], &[]),
            _ => (&["inbox", "notes", "posts", "users"], &["home"]),
        };
        assert_eq!(keys(&raw["collections"]), collections, "{who}");
        assert_eq!(keys(&raw["globals"]), globals, "{who}");
        if *who == "admin" {
            // Dotted data paths at every depth, layout flattened, list fields without an index.
            let posts = keys(&raw["collections"]["posts"]["fields"]);
            assert_eq!(posts, KNOWN[0].2, "{who}");
        }
        // What's left out is no access at all, which the requests below bear out.
        let a = filled(&raw);
        let mine = me.unwrap_or(admin_id);

        let full = |stage: &str, owner: uuid::Uuid| {
            json!({"stage": stage, "owner": owner, "secret": "s", "peek": "p",
                   "seo": {"title": "t", "note": "n"}, "links": [{"url": "u", "label": "l"}],
                   "inRow": "r", "box": {"y": "y"}, "note": shared_note, "related": anchor})
        };
        let seeds = vec![
            full("published", mine),
            full("draft", mine),
            full("published", admin_id),
            full("draft", admin_id),
        ];
        let leaf = |path: &str| match path {
            "stage" => json!("published"),
            "owner" => json!(mine),
            "note" => json!(shared_note),
            "related" => json!(anchor),
            _ => json!("v"),
        };
        bears_out(Probe {
            who,
            app,
            admin: &admin,
            slug: "posts",
            answers: &a["collections"]["posts"],
            mark: "title",
            seeds,
            own: json!({"stage": "published", "owner": mine}),
            leaf,
        })
        .await;
        for slug in ["notes", "inbox"] {
            bears_out(Probe {
                who,
                app,
                admin: &admin,
                slug,
                answers: &a["collections"][slug],
                mark: "body",
                seeds: vec![json!({}), json!({})],
                own: json!({}),
                leaf: |_: &str| json!("v"),
            })
            .await;
        }

        // home: before it's ever saved, while it matches its read rule, and while it doesn't.
        let g = &a["globals"]["home"];
        sqlx::query("DELETE FROM documents WHERE id = $1")
            .bind(home_id)
            .execute(&pool)
            .await
            .unwrap();
        let (mut reads, mut updates, mut readable) = (Vec::new(), Vec::new(), Vec::new());
        for live in [None, Some(true), Some(false)] {
            if let Some(live) = live {
                let set = json!({"live": live, "banner": "b", "memo": "m"});
                assert_eq!(patch(&admin, "/api/globals/home", set).await.status, 200);
            }
            let res = get(app, "/api/globals/home").await;
            reads.push(went_through(&res));
            // An unsaved global shows defaults, which needn't hold every field.
            if went_through(&res) && live.is_some() {
                readable.push(res.json);
            }
            let res = patch(app, "/api/globals/home", json!({"banner": who})).await;
            updates.push(went_through(&res));
        }
        agrees(&format!("{who}: home read"), &g["read"], &reads);
        agrees(&format!("{who}: home update"), &g["update"], &updates);
        let set = json!({"live": true});
        assert_eq!(patch(&admin, "/api/globals/home", set).await.status, 200);
        for (path, f) in g["fields"].as_object().unwrap() {
            let what = format!("{who}: home field {path} read");
            field_read_agrees(&what, &f["read"], &readable, path);
            let value = if path == "live" {
                json!(true)
            } else {
                json!("v")
            };
            let res = patch(app, "/api/globals/home", json!({ path: value })).await;
            let what = format!("{who}: home field {path} update");
            agrees(&what, &f["update"], &[went_through(&res)]);
        }

        // users: their own account, someone else's, a new one.
        let u = &a["collections"]["users"];
        let victim = add_user(&pool, &format!("victim-{who}@example.com"), "viewer").await;
        let targets: Vec<uuid::Uuid> = me.iter().copied().chain([victim]).collect();
        let (mut reads, mut updates, mut readable) = (Vec::new(), Vec::new(), Vec::new());
        for t in &targets {
            let res = get(app, &format!("/api/users/{t}")).await;
            reads.push(went_through(&res));
            if went_through(&res) {
                readable.push(res.json);
            }
            let res = patch(app, &format!("/api/users/{t}"), json!({})).await;
            updates.push(went_through(&res));
        }
        agrees(&format!("{who}: users read"), &u["read"], &reads);
        agrees(&format!("{who}: users update"), &u["update"], &updates);
        let target = targets
            .iter()
            .zip(&updates)
            .find(|(_, ok)| **ok)
            .map_or(victim, |(t, _)| *t);
        for (path, f) in u["fields"].as_object().unwrap() {
            let what = format!("{who}: users field {path} read");
            field_read_agrees(&what, &f["read"], &readable, path);
            let res = patch(app, &format!("/api/users/{target}"), json!({ path: "N" })).await;
            let what = format!("{who}: users field {path} update");
            agrees(&what, &f["update"], &[went_through(&res)]);
            // `name` is required, so creating an account always writes it.
            let new = json!({"email": format!("new-{who}-{path}@example.com"),
                             "password": common::PASSWORD, "role": "viewer", path: "N"});
            let res = send(app, "POST", "/api/users", Some(&new.to_string())).await;
            let what = format!("{who}: users field {path} create");
            agrees(&what, &f["create"], &[went_through(&res)]);
            agrees(
                &format!("{who}: users create"),
                &u["create"],
                &[went_through(&res)],
            );
        }
        let res = send(app, "DELETE", &format!("/api/users/{victim}"), None).await;
        agrees(
            &format!("{who}: users delete"),
            &u["delete"],
            &[went_through(&res)],
        );
        let res = get(app, "/api/users").await;
        agrees(
            &format!("{who}: manage users"),
            &a["users"],
            &[went_through(&res)],
        );
        answers.push((*who, raw, a));
    }
    // Last, since a new collection changes everyone's answers.
    for ((who, app, _), (_, _, a)) in callers.iter().zip(&answers) {
        let schema =
            json!({"slug": format!("made_{who}"), "fields": [{"name": "t", "type": "text"}]});
        let res = send(
            app,
            "POST",
            "/api/admin/collections",
            Some(&schema.to_string()),
        )
        .await;
        agrees(
            &format!("{who}: schemas"),
            &a["schemas"],
            &[went_through(&res)],
        );
    }

    // The table the checks above bore out: every kind of answer, for every kind of rule.
    let c = "conditional";
    let table: Vec<(&str, Value)> = answers
        .iter()
        .map(|(who, _, a)| {
            let row = |v: &Value, actions: &[&str]| -> Vec<Value> {
                actions.iter().map(|k| v[*k].clone()).collect()
            };
            let all = ["read", "create", "update", "delete"];
            let cs = &a["collections"];
            (
                *who,
                json!({
                    "posts": row(&cs["posts"], &all),
                    "notes": row(&cs["notes"], &all),
                    "inbox": row(&cs["inbox"], &all),
                    "users": row(&cs["users"], &all),
                    "home": row(&a["globals"]["home"], &["read", "update"]),
                    "schemas": a["schemas"], "manage": a["users"],
                }),
            )
        })
        .collect();
    let (t, f) = (true, false);
    let row = |posts: Value, notes: Value, inbox: Value, users: Value, home: Value, admin: bool| {
        json!({"posts": posts, "notes": notes, "inbox": inbox, "users": users, "home": home,
               "schemas": admin, "manage": admin})
    };
    let expected = vec![
        (
            "anonymous",
            row(
                json!([c, f, f, f]),
                json!([t, f, f, f]),
                json!([f, t, f, f]),
                json!([f, f, f, f]),
                json!([c, f]),
                f,
            ),
        ),
        (
            "viewer",
            row(
                json!([c, f, c, f]),
                json!([t, f, f, f]),
                json!([f, t, f, f]),
                json!([c, f, c, f]),
                json!([c, f]),
                f,
            ),
        ),
        (
            "author",
            row(
                json!([c, t, c, c]),
                json!([t, t, f, c]),
                json!([f, t, f, f]),
                json!([c, f, c, f]),
                json!([c, c]),
                f,
            ),
        ),
        (
            "editor",
            row(
                json!([t, t, c, t]),
                json!([t, t, f, t]),
                json!([t, t, t, t]),
                json!([c, f, c, f]),
                json!([c, c]),
                f,
            ),
        ),
        (
            "admin",
            row(
                json!([t, t, t, t]),
                json!([t, t, t, t]),
                json!([t, t, t, t]),
                json!([t, t, t, t]),
                json!([t, t]),
                t,
            ),
        ),
        (
            "key",
            row(
                json!([t, f, f, f]),
                json!([t, t, t, t]),
                json!([f, f, f, f]),
                json!([f, f, f, f]),
                json!([f, f]),
                f,
            ),
        ),
    ];
    assert_eq!(table, expected);
    // Left out for the callers they're hidden from: fields nobody of that kind may touch.
    let fields = |who: usize, section: &str, slug: &str| {
        let raw = &answers[who].1;
        raw[section][slug]["fields"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    for (who, section, slug, path) in [
        (0, "collections", "posts", "box.y"),
        (0, "collections", "posts", "peek"),
        (0, "globals", "home", "memo"),
        (1, "collections", "posts", "secret"),
        (2, "collections", "posts", "secret"),
    ] {
        let listed = fields(who, section, slug);
        assert!(
            !listed.iter().any(|p| p == path),
            "{who}: {slug} {path} {listed:?}"
        );
    }
}

/// `posts` (or the global `settings`) as `edit` leaves it.
fn rule_schema(global: bool, edit: impl Fn(&mut Value)) -> Value {
    let mut s = if global {
        json!({"slug": "settings", "kind": "global", "fields": [{"name": "title", "type": "text"}]})
    } else {
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "stage", "type": "select", "options": ["draft", "published"]},
            {"name": "author", "type": "relationship", "relationTo": "users"}
        ]})
    };
    edit(&mut s);
    s
}

/// Every route that saves a schema (create, preview and change) refuses `schema` with
/// `status`, naming the problem in `message`.
async fn refused(admin: &Router, schema: &Value, status: u16, message: &str) {
    let slug = schema["slug"].as_str().unwrap();
    let mut fresh = schema.clone();
    fresh["slug"] = json!("fresh");
    let change = json!({ "schema": schema }).to_string();
    let attempts = [
        (
            "POST",
            "/api/admin/collections".to_owned(),
            fresh.to_string(),
        ),
        (
            "POST",
            format!("/api/admin/collections/{slug}/preview"),
            change.clone(),
        ),
        ("PUT", format!("/api/admin/collections/{slug}"), change),
    ];
    for (method, url, body) in attempts {
        if slug == "users" && method == "POST" && !url.ends_with("preview") {
            continue; // users is built in, so it's never created
        }
        let res = send(admin, method, &url, Some(&body)).await;
        assert_problem(&res, status);
        let said = res.json["errors"][0]["message"]
            .as_str()
            .or(res.json["detail"].as_str())
            .unwrap_or_default();
        assert!(said.contains(message), "{method} {url}: {}", res.json);
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn bad_rules_are_refused_when_saved(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let users = json!({"slug": "users", "fields": [
        {"name": "name", "type": "text", "required": true},
        {"name": "team", "type": "text"},
        {"name": "tags", "type": "text", "hasMany": true}
    ]});
    let res = send(
        &admin,
        "PUT",
        "/api/admin/collections/users",
        Some(&json!({ "schema": users }).to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    create(&admin, "/api/admin/collections", rule_schema(false, |_| {})).await;
    create(&admin, "/api/admin/collections", rule_schema(true, |_| {})).await;
    let reviewer = json!({"slug": "reviewer", "name": "Reviewer",
        "abilities": {"content": "read", "schemas": false, "users": false}});
    create(&admin, "/api/admin/roles", reviewer).await;

    let read = |rule: Value| rule_schema(false, move |s| s["access"] = json!({"read": rule}));
    for (schema, status, message) in [
        (
            read(json!(1)),
            400,
            "access.read: a rule is true, false, {\"role\": …}",
        ),
        (
            read(json!({"role": ["editor", "ghost"]})),
            422,
            "access.read: there's no role `ghost`",
        ),
        (
            read(json!({"team": "$user.nope"})),
            422,
            "access.read: `$user.nope` isn't id, role or a users field",
        ),
        (
            read(json!({"nope": "x"})),
            422,
            "access.read: where[nope][equals]: ",
        ),
        (
            read(json!({"title": {"bogus": "x"}})),
            422,
            "access.read: where[title][bogus]: unknown operator",
        ),
        (
            read(json!({"title": {"equals": "$user.tags"}})),
            422,
            "access.read: `$user.tags` holds a list, so it works only with in or not_in",
        ),
        (
            rule_schema(false, |s| {
                s["access"] = json!({"create": {"stage": "published"}})
            }),
            422,
            "access.create: can use only roles, signedIn, true and false",
        ),
        (
            rule_schema(false, |s| {
                s["fields"][0]["access"] = json!({"read": {"stage": "published"}})
            }),
            422,
            "field `title`: access.read: field rules can use only roles, signedIn, true and false",
        ),
        (
            rule_schema(false, |s| {
                s["fields"].as_array_mut().unwrap().push(
                    json!({"type": "row", "access": {"read": false}, "fields": [
                        {"name": "summary", "type": "text"}
                    ]}),
                )
            }),
            422,
            "field `fields.3`: layout fields hold no data, so access rules go on the fields inside",
        ),
        (
            rule_schema(true, |s| s["access"] = json!({"delete": true})),
            422,
            "access.delete: globals have only read and update rules",
        ),
        (
            rule_schema(true, |s| {
                s["access"] = json!({"create": {"role": "editor"}})
            }),
            422,
            "access.create: globals have only read and update rules",
        ),
        (
            {
                let mut u = users.clone();
                u["access"] = json!({"read": true});
                u
            },
            422,
            "access: rules on users aren't enforced yet",
        ),
    ] {
        refused(&admin, &schema, status, message).await;
    }

    // What the checks allow: custom roles, profile fields, and a list with `in`.
    for ok in [
        json!({"role": "reviewer"}),
        json!({"author": "$user.id", "title": "$user.team"}),
        json!({"title": {"in": "$user.tags"}}),
    ] {
        let res = send(
            &admin,
            "PUT",
            "/api/admin/collections/posts",
            Some(&json!({ "schema": read(ok.clone()) }).to_string()),
        )
        .await;
        assert_eq!(res.status, 200, "{ok}: {}", res.json);
    }

    // The README's example, as written there apart from its slug.
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "stories",
        "access": {
            "read": {"or": [{"published": true}, {"author": "$user.id"}]},
            "update": {"author": "$user.id"}
        },
        "fields": [
            {"name": "title", "type": "text", "required": true},
            {"name": "published", "type": "checkbox"},
            {"name": "author", "type": "relationship", "relationTo": "users"}
        ]}),
    )
    .await;

    // A change elsewhere that would break a saved rule is refused too: posts reads
    // `$user.team`, and its author's profile.
    let res = send(
        &admin,
        "PUT",
        "/api/admin/collections/posts",
        Some(
            &json!({ "schema": read(json!({"title": "$user.team", "author.team": "red"})) })
                .to_string(),
        ),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let mut no_team = users.clone();
    no_team["fields"].as_array_mut().unwrap().remove(1);
    let res = send(
        &admin,
        "PUT",
        "/api/admin/collections/users",
        Some(&json!({ "schema": no_team, "confirm": true }).to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert!(
        res.json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .starts_with("this change would break posts: access.read: "),
        "{}",
        res.json
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn roles_named_by_rules_cant_be_deleted(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    for slug in ["reviewer", "proofer"] {
        let role = json!({"slug": slug, "name": slug,
            "abilities": {"content": "read", "schemas": false, "users": false}});
        create(&admin, "/api/admin/roles", role).await;
    }
    create(
        &admin,
        "/api/admin/collections",
        rule_schema(false, |s| {
            s["fields"][0]["access"] = json!({"update": {"or": [false, {"role": "proofer"}]}})
        }),
    )
    .await;
    create(
        &admin,
        "/api/admin/collections",
        rule_schema(true, |s| {
            s["access"] = json!({"read": {"role": ["editor", "reviewer"]}})
        }),
    )
    .await;
    for (slug, holder) in [("reviewer", "settings"), ("proofer", "posts")] {
        let res = send(&admin, "DELETE", &format!("/api/admin/roles/{slug}"), None).await;
        assert_problem(&res, 409);
        assert_eq!(
            res.json["detail"],
            format!("access rules in {holder} name the role `{slug}`; change them first"),
        );
    }
    let open = json!({ "schema": rule_schema(true, |_| {}) }).to_string();
    let res = send(
        &admin,
        "PUT",
        "/api/admin/collections/settings",
        Some(&open),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = send(&admin, "DELETE", "/api/admin/roles/reviewer", None).await;
    assert_eq!(res.status, 204, "{}", res.json);
}
