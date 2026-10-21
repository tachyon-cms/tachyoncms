//! Blocks through filters, field rules, links, populate, joins and references.

mod common;

use axum::Router;
use common::{Res, admin_app, app_as, assert_problem, send};
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

async fn visitor(pool: &PgPool) -> Router {
    tachyon_api::router(pool.clone()).await.unwrap()
}

fn id(doc: &Value) -> String {
    doc["id"].as_str().unwrap().to_owned()
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

/// A list response without what differs between two identical answers.
fn body(res: &Res) -> Value {
    assert_eq!(res.status, 200, "{}", res.json);
    res.json.clone()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn filters_reach_into_blocks(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "pages", "fields": [
            {"name": "title", "type": "text"},
            {"name": "layout", "type": "blocks", "blocks": [
                {"slug": "hero", "fields": [
                    {"name": "heading", "type": "text"},
                    {"name": "author", "type": "relationship", "relationTo": "authors"},
                    {"name": "tags", "type": "text", "hasMany": true},
                    {"name": "style", "type": "select", "options": ["dark", "light"]},
                    {"name": "cards", "type": "array", "fields": [{"name": "label", "type": "text"}]}
                ]},
                {"slug": "quote", "fields": [
                    {"name": "text", "type": "textarea"},
                    {"name": "author", "type": "relationship", "relationTo": "authors"},
                    {"name": "style", "type": "radio", "options": ["plain", "boxed"]}
                ]},
                // `author` here is text, so `layout.author.name` never reads these rows.
                {"slug": "note", "fields": [{"name": "author", "type": "text"}]}
            ]}
        ]}),
    )
    .await;
    let ann = id(&create(&admin, "/api/authors", json!({"name": "Ann"})).await);
    let bob = id(&create(&admin, "/api/authors", json!({"name": "Bob"})).await);
    create(
        &admin,
        "/api/pages",
        json!({"title": "A", "layout": [
            {"blockType": "hero", "heading": "Hi", "author": ann, "tags": ["x", "y"],
             "style": "dark", "cards": [{"label": "c1"}, {"label": "c2"}]},
            {"blockType": "quote", "text": "Q", "author": ann, "style": "boxed"}
        ]}),
    )
    .await;
    create(
        &admin,
        "/api/pages",
        json!({"title": "B", "layout": [{"blockType": "quote", "text": "Other", "author": bob}]}),
    )
    .await;
    create(&admin, "/api/pages", json!({"title": "C", "layout": []})).await;
    create(
        &admin,
        "/api/pages",
        json!({"title": "D", "layout": [{"blockType": "note", "author": bob}]}),
    )
    .await;

    for (query, want) in [
        ("where[layout.blockType][equals]=hero", &["A"][..]),
        ("where[layout.blockType][equals]=quote", &["A", "B"]),
        ("where[layout.blockType][in]=hero,quote", &["A", "B"]),
        ("where[layout.blockType][not_equals]=quote", &["C", "D"]),
        ("where[layout.blockType][like]=her", &["A"]),
        ("where[layout.blockType][exists]=true", &["A", "B", "D"]),
        // A name several block types share matches rows of any of them.
        ("where[layout.author.name][equals]=Bob", &["B"]),
        ("where[layout.author.name][equals]=Ann", &["A"]),
        ("where[layout.text][like]=oth", &["B"]),
        // Each type's options count: `boxed` is quote's, `dark` hero's.
        ("where[layout.style][equals]=boxed", &["A"]),
        ("where[layout.style][in]=dark,plain", &["A"]),
        // Lists inside a block.
        ("where[layout.tags][contains]=y", &["A"]),
        ("where[layout.cards.label][equals]=c2", &["A"]),
        ("where[layout.cards.label][not_equals]=c2", &["B", "C", "D"]),
        (
            "where[or][0][layout.heading][equals]=Hi&where[or][1][title][equals]=C",
            &["A", "C"],
        ),
    ] {
        let url = format!("/api/pages?{}", query.replace("{bob}", &bob));
        assert_eq!(titles(&get(&admin, &url).await), want, "{url}");
    }

    for (query, message) in [
        (
            "where[layout.blockType][equals]=nope",
            "where[layout.blockType][equals]: expected one of: hero, quote, note",
        ),
        (
            "where[layout.author][equals]=x",
            "where[layout.author][equals]: `layout.author` means different things in block types hero and note",
        ),
        (
            "where[layout.style][equals]=nope",
            "where[layout.style][equals]: expected one of: dark, light, plain, boxed",
        ),
        (
            "where[layout.nope][equals]=x",
            "where[layout.nope][equals]: unknown field `layout.nope`",
        ),
        (
            "where[layout.author.nope][equals]=x",
            "where[layout.author.nope][equals]: `layout.author.nope`: authors has no field `nope`",
        ),
        (
            "where[layout][exists]=true",
            "where[layout][exists]: `layout` can't be filtered",
        ),
        (
            "sort=layout.heading",
            "sort: `layout.heading` is inside a list and can't be sorted",
        ),
    ] {
        let res = get(&admin, &format!("/api/pages?{query}")).await;
        assert_problem(&res, 400);
        assert_eq!(res.json["detail"], message, "{query}");
    }

    // One name with different value kinds in two block types: saving is fine, filtering
    // on it says which.
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "mixed", "fields": [{"name": "layout", "type": "blocks", "blocks": [
            {"slug": "a", "fields": [{"name": "x", "type": "text"}, {"name": "same", "type": "number"}]},
            {"slug": "b", "fields": [{"name": "x", "type": "number"}, {"name": "same", "type": "number"}]}
        ]}]}),
    )
    .await;
    let res = get(&admin, "/api/mixed?where[layout.x][equals]=1").await;
    assert_problem(&res, 400);
    assert_eq!(
        res.json["detail"],
        "where[layout.x][equals]: `layout.x` means different things in block types a and b"
    );
    assert_eq!(
        get(&admin, "/api/mixed?where[layout.same][gt]=1")
            .await
            .status,
        200
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn block_rows_follow_list_rules(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let ed = json!({"read": {"role": "editor"}, "create": {"role": "editor"}, "update": {"role": "editor"}});
    let secret = json!({"name": "secret", "type": "text", "access": ed});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "pages",
            "access": {"create": {"role": ["editor", "author", "viewer"]}, "update": {"role": ["editor", "author", "viewer"]}},
            "fields": [
            {"name": "title", "type": "text"},
            {"name": "layout", "type": "blocks", "blocks": [
                {"slug": "hero", "fields": [
                    {"name": "heading", "type": "text"},
                    {"type": "row", "fields": [secret]},
                    {"name": "cards", "type": "array", "fields": [{"name": "label", "type": "text"}, secret]},
                    {"name": "inner", "type": "blocks", "blocks": [
                        {"slug": "note", "fields": [{"name": "body", "type": "text"}, secret]}
                    ]}
                ]},
                // The same name with no rule: a quote row's `secret` is anyone's.
                {"slug": "quote", "fields": [
                    {"name": "text", "type": "text"},
                    {"name": "secret", "type": "text"},
                    {"name": "inner", "type": "blocks", "blocks": [
                        {"slug": "note", "fields": [{"name": "body", "type": "text"}, secret]},
                        {"slug": "plain", "fields": [{"name": "body", "type": "text"}]}
                    ]}
                ]}
            ]}
        ]}),
    )
    .await;
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    let author = app_as(&pool, "author@example.com", "author").await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;
    let layout = json!([
        {"blockType": "hero", "heading": "H", "secret": "s1",
         "cards": [{"label": "a", "secret": "s2"}, {"label": "b", "secret": "s3"}],
         "inner": [{"blockType": "note", "body": "n", "secret": "s4"}]},
        {"blockType": "quote", "text": "T", "secret": "s5"}
    ]);
    let page = create(
        &admin,
        "/api/pages",
        json!({"title": "P", "layout": layout}),
    )
    .await;
    let url = format!("/api/pages/{}", id(&page));

    // A viewer's read strips hero's `secret` from its rows, at every depth, in lists too;
    // a quote row answers to quote's rules, which hide nothing.
    for app in [&viewer, &author] {
        let seen = get(app, &url).await.json;
        let listed = get(app, "/api/pages").await.json;
        for doc in [&seen, &listed["docs"][0]] {
            let hero = &doc["layout"][0];
            assert!(!hero.to_string().contains("secret"), "{doc}");
            assert_eq!(hero["cards"][1]["label"], "b");
            assert_eq!(hero["inner"][0]["body"], "n");
            assert_eq!(
                doc["layout"][1],
                json!({"id": doc["layout"][1]["id"],
                "blockType": "quote", "text": "T", "secret": "s5"})
            );
        }
    }
    let full = get(&editor, &url).await.json;
    assert_eq!(full["layout"], get(&admin, &url).await.json["layout"]);
    assert_eq!(full["layout"][0]["inner"][0]["secret"], "s4");
    assert_eq!(full["layout"][1]["secret"], "s5");

    // A filter has no row to say which type it means, so `layout.secret` answers for every
    // type: refused as for any hidden field. Unknown names stay 400; the type is anyone's.
    for app in [&viewer, &author] {
        for query in [
            "where[layout.secret][equals]=s5",
            "where[layout.secret][equals]=",
            "where[layout.cards.secret][exists]=true",
            "where[layout.inner.secret][like]=s",
        ] {
            assert_problem(&get(app, &format!("/api/pages?{query}")).await, 403);
        }
        assert_problem(
            &get(app, "/api/pages?where[layout.nope][equals]=1").await,
            400,
        );
        assert_eq!(
            titles(&get(app, "/api/pages?where[layout.blockType][equals]=quote").await),
            ["P"]
        );
        let bad = get(app, "/api/pages?where[layout.blockType][equals]=nope").await;
        assert_problem(&bad, 400);
        assert_eq!(
            bad.json["detail"],
            "where[layout.blockType][equals]: expected one of: hero, quote"
        );
    }
    assert_eq!(
        titles(&get(&editor, "/api/pages?where[layout.secret][equals]=s5").await),
        ["P"]
    );
    assert_eq!(
        titles(&get(&editor, "/api/pages?where[layout.inner.secret][equals]=s4").await),
        ["P"]
    );

    // A PATCH replaces the list whole, so it writes `secret` in every row: refused by name,
    // even with no `secret` in the body.
    let shown = get(&author, &url).await.json;
    let res = patch(&author, &url, json!({"layout": shown["layout"]})).await;
    assert_problem(&res, 403);
    let detail = res.json["detail"].as_str().unwrap();
    assert!(detail.contains("`layout.secret`"), "{detail}");
    assert!(detail.contains("`layout.cards.secret`"), "{detail}");
    assert!(detail.contains("`layout.inner.secret`"), "{detail}");
    assert_eq!(get(&admin, &url).await.json["layout"], full["layout"]);
    assert_eq!(
        patch(&author, &url, json!({"title": "P2"})).await.status,
        200
    );
    // Only the types the list holds or will hold count: with no hero row stored or sent,
    // the Author may replace it. Adding a hero row brings hero's rules in.
    let quotes = create(
        &admin,
        "/api/pages",
        json!({"title": "Q", "layout": [{"blockType": "quote", "text": "a"}]}),
    )
    .await;
    let q_url = format!("/api/pages/{}", id(&quotes));
    let res = patch(
        &author,
        &q_url,
        json!({"layout": [{"blockType": "quote", "secret": "z"}]}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["layout"][0]["secret"], "z");
    let res = patch(
        &author,
        &q_url,
        json!({"layout": [{"blockType": "hero", "heading": "h"}]}),
    )
    .await;
    assert_problem(&res, 403);
    assert!(
        res.json["detail"]
            .as_str()
            .unwrap()
            .contains("`layout.secret`")
    );
    // At every depth: nested rows answer to their own type, sent or replaced.
    let nested = |types: &[&str]| {
        let inner: Vec<Value> = types
            .iter()
            .map(|t| json!({"blockType": t, "body": "b"}))
            .collect();
        json!({"layout": [{"blockType": "quote", "text": "t", "inner": inner}]})
    };
    let n_url = format!(
        "/api/pages/{}",
        id(&create(&admin, "/api/pages", nested(&["plain"])).await)
    );
    let res = patch(&viewer, &n_url, nested(&["plain", "plain"])).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = patch(&viewer, &n_url, nested(&["plain", "note"])).await;
    assert_problem(&res, 403);
    assert_eq!(
        res.json["detail"],
        "You don't have permission to write `layout.inner.secret`."
    );
    assert_eq!(patch(&admin, &n_url, nested(&["note"])).await.status, 200);
    let res = patch(&viewer, &n_url, nested(&["plain"])).await;
    assert_problem(&res, 403);
    assert_eq!(
        res.json["detail"],
        "You don't have permission to write `layout.inner.secret`."
    );
    // Creating: each row answers to its own type, so quote's `secret` is the viewer's to
    // write and hero's isn't.
    let made = send(
        &viewer,
        "POST",
        "/api/pages",
        Some(
            &json!({"title": "Mine", "layout": [{"blockType": "quote", "text": "x", "secret": "q"}]})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(made.status, 201, "{}", made.json);
    assert_eq!(made.json["layout"][0]["secret"], "q");
    let refused = send(
        &viewer,
        "POST",
        "/api/pages",
        Some(
            &json!({"title": "Mine", "layout": [{"blockType": "hero", "secret": "x"}]}).to_string(),
        ),
    )
    .await;
    assert_problem(&refused, 403);
    assert_eq!(
        refused.json["detail"],
        "You don't have permission to write `layout.secret`."
    );

    // A bad stored value inside a row is named only to someone who can read it in that
    // row (R28): hero's is hidden from the Author, quote's isn't.
    sqlx::query(
        "UPDATE documents SET published = jsonb_set(jsonb_set(published, '{layout,0,secret}', '5'), \
         '{layout,1,secret}', '5') WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(&id(&page)).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    let res = patch(&author, &url, json!({"title": "P3"})).await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"],
        json!([
            {"path": "layout.1.secret", "message": "expected text"},
            {"path": "", "message": "a field you can't see needs fixing; ask someone who can see it"}
        ])
    );
    let res = patch(&editor, &url, json!({"title": "P3"})).await;
    assert_problem(&res, 422);
    let paths: Vec<&str> = res.json["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["layout.0.secret", "layout.1.secret"]);

    // Rows keep their IDs through a PATCH that sends them back; a new row gets one.
    let ids = |doc: &Value| -> Vec<String> {
        doc["layout"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let before = ids(&full);
    let mut rows = full["layout"].as_array().unwrap().clone();
    rows.reverse();
    rows.push(json!({"blockType": "quote", "text": "new"}));
    let res = patch(&editor, &url, json!({"layout": rows})).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let after = ids(&res.json);
    assert_eq!(after[..2], [before[1].clone(), before[0].clone()]);
    assert!(!before.contains(&after[2]));
    assert_eq!(res.json["layout"][1]["inner"][0]["secret"], "s4");

    // `/api/access` names block fields without the block type, and ANDs their answers.
    let fields = |a: Value| a["collections"]["pages"]["fields"].clone();
    let v = fields(get(&viewer, "/api/access").await.json);
    assert_eq!(
        v["layout.heading"],
        json!({"read": true, "create": true, "update": false})
    );
    assert!(v.get("layout.secret").is_none(), "{v}");
    assert!(v.get("layout.inner.secret").is_none(), "{v}");
    let a = fields(get(&author, "/api/access").await.json);
    assert_eq!(
        a["layout"],
        json!({"read": true, "create": true, "update": false})
    );
    assert_eq!(
        a["layout.text"],
        json!({"read": true, "create": true, "update": false})
    );
    assert_eq!(a["layout.cards.label"]["create"], true);
    assert!(a.get("layout.secret").is_none(), "{a}");
    let e = fields(get(&editor, "/api/access").await.json);
    for path in [
        "layout",
        "layout.secret",
        "layout.cards.secret",
        "layout.inner.secret",
    ] {
        assert_eq!(
            e[path],
            json!({"read": true, "create": true, "update": true}),
            "{path}"
        );
    }
}

/// Filters through links inside block rows: `(query, what an Admin sees)`. A visitor sees
/// none of them, because each match is only through a hidden document.
const PROBES: &[(&str, &str)] = &[
    ("where[layout.author.name][equals]=Hid", "One"),
    ("where[layout.author.id][equals]={hid}", "One"),
    ("where[layout.author.stage][equals]=draft", "One"),
    ("where[layout.cards.who.name][equals]=Hid", "Two"),
    ("where[layout.inner.who.name][like]=hi", "Two"),
    ("where[layout.people.stage][equals]=draft", "Two"),
    (
        "where[or][0][layout.author.name][equals]=Hid&where[or][1][title][equals]=nope",
        "One",
    ),
];

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn links_inside_blocks_are_guarded(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let published = json!({"read": {"stage": "published"}});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "authors", "access": published, "fields": [
            {"name": "name", "type": "text"},
            {"name": "stage", "type": "select", "options": ["draft", "published"]}
        ]}),
    )
    .await;
    let who = json!({"name": "who", "type": "relationship", "relationTo": "authors"});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "pages", "fields": [
            {"name": "title", "type": "text"},
            {"name": "layout", "type": "blocks", "blocks": [
                {"slug": "quote", "fields": [
                    {"name": "author", "type": "relationship", "relationTo": "authors"}
                ]},
                {"slug": "hero", "fields": [
                    {"name": "cards", "type": "array", "fields": [who]},
                    {"name": "people", "type": "relationship", "relationTo": "authors", "hasMany": true},
                    {"name": "inner", "type": "blocks", "blocks": [{"slug": "card", "fields": [who]}]}
                ]}
            ]}
        ]}),
    )
    .await;
    let author = |name: &str, stage: &str| json!({"name": name, "stage": stage});
    let ann = id(&create(&admin, "/api/authors", author("Ann", "published")).await);
    let hid = id(&create(&admin, "/api/authors", author("Hid", "draft")).await);
    let one = id(&create(
        &admin,
        "/api/pages",
        json!({"title": "One", "layout": [{"blockType": "quote", "author": hid}]}),
    )
    .await);
    let two = id(&create(
        &admin,
        "/api/pages",
        json!({"title": "Two", "layout": [
            {"blockType": "quote", "author": ann},
            {"blockType": "hero", "cards": [{"who": ann}, {"who": hid}],
             "people": [hid],
             "inner": [{"blockType": "card", "who": hid}]}
        ]}),
    )
    .await);
    let anon = visitor(&pool).await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;

    // Populated: a hidden document stays an ID inside every kind of row, at any depth.
    let seen = get(&anon, &format!("/api/pages/{two}?depth=2")).await.json;
    assert_eq!(seen["layout"][0]["author"]["name"], "Ann");
    let hero = &seen["layout"][1];
    assert_eq!(hero["cards"][0]["who"]["name"], "Ann");
    assert_eq!(hero["cards"][1]["who"], hid.as_str());
    assert_eq!(hero["people"][0], hid.as_str());
    assert_eq!(hero["inner"][0]["who"], hid.as_str());
    let seen = get(&anon, &format!("/api/pages/{one}")).await.json;
    assert_eq!(seen["layout"][0]["author"], hid.as_str());
    let full = get(&admin, &format!("/api/pages/{two}?depth=2")).await.json;
    assert_eq!(full["layout"][1]["inner"][0]["who"]["name"], "Hid");

    // Filters through them behave as if the hidden document weren't there: the same answer
    // as for a name nobody has, totals and cursors included.
    for (query, admin_sees) in PROBES {
        let query = query.replace("{hid}", &hid);
        let url = format!("/api/pages?{query}");
        assert_eq!(titles(&get(&admin, &url).await), [*admin_sees], "{url}");
        for app in [&anon, &editor] {
            let probe = format!("/api/pages?count=true&limit=1&{query}");
            let absent = probe
                .replace("Hid", "Nobody")
                .replace("hi", "nobody")
                .replace("draft", "nope")
                .replace(&hid, "0199d5c4-0000-7000-8000-000000000000");
            let (hidden, nothing) = (get(app, &probe).await, get(app, &absent).await);
            assert!(titles(&hidden).is_empty(), "{probe}");
            if !absent.contains("stage") {
                assert_eq!(body(&hidden), body(&nothing), "{probe}");
            }
            assert_eq!(hidden.json["totalDocs"], 0);
            assert!(hidden.json.get("nextCursor").is_none());
        }
    }
    // The negated operators mean "no row matches", so a hidden document counts as no
    // match: the same answer as for a name nobody has.
    for (query, admin_sees) in [
        ("where[layout.author.name][not_equals]=Hid", &["Two"][..]),
        ("where[layout.author.name][not_in]=Hid,Zed", &["Two"]),
        ("where[layout.author.stage][not_in]=draft", &["Two"]),
        ("where[layout.inner.who.name][exists]=false", &["One"]),
        ("where[layout.inner.who.id][not_equals]={hid}", &["One"]),
    ] {
        let url = format!("/api/pages?{}", query.replace("{hid}", &hid));
        assert_eq!(titles(&get(&admin, &url).await), admin_sees, "{url}");
        for app in [&anon, &editor] {
            assert_eq!(titles(&get(app, &url).await), ["One", "Two"], "{url}");
        }
    }
    assert_eq!(
        titles(
            &get(
                &anon,
                "/api/pages?where[layout.author.name][not_equals]=Nobody"
            )
            .await
        ),
        ["One", "Two"]
    );

    // What a readable document holds is found as usual.
    assert_eq!(
        titles(&get(&anon, "/api/pages?where[layout.cards.who.name][equals]=Ann").await),
        ["Two"]
    );

    // A new link to a hidden document is refused exactly as one to no document (R14),
    // at its row's path.
    let missing = "0199d5c4-0000-7000-8000-000000000000";
    let refusal = |target: &str| {
        json!({"title": "X", "layout": [
            {"blockType": "hero", "inner": [{"blockType": "card", "who": target}]}
        ]})
    };
    let mut answers = Vec::new();
    for target in [hid.as_str(), missing] {
        let res = send(
            &editor,
            "POST",
            "/api/pages",
            Some(&refusal(target).to_string()),
        )
        .await;
        assert_problem(&res, 422);
        answers.push(res.json["errors"].clone());
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[0][0]["path"], "layout.0.inner.0.who");
    // One already there is kept when the row is sent back.
    let stored = get(&editor, &format!("/api/pages/{two}?depth=0"))
        .await
        .json;
    let res = patch(
        &editor,
        &format!("/api/pages/{two}"),
        json!({"layout": stored["layout"]}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn joins_and_references_see_block_links(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let name = json!({"name": "name", "type": "text"});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "authors", "fields": [name]}),
    )
    .await;
    let ed = json!({"read": {"role": "editor"}});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "pages", "fields": [
            {"name": "title", "type": "text"},
            {"name": "layout", "type": "blocks", "blocks": [
                {"slug": "hero", "fields": [{"name": "heading", "type": "text"}]},
                {"slug": "quote", "fields": [
                    {"name": "author", "type": "relationship", "relationTo": "authors"}
                ]},
                {"slug": "credits", "fields": [
                    {"name": "author", "type": "relationship", "relationTo": "authors", "hasMany": true}
                ]},
                {"slug": "aside", "fields": [
                    {"name": "editor", "type": "relationship", "relationTo": "authors", "access": ed}
                ]}
            ]}
        ]}),
    )
    .await;
    let change = json!({"schema": {"slug": "authors", "fields": [
        name,
        {"name": "pages", "type": "join", "collection": "pages", "on": "layout.author"},
        {"name": "edited", "type": "join", "collection": "pages", "on": "layout.editor"}
    ]}});
    let res = send(
        &admin,
        "PUT",
        "/api/admin/collections/authors",
        Some(&change.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let ann = id(&create(&admin, "/api/authors", json!({"name": "Ann"})).await);
    let p1 = id(&create(
        &admin,
        "/api/pages",
        json!({"title": "P1", "layout": [
            {"blockType": "hero", "heading": "H"},
            {"blockType": "quote", "author": ann}
        ]}),
    )
    .await);
    let p2 = id(&create(
        &admin,
        "/api/pages",
        json!({"title": "P2", "layout": [
            {"blockType": "credits", "author": [ann]},
            {"blockType": "aside", "editor": ann}
        ]}),
    )
    .await);
    create(&admin, "/api/pages", json!({"title": "P3", "layout": []})).await;
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;

    let joined = |doc: &Value, join: &str| -> Vec<String> {
        let mut out: Vec<String> = doc[join]["docs"]
            .as_array()
            .unwrap_or_else(|| panic!("{doc}"))
            .iter()
            .map(|d| d["title"].as_str().unwrap().to_owned())
            .collect();
        out.sort();
        out
    };
    let url = format!("/api/authors/{ann}?joins[pages][count]=true");
    for app in [&admin, &viewer] {
        let doc = get(app, &url).await.json;
        assert_eq!(joined(&doc, "pages"), ["P1", "P2"]);
        assert_eq!(doc["pages"]["totalDocs"], 2);
    }
    // A join through a field the caller can't read is empty for them.
    assert_eq!(joined(&get(&admin, &url).await.json, "edited"), ["P2"]);
    assert_eq!(joined(&get(&editor, &url).await.json, "edited"), ["P2"]);
    assert!(joined(&get(&viewer, &url).await.json, "edited").is_empty());

    let paths = |res: Res| -> Vec<String> {
        assert_eq!(res.status, 200, "{}", res.json);
        let mut out: Vec<String> = res.json["docs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "{} {}",
                    r["id"].as_str().unwrap(),
                    r["path"].as_str().unwrap()
                )
            })
            .collect();
        out.sort();
        out
    };
    let refs = format!("/api/authors/{ann}/references");
    let mut all = vec![
        format!("{p1} layout.1.author"),
        format!("{p2} layout.0.author.0"),
        format!("{p2} layout.1.editor"),
    ];
    all.sort();
    assert_eq!(paths(get(&admin, &refs).await), all);
    assert_eq!(paths(get(&editor, &refs).await), all);
    all.retain(|p| !p.ends_with("editor"));
    assert_eq!(paths(get(&viewer, &refs).await), all);

    // Links inside block rows hold a document in place: the count is only an Admin's (R15).
    let res = send(&admin, "DELETE", &format!("/api/authors/{ann}"), None).await;
    assert_problem(&res, 409);
    assert_eq!(
        res.json["detail"],
        "2 other documents link to this one; remove those links first"
    );
    let res = send(&editor, "DELETE", &format!("/api/authors/{ann}"), None).await;
    assert_problem(&res, 409);
    assert_eq!(
        res.json["detail"],
        "Other documents link to this one; remove those links first"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn rows_of_removed_block_types_show_only_their_type(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let name = json!({"name": "name", "type": "text"});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "authors", "fields": [name]}),
    )
    .await;
    // No field rules anywhere: nothing is hidden from anyone.
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "pages", "fields": [
            {"name": "title", "type": "text"},
            {"name": "layout", "type": "blocks", "blocks": [{"slug": "card", "fields": [
                {"name": "who", "type": "relationship", "relationTo": "authors"},
                {"name": "note", "type": "text"}
            ]}]}
        ]}),
    )
    .await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "shelf", "fields": [
            {"name": "page", "type": "relationship", "relationTo": "pages"}
        ]}),
    )
    .await;
    let change = json!({"schema": {"slug": "authors", "fields": [
        name,
        {"name": "pages", "type": "join", "collection": "pages", "on": "layout.who"}
    ]}});
    let res = send(
        &admin,
        "PUT",
        "/api/admin/collections/authors",
        Some(&change.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let ann = id(&create(&admin, "/api/authors", json!({"name": "Ann"})).await);
    let page = id(&create(
        &admin,
        "/api/pages",
        json!({"title": "P", "layout": [
            {"blockType": "card", "who": ann, "note": "keep out"},
            {"blockType": "card", "who": ann, "note": "fine"}
        ]}),
    )
    .await);
    let shelf = id(&create(&admin, "/api/shelf", json!({"page": page})).await);
    // A row left behind by a block type that's gone, as removing one with rows leaves it.
    sqlx::query(
        "UPDATE documents SET published = jsonb_set(published, '{layout,0,blockType}', '\"gone\"') \
         WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(&page).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    let anon = visitor(&pool).await;
    for app in [&admin, &anon] {
        let layouts = [
            get(app, "/api/pages").await.json["docs"][0]["layout"].clone(),
            get(app, &format!("/api/pages/{page}")).await.json["layout"].clone(),
            get(app, &format!("/api/shelf/{shelf}?depth=1")).await.json["page"]["layout"].clone(),
            get(app, &format!("/api/authors/{ann}")).await.json["pages"]["docs"][0]["layout"]
                .clone(),
        ];
        for layout in layouts {
            assert_eq!(
                layout[0],
                json!({"id": layout[0]["id"], "blockType": "gone"}),
                "{layout}"
            );
            assert_eq!(layout[1]["note"], "fine", "{layout}");
        }
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn profile_block_rows_answer_to_their_own_type(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let ed = json!({"read": {"role": "editor"}, "update": {"role": "editor"}});
    let users = json!({"schema": {"slug": "users", "fields": [
        {"name": "name", "type": "text", "required": true},
        {"name": "bio", "type": "blocks", "blocks": [
            {"slug": "private", "fields": [{"name": "note", "type": "text", "access": ed}]},
            {"slug": "public", "fields": [{"name": "note", "type": "text"}]}
        ]}
    ]}});
    let res = send(
        &admin,
        "PUT",
        "/api/admin/collections/users",
        Some(&users.to_string()),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let vic = common::add_user(&pool, "vic@example.com", "viewer").await;
    let me = format!("/api/users/{vic}");
    let viewer = common::signed_in(visitor(&pool).await, "vic@example.com").await;
    let public = |note: &str| json!({"blockType": "public", "note": note});

    // Only public rows held or sent: the Viewer's own to replace.
    let res = patch(&viewer, &me, json!({"bio": [public("hi")]})).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["bio"][0]["note"], "hi");
    let res = patch(
        &viewer,
        &me,
        json!({"bio": [{"blockType": "private", "note": "x"}]}),
    )
    .await;
    assert_problem(&res, 403);
    assert_eq!(
        res.json["detail"],
        "You don't have permission to write `bio.note`."
    );

    // Once a private row is stored, replacing the list replaces it too.
    let res = patch(
        &admin,
        &me,
        json!({"bio": [{"blockType": "private", "note": "x"}, public("hi")]}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = patch(&viewer, &me, json!({"bio": [public("bye")]})).await;
    assert_problem(&res, 403);
    assert_eq!(
        res.json["detail"],
        "You don't have permission to write `bio.note`."
    );
    // A name alone leaves the list alone.
    assert_eq!(
        patch(&viewer, &me, json!({"name": "Vic"})).await.status,
        200
    );
    // Reading: each row by its own type.
    let seen = get(&viewer, &me).await.json;
    assert_eq!(
        seen["bio"][0],
        json!({"id": seen["bio"][0]["id"], "blockType": "private"})
    );
    assert_eq!(seen["bio"][1]["note"], "hi");
}

async fn put(app: &Router, url: &str, body: Value) -> Res {
    send(app, "PUT", url, Some(&body.to_string())).await
}

/// A shared `cta` block with `fields`, as a schema.
fn cta(fields: Value) -> Value {
    json!({"slug": "cta", "kind": "block", "fields": fields})
}

/// A collection whose blocks field `list` offers the shared `cta` and an inline `quote`.
fn referencing(slug: &str, list: &str) -> Value {
    json!({"slug": slug, "access": {"read": true}, "fields": [
        {"name": "title", "type": "text"},
        {"name": list, "type": "blocks", "blocks": [
            {"blockRef": "cta"},
            {"slug": "quote", "fields": [{"name": "text", "type": "text"}]}
        ]}
    ]})
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn shared_blocks_expand_everywhere(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let other = visitor(&pool).await;
    create(
        &admin,
        "/api/admin/collections",
        cta(json!([{"name": "label", "type": "text", "required": true}])),
    )
    .await;
    create(
        &admin,
        "/api/admin/collections",
        referencing("pages", "layout"),
    )
    .await;
    create(
        &admin,
        "/api/admin/collections",
        referencing("posts", "sections"),
    )
    .await;

    // The schema keeps the reference; documents see the definition.
    let saved = get(&admin, "/api/admin/collections/pages").await.json;
    assert_eq!(
        saved["schema"]["fields"][1]["blocks"][0],
        json!({"blockRef": "cta"})
    );
    for (url, list) in [("/api/pages", "layout"), ("/api/posts", "sections")] {
        let doc = create(
            &admin,
            url,
            json!({"title": "A", list: [{"blockType": "cta", "label": "Go"}, {"blockType": "quote", "text": "Q"}]}),
        )
        .await;
        let read = get(&admin, &format!("{url}/{}", id(&doc))).await.json;
        assert_eq!(read[list][0]["label"], "Go");
        assert_eq!(read[list][1]["text"], "Q");
        // The block's own rules apply in every collection.
        let res = send(
            &admin,
            "POST",
            url,
            Some(&json!({"title": "B", list: [{"blockType": "cta"}]}).to_string()),
        )
        .await;
        assert_problem(&res, 422);
        assert_eq!(res.json["errors"][0]["path"], format!("{list}.0.label"));
    }

    // A schema is checked with its shared blocks in place: a select may reach into one.
    let mut notes = referencing("notes", "layout");
    notes["defaultPopulate"] = json!({"layout": {"label": true}});
    create(&admin, "/api/admin/collections", notes).await;

    // Changing the block changes it everywhere, on this server at once.
    let res = put(
        &admin,
        "/api/admin/collections/cta",
        json!({"schema": cta(json!([
            {"name": "label", "type": "text", "required": true},
            {"name": "url", "type": "text"}
        ]))}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    // Writes checked against the old fields must retry, so each schema naming it moves on.
    let saved = get(&admin, "/api/admin/collections/posts").await.json;
    assert_eq!(saved["schemaVersion"], 2);
    for (url, list) in [("/api/pages", "layout"), ("/api/posts", "sections")] {
        assert_eq!(
            res.json["preview"]["collections"][&url[5..]]["diff"]["changes"],
            json!([{"type": "added", "path": format!("{list}.cta.url"), "kind": "text"}])
        );
        let doc = create(
            &admin,
            url,
            json!({"title": "C", list: [{"blockType": "cta", "label": "Go", "url": "/x"}]}),
        )
        .await;
        assert_eq!(doc[list][0]["url"], "/x");
    }

    // ...and on another server through NOTIFY.
    for _ in 0..50 {
        let res = get(&other, "/api/posts?where[sections.url][equals]=/x").await;
        if res.status == 200 {
            assert_eq!(titles(&res), ["C"]);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the other server never saw the changed block");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn breaking_a_shared_block_is_refused(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let fields = json!([{"name": "label", "type": "text"}, {"name": "note", "type": "text"}]);
    create(&admin, "/api/admin/collections", cta(fields.clone())).await;
    create(
        &admin,
        "/api/admin/collections",
        referencing("pages", "layout"),
    )
    .await;
    create(
        &admin,
        "/api/admin/collections",
        referencing("posts", "sections"),
    )
    .await;
    let page = id(&create(
        &admin,
        "/api/pages",
        json!({"title": "P", "layout": [{"blockType": "cta", "label": "Go", "note": "n1"}]}),
    )
    .await);
    let post = id(&create(
        &admin,
        "/api/posts",
        json!({"title": "Q", "sections": [{"blockType": "cta", "note": "n2"}]}),
    )
    .await);

    // Requiring `label` leaves the post invalid: the report names it, per collection.
    let required = json!([
        {"name": "label", "type": "text", "required": true}, {"name": "note", "type": "text"}
    ]);
    let res = put(
        &admin,
        "/api/admin/collections/cta",
        json!({"schema": cta(required)}),
    )
    .await;
    assert_problem(&res, 409);
    let report = &res.json["preview"]["collections"];
    assert_eq!(report["pages"]["invalid"], 0);
    assert_eq!(report["posts"]["invalid"], 1);
    assert_eq!(report["posts"]["examples"][0]["id"], post);
    assert_eq!(
        report["posts"]["examples"][0]["errors"][0]["path"],
        "sections.0.label"
    );

    // Removing a field documents use needs confirmation, with the change named per collection.
    let removed = json!([{"name": "label", "type": "text"}]);
    let res = put(
        &admin,
        "/api/admin/collections/cta",
        json!({"schema": cta(removed.clone())}),
    )
    .await;
    assert_problem(&res, 409);
    for (slug, list) in [("pages", "layout"), ("posts", "sections")] {
        assert_eq!(
            res.json["preview"]["collections"][slug]["diff"]["changes"],
            json!([{"type": "removed", "path": format!("{list}.cta.note"), "kind": "text"}])
        );
        assert_eq!(res.json["preview"]["collections"][slug]["documents"], 1);
    }
    assert_eq!(
        get(&admin, &format!("/api/pages/{page}")).await.json["layout"][0]["note"],
        "n1",
        "a refused change changes nothing"
    );

    // A rename names the field within the block, and carries data in every collection.
    let renamed = json!([{"name": "label", "type": "text"}, {"name": "remark", "type": "text"}]);
    let res = put(
        &admin,
        "/api/admin/collections/cta",
        json!({"schema": cta(renamed), "renames": {"note": "remark"}}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        get(&admin, &format!("/api/pages/{page}")).await.json["layout"][0]["remark"],
        "n1"
    );
    assert_eq!(
        get(&admin, &format!("/api/posts/{post}")).await.json["sections"][0]["remark"],
        "n2"
    );

    let res = put(
        &admin,
        "/api/admin/collections/cta",
        json!({"schema": cta(removed), "confirm": true}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let doc = get(&admin, &format!("/api/pages/{page}")).await.json;
    assert!(doc["layout"][0].get("remark").is_none(), "{doc}");

    // A referenced block can't go; an unreferenced one can.
    let res = send(&admin, "DELETE", "/api/admin/collections/cta", None).await;
    assert_problem(&res, 409);
    assert!(
        res.json["detail"].as_str().unwrap().contains("pages"),
        "{}",
        res.json
    );
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "spare", "kind": "block", "fields": [{"name": "x", "type": "text"}]}),
    )
    .await;
    let res = send(&admin, "DELETE", "/api/admin/collections/spare", None).await;
    assert_eq!(res.status, 204, "{}", res.json);
    // Collections and globals aren't deleted this way.
    assert_problem(
        &send(&admin, "DELETE", "/api/admin/collections/pages", None).await,
        409,
    );

    // A change that breaks a join elsewhere, through a block's field, is refused.
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    let byline = |fields: Value| json!({"slug": "byline", "kind": "block", "fields": fields});
    let who = json!([{"name": "who", "type": "relationship", "relationTo": "authors"}]);
    create(&admin, "/api/admin/collections", byline(who)).await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "stories", "fields": [
            {"name": "layout", "type": "blocks", "blocks": [{"blockRef": "byline"}]}
        ]}),
    )
    .await;
    let res = put(
        &admin,
        "/api/admin/collections/authors",
        json!({"schema": {"slug": "authors", "fields": [
            {"name": "name", "type": "text"},
            {"name": "stories", "type": "join", "collection": "stories", "on": "layout.who"}
        ]}}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = put(
        &admin,
        "/api/admin/collections/byline",
        json!({"schema": byline(json!([{"name": "name", "type": "text"}])), "confirm": true}),
    )
    .await;
    assert_problem(&res, 422);
    assert!(
        res.json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .starts_with("this change would break authors"),
        "{}",
        res.json
    );

    // A reference must name a shared block.
    let mut bad = referencing("notes", "layout");
    bad["fields"][1]["blocks"][0] = json!({"blockRef": "spare"});
    let res = send(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(&bad.to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert!(
        res.json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("spare"),
        "{}",
        res.json
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn blocks_have_no_document_routes(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let reviewer = json!({"slug": "reviewer", "name": "Reviewer",
                          "abilities": {"content": "read", "schemas": false, "users": false}});
    let res = send(
        &admin,
        "POST",
        "/api/admin/roles",
        Some(&reviewer.to_string()),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    create(
        &admin,
        "/api/admin/collections",
        cta(json!([{"name": "label", "type": "text", "access": {"read": {"role": ["reviewer"]}}}])),
    )
    .await;
    for (method, url) in [
        ("GET", "/api/cta"),
        ("POST", "/api/cta"),
        ("GET", "/api/globals/cta"),
        ("PATCH", "/api/globals/cta"),
    ] {
        let res = send(&admin, method, url, Some("{}")).await;
        assert_problem(&res, 404);
    }
    let access = get(&admin, "/api/access").await;
    assert_eq!(access.status, 200);
    assert!(
        !access.json.to_string().contains("\"cta\""),
        "{}",
        access.json
    );
    // Nothing links to or joins a block.
    let link = json!({"slug": "posts", "fields": [
        {"name": "c", "type": "relationship", "relationTo": "cta"}
    ]});
    assert_problem(
        &send(
            &admin,
            "POST",
            "/api/admin/collections",
            Some(&link.to_string()),
        )
        .await,
        422,
    );
    // A block takes no access rules or other blocks' references.
    for bad in [
        json!({"slug": "b1", "kind": "block", "access": {"read": true}, "fields": []}),
        json!({"slug": "b2", "kind": "block", "fields": [
            {"name": "inner", "type": "blocks", "blocks": [{"blockRef": "cta"}]}
        ]}),
        json!({"slug": "blocks", "kind": "block", "fields": []}),
    ] {
        let res = send(
            &admin,
            "POST",
            "/api/admin/collections",
            Some(&bad.to_string()),
        )
        .await;
        assert_problem(&res, 422);
    }
    // A role a shared block's field rules name can't be deleted.
    let res = send(&admin, "DELETE", "/api/admin/roles/reviewer", None).await;
    assert_problem(&res, 409);
    assert!(
        res.json["detail"].as_str().unwrap().contains("cta"),
        "{}",
        res.json
    );
}
