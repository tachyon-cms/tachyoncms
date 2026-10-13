#![allow(dead_code)] // each test binary uses a different subset of these helpers

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

pub struct Res {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub json: Value,
}

pub async fn send(app: &Router, method: &str, uri: &str, body: Option<&str>) -> Res {
    send_with(app, Request::builder().method(method).uri(uri), body).await
}

pub async fn send_with(
    app: &Router,
    mut req: axum::http::request::Builder,
    body: Option<&str>,
) -> Res {
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_owned()));
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    Res {
        status,
        headers,
        json,
    }
}

pub fn assert_problem(res: &Res, status: u16) {
    assert_eq!(res.status.as_u16(), status, "{}", res.json);
    assert_eq!(res.headers["content-type"], "application/problem+json");
    assert_eq!(res.json["status"], status);
    assert!(
        res.json["requestId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{}",
        res.json
    );
}

pub const PASSWORD: &str = "a long enough passphrase";
pub const ORIGIN: &str = "http://localhost:3000";
pub const ADMIN: &str = "admin@example.com";

/// Signs in as `email` and wraps `app` so every request carries that session cookie and a
/// matching `Origin` (unless the request brings its own credentials).
pub async fn signed_in(app: Router, email: &str) -> Router {
    let body = serde_json::json!({"email": email, "password": PASSWORD}).to_string();
    let res = send(&app, "POST", "/api/auth/login", Some(&body)).await;
    assert_eq!(res.status, 200, "login as {email}: {}", res.json);
    let cookie = res.headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let cookie = HeaderValue::from_str(&cookie).unwrap();
    app.layer(axum::middleware::map_request(
        move |mut req: Request<Body>| {
            let cookie = cookie.clone();
            async move {
                let headers = req.headers_mut();
                if !headers.contains_key("authorization") && !headers.contains_key("cookie") {
                    headers.insert("cookie", cookie);
                }
                if !headers.contains_key("origin") {
                    headers.insert("origin", HeaderValue::from_static(ORIGIN));
                }
                req
            }
        },
    ))
}

/// The API, set up, signed in as its first Admin.
pub async fn admin_app(pool: PgPool) -> Router {
    let app = tachyon_api::router(pool).await.unwrap();
    let body = serde_json::json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "setup: {}", res.json);
    signed_in(app, ADMIN).await
}

/// Adds a user with `role` and the shared test password (needs `admin_app` to have run).
pub async fn add_user(pool: &PgPool, email: &str, role: &str) -> Uuid {
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM accounts WHERE email = $1")
        .bind(ADMIN)
        .fetch_one(pool)
        .await
        .unwrap();
    let users = tachyon_db::auth::ensure_users_collection(pool)
        .await
        .unwrap();
    let new = tachyon_db::auth::NewAccount {
        email,
        name: role,
        password_hash: &hash,
        role,
    };
    tachyon_db::auth::create_account(pool, &users, &new, false)
        .await
        .unwrap()
}

/// The API signed in as a new user with `role`.
pub async fn app_as(pool: &PgPool, email: &str, role: &str) -> Router {
    add_user(pool, email, role).await;
    let app = tachyon_api::router(pool.clone()).await.unwrap();
    signed_in(app, email).await
}
