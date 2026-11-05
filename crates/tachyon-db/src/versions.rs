//! Past copies of documents.

use serde_json::{Map, Value};
use sqlx::PgConnection;
use sqlx::types::Json;
use uuid::Uuid;

use crate::collections::Collection;
use crate::{DEFAULT_TENANT, DbError, DocCopy};

/// Records `data` as a version of document `doc` of `collection`, `kind` saying which copy
/// it was, then drops the oldest past the collection's `maxVersions` (0 keeps every one).
/// A version a queued or running job holds, such as a scheduled publish's, is never
/// dropped. The version's collection is the document's own, read from its row.
pub async fn record_version(
    conn: &mut PgConnection,
    doc: Uuid,
    collection: &Collection,
    kind: DocCopy,
    data: &Map<String, Value>,
    by: Option<Uuid>,
) -> Result<Uuid, DbError> {
    let id = sqlx::query_scalar!(
        r#"INSERT INTO versions (id, tenant_id, collection_id, document_id, kind, data, created_by)
           SELECT $1, tenant_id, collection_id, id, $4, $5, $6 FROM documents
           WHERE tenant_id = $2 AND id = $3 AND collection_id = $7
           RETURNING id"#,
        Uuid::now_v7(),
        DEFAULT_TENANT,
        doc,
        kind.as_str(),
        Json(data) as _,
        by,
        collection.id,
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DbError::NotFound)?;
    let keep = collection
        .schema
        .versions
        .as_ref()
        .map_or(0, |v| v.max_versions);
    if keep > 0 {
        sqlx::query!(
            r#"DELETE FROM versions WHERE id IN (
                   SELECT id FROM versions
                   WHERE tenant_id = $1 AND document_id = $2
                   ORDER BY created_at DESC, id DESC OFFSET $3
               ) AND NOT EXISTS (
                   SELECT 1 FROM jobs j
                   WHERE j.tenant_id = $1 AND j.state IN ('queued', 'running')
                     AND j.payload->>'version' = versions.id::text)"#,
            DEFAULT_TENANT,
            doc,
            i64::from(keep),
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(id)
}
