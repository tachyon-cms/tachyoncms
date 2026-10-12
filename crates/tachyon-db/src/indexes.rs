//! Per-collection expression indexes. DDL can't take bind parameters, so the statements are
//! built from schema-validated field names (re-checked inside `field_expr`) and a `Uuid`.

use sqlx::types::Json;
use sqlx::{AssertSqlSafe, PgConnection, PgPool};
use tachyon_core::query::{ValueKind, value_kind};
use tachyon_core::{CollectionSchema, data_fields};
use uuid::Uuid;

use crate::DbError;
use crate::query::field_expr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Unique,
    /// Filters, and sorts lowest first.
    Filter,
    /// Sorts highest first. One B-tree can't keep missing values last in both directions.
    FilterDesc,
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
        ValueKind::LinkList => b'k',
        ValueKind::Point => b'p',
    };
    // FNV-1a: tiny, stable across releases, and good enough to avoid collisions here.
    // Ascending names predate the descending ones, so only descending adds a byte.
    let direction: &[u8] = if kind == IndexKind::FilterDesc {
        b"-"
    } else {
        b""
    };
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in collection_id
        .as_bytes()
        .iter()
        .chain(field.as_bytes())
        .chain([tag].iter())
        .chain(direction)
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let prefix = match kind {
        IndexKind::Unique => "uq",
        IndexKind::Filter | IndexKind::FilterDesc => "ix",
    };
    format!("documents_{prefix}_{hash:016x}")
}

/// Makes a collection's indexes match its schema: creates missing ones, drops ones no field
/// asks for any more, and rebuilds any a failed `CREATE INDEX CONCURRENTLY` left invalid.
/// Runs outside a transaction because the concurrent DDL must.
/// An index a collection's schema asks for.
pub(crate) struct Wanted {
    pub name: String,
    pub field: String,
    pub kind: IndexKind,
    pub unique: bool,
    /// Everything after `CREATE [UNIQUE] INDEX [CONCURRENTLY] <name> `.
    pub definition: String,
}

pub(crate) fn wanted_indexes(collection_id: Uuid, schema: &CollectionSchema) -> Vec<Wanted> {
    let scope = format!("WHERE collection_id = '{collection_id}' AND deleted_at IS NULL");
    let mut out = Vec::new();
    for f in data_fields(&schema.fields) {
        let Some(value) = value_kind(&f.kind) else {
            continue;
        };
        let expr = field_expr(std::slice::from_ref(&f.name), value);
        let mut want = |kind: IndexKind, definition: String| {
            out.push(Wanted {
                name: index_name(collection_id, &f.name, kind, value),
                field: f.name.clone(),
                kind,
                unique: kind == IndexKind::Unique,
                definition,
            });
        };
        if f.is_unique() {
            want(
                IndexKind::Unique,
                format!("ON documents (({expr})) {scope}"),
            );
        }
        // Both match `query::order_by`: missing values last, ties newest first.
        if f.filterable {
            want(
                IndexKind::Filter,
                format!("ON documents (({expr}), id DESC) {scope}"),
            );
            want(
                IndexKind::FilterDesc,
                format!("ON documents (({expr}) DESC NULLS LAST, id DESC) {scope}"),
            );
        }
    }
    out
}

impl Wanted {
    /// How a person reads it in a preview: `title (unique)`.
    pub fn label(&self) -> String {
        let kind = match self.kind {
            IndexKind::Unique => "unique",
            IndexKind::Filter => "filter",
            IndexKind::FilterDesc => "filter, highest first",
        };
        format!("{} ({kind})", self.field)
    }
}

/// Makes a collection's indexes match its schema: creates missing ones, drops ones no field
/// asks for any more, and rebuilds any a failed `CREATE INDEX CONCURRENTLY` left invalid.
/// Only touches indexes it names itself. A per-collection advisory lock stops two servers
/// reconciling at once, and the schema is read inside that lock so the newest one wins.
pub async fn reconcile_indexes(pool: &PgPool, collection_id: Uuid) -> Result<(), DbError> {
    let key = i64::from_le_bytes(collection_id.as_bytes()[..8].try_into().expect("8 bytes"));
    let mut conn = pool.acquire().await?;
    sqlx::query!("SELECT pg_advisory_lock($1)", key)
        .fetch_one(&mut *conn)
        .await?;
    let result = reconcile_locked(&mut conn, collection_id).await;
    sqlx::query!("SELECT pg_advisory_unlock($1)", key)
        .fetch_one(&mut *conn)
        .await?;
    result
}

async fn reconcile_locked(conn: &mut PgConnection, collection_id: Uuid) -> Result<(), DbError> {
    let schema = sqlx::query_scalar!(
        r#"SELECT schema AS "schema: Json<CollectionSchema>" FROM collections WHERE id = $1"#,
        collection_id,
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DbError::NotFound)?
    .0;
    let wanted = wanted_indexes(collection_id, &schema);
    let existing = existing_indexes(&mut *conn, collection_id).await?;

    for index in &existing {
        if !index.valid || !wanted.iter().any(|w| w.name == index.name) {
            let quoted = index.name.replace('"', "\"\"");
            ddl(
                &mut *conn,
                format!("DROP INDEX CONCURRENTLY IF EXISTS \"{quoted}\""),
            )
            .await?;
        }
    }
    for w in wanted {
        if !existing.iter().any(|i| i.name == w.name && i.valid) {
            let unique = if w.unique { "UNIQUE " } else { "" };
            ddl(
                &mut *conn,
                format!(
                    "CREATE {unique}INDEX CONCURRENTLY {} {}",
                    w.name, w.definition
                ),
            )
            .await?;
        }
    }
    Ok(())
}

pub(crate) struct Existing {
    pub name: String,
    pub valid: bool,
}

/// Indexes this module created for the collection (by name prefix and predicate).
pub(crate) async fn existing_indexes(
    conn: &mut PgConnection,
    collection_id: Uuid,
) -> Result<Vec<Existing>, DbError> {
    Ok(sqlx::query_as!(
        Existing,
        r#"SELECT c.relname AS "name!", i.indisvalid AS "valid!"
           FROM pg_index i
           JOIN pg_class c ON c.oid = i.indexrelid
           WHERE i.indrelid = 'documents'::regclass
             AND left(c.relname, 13) IN ('documents_uq_', 'documents_ix_')
             AND pg_get_expr(i.indpred, i.indrelid) LIKE '%' || $1 || '%'"#,
        format!("collection_id = '{collection_id}'::uuid"),
    )
    .fetch_all(&mut *conn)
    .await?)
}

pub(crate) async fn ddl(conn: &mut PgConnection, sql: String) -> Result<(), DbError> {
    sqlx::query(AssertSqlSafe(sql)).execute(&mut *conn).await?;
    Ok(())
}
