//! Gives every request an ID, returns it in `x-request-id`, and makes it available to error responses.

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument;
use uuid::Uuid;

tokio::task_local! {
    static REQUEST_ID: String;
}

/// The current request's ID. Empty outside a request.
pub fn current() -> String {
    REQUEST_ID.try_with(Clone::clone).unwrap_or_default()
}

/// Keeps a client-supplied ID only if it is short and plain, so it can't forge log lines.
fn is_safe(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub async fn middleware(req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| is_safe(v))
        .map_or_else(|| Uuid::now_v7().to_string(), str::to_owned);
    let span =
        tracing::info_span!("request", id = %id, method = %req.method(), path = %req.uri().path());
    let mut res = REQUEST_ID
        .scope(id.clone(), next.run(req))
        .instrument(span)
        .await;
    res.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&id).expect("request id is plain ASCII"),
    );
    res
}
