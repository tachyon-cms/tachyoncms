//! Documents repository.
//!
//! Reads and writes take the copy they work on: `published`, or the working `draft` of a
//! collection with drafts. Callers pick `Draft` only for such a collection.

use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use tachyon_core::FieldError;
use tachyon_core::query::Filter;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{DEFAULT_TENANT, DbError, map_unique};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    pub id: Uuid,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// When the published copy was last published, on collections with drafts; `None`
    /// while it never has been, and always on collections without drafts.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub published_at: Option<OffsetDateTime>,
    #[serde(flatten)]
    pub data: Json<Map<String, Value>>,
}

pub use tachyon_core::query::DocCopy;

/// A link to store alongside a document.
#[derive(Debug, Clone)]
pub struct Relation {
    pub path: String,
    /// Where validation names the link, for errors about it (`RelationRef::at`).
    pub at: String,
    pub collection_id: Uuid,
    pub id: Uuid,
}

/// A validated document ready to write, with the links it holds.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub data: Map<String, Value>,
    pub relations: Vec<Relation>,
}

impl Prepared {
    pub fn plain(data: Map<String, Value>) -> Self {
        Self {
            data,
            relations: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Reference {
    pub collection: String,
    pub id: Uuid,
    pub path: String,
    /// The copy holding the link.
    pub copy: DocCopy,
}

pub async fn insert(
    pool: &PgPool,
    collection_id: Uuid,
    schema_version: i32,
    id: Option<Uuid>,
    doc: &Prepared,
) -> Result<Document, DbError> {
    insert_as(pool, collection_id, schema_version, id, doc, None).await
}

/// Who is writing, and whether they may only touch documents they created.
#[derive(Debug, Clone, Copy)]
pub struct Actor {
    pub id: Uuid,
    pub own_only: bool,
}

/// `insert`, recording `actor` as the document's creator and last editor.
pub async fn insert_as(
    pool: &PgPool,
    collection_id: Uuid,
    schema_version: i32,
    id: Option<Uuid>,
    doc: &Prepared,
    actor: Option<Uuid>,
) -> Result<Document, DbError> {
    let mut tx = pool.begin().await?;
    let created = insert_in(
        &mut tx,
        collection_id,
        schema_version,
        id,
        DocCopy::Published,
        doc,
        actor,
    )
    .await?;
    tx.commit().await?;
    Ok(created)
}

/// `insert_as` into `copy`, inside the caller's transaction. A new draft is unpublished:
/// `published` stays NULL until it's published.
pub async fn insert_in(
    conn: &mut PgConnection,
    collection_id: Uuid,
    schema_version: i32,
    id: Option<Uuid>,
    copy: DocCopy,
    doc: &Prepared,
    actor: Option<Uuid>,
) -> Result<Document, DbError> {
    check_schema(&mut *conn, collection_id, schema_version).await?;
    let draft = copy == DocCopy::Draft;
    let created = sqlx::query_as!(
        Document,
        r#"INSERT INTO documents (id, tenant_id, collection_id, status, published, draft, published_at, created_by, updated_by)
           VALUES ($1, $2, $3, CASE WHEN $6 THEN 'draft' ELSE 'published' END,
                   CASE WHEN $6 THEN NULL ELSE $4::jsonb END, CASE WHEN $6 THEN $4::jsonb END,
                   CASE WHEN $6 THEN NULL ELSE now() END, $5, $5)
           RETURNING id, status, created_at, updated_at,
                     CASE WHEN draft IS NOT NULL THEN published_at END AS published_at,
                     CASE WHEN $6 THEN draft ELSE published END AS "data!: Json<Map<String, Value>>""#,
        id.unwrap_or_else(Uuid::now_v7),
        DEFAULT_TENANT,
        collection_id,
        Json(&doc.data) as _,
        actor,
        draft,
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(map_unique)?;
    sync_relations(&mut *conn, created.id, copy, &doc.relations).await?;
    Ok(created)
}

/// Locks the collection row for the rest of the transaction (shared with other writers,
/// exclusive with schema changes) and confirms nobody changed its fields meanwhile.
async fn check_schema(
    conn: &mut PgConnection,
    collection_id: Uuid,
    schema_version: i32,
) -> Result<(), DbError> {
    let current = sqlx::query_scalar!(
        "SELECT schema_version FROM collections WHERE id = $1 FOR KEY SHARE",
        collection_id,
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DbError::NotFound)?;
    if current == schema_version {
        Ok(())
    } else {
        Err(DbError::SchemaChanged)
    }
}

/// Live document `id`, as its `copy`. A published read never finds one that's unpublished.
pub async fn get(
    pool: &PgPool,
    collection_id: Uuid,
    id: Uuid,
    copy: DocCopy,
) -> Result<Document, DbError> {
    sqlx::query_as!(
        Document,
        r#"SELECT id, status, created_at, updated_at,
                  CASE WHEN draft IS NOT NULL THEN published_at END AS published_at,
                  CASE WHEN $4 THEN draft ELSE published END AS "data!: Json<Map<String, Value>>"
           FROM documents
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NULL
             AND CASE WHEN $4 THEN draft IS NOT NULL ELSE published IS NOT NULL END"#,
        DEFAULT_TENANT,
        collection_id,
        id,
        copy == DocCopy::Draft,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

/// Live published documents with these IDs, from any collection, paired with their
/// collection ID.
pub async fn get_many(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<(Uuid, Document)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT id, collection_id, status, created_at, updated_at,
                  CASE WHEN draft IS NOT NULL THEN published_at END AS published_at,
                  published AS "data!: Json<Map<String, Value>>"
           FROM documents
           WHERE tenant_id = $1 AND id = ANY($2) AND deleted_at IS NULL
             AND published IS NOT NULL"#,
        DEFAULT_TENANT,
        ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let doc = Document {
                id: r.id,
                status: r.status,
                created_at: r.created_at,
                updated_at: r.updated_at,
                published_at: r.published_at,
                data: r.data,
            };
            (r.collection_id, doc)
        })
        .collect())
}

/// Locks the document, passes its current data to `apply`, and stores what `apply` returns.
pub async fn update<E: From<DbError>>(
    pool: &PgPool,
    collection_id: Uuid,
    schema_version: i32,
    id: Uuid,
    apply: impl FnOnce(&Map<String, Value>) -> Result<Prepared, E>,
) -> Result<Document, E> {
    update_as(pool, collection_id, schema_version, id, None, None, apply).await
}

/// `update`, as `actor`: recorded as the last editor, and refused with `NotOwner` when they
/// may only change their own documents and this isn't one. With a `guard`, the stored
/// document must match it, checked on the locked row, or the update is `Forbidden`.
pub async fn update_as<E: From<DbError>>(
    pool: &PgPool,
    collection_id: Uuid,
    schema_version: i32,
    id: Uuid,
    actor: Option<Actor>,
    guard: Option<&Filter>,
    apply: impl FnOnce(&Map<String, Value>) -> Result<Prepared, E>,
) -> Result<Document, E> {
    let mut tx = pool.begin().await.map_err(DbError::from)?;
    let doc = update_in(
        &mut tx,
        collection_id,
        schema_version,
        id,
        DocCopy::Published,
        actor,
        guard,
        apply,
    )
    .await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(doc)
}

/// `update_as` of `copy`, inside the caller's transaction, so other changes can commit
/// with it. `apply` sees that copy, and `guard` judges it; a document without it is
/// `NotFound`.
#[allow(clippy::too_many_arguments)] // `update_as`'s, plus the transaction and the copy
pub async fn update_in<E: From<DbError>>(
    tx: &mut PgConnection,
    collection_id: Uuid,
    schema_version: i32,
    id: Uuid,
    copy: DocCopy,
    actor: Option<Actor>,
    guard: Option<&Filter>,
    apply: impl FnOnce(&Map<String, Value>) -> Result<Prepared, E>,
) -> Result<Document, E> {
    check_schema(&mut *tx, collection_id, schema_version).await?;
    let draft = copy == DocCopy::Draft;
    let current = sqlx::query!(
        r#"SELECT CASE WHEN $4 THEN draft ELSE published END AS "data!: Json<Map<String, Value>>",
                  created_by
           FROM documents
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NULL
             AND CASE WHEN $4 THEN draft ELSE published END IS NOT NULL
           FOR NO KEY UPDATE"#,
        DEFAULT_TENANT,
        collection_id,
        id,
        draft,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::from)?
    .ok_or(DbError::NotFound)?;
    if let Some(a) = actor
        && a.own_only
        && current.created_by != Some(a.id)
    {
        return Err(DbError::NotOwner.into());
    }
    check_guard(&mut *tx, id, copy, guard).await?;
    let next = apply(&current.data.0)?;
    let doc = sqlx::query_as!(
        Document,
        r#"UPDATE documents
           SET published = CASE WHEN $6 THEN published ELSE $4::jsonb END,
               draft = CASE WHEN $6 THEN $4::jsonb ELSE draft END,
               updated_at = now(), updated_by = COALESCE($5, updated_by)
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3
           RETURNING id, status, created_at, updated_at,
                     CASE WHEN draft IS NOT NULL THEN published_at END AS published_at,
                     CASE WHEN $6 THEN draft ELSE published END AS "data!: Json<Map<String, Value>>""#,
        DEFAULT_TENANT,
        collection_id,
        id,
        Json(&next.data) as _,
        actor.map(|a| a.id),
        draft,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(map_unique)?;
    sync_relations(&mut *tx, id, copy, &next.relations).await?;
    Ok(doc)
}

/// Soft delete. Refused while other live documents link here. Restoring from Trash arrives in M4.
pub async fn delete(pool: &PgPool, collection_id: Uuid, id: Uuid) -> Result<(), DbError> {
    delete_as(pool, collection_id, id, None, None, DocCopy::Published).await
}

/// `delete`, as `actor` and under `guard` (see `update_as`), which judges `copy`. Deleting
/// a user also ends their sessions and keys, and is refused when they're the last Admin,
/// all in the same transaction.
pub async fn delete_as(
    pool: &PgPool,
    collection_id: Uuid,
    id: Uuid,
    actor: Option<Actor>,
    guard: Option<&Filter>,
    copy: DocCopy,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    // Lock first, so a concurrent write can't add a link between the check and the delete.
    let created_by = sqlx::query_scalar!(
        r#"SELECT created_by FROM documents
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NULL
           FOR UPDATE"#,
        DEFAULT_TENANT,
        collection_id,
        id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if let Some(a) = actor
        && a.own_only
        && created_by != Some(a.id)
    {
        return Err(DbError::NotOwner);
    }
    check_guard(&mut tx, id, copy, guard).await?;
    // A link from either copy counts: a draft may be published later.
    let linked = sqlx::query_scalar!(
        r#"SELECT count(DISTINCT r.from_doc) AS "n!" FROM relations r
           JOIN documents d ON d.id = r.from_doc
           WHERE r.to_doc = $1 AND r.from_doc <> $1 AND d.deleted_at IS NULL"#,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if linked > 0 {
        return Err(DbError::Referenced(linked));
    }
    crate::auth::before_removing(&mut tx, id).await?;
    sqlx::query!(
        "UPDATE documents SET deleted_at = now(), updated_by = COALESCE($2, updated_by) WHERE id = $1",
        id,
        actor.map(|a| a.id),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Refuses the write unless the locked document's `copy` matches `guard`.
async fn check_guard(
    conn: &mut PgConnection,
    id: Uuid,
    copy: DocCopy,
    guard: Option<&Filter>,
) -> Result<(), DbError> {
    match guard {
        Some(f) if !crate::query::matches(conn, id, copy, f).await? => Err(DbError::Forbidden),
        _ => Ok(()),
    }
}

/// Live documents that link to `id`, once for each of `copies` holding the link. A rich
/// text link's index in `path` (`layout.0.intro.1`) counts links the caller can't see too:
/// a change that hides their bare IDs must renumber it.
pub async fn references(
    pool: &PgPool,
    id: Uuid,
    copies: &[DocCopy],
) -> Result<Vec<Reference>, DbError> {
    let copies: Vec<&str> = copies.iter().map(|c| c.as_str()).collect();
    let rows = sqlx::query!(
        r#"SELECT c.slug AS collection, d.id, r.field_path AS path, r.copy
           FROM relations r
           JOIN documents d ON d.id = r.from_doc
           JOIN collections c ON c.id = d.collection_id
           WHERE r.to_doc = $1 AND r.from_doc <> $1 AND d.deleted_at IS NULL
             AND r.copy = ANY($2::text[])
           ORDER BY d.id DESC, r.field_path, r.copy"#,
        id,
        &copies as &[&str],
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(Reference {
                collection: r.collection,
                id: r.id,
                path: r.path,
                copy: DocCopy::parse(&r.copy).ok_or_else(|| {
                    DbError::Sqlx(sqlx::Error::Decode(
                        format!("unknown document copy `{}`", r.copy).into(),
                    ))
                })?,
            })
        })
        .collect()
}

/// What a link to no document (in its collection) is told.
pub const MISSING_LINK: &str = "doesn't match any document in the related collection";

/// Checks every link points at a live document in the expected collection, then replaces
/// the document's rows in `relations` for `copy`. Targets are key-share locked: that blocks
/// a concurrent delete (which takes FOR UPDATE) but not an ordinary save (FOR NO KEY
/// UPDATE), so two documents linking to each other can be saved at the same time without
/// deadlock.
pub(crate) async fn sync_relations(
    conn: &mut PgConnection,
    from_doc: Uuid,
    copy: DocCopy,
    relations: &[Relation],
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM relations WHERE from_doc = $1 AND copy = $2",
        from_doc,
        copy.as_str(),
    )
    .execute(&mut *conn)
    .await?;
    if relations.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = relations.iter().map(|r| r.id).collect();
    let collections: Vec<Uuid> = relations.iter().map(|r| r.collection_id).collect();
    let found: Vec<(Uuid, Uuid)> = sqlx::query!(
        r#"SELECT d.id, d.collection_id FROM documents d
           JOIN unnest($1::uuid[], $2::uuid[]) AS t(id, collection_id)
             ON d.id = t.id AND d.collection_id = t.collection_id
           WHERE d.tenant_id = $3 AND d.deleted_at IS NULL
           FOR KEY SHARE OF d"#,
        &ids,
        &collections,
        DEFAULT_TENANT,
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| (row.id, row.collection_id))
    .collect();
    // Check the pair, not just the ID: a correct link elsewhere in the document must not
    // vouch for a link that names the wrong collection for the same ID.
    // NOTE: linear scan per link; fine for the few dozen links a document holds.
    let missing: Vec<FieldError> = relations
        .iter()
        .filter(|r| !found.contains(&(r.id, r.collection_id)))
        .map(|r| FieldError {
            path: r.at.clone(),
            message: MISSING_LINK.into(),
        })
        .collect();
    if !missing.is_empty() {
        return Err(DbError::Validation(missing));
    }
    let paths: Vec<String> = relations.iter().map(|r| r.path.clone()).collect();
    sqlx::query!(
        "INSERT INTO relations (tenant_id, from_doc, copy, field_path, to_doc)
         SELECT $1, $2, $3, path, id FROM unnest($4::text[], $5::uuid[]) AS t(path, id)",
        DEFAULT_TENANT,
        from_doc,
        copy.as_str(),
        &paths,
        &ids,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}
