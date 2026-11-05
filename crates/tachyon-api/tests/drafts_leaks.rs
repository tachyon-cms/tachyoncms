//! Drafts never leak: whatever a caller without the right to a draft reads, asks or filters
//! by, nothing of that draft comes back.

mod common;

use axum::Router;
use axum::http::Request;
use common::{Res, add_user, admin_app, send, signed_in};
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
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

/// A rich text value linking to each of `authors`.
fn links_to(authors: &[&str]) -> Value {
    let content: Vec<Value> = authors
        .iter()
        .map(|id| {
            json!({"type": "paragraph", "content": [{"type": "text", "text": "by", "marks": [
                {"type": "link", "attrs": {"doc": {"relationTo": "authors", "value": id}}}
            ]}]})
        })
        .collect();
    json!({"type": "doc", "content": content})
}

/// The app sending `key` as its bearer token on every request.
fn with_key(app: Router, key: &str) -> Router {
    let bearer = format!("Bearer {key}");
    app.layer(axum::middleware::map_request(
        move |mut req: Request<axum::body::Body>| {
            let bearer = bearer.clone();
            async move {
                let value = axum::http::HeaderValue::from_str(&bearer).unwrap();
                req.headers_mut().insert("authorization", value);
                req
            }
        },
    ))
}

/// What a list answers, without what differs between requests (a problem's request ID).
fn outcome(res: &Res) -> (u16, Value) {
    (res.status.as_u16(), res.json["docs"].clone())
}

/// Posts and authors with drafts. `ghost` is an author who was never published; `ann` is
/// published, with a secret in her draft; `pub` is a published post that links to both,
/// from its draft only, in a relationship, a virtual and rich text. One more post, linking
/// to the ghost and holding the secret in its draft, is in the trash, and `pub` has a
/// publish scheduled. `mine` is the other Author's own post, linking to Ann.
struct Site {
    admin: Router,
    ghost: String,
    ann: String,
    post: String,
    mine: String,
    other: Router,
}

const SECRET: &str = "SECRET";

async fn site(pool: &PgPool) -> Site {
    let admin = admin_app(pool.clone()).await;
    let authors = |fields: Value| {
        json!({"slug": "authors", "url": "/a/{slug}", "versions": {"drafts": true},
               "fields": fields})
    };
    let author_fields = json!([
        {"name": "name", "type": "text"},
        {"name": "slug", "type": "slug"}
    ]);
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(authors(author_fields.clone())),
    )
    .await;
    ok(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(
            json!({"slug": "posts", "versions": {"drafts": true}, "fields": [
                {"name": "title", "type": "text"},
                {"name": "author", "type": "relationship", "relationTo": "authors"},
                {"name": "coauthor", "type": "relationship", "relationTo": "authors"},
                {"name": "authorName", "type": "text", "virtual": "author.name"},
                {"name": "coauthorName", "type": "text", "virtual": "coauthor.name"},
                {"name": "body", "type": "richText", "relationTo": ["authors"]}
            ]}),
        ),
    )
    .await;
    let mut with_joins = author_fields.clone();
    for (name, on) in [("posts", "author"), ("coposts", "coauthor")] {
        with_joins
            .as_array_mut()
            .unwrap()
            .push(json!({"name": name, "type": "join", "collection": "posts", "on": on}));
    }
    ok(
        &admin,
        "PUT",
        "/api/admin/collections/authors",
        Some(json!({"schema": authors(with_joins)})),
    )
    .await;

    let author = |name: &str, slug: &str| json!({"name": name, "slug": slug});
    let ghost = id_of(
        &ok(
            &admin,
            "POST",
            "/api/authors",
            Some(author("Ghost", "ghost")),
        )
        .await,
    );
    let ann = id_of(&ok(&admin, "POST", "/api/authors", Some(author("Ann", "ann"))).await);
    let publish = |slug: &'static str, id: String| {
        let admin = admin.clone();
        async move { ok(&admin, "POST", &format!("/api/{slug}/{id}/publish"), None).await }
    };
    publish("authors", ann.clone()).await;
    let secret = author(&format!("{SECRET} name"), "secret-slug");
    ok(
        &admin,
        "PATCH",
        &format!("/api/authors/{ann}"),
        Some(secret),
    )
    .await;

    let post = id_of(&ok(&admin, "POST", "/api/posts", Some(json!({"title": "Pub"}))).await);
    publish("posts", post.clone()).await;
    let draft = json!({"title": format!("{SECRET} title"), "author": ghost, "coauthor": ann,
                       "body": links_to(&[&ghost, &ann])});
    ok(&admin, "PATCH", &format!("/api/posts/{post}"), Some(draft)).await;
    let when = (OffsetDateTime::now_utc() + time::Duration::days(1))
        .replace_nanosecond(0)
        .unwrap()
        .format(&Rfc3339)
        .unwrap();
    ok(
        &admin,
        "PUT",
        &format!("/api/posts/{post}/schedule"),
        Some(json!({"publishAt": when})),
    )
    .await;

    let gone =
        json!({"title": format!("{SECRET} gone"), "author": ghost, "body": links_to(&[&ghost])});
    let gone = id_of(&ok(&admin, "POST", "/api/posts", Some(gone)).await);
    ok(&admin, "DELETE", &format!("/api/posts/{gone}"), None).await;

    add_user(pool, "other@example.com", "author").await;
    let other = signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "other@example.com",
    )
    .await;
    let theirs = json!({"title": "Mine", "author": ann, "body": links_to(&[&ann])});
    let mine = id_of(&ok(&other, "POST", "/api/posts", Some(theirs)).await);
    Site {
        admin,
        ghost,
        ann,
        post,
        mine,
        other,
    }
}

/// `where` probes on posts that could only match through a draft: each must answer
/// exactly as the same probe for a value nothing has.
fn probes(ghost: &str) -> Vec<(String, String)> {
    let nothing = "Nobody";
    let missing = Uuid::now_v7().to_string();
    [
        ("author.name", "Ghost", nothing),
        ("author.id", ghost, missing.as_str()),
        ("author", ghost, missing.as_str()),
        ("authorName", "Ghost", nothing),
        ("coauthor.name", "SECRET%20name", nothing),
        ("coauthor.slug", "secret-slug", nothing),
        ("coauthorName", "SECRET%20name", nothing),
        ("title", "SECRET%20title", nothing),
    ]
    .into_iter()
    .flat_map(|(path, hit, miss)| {
        ["", "&draft=true"].map(|draft| {
            let probe = |v: &str| format!("/api/posts?where[{path}][equals]={v}{draft}");
            (probe(hit), probe(miss))
        })
    })
    .collect()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn drafts_never_leak(pool: PgPool) {
    let s = site(&pool).await;
    let (ghost, ann, post, mine) = (&s.ghost, &s.ann, &s.post, &s.mine);
    let read_key = ok(
        &s.admin,
        "POST",
        "/api/auth/keys",
        Some(json!({"name": "site", "scopes": ["posts:read", "authors:read"]})),
    )
    .await["key"]
        .as_str()
        .unwrap()
        .to_owned();
    add_user(&pool, "viewer@example.com", "viewer").await;
    let fresh = || async { tachyon_api::router(pool.clone()).await.unwrap() };
    let callers = [
        ("anonymous", fresh().await),
        (
            "viewer",
            signed_in(fresh().await, "viewer@example.com").await,
        ),
        ("read key", with_key(fresh().await, &read_key)),
        ("other author", s.other.clone()),
    ];
    let leaks = |s: &str| {
        [SECRET, "secret-slug", "Ghost", "/a/ghost", ghost.as_str()]
            .into_iter()
            .find(|w| s.contains(w))
    };

    let vid = ok(
        &s.admin,
        "GET",
        &format!("/api/posts/{post}/versions"),
        None,
    )
    .await["docs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let ann_vid = ok(
        &s.admin,
        "GET",
        &format!("/api/authors/{ann}/versions"),
        None,
    )
    .await["docs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut reads = Vec::new();
    for draft in ["", "&draft=true"] {
        for format in ["json", "html", "markdown", "text"] {
            for depth in [0, 2] {
                let q = format!("?depth={depth}&format={format}{draft}");
                for doc in [
                    "/api/posts".to_owned(),
                    format!("/api/posts/{post}"),
                    format!("/api/posts/{mine}"),
                    "/api/authors".to_owned(),
                    format!("/api/authors/{ann}"),
                    format!("/api/authors/{ghost}"),
                ] {
                    reads.push(format!("{doc}{q}"));
                }
            }
        }
        // Joins, shaped and limited.
        reads.push(format!(
            "/api/authors/{ann}?joins[coposts][limit]=5&depth=1{draft}"
        ));
    }
    reads.extend([
        format!("/api/authors/{ghost}/references"),
        format!("/api/authors/{ann}/references"),
        format!("/api/posts/{post}/references"),
        format!("/api/posts/{post}/versions"),
        format!("/api/posts/{post}/versions/{vid}"),
        format!("/api/authors/{ann}/versions"),
        format!("/api/authors/{ann}/versions/{ann_vid}"),
        format!("/api/authors/{ghost}/versions"),
        format!("/api/posts/{post}/diff?from=published&to=draft"),
        format!("/api/posts/{post}/diff?from={vid}&to=published"),
        format!("/api/authors/{ann}/diff?from=published&to=draft"),
        format!("/api/posts/{post}/schedule"),
        "/api/posts/trash".to_owned(),
        "/api/authors/trash".to_owned(),
    ]);

    // Each read that can show a draft shows the Admin what it would hide, so the checks
    // below bite. Version lists and schedules hold no content, and the published copies
    // link to nothing.
    for url in &reads {
        let res = call(&s.admin, "GET", url, None).await;
        let shows = ["draft=true", "/versions/", "/diff", "/posts/trash"]
            .iter()
            .any(|w| url.contains(w));
        assert_eq!(
            leaks(&res.json.to_string()).is_some(),
            shows,
            "{url}: {}",
            res.json
        );
    }

    for (who, app) in &callers {
        for url in &reads {
            let res = call(app, "GET", url, None).await;
            let body = res.json.to_string();
            assert_eq!(leaks(&body), None, "{who}: {url}: {} {body}", res.status);
        }
        // A link from a draft they can't read is no reference of theirs: Ann's only other
        // one is from the other Author's own draft.
        let refs = call(app, "GET", &format!("/api/authors/{ann}/references"), None).await;
        let docs = refs.json["docs"].as_array().unwrap();
        assert!(
            docs.iter().all(|r| r["id"] == mine.as_str()),
            "{who}: {}",
            refs.json
        );
        // Nor is it a joined document.
        let joined = call(app, "GET", &format!("/api/authors/{ann}"), None).await;
        assert_eq!(joined.status, 200, "{who}: {}", joined.json);
        assert_eq!(
            joined.json["coposts"]["docs"],
            json!([]),
            "{who}: {}",
            joined.json
        );
        assert_eq!(joined.json["name"], "Ann", "{who}");
        for (hit, miss) in probes(ghost) {
            let (a, b) = (
                call(app, "GET", &hit, None).await,
                call(app, "GET", &miss, None).await,
            );
            assert_eq!(outcome(&a), outcome(&b), "{who}: {hit}: {}", a.json);
        }
    }
    // The Admin's probes do hit, so equal answers above mean something.
    for (hit, miss) in probes(ghost) {
        if !hit.contains("draft=true") {
            continue;
        }
        let (a, b) = (
            call(&s.admin, "GET", &hit, None).await,
            call(&s.admin, "GET", &miss, None).await,
        );
        assert_ne!(outcome(&a), outcome(&b), "{hit}");
    }

    // The other Author reads their own draft, and its link to Ann as what's published.
    let theirs = ok(
        &s.other,
        "GET",
        &format!("/api/posts/{mine}?draft=true&depth=1&format=html"),
        None,
    )
    .await;
    assert_eq!(theirs["author"]["name"], "Ann", "{theirs}");
    assert_eq!(theirs["authorName"], "Ann");
    assert!(
        theirs["body"].as_str().unwrap().contains("/a/ann"),
        "{theirs}"
    );
}
