//! Per-collection expression indexes. DDL can't take bind parameters, so the statements are
//! built from schema-validated field names (re-checked inside `field_expr`) and a `Uuid`.

use sqlx::{AssertSqlSafe, PgPool};
use tachyon_core::CollectionSchema;
use tachyon_core::query::{ValueKind, value_kind};
use uuid::Uuid;

use crate::DbError;
use crate::query::field_expr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Unique,
    Filter,
}

/// A stable name under Postgres's 63-byte limit. The comparison kind is part of the hash,
/// so retyping a field yields a new name and reconcile replaces the index.
pub fn index_name(collection_id: Uuid, field: &str, kind: IndexKind, value: ValueKind) -> String {
    let tag = match value {
        ValueKind::Text => b't',
        ValueKind::Number => b'n',
        ValueKind::Boolean => b'b',
        ValueKind::Date => b'd',
        ValueKind::List => b'l',
    };
    // FNV-1a: tiny, stable across releases, and good enough to avoid collisions here.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in collection_id
        .as_bytes()
        .iter()
        .chain(field.as_bytes())
        .chain([tag].iter())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let prefix = match kind {
        IndexKind::Unique => "uq",
        IndexKind::Filter => "ix",
    };
    format!("documents_{prefix}_{hash:016x}")
}

/// Makes a collection's indexes match its schema: creates missing ones, drops ones no field
/// asks for any more, and rebuilds any a failed `CREATE INDEX CONCURRENTLY` left invalid.
/// Runs outside a transaction because the concurrent DDL must.
pub async fn reconcile_indexes(
    pool: &PgPool,
    collection_id: Uuid,
    schema: &CollectionSchema,
) -> Result<(), DbError> {
    let scope = format!("WHERE collection_id = '{collection_id}' AND deleted_at IS NULL");
    let wanted: Vec<(String, String)> = schema
        .fields
        .iter()
        .filter_map(|f| {
            let kind = value_kind(&f.kind)?;
            let expr = field_expr(std::slice::from_ref(&f.name), kind);
            if f.is_unique() {
                let name = index_name(collection_id, &f.name, IndexKind::Unique, kind);
                let sql = format!(
                    "CREATE UNIQUE INDEX CONCURRENTLY {name} ON documents (({expr})) {scope}"
                );
                Some((name, sql))
            } else if f.filterable {
                let name = index_name(collection_id, &f.name, IndexKind::Filter, kind);
                let sql = format!(
                    "CREATE INDEX CONCURRENTLY {name} ON documents (({expr}), id DESC) {scope}"
                );
                Some((name, sql))
            } else {
                None
            }
        })
        .collect();

    let existing = sqlx::query!(
        r#"SELECT c.relname AS "name!", i.indisvalid AS "valid!"
           FROM pg_index i
           JOIN pg_class c ON c.oid = i.indexrelid
           WHERE i.indrelid = 'documents'::regclass
             AND pg_get_expr(i.indpred, i.indrelid) LIKE '%' || $1 || '%'"#,
        format!("collection_id = '{collection_id}'::uuid"),
    )
    .fetch_all(pool)
    .await?;

    for index in &existing {
        if !index.valid || !wanted.iter().any(|(name, _)| *name == index.name) {
            let quoted = index.name.replace('"', "\"\"");
            ddl(
                pool,
                format!("DROP INDEX CONCURRENTLY IF EXISTS \"{quoted}\""),
            )
            .await?;
        }
    }
    for (name, create) in wanted {
        if !existing.iter().any(|i| i.name == name && i.valid) {
            ddl(pool, create).await?;
        }
    }
    Ok(())
}

async fn ddl(pool: &PgPool, sql: String) -> Result<(), DbError> {
    sqlx::query(AssertSqlSafe(sql)).execute(pool).await?;
    Ok(())
}
