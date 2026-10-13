//! HTTP API for TachyonCMS.

pub mod auth;
mod collections;
mod documents;
mod error;
mod globals;
mod joins;
mod params;
mod populate;
mod request_id;
mod schemas;

use std::sync::Arc;
use std::time::Duration;

use std::any::Any;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use sqlx::PgPool;
use tachyon_db::DbError;
use tower_http::catch_panic::CatchPanicLayer;

pub use error::ApiError;
use schemas::Schemas;

#[derive(Clone)]
pub(crate) struct AppState {
    pub pool: PgPool,
    pub schemas: Arc<Schemas>,
    pub auth: Arc<auth::AuthState>,
}

/// How the router is set up. `Default` suits development and tests.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub auth: auth::AuthConfig,
}

/// Why the API couldn't start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("{0}")]
    Config(String),
}

/// The API with default options; see `router_with`.
pub async fn router(pool: PgPool) -> Result<Router, StartError> {
    router_with(pool, Options::default()).await
}

pub async fn router_with(pool: PgPool, options: Options) -> Result<Router, StartError> {
    let auth = Arc::new(auth::AuthState::new(options.auth).map_err(StartError::Config)?);
    let schemas = Arc::new(Schemas::load(&pool).await?);
    // Builds any index a failed change left missing, in the background: a big build, or one
    // waiting on another server's, mustn't keep this one from answering.
    let collections = tachyon_db::collections::list(&pool).await?;
    let heal = pool.clone();
    tokio::spawn(async move {
        for c in collections {
            if let Err(error) = tachyon_db::indexes::reconcile_indexes(&heal, c.id).await {
                tracing::warn!(%error, collection = %c.schema.slug, "couldn't build indexes");
            }
        }
    });
    schemas::spawn_listener(pool.clone(), schemas.clone());
    let routes = Router::new()
        .route(
            "/api/admin/collections",
            get(collections::list).post(collections::create),
        )
        .route(
            "/api/admin/collections/{slug}",
            get(collections::get_one).put(collections::update),
        )
        .route(
            "/api/admin/collections/{slug}/preview",
            post(collections::preview),
        )
        .route(
            "/api/globals/{slug}",
            get(globals::get).patch(globals::update),
        )
        .route(
            "/api/{collection}",
            get(documents::list).post(documents::create),
        )
        .route(
            "/api/{collection}/{id}",
            get(documents::get)
                .patch(documents::update)
                .delete(documents::delete),
        )
        .route(
            "/api/{collection}/{id}/references",
            get(documents::references),
        )
        .merge(auth::routes())
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed });
    let state = AppState {
        pool,
        schemas,
        auth,
    };
    let routes = routes.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        auth::principal::resolve,
    ));
    Ok(layers(routes, Duration::from_secs(30)).with_state(state))
}

/// What every route sits behind: a body limit, a time limit and a panic guard, all answering
/// in problem+json with the request's ID. Outermost last.
fn layers<S: Clone + Send + Sync + 'static>(routes: Router<S>, limit: Duration) -> Router<S> {
    routes
        .fallback(|| async { ApiError::NotFound })
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .layer(CatchPanicLayer::custom(|panic: Box<dyn Any + Send>| {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            ApiError::Internal(format!("handler panicked: {message}")).into_response()
        }))
        .layer(axum::middleware::from_fn_with_state(limit, time_limit))
        .layer(axum::middleware::from_fn(request_id::middleware))
}

async fn time_limit(State(limit): State<Duration>, req: Request, next: Next) -> Response {
    tokio::time::timeout(limit, next.run(req))
        .await
        .unwrap_or_else(|_| ApiError::Timeout.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    async fn call(app: Router, uri: &str) -> (StatusCode, String, Value) {
        let res = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let kind = res
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            kind,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn app() -> Router {
        let routes = Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "late"
                }),
            )
            .route(
                "/panic",
                get(|| async {
                    if true {
                        panic!("boom");
                    }
                    "unreachable"
                }),
            );
        layers(routes, Duration::from_millis(50))
    }

    #[tokio::test]
    async fn slow_requests_get_503_problem_json() {
        let (status, kind, body) = call(app(), "/slow").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(kind, "application/problem+json");
        assert!(!body["requestId"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn panics_get_500_problem_json_with_request_id() {
        let (status, kind, body) = call(app(), "/panic").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(kind, "application/problem+json");
        assert!(!body["requestId"].as_str().unwrap().is_empty());
    }
}
