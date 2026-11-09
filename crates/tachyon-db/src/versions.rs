//! Past copies of documents.

use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
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

/// A past copy of a document.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Version {
    pub id: Uuid,
    /// The copy it was: `draft` or `published`.
    pub kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub created_by: Option<Uuid>,
    /// Left out of lists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Map<String, Value>>,
}

/// Up to `limit` versions of live document `doc`, newest first, after skipping `offset`,
/// without their data; with `drafts` false, only those of what was published.
pub async fn list(
    pool: &PgPool,
    doc: Uuid,
    drafts: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<Version>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT v.id, v.kind, v.created_at, v.created_by FROM versions v
           JOIN documents d ON d.id = v.document_id AND d.collection_id = v.collection_id
           WHERE v.tenant_id = $1 AND v.document_id = $2 AND d.deleted_at IS NULL
             AND ($5 OR v.kind = 'published')
           ORDER BY v.created_at DESC, v.id DESC
           LIMIT $3 OFFSET $4"#,
        DEFAULT_TENANT,
        doc,
        limit,
        offset,
        drafts,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Version {
            id: r.id,
            kind: r.kind,
            created_at: r.created_at,
            created_by: r.created_by,
            data: None,
        })
        .collect())
}

/// Version `id` of live document `doc`, with its data.
pub async fn get(pool: &PgPool, doc: Uuid, id: Uuid) -> Result<Version, DbError> {
    let r = sqlx::query!(
        r#"SELECT v.id, v.kind, v.created_at, v.created_by,
                  v.data AS "data!: Json<Map<String, Value>>"
           FROM versions v
           JOIN documents d ON d.id = v.document_id AND d.collection_id = v.collection_id
           WHERE v.tenant_id = $1 AND v.document_id = $2 AND v.id = $3
             AND d.deleted_at IS NULL"#,
        DEFAULT_TENANT,
        doc,
        id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)?;
    Ok(Version {
        id: r.id,
        kind: r.kind,
        created_at: r.created_at,
        created_by: r.created_by,
        data: Some(r.data.0),
    })
}
