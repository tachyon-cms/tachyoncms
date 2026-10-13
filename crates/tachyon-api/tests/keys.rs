mod common;

use axum::Router;
use axum::http::Request;
use common::{Res, admin_app, app_as, assert_problem, send, send_with};
use serde_json::{Value, json};
use sqlx::PgPool;

async fn with_key(app: &Router, key: &str, method: &str, uri: &str, body: Option<Value>) -> Res {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"));
    send_with(app, req, body.map(|b| b.to_string()).as_deref()).await
}

async fn site(pool: &PgPool) -> Router {
    let admin = admin_app(pool.clone()).await;
    for slug in ["posts", "pages"] {
        let schema = json!({"slug": slug, "fields": [{"name": "title", "type": "text"}]});
        send(
            &admin,
            "POST",
            "/api/admin/collections",
            Some(&schema.to_string()),
        )
        .await;
    }
    admin
}

async fn make_key(app: &Router, body: Value) -> Res {
    send(app, "POST", "/api/auth/keys", Some(&body.to_string())).await
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn keys_are_shown_once_and_stored_hashed(pool: PgPool) {
    let admin = site(&pool).await;
    let res = make_key(&admin, json!({"name": "website", "scopes": ["posts:read"]})).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let key = res.json["key"].as_str().unwrap().to_owned();
    assert!(key.starts_with(&format!("tcy_{}_", res.json["prefix"].as_str().unwrap())));
    let list = send(&admin, "GET", "/api/auth/keys", None).await;
    assert_eq!(list.json["docs"][0]["name"], "website");
    assert!(
        list.json["docs"][0].get("key").is_none(),
        "the secret is never shown again"
    );
    let stored: Vec<u8> = sqlx::query_scalar("SELECT secret_hash FROM api_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    let secret = key.rsplit('_').next().unwrap();
    assert_ne!(stored, secret.as_bytes());
    assert_eq!(stored.len(), 32, "a SHA-256");
    assert!(list.json["docs"][0]["lastUsedAt"].is_null());
    with_key(&admin, &key, "GET", "/api/posts", None).await;
    let list = send(&admin, "GET", "/api/auth/keys", None).await;
    assert!(list.json["docs"][0]["lastUsedAt"].is_string());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn scopes_narrow_what_a_key_can_do(pool: PgPool) {
    let admin = site(&pool).await;
    let key = make_key(&admin, json!({"name": "site", "scopes": ["posts:read"]}))
        .await
        .json["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    assert_eq!(
        with_key(&anon, &key, "GET", "/api/posts", None)
            .await
            .status,
        200
    );
    assert_problem(&with_key(&anon, &key, "GET", "/api/pages", None).await, 403);
    assert_problem(
        &with_key(
            &anon,
            &key,
            "POST",
            "/api/posts",
            Some(json!({"title": "x"})),
        )
        .await,
        403,
    );
    assert_problem(&with_key(&anon, &key, "GET", "/api/users", None).await, 403);
    let schema = json!({"slug": "more", "fields": []});
    assert_problem(
        &with_key(&anon, &key, "POST", "/api/admin/collections", Some(schema)).await,
        403,
    );

    let writer = make_key(&admin, json!({"name": "w", "scopes": ["posts:write"]}))
        .await
        .json["key"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        with_key(
            &anon,
            &writer,
            "POST",
            "/api/posts",
            Some(json!({"title": "x"}))
        )
        .await
        .status,
        201
    );

    // A key can't make keys, and only an Admin can grant the admin scope.
    let res = with_key(
        &anon,
        &writer,
        "POST",
        "/api/auth/keys",
        Some(json!({"name": "k", "scopes": ["posts:read"]})),
    )
    .await;
    assert_problem(&res, 403);
    let editor = app_as(&pool, "e@example.com", "editor").await;
    assert_problem(
        &make_key(&editor, json!({"name": "k", "scopes": ["admin"]})).await,
        403,
    );
    assert_eq!(
        make_key(&editor, json!({"name": "k", "scopes": ["*:write"]}))
            .await
            .status,
        201
    );
    assert_problem(
        &make_key(&editor, json!({"name": "k", "scopes": ["posts:delete"]})).await,
        422,
    );
    assert_problem(
        &make_key(&editor, json!({"name": "", "scopes": ["posts:read"]})).await,
        422,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn expired_and_revoked_keys_are_refused(pool: PgPool) {
    let admin = site(&pool).await;
    let past = make_key(
        &admin,
        json!({"name": "old", "scopes": ["posts:read"], "expiresAt": "2020-01-01T00:00:00Z"}),
    )
    .await;
    assert_problem(&past, 422);
    let res = make_key(
        &admin,
        json!({"name": "k", "scopes": ["posts:read"], "expiresAt": "2099-01-01T00:00:00Z"}),
    )
    .await;
    let key = res.json["key"].as_str().unwrap().to_owned();
    let id = res.json["id"].as_str().unwrap().to_owned();
    assert_eq!(
        with_key(&admin, &key, "GET", "/api/posts", None)
            .await
            .status,
        200
    );
    sqlx::query("UPDATE api_keys SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_problem(
        &with_key(&admin, &key, "GET", "/api/posts", None).await,
        401,
    );
    sqlx::query("UPDATE api_keys SET expires_at = NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        send(&admin, "DELETE", &format!("/api/auth/keys/{id}"), None)
            .await
            .status,
        204
    );
    assert_problem(
        &with_key(&admin, &key, "GET", "/api/posts", None).await,
        401,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn rotation_replaces_the_secret(pool: PgPool) {
    let admin = site(&pool).await;
    let res = make_key(&admin, json!({"name": "k", "scopes": ["posts:read"]})).await;
    let (old, id) = (
        res.json["key"].as_str().unwrap().to_owned(),
        res.json["id"].as_str().unwrap().to_owned(),
    );
    let rotated = send(&admin, "POST", &format!("/api/auth/keys/{id}/rotate"), None).await;
    assert_eq!(rotated.status, 201, "{}", rotated.json);
    let new = rotated.json["key"].as_str().unwrap();
    assert_ne!(new, old);
    assert_eq!(rotated.json["name"], "k");
    assert_eq!(rotated.json["scopes"], json!(["posts:read"]));
    assert_problem(
        &with_key(&admin, &old, "GET", "/api/posts", None).await,
        401,
    );
    assert_eq!(
        with_key(&admin, new, "GET", "/api/posts", None)
            .await
            .status,
        200
    );
}
