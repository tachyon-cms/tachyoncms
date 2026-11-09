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
    send_body(app, req, body).await
}

/// `req` with `body` as it is, headers and all left to the caller.
pub async fn send_body(app: &Router, req: axum::http::request::Builder, body: Body) -> Res {
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

/// `app` with `key` as the bearer token on every request.
pub fn with_key(app: Router, key: &str) -> Router {
    let bearer = HeaderValue::from_str(&format!("Bearer {key}")).unwrap();
    app.layer(axum::middleware::map_request(
        move |mut req: Request<Body>| {
            let bearer = bearer.clone();
            async move {
                req.headers_mut().insert("authorization", bearer);
                req
            }
        },
    ))
}

/// The boundary `multipart` separates its parts with.
pub const BOUNDARY: &str = "tachyon-test-boundary";

/// A `multipart/form-data` body and its content type: `file` (its bytes, sent as
/// `image/png` whatever they are, under `name`) and `_payload` (JSON), each when given.
pub fn multipart(file: Option<&[u8]>, name: &str, payload: Option<&Value>) -> (String, Vec<u8>) {
    let mut body = Vec::new();
    if let Some(bytes) = file {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    if let Some(payload) = payload {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"_payload\"\r\n\r\n{payload}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

/// Sends `multipart`'s body to `uri`.
pub async fn upload(
    app: &Router,
    method: &str,
    uri: &str,
    file: Option<&[u8]>,
    name: &str,
    payload: Option<&Value>,
) -> Res {
    let (kind, body) = multipart(file, name, payload);
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", kind);
    send_body(app, req, Body::from(body)).await
}

/// A valid `w`×`h` grey PNG, its pixel data stored without compression.
pub fn png(w: u32, h: u32) -> Vec<u8> {
    fn crc(bytes: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in bytes {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 == 1 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let sum = crc(&out[start..]);
        out.extend_from_slice(&sum.to_be_bytes());
    }
    // Each row: filter 0, then one grey byte per pixel.
    let mut raw = Vec::new();
    for _ in 0..h {
        raw.push(0);
        raw.extend(std::iter::repeat_n(0x80, w as usize));
    }
    // zlib with stored deflate blocks of at most 65,535 bytes.
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}
