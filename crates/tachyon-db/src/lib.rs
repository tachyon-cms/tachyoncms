//! PostgreSQL storage for TachyonCMS.

pub mod collections;
pub mod documents;

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
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}
