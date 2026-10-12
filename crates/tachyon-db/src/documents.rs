//! Documents repository.
//!
//! M1 has no drafts: every write goes straight to `published`.

use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::PgPool;
use sqlx::types::Json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{DEFAULT_TENANT, DbError};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    pub id: Uuid,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    #[serde(flatten)]
    pub data: Json<Map<String, Value>>,
}

#[derive(Debug)]
pub struct Page {
    pub docs: Vec<Document>,
    /// Only counted when asked for, because counting a large collection is slow.
    pub total: Option<i64>,
}

pub async fn insert(
    pool: &PgPool,
    collection_id: Uuid,
    data: &Map<String, Value>,
) -> Result<Document, DbError> {
    Ok(sqlx::query_as!(
        Document,
        r#"INSERT INTO documents (id, tenant_id, collection_id, status, published, published_at)
           VALUES ($1, $2, $3, 'published', $4, now())
           RETURNING id, status, created_at, updated_at, published AS "data!: Json<Map<String, Value>>""#,
        Uuid::now_v7(),
        DEFAULT_TENANT,
        collection_id,
        Json(data) as _,
    )
    .fetch_one(pool)
    .await?)
}

pub async fn get(pool: &PgPool, collection_id: Uuid, id: Uuid) -> Result<Document, DbError> {
    sqlx::query_as!(
        Document,
        r#"SELECT id, status, created_at, updated_at, published AS "data!: Json<Map<String, Value>>"
           FROM documents
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NULL"#,
        DEFAULT_TENANT,
        collection_id,
        id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

pub async fn list(
    pool: &PgPool,
    collection_id: Uuid,
    limit: i64,
    offset: i64,
    count: bool,
) -> Result<Page, DbError> {
    let docs = sqlx::query_as!(
        Document,
        r#"SELECT id, status, created_at, updated_at, published AS "data!: Json<Map<String, Value>>"
           FROM documents
           WHERE tenant_id = $1 AND collection_id = $2 AND deleted_at IS NULL
           ORDER BY id DESC
           LIMIT $3 OFFSET $4"#,
        DEFAULT_TENANT,
        collection_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    let total = if count {
        Some(
            sqlx::query_scalar!(
                r#"SELECT count(*) AS "n!" FROM documents
                   WHERE tenant_id = $1 AND collection_id = $2 AND deleted_at IS NULL"#,
                DEFAULT_TENANT,
                collection_id,
            )
            .fetch_one(pool)
            .await?,
        )
    } else {
        None
    };
    Ok(Page { docs, total })
}

/// Locks the document, passes its current data to `apply`, and stores what `apply` returns.
pub async fn update<E: From<DbError>>(
    pool: &PgPool,
    collection_id: Uuid,
    id: Uuid,
    apply: impl FnOnce(&Map<String, Value>) -> Result<Map<String, Value>, E>,
) -> Result<Document, E> {
    let mut tx = pool.begin().await.map_err(DbError::from)?;
    let current = sqlx::query_scalar!(
        r#"SELECT published AS "data!: Json<Map<String, Value>>"
           FROM documents
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NULL
           FOR UPDATE"#,
        DEFAULT_TENANT,
        collection_id,
        id,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::from)?
    .ok_or(DbError::NotFound)?;
    let next = apply(&current.0)?;
    let doc = sqlx::query_as!(
        Document,
        r#"UPDATE documents SET published = $4, updated_at = now()
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3
           RETURNING id, status, created_at, updated_at, published AS "data!: Json<Map<String, Value>>""#,
        DEFAULT_TENANT,
        collection_id,
        id,
        Json(&next) as _,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(doc)
}

/// Soft delete. Restoring from Trash and emptying it arrive in M4.
pub async fn delete(pool: &PgPool, collection_id: Uuid, id: Uuid) -> Result<(), DbError> {
    let done = sqlx::query!(
        "UPDATE documents SET deleted_at = now()
         WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NULL",
        DEFAULT_TENANT,
        collection_id,
        id,
    )
    .execute(pool)
    .await?;
    if done.rows_affected() == 0 {
        Err(DbError::NotFound)
    } else {
        Ok(())
    }
}
