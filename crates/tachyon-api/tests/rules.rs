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
            {"name": "age", "type": "number"}
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
    let author = |name: &str, age: u32, stage: &str, mentor: Option<&str>| json!({"name": name, "age": age, "stage": stage, "mentor": mentor});
    let ann = id(&create(&admin, "/api/authors", author("Ann", 30, "published", None)).await);
    // Hid is a hidden author with a mentor of his own, so Mia -> Hid -> Ann has a hidden middle.
    let hid = id(&create(
        &admin,
        "/api/authors",
        author("Hid", 50, "draft", Some(&ann)),
    )
    .await);
    let mia = id(&create(
        &admin,
        "/api/authors",
        author("Mia", 20, "published", Some(&hid)),
    )
    .await);
    let bo = id(&create(
        &admin,
        "/api/authors",
        author("Bo", 40, "published", Some(&ann)),
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
    assert_eq!(get(&anon, "/api/posts?sort=author").await.status, 200);
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
