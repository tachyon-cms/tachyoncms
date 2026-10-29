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
    assert_eq!(res.json["errors"][0]["path"], "body.1");
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
