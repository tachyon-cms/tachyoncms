//! Collections repository.

use sqlx::PgPool;
use sqlx::types::Json;
use tachyon_core::CollectionSchema;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{DEFAULT_TENANT, DbError};

#[derive(Debug, Clone)]
pub struct Collection {
    pub id: Uuid,
    pub schema: Json<CollectionSchema>,
    pub schema_version: i32,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

pub async fn create(pool: &PgPool, schema: &CollectionSchema) -> Result<Collection, DbError> {
    sqlx::query_as!(
        Collection,
        r#"INSERT INTO collections (id, tenant_id, slug, kind, schema)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id, schema AS "schema: Json<CollectionSchema>", schema_version, created_at, updated_at"#,
        Uuid::now_v7(),
        DEFAULT_TENANT,
        schema.slug,
        schema.kind.as_str(),
        Json(schema) as _,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => {
            DbError::Conflict(format!("collection `{}`", schema.slug))
        }
        e => e.into(),
    })
}

pub async fn get_by_slug(pool: &PgPool, slug: &str) -> Result<Collection, DbError> {
    sqlx::query_as!(
        Collection,
        r#"SELECT id, schema AS "schema: Json<CollectionSchema>", schema_version, created_at, updated_at
           FROM collections WHERE tenant_id = $1 AND slug = $2"#,
        DEFAULT_TENANT,
        slug,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

pub async fn list(pool: &PgPool) -> Result<Vec<Collection>, DbError> {
    Ok(sqlx::query_as!(
        Collection,
        r#"SELECT id, schema AS "schema: Json<CollectionSchema>", schema_version, created_at, updated_at
           FROM collections WHERE tenant_id = $1 ORDER BY slug"#,
        DEFAULT_TENANT,
    )
    .fetch_all(pool)
    .await?)
}
