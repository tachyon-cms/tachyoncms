use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tachyon_core::FieldError;
use tachyon_db::DbError;

/// A write that met another's locks and was undone, which may safely run again.
pub(crate) const TRY_AGAIN: &str =
    "Another change to the same documents got in the way; try again.";

/// Every API error, rendered as RFC 9457 `application/problem+json`.
#[derive(Debug)]
pub enum ApiError {
    NotFound,
    MethodNotAllowed,
    BadRequest(String),
    /// No credentials, or ones that don't work.
    Unauthorized(String),
    /// Credentials that work, for someone who may not do this.
    Forbidden(String),
    BodyTooLarge,
    /// The request ran past its time limit.
    Timeout,
    Conflict(String),
    Validation(Vec<FieldError>),
    /// A unique index rejected the write; handlers turn this into a field error when they can.
    Unique(String),
    /// A schema change needs `"confirm": true`; carries the preview to show the user.
    NeedsConfirmation(serde_json::Value),
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
            DbError::InvalidQuery(message) => Self::BadRequest(message.to_owned()),
            DbError::Refused(why) => Self::Conflict(why),
            DbError::NotOwner => {
                Self::Forbidden("You can only change documents you created.".into())
            }
            DbError::Forbidden => {
                Self::Forbidden("An access rule doesn't let you change this document.".into())
            }
            DbError::SchemaChanged => Self::Conflict(
                "This collection's fields just changed. Reload and try again.".into(),
            ),
            // Two writes that met each other's locks: Postgres undid one, which may run again.
            DbError::Sqlx(sqlx::Error::Database(e))
                if matches!(e.code().as_deref(), Some("40P01" | "40001")) =>
            {
                Self::Conflict(TRY_AGAIN.into())
            }
            DbError::Sqlx(e) => Self::Internal(e.to_string()),
        }
    }
}

impl ApiError {
    /// The status and problem+json body this error answers with.
    pub(crate) fn problem(self) -> (StatusCode, serde_json::Value) {
        let mut extra = None;
        let (status, title, detail, errors) = match self {
            Self::NeedsConfirmation(preview) => {
                extra = Some(("preview", preview));
                (
                    StatusCode::CONFLICT,
                    "Confirmation required",
                    Some("This change removes or retypes fields, or leaves documents that won't validate. Send it again with \"confirm\": true to go ahead.".to_owned()),
                    None,
                )
            }
            Self::NotFound => (StatusCode::NOT_FOUND, "Not found", None, None),
            Self::MethodNotAllowed => (
                StatusCode::METHOD_NOT_ALLOWED,
                "Method not allowed",
                None,
                None,
            ),
            Self::BadRequest(d) => (StatusCode::BAD_REQUEST, "Bad request", Some(d), None),
            Self::Unauthorized(d) => (StatusCode::UNAUTHORIZED, "Unauthorized", Some(d), None),
            Self::Forbidden(d) => (StatusCode::FORBIDDEN, "Forbidden", Some(d), None),
            Self::BodyTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "Content too large",
                Some("the request body is over 10 MB".to_owned()),
                None,
            ),
            Self::Timeout => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Service unavailable",
                Some("the request took too long; try again".to_owned()),
                None,
            ),
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
        if let Some((key, value)) = extra {
            body[key] = value;
        }
        (status, body)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = self.problem();
        (
            status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            body.to_string(),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_deadlock_says_try_again(pool: PgPool) {
        sqlx::raw_sql("CREATE TABLE t (id int PRIMARY KEY); INSERT INTO t VALUES (1), (2)")
            .execute(&pool)
            .await
            .unwrap();
        let lock = |id: i32| sqlx::query("SELECT id FROM t WHERE id = $1 FOR UPDATE").bind(id);
        let (mut a, mut b) = (pool.begin().await.unwrap(), pool.begin().await.unwrap());
        lock(1).execute(&mut *a).await.unwrap();
        lock(2).execute(&mut *b).await.unwrap();
        // Each now waits on the other's row; Postgres undoes one of them.
        let (x, y) = tokio::join!(lock(2).execute(&mut *a), lock(1).execute(&mut *b));
        let e = x.err().or(y.err()).expect("one of them deadlocked");
        assert!(matches!(
            ApiError::from(DbError::from(e)),
            ApiError::Conflict(d) if d.contains("try again")
        ));
    }
}
