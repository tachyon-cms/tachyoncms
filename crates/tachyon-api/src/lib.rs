//! HTTP API for TachyonCMS.

mod collections;
mod documents;
mod error;
mod request_id;
mod schemas;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::get;
use sqlx::PgPool;
use tachyon_db::DbError;
use tower_http::timeout::TimeoutLayer;

pub use error::ApiError;
use schemas::Schemas;

#[derive(Clone)]
pub(crate) struct AppState {
    pub pool: PgPool,
    pub schemas: Arc<Schemas>,
}

pub async fn router(pool: PgPool) -> Result<Router, DbError> {
    let schemas = Arc::new(Schemas::load(&pool).await?);
    schemas::spawn_listener(pool.clone(), schemas.clone());
    Ok(Router::new()
        .route(
            "/api/admin/collections",
            get(collections::list).post(collections::create),
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
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed })
        .fallback(|| async { ApiError::NotFound })
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(30),
        ))
        .layer(axum::middleware::from_fn(request_id::middleware))
        .with_state(AppState { pool, schemas }))
}
