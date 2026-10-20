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
        {"name": "team", "type": "text"}
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
    let writer = app_as(&pool, "wes@example.com", "author").await;
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
    // Amy can't hand it to Bob: she can't read his profile, so the link is refused as if he
    // didn't exist. Once an Admin does, it isn't hers to change.
    let res = patch(&amy, &url(&a1), json!({"author": bob_id})).await;
    assert_problem(&res, 422);
    let res = patch(&admin, &url(&a1), json!({"author": bob_id})).await;
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
