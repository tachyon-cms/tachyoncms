#![allow(dead_code)] // each test binary uses a different subset of these helpers

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

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
