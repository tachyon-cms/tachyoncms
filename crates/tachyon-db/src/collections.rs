//! Collections repository.

use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use tachyon_core::{CollectionKind, CollectionSchema};
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
    let mut tx = pool.begin().await?;
    // R42's lock, held to commit like every schema save, so a shared block this one names
    // can't change or go before it's stored.
    crate::auth::lock_rule_roles(&mut tx).await?;
    crate::auth::check_rule_roles(&mut tx, schema).await?;
    // Those name others in turn, but each of those was checked when it was saved, and
    // can't go while named.
    let refs = schema.block_refs();
    let found = sqlx::query_scalar!(
        "SELECT slug FROM collections WHERE tenant_id = $1 AND kind = 'block' AND slug = ANY($2)",
        DEFAULT_TENANT,
        &refs,
    )
    .fetch_all(&mut *tx)
    .await?;
    if let Some(missing) = refs.iter().find(|r| !found.contains(r)) {
        return Err(DbError::Refused(format!(
            "blockRef `{missing}` doesn't match a shared block"
        )));
    }
    let created = insert(&mut tx, Uuid::now_v7(), schema).await?;
    // Delivered to every listening server when the transaction commits.
    sqlx::query!("SELECT pg_notify('tachyon_schema', $1)", schema.slug)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(created)
}

/// Stores a new schema as `id`. Its checks are the caller's.
pub(crate) async fn insert(
    conn: &mut PgConnection,
    id: Uuid,
    schema: &CollectionSchema,
) -> Result<Collection, DbError> {
    sqlx::query_as!(
        Collection,
        r#"INSERT INTO collections (id, tenant_id, slug, kind, schema)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id, schema AS "schema: Json<CollectionSchema>", schema_version, created_at, updated_at"#,
        id,
        DEFAULT_TENANT,
        schema.slug,
        schema.kind.as_str(),
        Json(schema) as _,
    )
    .fetch_one(conn)
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

/// Removes the shared block `slug`, refused while any schema names it. Naming it through
/// another shared block counts: that block names it directly, so it's the one reported.
pub async fn delete_block(pool: &PgPool, slug: &str) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    // Under R42's lock, no schema naming it can be saved in between.
    crate::auth::lock_rule_roles(&mut tx).await?;
    let all = sqlx::query_scalar!(
        r#"SELECT schema AS "schema: Json<CollectionSchema>" FROM collections
           WHERE tenant_id = $1 ORDER BY slug"#,
        DEFAULT_TENANT,
    )
    .fetch_all(&mut *tx)
    .await?;
    let block = all
        .iter()
        .find(|s| s.slug == slug)
        .ok_or(DbError::NotFound)?;
    if block.kind != CollectionKind::Block {
        return Err(DbError::Refused(format!(
            "`{slug}` isn't a shared block; collections and globals can't be deleted yet"
        )));
    }
    if let Some(user) = all
        .iter()
        .find(|s| s.block_refs().iter().any(|r| r == slug))
    {
        return Err(DbError::Refused(format!(
            "{} uses the shared block `{slug}`; remove it there first",
            user.slug
        )));
    }
    sqlx::query!(
        "DELETE FROM collections WHERE tenant_id = $1 AND slug = $2",
        DEFAULT_TENANT,
        slug
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("SELECT pg_notify('tachyon_schema', $1)", slug)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
