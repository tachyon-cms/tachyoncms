//! Rich text through the API: block node field rules, links, references and shared blocks.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use common::{
    ADMIN, PASSWORD, Res, add_user, admin_app, app_as, assert_problem, png, send, signed_in, upload,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_api::{Options, UploadsConfig};
use tachyon_db::documents::MISSING_LINK;
use tachyon_media::storage::{StorageConfig, Storages};

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

/// U+0000 is a 422 at the node holding it, sent as JSON or as Markdown, never a database
/// error; so is a list numbered past the cap (W1-R41).
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn nul_and_huge_list_numbers_are_422_at_their_node(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "pages", "fields": [
            {"name": "title", "type": "text"},
            {"name": "slug", "type": "slug"},
            {"name": "body", "type": "richText", "relationTo": ["pages"], "blocks": [
                {"slug": "callout", "fields": [{"name": "kind", "type": "text"}]},
                {"slug": "embed", "fields": [{"name": "meta", "type": "json"}]}
            ]},
            {"name": "layout", "type": "blocks", "blocks": [
                {"slug": "copy", "fields": [{"name": "copy", "type": "richText", "relationTo": ["pages"]}]}
            ]}
        ]}),
    )
    .await;
    let md = |s: &str| json!({"body": {"format": "markdown", "value": s}});
    for (body, path) in [
        (
            json!({"body": text(json!([{"type": "paragraph", "content": [
                {"type": "text", "text": "a\u{0}b"}
            ]}]))}),
            "body.content.0.content.0.text",
        ),
        (
            json!({"body": text(json!([node("callout", json!({"kind": "\u{0}"}))]))}),
            "body.content.0.attrs.fields.kind",
        ),
        (md("a\u{0}b"), "body.content.0.content.0.text"),
        (
            md("::callout{kind=\"\u{0}\"}"),
            "body.content.0.attrs.fields.kind",
        ),
        (md("1000001. a"), "body.content.0.attrs.start"),
        // A slug is refused before it's looked up, never sent to the database.
        (
            md("::ref{collection=\"pages\" slug=\"a\u{0}\"}"),
            "body.content.0.attrs.slug",
        ),
        (
            md("[a](<doc:pages/a\u{0}>)"),
            "body.content.0.content.0.marks.0.attrs.doc.slug",
        ),
        (
            json!({"layout": [{"blockType": "copy", "copy": {"format": "markdown",
                "value": "::ref{collection=\"pages\" slug=\"\u{0}\"}"}}]}),
            "layout.0.copy.content.0.attrs.slug",
        ),
    ] {
        let res = send(&admin, "POST", "/api/pages", Some(&body.to_string())).await;
        assert_problem(&res, 422);
        assert_eq!(
            res.json["errors"]
                .as_array()
                .map(|e| (e.len(), e[0]["path"].clone())),
            Some((1, json!(path))),
            "{body}: {}",
            res.json
        );
    }

    // A json value shaped like rich text is data: its slug is never looked up, so a
    // missing one isn't a 422, and it's stored exactly as sent (W1-R44).
    let meta = r#"{"type":"doc","content":[{"type":"relationship","attrs":{"relationTo":"pages","slug":"missing","line":3}}]}"#;
    let src = format!("::embed{{meta=\"{}\"}}", meta.replace('"', "\\\""));
    let page = create(&admin, "/api/pages", md(&src)).await;
    assert_eq!(
        page["body"]["content"][0]["attrs"]["fields"]["meta"],
        serde_json::from_str::<Value>(meta).unwrap()
    );

    // A missing slug is named clipped, however long it is.
    let long = "s".repeat(1_000_000);
    let res = send(
        &admin,
        "POST",
        "/api/pages",
        Some(&md(&format!("[a](doc:pages/{long})")).to_string()),
    )
    .await;
    assert_problem(&res, 422);
    let message = res.json["errors"][0]["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("`{}…`", &long[..100])),
        "{message}"
    );
    assert!(message.len() < 300, "{}", message.len());
    assert!(
        res.json.to_string().len() < 2_000,
        "{}",
        res.json.to_string().len()
    );
}

/// A test's spool and store for files, removed with everything in them when it's done.
fn file_dirs() -> tempfile::TempDir {
    let base = tempfile::Builder::new()
        .prefix("tachyon-rich-text-")
        .tempdir()
        .unwrap();
    for sub in ["spool", "store"] {
        std::fs::create_dir_all(base.path().join(sub)).unwrap();
    }
    base
}

/// The API with files kept in `d`, signed in as `email` (the first Admin when `None`).
async fn with_files(pool: &PgPool, d: &tempfile::TempDir, user: Option<(&str, &str)>) -> Router {
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
    let app = tachyon_api::router_with(pool.clone(), options)
        .await
        .unwrap();
    match user {
        None => {
            let body = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
            let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
            assert_eq!(res.status, 201, "setup: {}", res.json);
            signed_in(app, ADMIN).await
        }
        Some((email, role)) => {
            add_user(pool, email, role).await;
            signed_in(app, email).await
        }
    }
}

fn file_node(to: &str, id: &str, fields: Value) -> Value {
    json!({"type": "upload", "attrs": {"relationTo": to, "value": id, "fields": fields}})
}

/// Upload nodes hold files from `media`, which anyone signed in reads, and from `private`,
/// which only editors read: links like any other in the text, rendered as the file.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn upload_nodes_hold_files(pool: PgPool) {
    let d = file_dirs();
    let admin = with_files(&pool, &d, None).await;
    for schema in [
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
        json!({"slug": "media", "upload": true, "fields": [{"name": "alt", "type": "text"}]}),
        json!({"slug": "private", "upload": true, "access": {"read": {"role": "editor"}},
            "fields": []}),
    ] {
        create(&admin, "/api/admin/collections", schema).await;
    }
    // Upload nodes hold only files.
    let pages = |collections: Value| {
        json!({"slug": "pages",
            "access": {"create": {"role": ["editor", "viewer"]}, "update": {"role": ["editor", "viewer"]}},
            "fields": [{"name": "body", "type": "richText",
                "relationTo": ["media", "private", "authors"],
                "upload": {"collections": collections}}]})
    };
    let res = send(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(&pages(json!({"authors": {}})).to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"],
        json!([{"path": "fields.body.upload.collections",
            "message": "`authors` doesn't hold files; upload nodes link only to collections with upload on"}])
    );
    let media_node = json!({"fields": [
        {"name": "caption", "type": "text"},
        {"name": "credit", "type": "relationship", "relationTo": "authors"}
    ]});
    create(
        &admin,
        "/api/admin/collections",
        pages(json!({"media": media_node, "private": {}})),
    )
    .await;
    let alt = json!({"alt": "Cover <b>"});
    let res = upload(
        &admin,
        "POST",
        "/api/media",
        Some(&png(8, 6)),
        "a.png",
        Some(&alt),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let cover = id(&res.json);
    let res = upload(
        &admin,
        "POST",
        "/api/private",
        Some(&png(4, 4)),
        "secret.png",
        None,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let hidden = id(&res.json);
    let ann = id(&create(&admin, "/api/authors", json!({"name": "Ann"})).await);

    let body = text(json!([
        file_node("media", &cover, json!({"caption": "C", "credit": ann})),
        {"type": "paragraph", "content": [{"type": "text", "text": "between"}]},
        file_node("private", &hidden, json!({}))
    ]));
    let page = id(&create(&admin, "/api/pages", json!({"body": body})).await);
    let url = format!("/api/pages/{page}");
    let viewer = with_files(&pool, &d, Some(("viewer@example.com", "viewer"))).await;
    let editor = with_files(&pool, &d, Some(("editor@example.com", "editor"))).await;

    // Rendered as the file the caller reads; one they can't is left out (W1-R24).
    let html = |res: Res| {
        assert_eq!(res.status, 200, "{}", res.json);
        res.json["body"].as_str().unwrap().to_owned()
    };
    let shown = html(get(&viewer, &format!("{url}?format=html")).await);
    // The node's own fields are `data-*`, as a block node's are.
    assert_eq!(
        shown,
        format!(
            concat!(
                "<img src=\"http://localhost:3000/api/media/file/a.png\" width=\"8\" height=\"6\" ",
                "alt=\"Cover &lt;b&gt;\" data-caption=\"C\" data-credit=\"{}\"><p>between</p>"
            ),
            ann
        )
    );
    let all = html(get(&editor, &format!("{url}?format=html")).await);
    assert!(
        all.ends_with(concat!(
            "<img src=\"http://localhost:3000/api/private/file/secret.png\" ",
            "width=\"4\" height=\"4\" alt=\"\">"
        )),
        "{all}"
    );
    // A node with fields is a directive, named by its file.
    assert_eq!(
        html(get(&viewer, &format!("{url}?format=markdown")).await),
        format!("::media:media/{cover}{{caption=\"C\" credit=\"{ann}\"}}\n\nbetween")
    );
    assert_eq!(
        html(get(&viewer, &format!("{url}?format=text")).await),
        "Cover <b>\n\nbetween"
    );
    // Populated per depth; a file the caller can't read stays an ID.
    let deep = get(&viewer, &format!("{url}?depth=1")).await.json;
    let nodes = &deep["body"]["content"];
    assert_eq!(nodes[0]["attrs"]["value"]["alt"], "Cover <b>");
    assert_eq!(nodes[0]["attrs"]["fields"]["credit"]["name"], "Ann");
    assert_eq!(nodes[2]["attrs"]["value"], hidden.as_str());

    // References and delete protection see the node and the links in its fields.
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
    assert_eq!(
        refs(get(&admin, &format!("/api/media/{cover}/references")).await),
        ["body.0"]
    );
    assert_eq!(
        refs(get(&admin, &format!("/api/authors/{ann}/references")).await),
        ["body.0.credit"]
    );
    let res = send(&admin, "DELETE", &format!("/api/media/{cover}"), None).await;
    assert_problem(&res, 409);

    // A file the caller can't read is refused as one that isn't there (W1-R14), sent as
    // JSON or as Markdown.
    let refused = |res: Res| {
        assert_problem(&res, 422);
        res.json["errors"].clone()
    };
    let gone = "0199d5c4-0000-7000-8000-00000000dead";
    let sent = |id: &str| json!({"body": text(json!([file_node("private", id, json!({}))]))});
    let unreadable = refused(
        send(
            &viewer,
            "POST",
            "/api/pages",
            Some(&sent(&hidden).to_string()),
        )
        .await,
    );
    assert_eq!(
        unreadable,
        json!([{"path": "body.content.0.attrs.value", "message": MISSING_LINK}])
    );
    assert_eq!(
        refused(send(&viewer, "POST", "/api/pages", Some(&sent(gone).to_string())).await),
        unreadable
    );
    let md = |src: String| json!({"body": {"format": "markdown", "value": src}});
    let unreadable = refused(
        send(
            &viewer,
            "POST",
            "/api/pages",
            Some(&md(format!("![x](media:private/{hidden})")).to_string()),
        )
        .await,
    );
    assert_eq!(
        unreadable,
        json!([{"path": "body.content.0.attrs.value", "message": MISSING_LINK}])
    );
    for src in [
        format!("![x](media:private/{gone})"),
        format!("::media:private/{hidden}"),
        format!("::media:private/{gone}"),
    ] {
        let res = send(&viewer, "POST", "/api/pages", Some(&md(src).to_string())).await;
        assert_eq!(refused(res), unreadable);
    }
    // Markdown names a file by ID: the collection too, where the field holds more than one.
    let made = create(
        &viewer,
        "/api/pages",
        md(format!("![Ignored](media:media/{cover})")),
    )
    .await;
    let node = &made["body"]["content"][0];
    assert_eq!(
        (&node["type"], &node["attrs"]["relationTo"]),
        (&json!("upload"), &json!("media"))
    );
    assert_eq!(node["attrs"]["value"]["id"], cover.as_str());
    let res = send(
        &viewer,
        "POST",
        "/api/pages",
        Some(&md(format!("![x](media:{cover})")).to_string()),
    )
    .await;
    assert_problem(&res, 422);
    let res = send(
        &viewer,
        "POST",
        "/api/pages",
        Some(&md("![x](./a.png)".into()).to_string()),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["message"],
        "line 1: images must be media:<id>; local paths arrive with content push"
    );

    // A node's fields come back through Markdown read and written back.
    let captioned = text(json!([file_node(
        "media",
        &cover,
        json!({"caption": "Taken \"here\"", "credit": ann})
    )]));
    let other = id(&create(&admin, "/api/pages", json!({"body": captioned})).await);
    let other = format!("/api/pages/{other}");
    let exported = html(get(&viewer, &format!("{other}?format=markdown")).await);
    let res = patch(&viewer, &other, md(exported)).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let back = get(&viewer, &format!("{other}?depth=0")).await.json;
    assert_eq!(back["body"], captioned);

    // The hidden node stays where it is through a read and a write of what was read.
    let read = get(&viewer, &format!("{url}?depth=0")).await.json;
    assert_eq!(
        read["body"]["content"][2]["attrs"]["value"],
        hidden.as_str()
    );
    let res = patch(&viewer, &url, json!({"body": read["body"]})).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        get(&editor, &format!("{url}?depth=0")).await.json["body"]["content"][2]["attrs"]["value"],
        hidden.as_str()
    );
}

/// An upload node's fields answer to field rules as a block node's do: read, written and
/// exported as Markdown only by who may read them.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn upload_node_fields_answer_to_field_rules(pool: PgPool) {
    let d = file_dirs();
    let admin = with_files(&pool, &d, None).await;
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "media", "upload": true, "fields": []}),
    )
    .await;
    let ed = json!({"read": {"role": "editor"}, "create": {"role": "editor"}, "update": {"role": "editor"}});
    create(
        &admin,
        "/api/admin/collections",
        json!({"slug": "cards",
        "access": {"update": {"role": ["editor", "viewer"]}},
        "fields": [{"name": "body", "type": "richText", "relationTo": ["media"],
            "upload": {"collections": {"media": {"fields": [
                {"name": "caption", "type": "text"},
                {"name": "internal", "type": "text", "access": ed}
            ]}}}}]}),
    )
    .await;
    let res = upload(
        &admin,
        "POST",
        "/api/media",
        Some(&png(4, 4)),
        "a.png",
        None,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    let file = id(&res.json);
    let body = text(json!([file_node(
        "media",
        &file,
        json!({"caption": "C", "internal": "I"})
    )]));
    let card = id(&create(&admin, "/api/cards", json!({ "body": body })).await);
    let url = format!("/api/cards/{card}");
    let viewer = with_files(&pool, &d, Some(("viewer@example.com", "viewer"))).await;
    let editor = with_files(&pool, &d, Some(("editor@example.com", "editor"))).await;

    let read = get(&viewer, &format!("{url}?depth=0")).await.json;
    assert_eq!(
        read["body"]["content"][0]["attrs"]["fields"],
        json!({"caption": "C"})
    );
    let as_markdown = format!("{url}?format=markdown");
    assert_eq!(
        get(&viewer, &as_markdown).await.json["body"],
        format!("::media:media/{file}{{caption=\"C\"}}")
    );
    assert_eq!(
        get(&editor, &as_markdown).await.json["body"],
        format!("::media:media/{file}{{caption=\"C\" internal=\"I\"}}")
    );
    // The text is written whole, so only who may write every field in it may write it.
    let res = patch(&viewer, &url, json!({"body": read["body"]})).await;
    assert_problem(&res, 403);
    let res = patch(&editor, &url, json!({ "body": body })).await;
    assert_eq!(res.status, 200, "{}", res.json);
}

/// A draft's upload node shows a draft file its reader may read, by the file route; what's
/// live holds only files that are live (T4-1). Unreadable is missing (W1-R14).
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn upload_nodes_follow_draft_links(pool: PgPool) {
    let d = file_dirs();
    let admin = with_files(&pool, &d, None).await;
    let body = json!({"name": "body", "type": "richText", "relationTo": ["shots"],
        "upload": {"collections": {"shots": {}}}});
    for schema in [
        // Only authors (and admins) read draft shots.
        json!({"slug": "shots", "upload": true, "versions": {"drafts": true},
            "access": {"readDrafts": {"role": "author"}},
            "fields": [{"name": "alt", "type": "text"}]}),
        json!({"slug": "pages", "versions": {"drafts": true}, "fields": [body]}),
        json!({"slug": "notes", "access": {"create": {"role": ["author", "viewer"]}},
            "fields": [body]}),
    ] {
        create(&admin, "/api/admin/collections", schema).await;
    }
    let author = with_files(&pool, &d, Some(("author@example.com", "author"))).await;
    let editor = with_files(&pool, &d, Some(("editor@example.com", "editor"))).await;
    let viewer = with_files(&pool, &d, Some(("viewer@example.com", "viewer"))).await;
    let shot = |app: Router, name: &'static str| async move {
        let alt = json!({ "alt": name });
        let res = upload(
            &app,
            "POST",
            "/api/shots",
            Some(&png(4, 4)),
            name,
            Some(&alt),
        )
        .await;
        assert_eq!(res.status, 201, "{}", res.json);
        id(&res.json)
    };
    // Never published: the author's own draft.
    let own = shot(author.clone(), "own.png").await;
    let live = shot(admin.clone(), "live.png").await;
    let res = send(&admin, "POST", &format!("/api/shots/{live}/publish"), None).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let nodes = |ids: &[&str]| {
        let nodes: Vec<Value> = ids
            .iter()
            .map(|i| file_node("shots", i, json!({})))
            .collect();
        json!({ "body": text(json!(nodes)) })
    };

    // A draft links a draft its writer reads, shown by the file route, never a bucket URL.
    let page = id(&create(&author, "/api/pages", nodes(&[&own, &live])).await);
    let html = |res: Res| {
        assert_eq!(res.status, 200, "{}", res.json);
        res.json["body"].as_str().unwrap().to_owned()
    };
    let draft = format!("/api/pages/{page}?draft=true");
    // Read as a draft, each file's link asks for the draft's; the route gives the published
    // file to who can't read it.
    let img = |name: &str| {
        format!(
            "<img src=\"http://localhost:3000/api/shots/file/{name}?draft=true\" width=\"4\" height=\"4\" alt=\"{name}\">"
        )
    };
    assert_eq!(
        html(get(&author, &format!("{draft}&format=html")).await),
        format!("{}{}", img("own.png"), img("live.png"))
    );
    // A reader who may read the page's draft but not the file's sees no trace of it.
    assert_eq!(
        html(get(&editor, &format!("{draft}&format=html")).await),
        img("live.png")
    );
    assert_eq!(
        html(get(&editor, &format!("{draft}&format=markdown")).await),
        format!("![live.png](media:shots/{live})")
    );

    // What's live links only to what's live: a draft file is as missing as a random one.
    let missing = |res: Res| {
        assert_problem(&res, 422);
        res.json["errors"].clone()
    };
    let gone = "0199d5c4-0000-7000-8000-00000000dead";
    let want = json!([{"path": "body.content.0.attrs.value", "message": MISSING_LINK}]);
    let res = send(&admin, "POST", &format!("/api/pages/{page}/publish"), None).await;
    assert_eq!(missing(res), want);
    let md = |src: String| json!({"body": {"format": "markdown", "value": src}});
    for app in [&author, &viewer] {
        for sent in [
            nodes(&[&own]),
            nodes(&[gone]),
            md(format!("![x](media:{own})")),
            md(format!("![x](media:{gone})")),
        ] {
            let res = send(app, "POST", "/api/notes", Some(&sent.to_string())).await;
            assert_eq!(missing(res), want);
        }
    }
    create(&viewer, "/api/notes", nodes(&[&live])).await;
}
