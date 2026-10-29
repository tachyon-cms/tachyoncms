//! Rich text through the API: block node field rules, links, references and shared blocks.

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

async fn put(app: &Router, url: &str, body: Value) -> Res {
    send(app, "PUT", url, Some(&body.to_string())).await
}

fn id(doc: &Value) -> String {
    doc["id"].as_str().unwrap().to_owned()
}

fn text(content: Value) -> Value {
    json!({"type": "doc", "content": content})
}

fn node(block_type: &str, fields: Value) -> Value {
    json!({"type": "block", "attrs": {"blockType": block_type, "fields": fields}})
}

fn doc_link(collection: &str, id: &str) -> Value {
    json!({"type": "paragraph", "content": [{"type": "text", "text": "see", "marks": [
        {"type": "link", "attrs": {"doc": {"relationTo": collection, "value": id}}}
    ]}]})
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn block_nodes_follow_field_rules(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let ed = json!({"read": {"role": "editor"}, "create": {"role": "editor"}, "update": {"role": "editor"}});
    let secret = json!({"name": "secret", "type": "text", "access": ed});
    for schema in [
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
        json!({"slug": "posts", "fields": [{"name": "title", "type": "text"}]}),
        json!({"slug": "secrets", "access": {"read": {"role": "editor"}},
            "fields": [{"name": "title", "type": "text"}]}),
    ] {
        create(&admin, "/api/admin/collections", schema).await;
    }
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "pages",
            "access": {"create": {"role": ["editor", "viewer"]}, "update": {"role": ["editor", "viewer"]}},
            "fields": [
            {"name": "title", "type": "text"},
            {"name": "body", "type": "richText", "relationTo": ["posts", "authors", "secrets"], "blocks": [
                {"slug": "hero", "fields": [
                    {"name": "heading", "type": "text"},
                    secret,
                    {"name": "who", "type": "relationship", "relationTo": "authors", "access": {"read": {"role": "editor"}}},
                    {"name": "inner", "type": "richText", "blocks": [
                        {"slug": "note", "fields": [{"name": "text", "type": "text"}, secret]}
                    ]}
                ]},
                // The same name with no rule: a quote node's `secret` is anyone's.
                {"slug": "quote", "fields": [{"name": "text", "type": "text"}, {"name": "secret", "type": "text"}]}
            ]}
        ]}),
    )
    .await;
    let ann = id(&create(&admin, "/api/authors", json!({"name": "Ann"})).await);
    let post = id(&create(&admin, "/api/posts", json!({"title": "Hello"})).await);
    let hidden = id(&create(&admin, "/api/secrets", json!({"title": "Classified"})).await);
    let hero = node(
        "hero",
        json!({"heading": "H", "secret": "s1", "who": ann,
            "inner": text(json!([{"type": "blockquote", "content": [node("note", json!({"text": "n", "secret": "s4"}))]}]))}),
    );
    let body = text(json!([
        doc_link("posts", &post),
        {"type": "relationship", "attrs": {"relationTo": "authors", "value": ann}},
        {"type": "bulletList", "content": [{"type": "listItem", "content": [hero]}]},
        node("quote", json!({"text": "T", "secret": "s5"})),
        doc_link("secrets", &hidden)
    ]));
    let page = create(&admin, "/api/pages", json!({"title": "P", "body": body})).await;
    let url = format!("/api/pages/{}", id(&page));
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;

    // A viewer's read strips hero's ruled fields from its node at every depth, in single
    // reads, lists and populated reads; a quote node answers to quote's rules.
    let hero_at = "/body/content/2/content/0/content/0/attrs/fields";
    for path in [url.clone(), format!("{url}?depth=1"), "/api/pages".into()] {
        let res = get(&viewer, &path).await;
        assert_eq!(res.status, 200, "{}", res.json);
        let doc = res.json.get("docs").map_or(&res.json, |d| &d[0]).clone();
        let fields = doc.pointer(hero_at).unwrap();
        assert_eq!(fields["heading"], "H");
        let shown = fields.to_string();
        assert!(!shown.contains("s1") && !shown.contains("s4"), "{fields}");
        assert!(fields.get("who").is_none(), "{fields}");
        assert_eq!(
            fields.pointer("/inner/content/0/content/0/attrs/fields"),
            Some(&json!({"text": "n"}))
        );
        assert_eq!(doc["body"]["content"][3]["attrs"]["fields"]["secret"], "s5");
    }
    let full = get(&editor, &url).await.json;
    assert_eq!(full.pointer(hero_at).unwrap()["secret"], "s1");

    // Populating fills links the caller may read; a hidden document stays a bare ID.
    let deep = get(&viewer, &format!("{url}?depth=1")).await.json;
    let content = &deep["body"]["content"];
    assert_eq!(
        content[0]["content"][0]["marks"][0]["attrs"]["doc"]["value"]["title"],
        "Hello"
    );
    assert_eq!(content[1]["attrs"]["value"]["name"], "Ann");
    assert_eq!(
        content[4]["content"][0]["marks"][0]["attrs"]["doc"]["value"],
        hidden.as_str()
    );
    assert!(!deep.to_string().contains("Classified"));

    // Writes: a node answers to its own type. The text is replaced whole, so replacing a
    // stored hero node counts hero's fields.
    let res = patch(
        &viewer,
        &url,
        json!({"body": text(json!([node("hero", json!({"secret": "x"}))]))}),
    )
    .await;
    assert_problem(&res, 403);
    assert!(
        res.json["detail"].as_str().unwrap().contains("body.secret"),
        "{}",
        res.json
    );
    let quotes = json!({"body": text(json!([node("quote", json!({"secret": "mine"}))]))});
    let res = patch(&viewer, &url, quotes.clone()).await;
    assert_problem(&res, 403);
    let own = create(
        &viewer,
        "/api/pages",
        json!({"title": "V", "body": quotes["body"]}),
    )
    .await;
    let own_url = format!("/api/pages/{}", id(&own));
    let res = patch(&viewer, &own_url, quotes.clone()).await;
    assert_eq!(res.status, 200, "{}", res.json);
    // Linking a document the caller can't read is refused as if it didn't exist (R14).
    let mut linked = quotes.clone();
    linked["body"]["content"]
        .as_array_mut()
        .unwrap()
        .push(doc_link("secrets", &hidden));
    let res = patch(&viewer, &own_url, linked.clone()).await;
    assert_problem(&res, 422);
    // The 422 names the link's node, which the editor can point at (finding: not `body.1`).
    assert_eq!(
        res.json["errors"][0]["path"],
        "body.content.1.content.0.marks.0.attrs.doc.value"
    );
    let missing = linked
        .to_string()
        .replace(&hidden, "0199d5c4-0000-7000-8000-00000000dead");
    let gone = patch(&viewer, &own_url, serde_json::from_str(&missing).unwrap()).await;
    assert_eq!(gone.json["errors"], res.json["errors"]);
    // Bad text is refused with the node's path.
    let res = patch(
        &viewer,
        &own_url,
        json!({"body": text(json!([{"type": "paragraph", "content": [{"type": "text", "text": "x",
            "marks": [{"type": "link", "attrs": {"href": "JaVaScRiPt:alert(1)"}}]}]}]))}),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["path"],
        "body.content.0.content.0.marks.0.attrs.href"
    );

    // References and delete protection see links in the text and in its block nodes; a
    // field the caller can't read hides its links.
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
    let ann_refs = format!("/api/authors/{ann}/references");
    assert_eq!(refs(get(&admin, &ann_refs).await), ["body.1", "body.2.who"]);
    assert_eq!(
        refs(get(&editor, &ann_refs).await),
        ["body.1", "body.2.who"]
    );
    assert_eq!(refs(get(&viewer, &ann_refs).await), ["body.1"]);
    assert_eq!(
        refs(get(&admin, &format!("/api/posts/{post}/references")).await),
        ["body.0"]
    );
    let res = send(&admin, "DELETE", &format!("/api/posts/{post}"), None).await;
    assert_problem(&res, 409);
    assert_eq!(
        res.json["detail"],
        "1 other document links to this one; remove that link first"
    );

    // Filters: rich text has a value or not; nothing inside it can be named.
    let res = get(&viewer, "/api/pages?where[body][exists]=true").await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["docs"].as_array().unwrap().len(), 2);
    for bad in [
        "where[body][equals]=x",
        "where[body.heading][equals]=H",
        "sort=body",
    ] {
        assert_problem(&get(&viewer, &format!("/api/pages?{bad}")).await, 400);
    }
}

/// One block type makes `body` rich text (and `extra` json), another a group holding a
/// link the viewer can't read: that link stays out of their references.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_hidden_link_beside_rich_text_of_its_name_stays_hidden(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    let secret = json!({"name": "secret", "type": "relationship", "relationTo": "authors",
        "access": {"read": {"role": "editor"}}});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "pages", "fields": [{"name": "layout", "type": "blocks", "blocks": [
            {"slug": "a", "fields": [{"name": "body", "type": "richText"}, {"name": "extra", "type": "json"}]},
            {"slug": "b", "fields": [
                {"name": "body", "type": "group", "fields": [secret]},
                {"name": "extra", "type": "group", "fields": [secret]}
            ]}
        ]}]}),
    )
    .await;
    let ann = id(&create(&admin, "/api/authors", json!({"name": "Ann"})).await);
    create(
        &admin,
        "/api/pages",
        json!({"layout": [{"blockType": "b", "body": {"secret": ann}, "extra": {"secret": ann}}]}),
    )
    .await;
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    let editor = app_as(&pool, "editor@example.com", "editor").await;
    let url = format!("/api/authors/{ann}/references");
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
        paths(get(&editor, &url).await),
        ["layout.0.body.secret", "layout.0.extra.secret"]
    );
    assert!(paths(get(&viewer, &url).await).is_empty());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn shared_blocks_in_rich_text_expand_change_and_stay(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let cta = |fields: Value| json!({"slug": "cta", "kind": "block", "fields": fields});
    let label = json!({"name": "label", "type": "text", "required": true});
    create(
        &admin,
        "/api/admin/collections",
        cta(json!([label, {"name": "note", "type": "text"}])),
    )
    .await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "pages", "fields": [
            {"name": "body", "type": "richText", "blocks": [{"blockRef": "cta"}]}
        ]}),
    )
    .await;
    let saved = get(&admin, "/api/admin/collections/pages").await.json;
    assert_eq!(
        saved["schema"]["fields"][0]["blocks"][0],
        json!({"blockRef": "cta"})
    );

    // Documents see the block's definition.
    let body = text(
        json!([{"type": "blockquote", "content": [node("cta", json!({"label": "Go", "note": "n1"}))]}]),
    );
    let page = id(&create(&admin, "/api/pages", json!({"body": body})).await);
    let url = format!("/api/pages/{page}");
    let at = "/body/content/0/content/0/attrs/fields";
    assert_eq!(
        get(&admin, &url).await.json.pointer(at),
        Some(&json!({"label": "Go", "note": "n1"}))
    );
    let res = send(
        &admin,
        "POST",
        "/api/pages",
        Some(&json!({"body": text(json!([node("cta", json!({}))]))}).to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["path"],
        "body.content.0.attrs.fields.label"
    );

    // A rename in the block carries the data in every node of that type.
    let res = put(
        &admin,
        "/api/admin/collections/cta",
        json!({"schema": cta(json!([label, {"name": "remark", "type": "text"}])), "renames": {"note": "remark"}}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        res.json["preview"]["collections"]["pages"]["diff"]["changes"],
        json!([{"type": "renamed", "from": "body.cta.note", "to": "body.cta.remark"}])
    );
    assert_eq!(
        get(&admin, &url).await.json.pointer(at),
        Some(&json!({"label": "Go", "remark": "n1"}))
    );

    // A change that leaves documents invalid waits, counting them.
    let res = put(
        &admin,
        "/api/admin/collections/cta",
        json!({"schema": cta(json!([label, {"name": "remark", "type": "text"}, {"name": "extra", "type": "text", "required": true}]))}),
    )
    .await;
    assert_problem(&res, 409);
    let report = &res.json["preview"]["collections"]["pages"];
    assert_eq!(report["invalid"], 1);
    assert_eq!(
        report["examples"][0]["errors"][0]["path"],
        "body.content.0.content.0.attrs.fields.extra"
    );

    // A block named only in rich text can't be deleted.
    let res = send(&admin, "DELETE", "/api/admin/collections/cta", None).await;
    assert_problem(&res, 409);
    assert!(
        res.json["detail"].as_str().unwrap().contains("pages"),
        "{}",
        res.json
    );

    // Narrowing features while a document uses one waits too, with the count.
    let res = put(
        &admin,
        "/api/admin/collections/pages",
        json!({"schema": {"slug": "pages", "fields": [
            {"name": "body", "type": "richText", "features": ["block"], "blocks": [{"blockRef": "cta"}]}
        ]}}),
    )
    .await;
    assert_problem(&res, 409);
    assert_eq!(res.json["preview"]["invalid"], 1, "{}", res.json);
    assert_eq!(
        res.json["preview"]["examples"][0]["errors"][0]["message"],
        "the `blockquote` feature isn't turned on for this field"
    );
}

fn para(s: &str) -> Value {
    json!({"type": "paragraph", "content": [{"type": "text", "text": s}]})
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn format_param_applies_everywhere(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    for schema in [
        json!({"slug": "authors", "fields": [
            {"name": "name", "type": "text"},
            {"name": "bio", "type": "richText"}
        ]}),
        json!({"slug": "posts", "url": "/blog/{slug}", "fields": [
            {"name": "title", "type": "text"},
            {"name": "slug", "type": "slug"},
            {"name": "body", "type": "richText", "relationTo": ["posts"]},
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "related", "type": "relationship", "relationTo": "posts"}
        ]}),
        json!({"slug": "site", "kind": "global", "fields": [{"name": "footer", "type": "richText"}]}),
    ] {
        create(&admin, "/api/admin/collections", schema).await;
    }
    let joins = json!({"schema": {"slug": "authors", "fields": [
        {"name": "name", "type": "text"},
        {"name": "bio", "type": "richText"},
        {"name": "posts", "type": "join", "collection": "posts", "on": "author"}
    ]}});
    let res = put(&admin, "/api/admin/collections/authors", joins).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let ann = id(&create(
        &admin,
        "/api/authors",
        json!({"name": "Ann", "bio": text(json!([para("Writes <things>")]))}),
    )
    .await);
    let first = id(&create(
        &admin,
        "/api/posts",
        json!({"title": "Hello", "slug": "hello", "author": ann, "body": text(json!([para("First")]))}),
    )
    .await);
    let second = id(&create(
        &admin,
        "/api/posts",
        json!({"title": "Two", "slug": "two", "author": ann, "related": first,
            "body": text(json!([doc_link("posts", &first)]))}),
    )
    .await);
    let res = patch(
        &admin,
        "/api/globals/site",
        json!({"footer": text(json!([para("Bye *now*")]))}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);

    let ok = |res: Res| {
        assert_eq!(res.status, 200, "{}", res.json);
        res.json
    };
    // List and single reads, in each format.
    let list = ok(get(&admin, "/api/posts?format=html&sort=title").await);
    assert_eq!(list["docs"][0]["body"], "<p>First</p>");
    assert_eq!(
        list["docs"][1]["body"],
        "<p><a href=\"/blog/hello\">see</a></p>"
    );
    let one = ok(get(&admin, &format!("/api/posts/{second}?format=markdown")).await);
    assert_eq!(one["body"], "[see](doc:posts/hello)");
    let one = ok(get(&admin, &format!("/api/posts/{second}?format=text")).await);
    assert_eq!(one["body"], "see");
    let one = ok(get(&admin, &format!("/api/posts/{second}?format=json")).await);
    assert_eq!(one["body"]["type"], "doc");
    let one = ok(get(&admin, &format!("/api/posts/{second}")).await);
    assert_eq!(one["body"]["type"], "doc");

    // Populated documents, two deep, and their own links.
    let deep = ok(get(&admin, &format!("/api/posts/{second}?format=html&depth=2")).await);
    assert_eq!(deep["related"]["body"], "<p>First</p>");
    assert_eq!(
        deep["related"]["author"]["bio"],
        "<p>Writes &lt;things&gt;</p>"
    );
    assert_eq!(deep["author"]["bio"], "<p>Writes &lt;things&gt;</p>");
    // Joined documents.
    let joined = ok(get(&admin, &format!("/api/authors/{ann}?format=text")).await);
    assert_eq!(joined["bio"], "Writes <things>");
    let mut bodies: Vec<&str> = joined["posts"]["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["body"].as_str().unwrap())
        .collect();
    bodies.sort_unstable();
    assert_eq!(bodies, ["First", "see"], "{joined}");
    // Select-shaped.
    let shaped = ok(get(
        &admin,
        "/api/posts?format=markdown&select[body]=true&sort=title",
    )
    .await);
    assert_eq!(shaped["docs"][0], json!({"id": first, "body": "First"}));
    // Globals.
    let site = ok(get(&admin, "/api/globals/site?format=markdown").await);
    assert_eq!(site["footer"], "Bye \\*now\\*");

    for url in [
        "/api/posts?format=xml".to_owned(),
        format!("/api/posts/{second}?format=xml"),
        "/api/globals/site?format=xml".into(),
        "/api/posts?format=html&format=text".into(),
    ] {
        let res = get(&admin, &url).await;
        assert_problem(&res, 400);
    }
    let res = get(&admin, "/api/posts?format=xml").await;
    assert_eq!(
        res.json["detail"],
        "format must be json, html, markdown or text"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn internal_links_use_the_collection_url(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let ed = json!({"read": {"role": "editor"}});
    for schema in [
        json!({"slug": "authors", "fields": [
            {"name": "name", "type": "text"}, {"name": "handle", "type": "slug"}
        ]}),
        json!({"slug": "posts", "url": "/blog/{slug}", "fields": [
            {"name": "title", "type": "text"}, {"name": "slug", "type": "slug"}
        ]}),
        json!({"slug": "secrets", "url": "/s/{slug}", "access": {"read": {"role": "editor"}}, "fields": [
            {"name": "title", "type": "text"}, {"name": "slug", "type": "slug"}
        ]}),
        // The URL's field is one only editors may read.
        json!({"slug": "guides", "url": "https://docs.example/{slug}", "fields": [
            {"name": "title", "type": "text"}, {"name": "slug", "type": "slug", "access": ed}
        ]}),
    ] {
        create(&admin, "/api/admin/collections", schema).await;
    }
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "pages", "fields": [
            {"name": "body", "type": "richText", "relationTo": ["posts", "authors", "secrets", "guides"]}
        ]}),
    )
    .await;
    let ann = id(&create(
        &admin,
        "/api/authors",
        json!({"name": "Ann", "handle": "ann"}),
    )
    .await);
    let post = id(&create(
        &admin,
        "/api/posts",
        json!({"title": "Hello", "slug": "hello"}),
    )
    .await);
    let secret = id(&create(
        &admin,
        "/api/secrets",
        json!({"title": "Classified", "slug": "plans"}),
    )
    .await);
    let guide = id(&create(
        &admin,
        "/api/guides",
        json!({"title": "Guide", "slug": "setup"}),
    )
    .await);
    let new_tab = json!({"type": "paragraph", "content": [{"type": "text", "text": "tab", "marks": [
        {"type": "link", "attrs": {"doc": {"relationTo": "posts", "value": post}, "newTab": true}}
    ]}]});
    let body = text(json!([
        doc_link("posts", &post),
        doc_link("authors", &ann),
        doc_link("secrets", &secret),
        doc_link("guides", &guide),
        new_tab,
        {"type": "relationship", "attrs": {"relationTo": "authors", "value": ann}},
        {"type": "relationship", "attrs": {"relationTo": "secrets", "value": secret}}
    ]));
    let page = id(&create(&admin, "/api/pages", json!({"body": body})).await);
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;

    // At any depth, even 0, a link to a document the caller can read, in a collection with
    // a URL, is a link to that URL. Anything else is its text alone.
    for depth in 0..=2 {
        let url = format!("/api/pages/{page}?format=html&depth={depth}");
        let html = get(&viewer, &url).await.json["body"].clone();
        assert_eq!(
            html,
            concat!(
                "<p><a href=\"/blog/hello\">see</a></p>",
                "<p>see</p>",
                "<p>see</p>",
                "<p>see</p>",
                "<p><a href=\"/blog/hello\" target=\"_blank\" rel=\"noopener noreferrer\">tab</a></p>",
                "<div data-relationship>Ann</div>"
            ),
            "depth {depth}"
        );
        let md = get(
            &viewer,
            &format!("/api/pages/{page}?format=markdown&depth={depth}"),
        )
        .await
        .json["body"]
            .clone();
        assert_eq!(
            md,
            concat!(
                "[see](doc:posts/hello)\n\n",
                "[see](doc:authors/ann)\n\n",
                "see\n\n",
                "see\n\n",
                "[tab](doc:posts/hello)\n\n",
                "::ref{collection=\"authors\" slug=\"ann\"}"
            ),
            "depth {depth}"
        );
    }
    // Someone who may read them all gets every link the collections have URLs for.
    let html = get(&admin, &format!("/api/pages/{page}?format=html&depth=0"))
        .await
        .json["body"]
        .clone();
    let html = html.as_str().unwrap();
    assert!(html.contains("<a href=\"/s/plans\">see</a>"), "{html}");
    assert!(
        html.contains("<a href=\"https://docs.example/setup\">see</a>"),
        "{html}"
    );
    assert!(
        html.contains("<div data-relationship>Classified</div>"),
        "{html}"
    );
    // The JSON at depth 0 is untouched: links stay IDs.
    let json = get(&viewer, &format!("/api/pages/{page}?depth=0"))
        .await
        .json;
    assert_eq!(
        json.pointer("/body/content/0/content/0/marks/0/attrs/doc/value"),
        Some(&json!(post))
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_single_user_read_takes_format(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "posts", "url": "/blog/{slug}",
        "fields": [{"name": "title", "type": "text"}, {"name": "slug", "type": "slug"}]}),
    )
    .await;
    let post = id(&create(
        &admin,
        "/api/posts",
        json!({"title": "Hello", "slug": "hello"}),
    )
    .await);
    let profile = json!({"schema": {"slug": "users", "fields": [
        {"name": "name", "type": "text", "required": true},
        {"name": "bio", "type": "richText", "relationTo": ["posts"]}
    ]}});
    let res = put(&admin, "/api/admin/collections/users", profile).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let user = create(
        &admin,
        "/api/users",
        json!({"email": "ed@example.com", "name": "Ed", "password": common::PASSWORD,
            "role": "editor", "bio": text(json!([doc_link("posts", &post)]))}),
    )
    .await;
    let url = format!("/api/users/{}", id(&user));
    let res = get(&admin, &format!("{url}?format=html")).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["bio"], "<p><a href=\"/blog/hello\">see</a></p>");
    assert_eq!(res.json["email"], "ed@example.com");
    assert_eq!(get(&admin, &url).await.json["bio"]["type"], "doc");
    for bad in ["format=xml", "depth=1", "select[name]=true"] {
        assert_problem(&get(&admin, &format!("{url}?{bad}")).await, 400);
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn doc_links_resolve_through_access(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let ed = json!({"read": {"role": "editor"}});
    for schema in [
        json!({"slug": "authors", "fields": [
            {"name": "name", "type": "text"}, {"name": "handle", "type": "slug"}
        ]}),
        // Hidden document by document: a viewer reads only published posts.
        json!({"slug": "posts", "access": {"read": {"stage": "published"}}, "fields": [
            {"name": "title", "type": "text"}, {"name": "slug", "type": "slug"},
            {"name": "stage", "type": "text"}
        ]}),
        // Hidden whole.
        json!({"slug": "secrets", "access": {"read": {"role": "editor"}}, "fields": [
            {"name": "title", "type": "text"}, {"name": "slug", "type": "slug"}
        ]}),
        // The slug itself is hidden, so finding a guide by it would be a probe.
        json!({"slug": "guides", "fields": [
            {"name": "title", "type": "text"}, {"name": "slug", "type": "slug", "access": ed}
        ]}),
        json!({"slug": "settings", "kind": "global", "fields": [
            {"name": "footer", "type": "richText", "relationTo": ["posts"]}
        ]}),
    ] {
        create(&admin, "/api/admin/collections", schema).await;
    }
    let editors = json!({"create": {"role": "editor"}, "update": {"role": "editor"}});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "pages",
            "access": {"create": {"role": ["editor", "viewer"]}, "update": {"role": ["editor", "viewer"]}},
            "fields": [
            {"name": "body", "type": "richText", "relationTo": ["posts", "authors", "secrets", "guides"], "blocks": [
                {"slug": "callout", "fields": [
                    {"name": "kind", "type": "text", "access": editors},
                    {"name": "body", "type": "richText", "relationTo": ["posts"]}
                ]}
            ]}
        ]}),
    )
    .await;
    let ann = id(&create(
        &admin,
        "/api/authors",
        json!({"name": "Ann", "handle": "ann"}),
    )
    .await);
    let open = id(&create(
        &admin,
        "/api/posts",
        json!({"title": "Open", "slug": "open", "stage": "published"}),
    )
    .await);
    let viewer = app_as(&pool, "viewer@example.com", "viewer").await;
    let md = |s: &str| json!({"body": {"format": "markdown", "value": s}});

    // Each slug becomes the ID of the document it names, at the top and inside a block.
    let page = create(
        &viewer,
        "/api/pages",
        md("See [it](doc:posts/open).\n\n::ref{collection=\"authors\" slug=\"ann\"}\n\n:::callout\n[again](doc:posts/open)\n:::"),
    )
    .await;
    let body = &page["body"]["content"];
    let link = &body[0]["content"][1]["marks"][0]["attrs"]["doc"];
    assert_eq!(
        (link["relationTo"].as_str(), linked(link)),
        (Some("posts"), open.as_str())
    );
    let rel = &body[1]["attrs"];
    assert_eq!(
        (rel["relationTo"].as_str(), linked(rel)),
        (Some("authors"), ann.as_str())
    );
    let inner = &body[2]["attrs"]["fields"]["body"]["content"][0]["content"][0];
    assert_eq!(linked(&inner["marks"][0]["attrs"]["doc"]), open);
    // A stored callout node would make replacing the text count its fields.
    let plain = create(&viewer, "/api/pages", md("plain")).await;
    let url = format!("/api/pages/{}", id(&plain));
    let res = patch(&viewer, &url, md("[x](doc:posts/open)")).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        linked(&res.json["body"]["content"][0]["content"][0]["marks"][0]["attrs"]["doc"]),
        open
    );
    let res = patch(
        &admin,
        "/api/globals/settings",
        json!({"footer": {"format": "markdown", "value": "[x](doc:posts/open)"}}),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        linked(&res.json["footer"]["content"][0]["content"][0]["marks"][0]["attrs"]["doc"]),
        open
    );

    // Field rules see a directive's block fields, as they see a JSON block node's.
    let sent = md(":::callout{kind=\"warning\"}\nx\n:::").to_string();
    assert_problem(&send(&viewer, "POST", "/api/pages", Some(&sent)).await, 403);
    assert_problem(
        &patch(&viewer, &url, md(":::callout{kind=\"warning\"}\nx\n:::")).await,
        403,
    );

    // A target the viewer can't read, by document, by collection or by its slug field, is
    // refused exactly as one that isn't there.
    for (collection, extra) in [
        ("posts", json!({"stage": "draft"})),
        ("secrets", json!({})),
        ("guides", json!({})),
    ] {
        let mut doc = json!({"title": "Shut", "slug": "shut"});
        doc.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let shut = id(&create(&admin, &format!("/api/{collection}"), doc).await);
        let src = format!("Intro\n\n[x](doc:{collection}/shut)");
        let hidden = send(&viewer, "POST", "/api/pages", Some(&md(&src).to_string())).await;
        assert_problem(&hidden, 422);
        assert_eq!(
            hidden.json["errors"],
            json!([{"path": "body.content.1.content.0.marks.0.attrs.doc",
                "message": format!("line 3: no `{collection}` document you can read has the slug `shut`")}]),
            "{collection}"
        );
        let res = send(&admin, "DELETE", &format!("/api/{collection}/{shut}"), None).await;
        assert_eq!(res.status, 204, "{}", res.json);
        let missing = send(&viewer, "POST", "/api/pages", Some(&md(&src).to_string())).await;
        assert_eq!(missing.status, hidden.status);
        for k in ["type", "title", "status", "detail", "errors"] {
            assert_eq!(missing.json[k], hidden.json[k], "{collection}: {k}");
        }
    }

    // Each write names at most 1,000 documents by slug, refused before any lookup.
    let many: String = (0..1001).map(|i| format!("[x](doc:posts/s{i}) ")).collect();
    let res = send(&viewer, "POST", "/api/pages", Some(&md(&many).to_string())).await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"],
        json!([{"path": "body", "message": "names more than 1000 documents by slug"}])
    );
    let fine: String = (0..1000).map(|i| format!("[x](doc:posts/s{i}) ")).collect();
    let res = send(&viewer, "POST", "/api/pages", Some(&md(&fine).to_string())).await;
    assert_eq!(
        res.json["errors"].as_array().map(Vec::len),
        Some(1000),
        "{}",
        res.json
    );

    // The users route converts too, before field rules.
    let profile = json!({"schema": {"slug": "users", "fields": [
        {"name": "name", "type": "text", "required": true},
        {"name": "bio", "type": "richText", "relationTo": ["posts"], "blocks": [
            {"slug": "callout", "fields": [{"name": "kind", "type": "text", "access": editors}]}
        ]}
    ]}});
    let res = put(&admin, "/api/admin/collections/users", profile).await;
    assert_eq!(res.status, 200, "{}", res.json);
    // Account errors and Markdown errors come back together.
    let res = send(
        &admin,
        "POST",
        "/api/users",
        Some(
            &json!({"email": "nope", "password": common::PASSWORD, "name": "X",
            "bio": {"format": "markdown", "value": "<b>x</b>"}})
            .to_string(),
        ),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"],
        json!([
            {"path": "email", "message": "expected an email address"},
            {"path": "bio", "message": "line 1: raw HTML isn't allowed; the only tags are <u>…</u> and <br>"}
        ])
    );
    let user = create(
        &admin,
        "/api/users",
        json!({"email": "ed@example.com", "password": common::PASSWORD, "role": "viewer", "name": "Ed",
            "bio": {"format": "markdown", "value": "[x](doc:posts/open)"}}),
    )
    .await;
    assert_eq!(
        linked(&user["bio"]["content"][0]["content"][0]["marks"][0]["attrs"]["doc"]),
        open
    );
    let ed = common::signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "ed@example.com",
    )
    .await;
    let res = patch(
        &ed,
        &format!("/api/users/{}", id(&user)),
        json!({"bio": {"format": "markdown", "value": "::callout{kind=\"x\"}"}}),
    )
    .await;
    assert_problem(&res, 403);
}

/// A link's target ID, whether the response populated it or not.
fn linked(link: &Value) -> &str {
    let v = &link["value"];
    v.get("id").unwrap_or(v).as_str().unwrap()
}
