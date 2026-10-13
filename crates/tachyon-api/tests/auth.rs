mod common;

use std::time::{Duration, Instant};

use axum::Router;
use axum::http::Request;
use common::{Res, assert_problem, send, send_with};
use serde_json::{Value, json};
use sqlx::PgPool;

const PASSWORD: &str = "a long enough passphrase";
const ORIGIN: &str = "http://localhost:3000";

async fn setup(app: &Router) -> Res {
    let body = json!({"email": "Admin@Example.com", "name": "Ada", "password": PASSWORD});
    send_with(
        app,
        Request::builder().method("POST").uri("/api/setup"),
        Some(&body.to_string()),
    )
    .await
}

async fn login(app: &Router, email: &str, password: &str, bearer: bool) -> Res {
    let body = json!({"email": email, "password": password, "bearer": bearer});
    send_with(
        app,
        Request::builder().method("POST").uri("/api/auth/login"),
        Some(&body.to_string()),
    )
    .await
}

/// The session cookie a response set, as `name=value`.
fn cookie(res: &Res) -> String {
    let set = res.headers["set-cookie"].to_str().unwrap();
    set.split(';').next().unwrap().to_owned()
}

async fn as_cookie(
    app: &Router,
    cookie: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Res {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie)
        .header("origin", ORIGIN);
    send_with(app, req, body.map(|b| b.to_string()).as_deref()).await
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn setup_creates_the_first_admin_once(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    assert_eq!(
        send(&app, "GET", "/api/setup", None).await.json,
        json!({"needed": true})
    );
    let (a, b) = tokio::join!(setup(&app), setup(&app));
    let mut statuses = [a.status.as_u16(), b.status.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [201, 409], "exactly one setup wins");
    let winner = if a.status == 201 { a } else { b };
    assert_eq!(winner.json["email"], "admin@example.com");
    assert_eq!(winner.json["role"], "admin");
    assert_eq!(winner.json["name"], "Ada");
    assert!(
        winner.json.get("passwordHash").is_none() && winner.json.get("password_hash").is_none()
    );
    assert_eq!(
        send(&app, "GET", "/api/setup", None).await.json,
        json!({"needed": false})
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn login_sets_a_secure_host_cookie_and_can_return_a_bearer_token(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    setup(&app).await;
    let res = login(&app, "admin@EXAMPLE.com", PASSWORD, false).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let set = res.headers["set-cookie"].to_str().unwrap();
    for part in [
        "__Host-tachyon_session=tcs_",
        "HttpOnly",
        "Secure",
        "SameSite=Strict",
        "Path=/",
    ] {
        assert!(set.contains(part), "{part} missing from {set}");
    }
    assert!(res.json.get("token").is_none());
    assert_eq!(res.json["user"]["email"], "admin@example.com");
    let me = as_cookie(&app, &cookie(&res), "GET", "/api/auth/me", None).await;
    assert_eq!(me.status, 200, "{}", me.json);
    assert_eq!(me.json["role"], "admin");

    let res = login(&app, "admin@example.com", PASSWORD, true).await;
    let token = res.json["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("tcs_"));
    let req = Request::builder()
        .uri("/api/auth/me")
        .header("authorization", format!("Bearer {token}"));
    assert_eq!(send_with(&app, req, None).await.status, 200);
    assert_problem(&send(&app, "GET", "/api/auth/me", None).await, 401);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn wrong_password_unknown_email_and_lockout_look_the_same(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    setup(&app).await;
    let timed = |email: &'static str, password: &'static str| {
        let app = app.clone();
        async move {
            let start = Instant::now();
            let res = login(&app, email, password, false).await;
            (res, start.elapsed())
        }
    };
    let (unknown, t_unknown) = timed("nobody@example.com", PASSWORD).await;
    let (wrong, t_wrong) = timed("admin@example.com", "not the right password").await;
    for res in [&unknown, &wrong] {
        assert_problem(res, 401);
        assert_eq!(res.json["detail"], "Email or password is wrong.");
    }
    let (lo, hi) = (t_unknown.min(t_wrong), t_unknown.max(t_wrong));
    assert!(
        hi < lo * 3 + Duration::from_millis(20),
        "unknown {t_unknown:?} vs wrong {t_wrong:?}"
    );

    for _ in 0..4 {
        login(&app, "admin@example.com", "not the right password", false).await;
    }
    let (locked, _) = timed("admin@example.com", PASSWORD).await;
    assert_problem(&locked, 401);
    assert_eq!(
        locked.json,
        json!({
            "type": locked.json["type"], "title": locked.json["title"], "status": 401,
            "detail": "Email or password is wrong.", "requestId": locked.json["requestId"]
        }),
        "a locked account looks like a wrong password"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn logout_and_session_revocation_end_access(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    setup(&app).await;
    let first = cookie(&login(&app, "admin@example.com", PASSWORD, false).await);
    let second = cookie(&login(&app, "admin@example.com", PASSWORD, false).await);
    let sessions = as_cookie(&app, &first, "GET", "/api/auth/sessions", None).await;
    assert_eq!(
        sessions.json["docs"].as_array().unwrap().len(),
        2,
        "{}",
        sessions.json
    );
    let current: Vec<&Value> = sessions.json["docs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["current"] == true)
        .collect();
    assert_eq!(current.len(), 1);
    let other_id = sessions.json["docs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["current"] == false)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let revoked = as_cookie(
        &app,
        &first,
        "DELETE",
        &format!("/api/auth/sessions/{other_id}"),
        None,
    )
    .await;
    assert_eq!(revoked.status, 204);
    assert_problem(
        &as_cookie(&app, &second, "GET", "/api/auth/me", None).await,
        401,
    );

    let out = as_cookie(&app, &first, "POST", "/api/auth/logout", None).await;
    assert_eq!(out.status, 204);
    assert!(
        out.headers["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_problem(
        &as_cookie(&app, &first, "GET", "/api/auth/me", None).await,
        401,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn changing_password_signs_out_other_sessions(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    setup(&app).await;
    let here = cookie(&login(&app, "admin@example.com", PASSWORD, false).await);
    let there = cookie(&login(&app, "admin@example.com", PASSWORD, false).await);
    let wrong = as_cookie(
        &app,
        &here,
        "POST",
        "/api/auth/password",
        Some(json!({"current": "not it at all", "new": "another long passphrase"})),
    )
    .await;
    assert_problem(&wrong, 403);
    let changed = as_cookie(
        &app,
        &here,
        "POST",
        "/api/auth/password",
        Some(json!({"current": PASSWORD, "new": "another long passphrase"})),
    )
    .await;
    assert_eq!(changed.status, 204, "{}", changed.json);
    assert_eq!(
        as_cookie(&app, &here, "GET", "/api/auth/me", None)
            .await
            .status,
        200
    );
    assert_problem(
        &as_cookie(&app, &there, "GET", "/api/auth/me", None).await,
        401,
    );
    assert_eq!(
        login(&app, "admin@example.com", "another long passphrase", false)
            .await
            .status,
        200
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn short_passwords_and_bad_emails_are_refused(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    for (body, path, message) in [
        (
            json!({"email": "a@b.co", "name": "A", "password": "short"}),
            "password",
            "a password needs at least 12 characters",
        ),
        (
            json!({"email": "nope", "name": "A", "password": PASSWORD}),
            "email",
            "expected an email address",
        ),
        (
            json!({"email": "a@b.co", "name": "", "password": PASSWORD}),
            "name",
            "is required",
        ),
    ] {
        let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
        assert_problem(&res, 422);
        assert_eq!(res.json["errors"][0]["path"], path);
        assert_eq!(res.json["errors"][0]["message"], message);
    }
}

/// A stand-in for the breach service that knows one password.
async fn breach_stub(known: &'static str) -> String {
    use axum::extract::Path;
    use sha1::Digest;
    let hash = hex::encode_upper(sha1::Sha1::digest(known.as_bytes()));
    let (prefix, suffix) = (hash[..5].to_owned(), hash[5..].to_owned());
    let app = axum::Router::new().route(
        "/range/{prefix}",
        axum::routing::get(move |Path(p): Path<String>| {
            let body = if p == prefix {
                format!("{suffix}:7\r\n")
            } else {
                String::new()
            };
            async move { body }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn breached_passwords_are_refused_at_setup_and_change(pool: PgPool) {
    let url = breach_stub("password1234567").await;
    let options = tachyon_api::Options {
        auth: tachyon_api::auth::AuthConfig {
            breach_check: true,
            breach_check_url: url,
            ..Default::default()
        },
    };
    let app = tachyon_api::router_with(pool, options).await.unwrap();
    let body = json!({"email": "a@b.co", "name": "A", "password": "password1234567"});
    let res = send(&app, "POST", "/api/setup", Some(&body.to_string())).await;
    assert_problem(&res, 422);
    assert_eq!(
        res.json["errors"][0]["message"],
        "This password has appeared in a data breach. Choose another."
    );
    assert_eq!(setup(&app).await.status, 201);
    let session = cookie(&login(&app, "admin@example.com", PASSWORD, false).await);
    let res = as_cookie(
        &app,
        &session,
        "POST",
        "/api/auth/password",
        Some(json!({"current": PASSWORD, "new": "password1234567"})),
    )
    .await;
    assert_problem(&res, 422);
    assert_eq!(res.json["errors"][0]["path"], "new");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn cookie_writes_need_a_matching_origin(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    setup(&app).await;
    let session = cookie(&login(&app, "admin@example.com", PASSWORD, false).await);
    for origin in [Some("https://evil.example"), None] {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/auth/logout")
            .header("cookie", &session);
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        let res = send_with(&app, req, None).await;
        assert_problem(&res, 403);
        assert_eq!(res.json["detail"], "This request came from another site.");
    }
    // Reads don't need it.
    let req = Request::builder()
        .uri("/api/auth/me")
        .header("cookie", &session);
    assert_eq!(send_with(&app, req, None).await.status, 200);
    assert_eq!(
        as_cookie(&app, &session, "POST", "/api/auth/logout", None)
            .await
            .status,
        204
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn bearer_tokens_skip_the_origin_check(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    setup(&app).await;
    let token = login(&app, "admin@example.com", PASSWORD, true).await.json["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let req = Request::builder()
        .method("POST")
        .uri("/api/auth/logout")
        .header("authorization", format!("Bearer {token}"))
        .header("origin", "https://another-front-end.example");
    assert_eq!(send_with(&app, req, None).await.status, 204);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn bad_or_revoked_credentials_are_401_not_anonymous(pool: PgPool) {
    let app = tachyon_api::router(pool).await.unwrap();
    setup(&app).await;
    for header in [
        "Bearer tcs_nope",
        "Bearer tcy_abcd1234_nope",
        "Basic abc",
        "Bearer something",
    ] {
        let req = Request::builder()
            .uri("/api/setup")
            .header("authorization", header);
        assert_problem(&send_with(&app, req, None).await, 401);
    }
    // A stale cookie is just signed out.
    let req = Request::builder()
        .uri("/api/setup")
        .header("cookie", "__Host-tachyon_session=tcs_gone");
    assert_eq!(send_with(&app, req, None).await.status, 200);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn public_urls_with_a_path_and_extra_origins_match(pool: PgPool) {
    let options = tachyon_api::Options {
        auth: tachyon_api::auth::AuthConfig {
            public_url: "https://cms.example.com/admin/".into(),
            allowed_origins: vec!["https://www.example.com".into()],
            ..Default::default()
        },
    };
    let app = tachyon_api::router_with(pool, options).await.unwrap();
    setup(&app).await;
    for (origin, status) in [
        ("https://cms.example.com", 204),
        ("https://www.example.com", 204),
        ("https://CMS.example.com:443", 204),
        ("http://cms.example.com", 403),
    ] {
        let session = cookie(&login(&app, "admin@example.com", PASSWORD, false).await);
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/logout")
            .header("cookie", &session)
            .header("origin", origin);
        assert_eq!(
            send_with(&app, req, None).await.status.as_u16(),
            status,
            "{origin}"
        );
    }
}
