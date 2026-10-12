//! PostgreSQL storage for TachyonCMS.

pub mod collections;
pub mod documents;
pub mod query;

pub use sqlx::PgPool;
use uuid::Uuid;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Every row belongs to a tenant. Until multi-tenancy ships, this is the only one.
pub const DEFAULT_TENANT: Uuid = Uuid::from_u128(1);

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("not found")]
    NotFound,
    #[error("{0} already exists")]
    Conflict(String),
    /// Field-level problems only the database can spot, such as a link to a missing document.
    #[error("invalid document")]
    Validation(Vec<tachyon_core::FieldError>),
    /// A unique index rejected the write. Holds the constraint name.
    #[error("unique violation on {0}")]
    UniqueViolation(String),
    /// Other live documents still link to this one.
    #[error("referenced by {0} documents")]
    Referenced(i64),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

pub(crate) fn map_unique(e: sqlx::Error) -> DbError {
    match e {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => {
            DbError::UniqueViolation(db.constraint().unwrap_or_default().to_owned())
        }
        e => e.into(),
    }
}
