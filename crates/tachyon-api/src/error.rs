use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tachyon_core::FieldError;
use tachyon_db::DbError;

/// Every API error, rendered as RFC 9457 `application/problem+json`.
#[derive(Debug)]
pub enum ApiError {
    NotFound,
    MethodNotAllowed,
    BadRequest(String),
    Conflict(String),
    Validation(Vec<FieldError>),
    /// A unique index rejected the write; handlers turn this into a field error when they can.
    Unique(String),
    /// The cause is logged, never sent to the client.
    Internal(String),
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::NotFound => Self::NotFound,
            DbError::Conflict(what) => Self::Conflict(format!("{what} already exists")),
            DbError::Validation(errors) => Self::Validation(errors),
            DbError::UniqueViolation(constraint) => Self::Unique(constraint),
            DbError::Referenced(1) => {
                Self::Conflict("1 other document links to this one; remove that link first".into())
            }
            DbError::Referenced(n) => Self::Conflict(format!(
                "{n} other documents link to this one; remove those links first"
            )),
            DbError::SchemaChanged => Self::Conflict(
                "This collection's fields just changed. Reload and try again.".into(),
            ),
            DbError::Sqlx(e) => Self::Internal(e.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, title, detail, errors) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "Not found", None, None),
            Self::MethodNotAllowed => (
                StatusCode::METHOD_NOT_ALLOWED,
                "Method not allowed",
                None,
                None,
            ),
            Self::BadRequest(d) => (StatusCode::BAD_REQUEST, "Bad request", Some(d), None),
            Self::Conflict(d) => (StatusCode::CONFLICT, "Conflict", Some(d), None),
            Self::Unique(_) => (
                StatusCode::CONFLICT,
                "Conflict",
                Some("a document with this value already exists".to_owned()),
                None,
            ),
            Self::Validation(e) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Validation failed",
                None,
                Some(e),
            ),
            Self::Internal(cause) => {
                tracing::error!(%cause, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error",
                    None,
                    None,
                )
            }
        };
        let mut body = json!({
            "type": "about:blank",
            "title": title,
            "status": status.as_u16(),
            "requestId": crate::request_id::current(),
        });
        if let Some(detail) = detail {
            body["detail"] = detail.into();
        }
        if let Some(errors) = errors {
            body["errors"] = json!(errors);
        }
        (
            status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            body.to_string(),
        )
            .into_response()
    }
}
