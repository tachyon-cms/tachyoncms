//! Per-collection expression indexes. DDL can't take bind parameters, so the statements are
//! built from schema-validated field names (re-checked inside `field_expr`) and a `Uuid`.

use sqlx::{AssertSqlSafe, PgPool};
use tachyon_core::CollectionSchema;
use tachyon_core::query::value_kind;
use uuid::Uuid;

use crate::DbError;
use crate::query::field_expr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Unique,
    Filter,
}

/// A stable name under Postgres's 63-byte limit, derived from the collection and field.
pub fn index_name(collection_id: Uuid, field: &str, kind: IndexKind) -> String {
    // FNV-1a: tiny, stable across releases, and good enough to avoid collisions here.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in collection_id.as_bytes().iter().chain(field.as_bytes()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let prefix = match kind {
        IndexKind::Unique => "uq",
        IndexKind::Filter => "ix",
    };
    format!("documents_{prefix}_{hash:016x}")
}

/// Creates the indexes a new collection's schema asks for. Runs outside a transaction
/// because `CREATE INDEX CONCURRENTLY` must.
pub async fn create_field_indexes(
    pool: &PgPool,
    collection_id: Uuid,
    schema: &CollectionSchema,
) -> Result<(), DbError> {
    for f in &schema.fields {
        let Some(kind) = value_kind(&f.kind) else {
            continue;
        };
        if !(f.is_unique() || f.filterable) {
            continue;
        }
        let expr = field_expr(std::slice::from_ref(&f.name), kind);
        let scope = format!("WHERE collection_id = '{collection_id}' AND deleted_at IS NULL");
        let sql = if f.is_unique() {
            let name = index_name(collection_id, &f.name, IndexKind::Unique);
            format!(
                "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS {name} ON documents (({expr})) {scope}"
            )
        } else {
            let name = index_name(collection_id, &f.name, IndexKind::Filter);
            format!(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS {name} ON documents (({expr}), id DESC) {scope}"
            )
        };
        sqlx::query(AssertSqlSafe(sql)).execute(pool).await?;
    }
    Ok(())
}
