//! Attacks found in review, pinned so they stay closed.

mod common;

use axum::Router;
use axum::http::Request;
use common::{
    ADMIN, PASSWORD, Res, add_user, admin_app, app_as, assert_problem, send, send_with, signed_in,
};
use serde_json::{Value, json};
use sqlx::PgPool;

async fn with_key(app: &Router, key: &str, method: &str, uri: &str, body: Option<Value>) -> Res {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"));
    send_with(app, req, body.map(|b| b.to_string()).as_deref()).await
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn users_cant_be_reached_through_the_generic_routes(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let me = send(&admin, "GET", "/api/auth/me", None).await.json["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let editor = app_as(&pool, "e@example.com", "editor").await;
    for app in [&anon, &editor, &admin] {
        assert_problem(&send(app, "GET", "/api/%75sers", None).await, 404);
        assert_problem(
            &send(app, "GET", &format!("/api/%75sers/{me}"), None).await,
            404,
        );
        assert_problem(
            &send(
                app,
                "PATCH",
                &format!("/api/%75sers/{me}"),
                Some(r#"{"name": "x"}"#),
            )
            .await,
            404,
        );
        assert_problem(
            &send(app, "DELETE", &format!("/api/%75sers/{me}"), None).await,
            404,
        );
        assert_problem(
            &send(app, "POST", "/api/%75sers", Some(r#"{"name": "x"}"#)).await,
            404,
        );
        assert_problem(
            &send(app, "GET", &format!("/api/users/{me}/references"), None).await,
            404,
        );
    }
    assert_eq!(
        send(&admin, "GET", "/api/auth/me", None).await.json["name"],
        "Ada"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn read_only_keys_stay_read_only(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let schema = json!({"slug": "posts", "fields": [{"name": "title", "type": "text"}]});
    send(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    let me = send(&admin, "GET", "/api/auth/me", None).await.json["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = send(
        &admin,
        "POST",
        "/api/auth/keys",
        Some(&json!({"name": "k", "scopes": ["posts:read"]}).to_string()),
    )
    .await
    .json["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    let sessions = send(&admin, "GET", "/api/auth/sessions", None).await;
    let session = sessions.json["docs"][0]["id"].as_str().unwrap().to_owned();
    for (method, uri, body) in [
        ("GET", "/api/auth/sessions".to_owned(), None),
        ("DELETE", format!("/api/auth/sessions/{session}"), None),
        ("GET", "/api/admin/collections".to_owned(), None),
        ("GET", "/api/admin/collections/posts".to_owned(), None),
        ("GET", "/api/admin/roles".to_owned(), None),
        ("GET", "/api/auth/me".to_owned(), None),
        ("GET", format!("/api/users/{me}"), None),
        (
            "PATCH",
            format!("/api/users/{me}"),
            Some(json!({"name": "x"})),
        ),
    ] {
        let res = with_key(&anon, &key, method, &uri, body).await;
        assert_problem(&res, 403);
    }
    assert_eq!(
        send(&admin, "GET", "/api/auth/me", None).await.status,
        200,
        "the session survives"
    );
    let admin_key = send(
        &admin,
        "POST",
        "/api/auth/keys",
        Some(&json!({"name": "a", "scopes": ["admin"]}).to_string()),
    )
    .await
    .json["key"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        with_key(&anon, &admin_key, "GET", "/api/admin/collections", None)
            .await
            .status,
        200
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn an_admin_setting_their_own_password_stays_signed_in(pool: PgPool) {
    let admin = admin_app(pool).await;
    let me = send(&admin, "GET", "/api/auth/me", None).await.json["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let body = json!({"password": "a brand new passphrase"});
    assert_eq!(
        send(
            &admin,
            "PATCH",
            &format!("/api/users/{me}"),
            Some(&body.to_string())
        )
        .await
        .status,
        200
    );
    assert_eq!(send(&admin, "GET", "/api/auth/me", None).await.status, 200);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn changing_email_doesnt_reveal_who_else_has_an_account(pool: PgPool) {
    let _admin = admin_app(pool.clone()).await;
    let me = add_user(&pool, "v@example.com", "viewer").await;
    let viewer = signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "v@example.com",
    )
    .await;
    let body = json!({"email": ADMIN, "currentPassword": PASSWORD});
    let res = send(
        &viewer,
        "PATCH",
        &format!("/api/users/{me}"),
        Some(&body.to_string()),
    )
    .await;
    assert_problem(&res, 409);
    assert_eq!(
        res.json["detail"],
        "That email can't be used. Choose another."
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_bearer_scheme_is_case_insensitive(pool: PgPool) {
    let app = admin_app(pool.clone()).await;
    let token = send(
        &app,
        "POST",
        "/api/auth/login",
        Some(&json!({"email": ADMIN, "password": PASSWORD, "bearer": true}).to_string()),
    )
    .await
    .json["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let anon = tachyon_api::router(pool).await.unwrap();
    let req = Request::builder()
        .uri("/api/auth/me")
        .header("authorization", format!("bearer {token}"));
    assert_eq!(send_with(&anon, req, None).await.status, 200);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn every_unsafe_cookie_write_needs_our_origin(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let me = send(&admin, "GET", "/api/auth/me", None).await.json["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let raw = tachyon_api::router(pool.clone()).await.unwrap();
    let login = send(
        &raw,
        "POST",
        "/api/auth/login",
        Some(&json!({"email": ADMIN, "password": PASSWORD}).to_string()),
    )
    .await;
    let cookie = login.headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    for (method, uri, origin) in [
        ("PUT", "/api/admin/roles/editor".to_owned(), Some("null")),
        ("DELETE", format!("/api/users/{me}"), None),
        (
            "PATCH",
            format!("/api/users/{me}"),
            Some("http://localhost:3001"),
        ),
        (
            "POST",
            "/api/auth/keys".to_owned(),
            Some("https://localhost:3000"),
        ),
    ] {
        let mut req = Request::builder()
            .method(method)
            .uri(&uri)
            .header("cookie", &cookie);
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        let res = send_with(&raw, req, Some("{}")).await;
        assert_problem(&res, 403);
        assert_eq!(
            res.json["detail"], "This request came from another site.",
            "{method} {uri} {origin:?}"
        );
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn two_admins_demoting_each_other_at_once_leave_one(pool: PgPool) {
    let first = admin_app(pool.clone()).await;
    let first_id = send(&first, "GET", "/api/auth/me", None).await.json["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let second_id = add_user(&pool, "two@example.com", "admin").await;
    let second = signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "two@example.com",
    )
    .await;
    let (to_second, to_first) = (
        format!("/api/users/{second_id}"),
        format!("/api/users/{first_id}"),
    );
    let (a, b) = tokio::join!(
        send(&first, "PATCH", &to_second, Some(r#"{"role": "editor"}"#)),
        send(&second, "PATCH", &to_first, Some(r#"{"role": "editor"}"#)),
    );
    let mut statuses = [a.status.as_u16(), b.status.as_u16()];
    statuses.sort_unstable();
    assert!(statuses.contains(&200), "{statuses:?}");
    assert_eq!(tachyon_db::auth::count_admins(&pool).await.unwrap(), 1);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_demoted_owner_narrows_their_keys(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let ed = add_user(&pool, "ed@example.com", "editor").await;
    let schema = json!({"slug": "posts", "fields": [{"name": "title", "type": "text"}]});
    send(
        &admin,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    let editor = signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "ed@example.com",
    )
    .await;
    let key = send(
        &editor,
        "POST",
        "/api/auth/keys",
        Some(&json!({"name": "k", "scopes": ["posts:write"]}).to_string()),
    )
    .await
    .json["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let anon = tachyon_api::router(pool.clone()).await.unwrap();
    assert_eq!(
        with_key(
            &anon,
            &key,
            "POST",
            "/api/posts",
            Some(json!({"title": "a"}))
        )
        .await
        .status,
        201
    );
    send(
        &admin,
        "PATCH",
        &format!("/api/users/{ed}"),
        Some(r#"{"role": "viewer"}"#),
    )
    .await;
    assert_problem(
        &with_key(
            &anon,
            &key,
            "POST",
            "/api/posts",
            Some(json!({"title": "b"})),
        )
        .await,
        403,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn keys_and_sessions_of_others_are_out_of_reach(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let key_id = send(
        &admin,
        "POST",
        "/api/auth/keys",
        Some(&json!({"name": "k", "scopes": ["posts:read"]}).to_string()),
    )
    .await
    .json["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let session = send(&admin, "GET", "/api/auth/sessions", None).await.json["docs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let editor = app_as(&pool, "e@example.com", "editor").await;
    assert_problem(
        &send(&editor, "DELETE", &format!("/api/auth/keys/{key_id}"), None).await,
        404,
    );
    assert_problem(
        &send(
            &editor,
            "POST",
            &format!("/api/auth/keys/{key_id}/rotate"),
            None,
        )
        .await,
        404,
    );
    assert_problem(
        &send(
            &editor,
            "DELETE",
            &format!("/api/auth/sessions/{session}"),
            None,
        )
        .await,
        404,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn idle_sessions_end_and_disappear_from_the_list(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let other = signed_in(tachyon_api::router(pool.clone()).await.unwrap(), ADMIN).await;
    assert_eq!(
        send(&admin, "GET", "/api/auth/sessions", None).await.json["docs"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let mine = send(&admin, "GET", "/api/auth/sessions", None).await;
    let current = mine.json["docs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["current"] == true)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    sqlx::query(
        "UPDATE sessions SET last_seen_at = now() - interval '8 days' WHERE id <> $1::uuid",
    )
    .bind(&current)
    .execute(&pool)
    .await
    .unwrap();
    assert_problem(&send(&other, "GET", "/api/auth/me", None).await, 401);
    assert_eq!(
        send(&admin, "GET", "/api/auth/sessions", None).await.json["docs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn users_record_who_created_them(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let admin_id: uuid::Uuid = send(&admin, "GET", "/api/auth/me", None).await.json["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let body =
        json!({"email": "n@example.com", "name": "N", "password": PASSWORD, "role": "viewer"});
    let created = send(&admin, "POST", "/api/users", Some(&body.to_string())).await;
    let id = created.json["id"].as_str().unwrap();
    let by: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT created_by FROM documents WHERE id = $1::uuid")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(by, Some(admin_id));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_users_name_field_stays(pool: PgPool) {
    let admin = admin_app(pool).await;
    for fields in [
        json!([{"name": "bio", "type": "textarea"}]),
        json!([{"name": "name", "type": "number"}]),
    ] {
        let change = json!({"schema": {"slug": "users", "fields": fields}, "confirm": true});
        let res = send(
            &admin,
            "PUT",
            "/api/admin/collections/users",
            Some(&change.to_string()),
        )
        .await;
        assert_problem(&res, 422);
        assert_eq!(
            res.json["errors"][0]["message"],
            "field `name`: users need a text field called name, which setup and sign-in fill"
        );
    }
}
