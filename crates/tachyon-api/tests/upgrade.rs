mod common;

use argon2::password_hash::PasswordHasher;
use argon2::{Algorithm, Argon2, Params, Version};
use axum::Router;
use common::{ADMIN, PASSWORD, Res, send, signed_in};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_db::{DEFAULT_TENANT, MIGRATOR};
use uuid::Uuid;

/// The last migration v0.2.0 shipped; everything after it runs on a filled database.
const V0_2_0: i64 = 20261020000000;

/// Every table v0.2.0 holds content in.
const TABLES: [&str; 6] = [
    "tenants",
    "roles",
    "collections",
    "documents",
    "relations",
    "accounts",
];

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Res {
    send(app, method, uri, body.map(|b| b.to_string()).as_deref()).await
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let res = call(app, method, uri, body).await;
    assert!(res.status.is_success(), "{method} {uri}: {}", res.json);
    res.json
}

/// Every row of every table, minus columns the upgrade adds, in a stable order.
async fn snapshot(pool: &PgPool) -> Value {
    let mut all = serde_json::Map::new();
    for table in TABLES {
        let rows: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT coalesce(jsonb_agg(to_jsonb(t) - 'copy' - 'published_updated_at' ORDER BY to_jsonb(t)::text), '[]')
             FROM {table} t"
        )))
        .fetch_one(pool)
        .await
        .unwrap();
        all.insert(table.to_owned(), rows);
    }
    Value::Object(all)
}

async fn insert_collection(pool: &PgPool, schema: Value) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO collections (id, tenant_id, slug, kind, schema) VALUES ($1, $2, $3, 'collection', $4)")
        .bind(id)
        .bind(DEFAULT_TENANT)
        .bind(schema["slug"].as_str().unwrap())
        .bind(&schema)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// A published document, as v0.2.0 wrote one: no draft copy, ever.
async fn insert_doc(pool: &PgPool, collection: Uuid, data: Value, trashed: bool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO documents (id, tenant_id, collection_id, status, published, published_at, deleted_at)
         VALUES ($1, $2, $3, 'published', $4, now(), CASE WHEN $5 THEN now() END)",
    )
    .bind(id)
    .bind(DEFAULT_TENANT)
    .bind(collection)
    .bind(data)
    .bind(trashed)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn link(pool: &PgPool, from: Uuid, path: &str, to: Uuid) {
    sqlx::query(
        "INSERT INTO relations (tenant_id, from_doc, field_path, to_doc) VALUES ($1, $2, $3, $4)",
    )
    .bind(DEFAULT_TENANT)
    .bind(from)
    .bind(path)
    .bind(to)
    .execute(pool)
    .await
    .unwrap();
}

struct Seed {
    authors: Uuid,
    ann: Uuid,
    hello: Uuid,
    gone: Uuid,
}

/// What a v0.2.0 site holds: an Admin, authors, and posts linking to them, one in trash.
async fn seed(pool: &PgPool) -> Seed {
    let users = insert_collection(
        pool,
        json!({"slug": "users", "fields": [{"name": "name", "type": "text", "required": true}]}),
    )
    .await;
    let ada = insert_doc(pool, users, json!({"name": "Ada"}), false).await;
    let params = Params::new(19_456, 2, 1, None).unwrap();
    let hash = Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(PASSWORD.as_bytes())
        .unwrap()
        .to_string();
    sqlx::query("INSERT INTO accounts (user_id, tenant_id, email, password_hash, role) VALUES ($1, $2, $3, $4, 'admin')")
        .bind(ada)
        .bind(DEFAULT_TENANT)
        .bind(ADMIN)
        .bind(hash)
        .execute(pool)
        .await
        .unwrap();

    let authors = insert_collection(
        pool,
        json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}),
    )
    .await;
    let posts = insert_collection(
        pool,
        json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "authors"}
        ]}),
    )
    .await;
    let ann = insert_doc(pool, authors, json!({"name": "Ann"}), false).await;
    let post = |title: &str| json!({"title": title, "author": ann.to_string()});
    let hello = insert_doc(pool, posts, post("Hello"), false).await;
    let gone = insert_doc(pool, posts, post("Gone"), true).await;
    // Trash kept its links in v0.2.0.
    link(pool, hello, "author", ann).await;
    link(pool, gone, "author", ann).await;
    Seed {
        authors,
        ann,
        hello,
        gone,
    }
}

/// The code of the error an insert fails with.
async fn refused(pool: &PgPool, sql: &'static str, s: &Seed) -> String {
    let err = sqlx::query(sql)
        .bind(Uuid::now_v7())
        .bind(DEFAULT_TENANT)
        .bind(s.authors)
        .bind(s.hello)
        .execute(pool)
        .await
        .unwrap_err();
    err.as_database_error()
        .unwrap()
        .code()
        .unwrap()
        .into_owned()
}

#[sqlx::test(migrations = false)]
async fn drafts_migration_upgrades_a_filled_database(pool: PgPool) {
    MIGRATOR.run_to(V0_2_0, &pool).await.unwrap();
    let s = seed(&pool).await;
    let before = snapshot(&pool).await;
    MIGRATOR.run(&pool).await.unwrap();

    assert_eq!(snapshot(&pool).await, before);
    let moved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM documents WHERE published_updated_at IS DISTINCT FROM updated_at",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        moved, 0,
        "the published copy last changed when the document did"
    );
    let copies: Vec<(String, i64)> =
        sqlx::query_as("SELECT copy, count(*) FROM relations GROUP BY copy")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(copies, vec![("published".to_owned(), 2)]);
    let drafts: i64 = sqlx::query_scalar("SELECT count(*) FROM documents WHERE draft IS NOT NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(drafts, 0);

    // A version or job naming the post under another collection is refused (23503 is a
    // foreign key violation).
    for sql in [
        "INSERT INTO versions (id, tenant_id, collection_id, document_id, kind, data)
         VALUES ($1, $2, $3, $4, 'published', '{}')",
        "INSERT INTO jobs (id, tenant_id, collection_id, document_id, kind, run_at, state)
         VALUES ($1, $2, $3, $4, 'publish', now(), 'queued')",
    ] {
        assert_eq!(refused(&pool, sql, &s).await, "23503", "{sql}");
    }

    // The v0.2.0 Admin still signs in and reads what was there.
    let app = tachyon_api::router(pool.clone()).await.unwrap();
    let admin = signed_in(app, ADMIN).await;
    let one = format!("/api/posts/{}", s.hello);
    let doc = ok(&admin, "GET", &one, None).await;
    assert_eq!(doc["title"], "Hello");
    assert_eq!(doc["author"]["id"], s.ann.to_string());
    let trash = ok(&admin, "GET", "/api/posts/trash", None).await;
    assert_eq!(trash["docs"][0]["id"], s.gone.to_string(), "{trash}");

    ok(
        &admin,
        "PUT",
        "/api/admin/collections/posts",
        Some(
            json!({"schema": {"slug": "posts", "versions": {"drafts": true}, "fields": [
                {"name": "title", "type": "text"},
                {"name": "author", "type": "relationship", "relationTo": "authors"}
            ]}}),
        ),
    )
    .await;
    assert_eq!(ok(&admin, "GET", &one, None).await["title"], "Hello");
    let list = ok(&admin, "GET", "/api/posts", None).await;
    assert_eq!(list["docs"].as_array().unwrap().len(), 1, "{list}");

    // A draft saves beside the live copy, then publishing makes it the live one.
    ok(&admin, "PATCH", &one, Some(json!({"title": "Hello again"}))).await;
    assert_eq!(ok(&admin, "GET", &one, None).await["title"], "Hello");
    let draft = ok(&admin, "GET", &format!("{one}?draft=true"), None).await;
    assert_eq!(draft["title"], "Hello again");
    ok(&admin, "POST", &format!("{one}/publish"), None).await;
    let live = ok(&admin, "GET", &one, None).await;
    assert_eq!(live["title"], "Hello again");
    assert_eq!(live["author"]["id"], s.ann.to_string());
    let versions = ok(&admin, "GET", &format!("{one}/versions"), None).await;
    assert!(
        !versions["docs"].as_array().unwrap().is_empty(),
        "{versions}"
    );
}
