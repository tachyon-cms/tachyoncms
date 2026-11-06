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

use crate::collections::Collection;
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
    /// When it went to the trash; only trash listings read it, so it's `None` elsewhere.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[sqlx(default)]
    pub deleted_at: Option<OffsetDateTime>,
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
                     CASE WHEN draft IS NOT NULL THEN published_at END AS published_at, deleted_at,
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
pub(crate) async fn check_schema(
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
                  CASE WHEN draft IS NOT NULL THEN published_at END AS published_at, deleted_at,
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
                deleted_at: None,
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
                     CASE WHEN draft IS NOT NULL THEN published_at END AS published_at, deleted_at,
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

/// Locks live document `id` of collection `collection_id` for a write inside `tx`, and
/// returns its `copy`: `NotFound` without that copy, `Forbidden` unless that copy matches
/// `guard` and, with an `owner`, they created it. The collection is locked first, as every
/// write locks it, and must still be at `schema_version`. Publishing locks the draft this
/// way.
pub async fn lock_in(
    tx: &mut PgConnection,
    collection_id: Uuid,
    schema_version: i32,
    id: Uuid,
    copy: DocCopy,
    guard: Option<&Filter>,
    owner: Option<Uuid>,
) -> Result<Map<String, Value>, DbError> {
    check_schema(&mut *tx, collection_id, schema_version).await?;
    let row = sqlx::query!(
        r#"SELECT CASE WHEN $4 THEN draft ELSE published END AS "data!: Json<Map<String, Value>>",
                  created_by
           FROM documents
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NULL
             AND CASE WHEN $4 THEN draft ELSE published END IS NOT NULL
           FOR NO KEY UPDATE"#,
        DEFAULT_TENANT,
        collection_id,
        id,
        copy == DocCopy::Draft,
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if owner.is_some_and(|o| row.created_by != Some(o)) {
        return Err(DbError::Forbidden);
    }
    check_guard(&mut *tx, id, copy, guard).await?;
    Ok(row.data.0)
}

/// Publishes document `id` of `c`, a collection with drafts, inside `tx`: `doc` (the
/// stored draft and its links when `None`) becomes the published copy, the status
/// `published` and `published_at` now, and a `published` version is recorded. The caller
/// has locked the row with `lock_in`, which also checked the schema, and validated what's
/// published. A unique clash is a `UniqueViolation`; a link to a document that's gone, a
/// `Validation` error.
pub async fn publish_in(
    tx: &mut PgConnection,
    c: &Collection,
    id: Uuid,
    doc: Option<&Prepared>,
    by: Option<Uuid>,
) -> Result<Document, DbError> {
    let published = sqlx::query_as!(
        Document,
        r#"UPDATE documents
           SET published = COALESCE($4::jsonb, draft), status = 'published',
               published_at = now(), updated_at = now(), updated_by = COALESCE($5, updated_by)
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NULL
             AND draft IS NOT NULL
           RETURNING id, status, created_at, updated_at, published_at, deleted_at,
                     published AS "data!: Json<Map<String, Value>>""#,
        DEFAULT_TENANT,
        c.id,
        id,
        doc.map(|d| Json(&d.data)) as _,
        by,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_unique)?
    .ok_or(DbError::NotFound)?;
    match doc {
        Some(doc) => sync_relations(&mut *tx, id, DocCopy::Published, &doc.relations).await?,
        None => {
            sqlx::query!(
                "DELETE FROM relations WHERE from_doc = $1 AND copy = 'published'",
                id
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!(
                "INSERT INTO relations (tenant_id, from_doc, copy, field_path, to_doc)
                 SELECT tenant_id, from_doc, 'published', field_path, to_doc FROM relations
                 WHERE from_doc = $1 AND copy = 'draft'",
                id
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    // Drafts imply versions, so a collection that publishes always keeps a history to add to.
    crate::versions::record_version(tx, id, c, DocCopy::Published, &published.data.0, by).await?;
    Ok(published)
}

/// Takes document `id` of `c` off the site inside `tx`: no published copy or links, status
/// `draft`, never published as far as reads tell. The draft stays as it is, and documents
/// that link here keep their links. The caller has locked the row (`lock_in`). Returns the
/// draft. One that isn't published is `Refused`, and nothing is written.
pub async fn unpublish_in(
    tx: &mut PgConnection,
    c: &Collection,
    id: Uuid,
    by: Option<Uuid>,
) -> Result<Document, DbError> {
    let published = sqlx::query_scalar!(
        r#"SELECT published IS NOT NULL AS "published!" FROM documents
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NULL
             AND draft IS NOT NULL"#,
        DEFAULT_TENANT,
        c.id,
        id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if !published {
        return Err(DbError::Refused(format!("`{id}` isn't published")));
    }
    let draft = sqlx::query_as!(
        Document,
        r#"UPDATE documents
           SET published = NULL, status = 'draft', published_at = NULL,
               updated_at = now(), updated_by = COALESCE($4, updated_by)
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NULL
             AND draft IS NOT NULL
           RETURNING id, status, created_at, updated_at, published_at, deleted_at,
                     draft AS "data!: Json<Map<String, Value>>""#,
        DEFAULT_TENANT,
        c.id,
        id,
        by,
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    sqlx::query!(
        "DELETE FROM relations WHERE from_doc = $1 AND copy = 'published'",
        id
    )
    .execute(&mut *tx)
    .await?;
    Ok(draft)
}

/// Soft delete, to the trash (`restore_in`, `purge_in`). Refused while other live
/// documents link here.
pub async fn delete(pool: &PgPool, collection_id: Uuid, id: Uuid) -> Result<(), DbError> {
    delete_as(pool, collection_id, id, None, None, DocCopy::Published).await
}

/// `delete`, as `actor` and under `guard` (see `update_as`), which judges `copy`. Deleting
/// a user also ends their sessions and keys, and is refused when they're the last Admin,
/// and any scheduled publish or unpublish of it is cancelled, all in the same transaction.
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
    // Nothing scheduled for it runs from the trash; the jobs' cascade only covers a purge.
    for kind in ["publish", "unpublish"] {
        crate::jobs::cancel(&mut tx, &format!("{kind}:{id}")).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Both stored copies of trashed document `id` of `collection_id`, the draft then the
/// published copy. The caller has locked it (`query::lock_trash_in`).
pub async fn trashed_in(
    conn: &mut PgConnection,
    collection_id: Uuid,
    id: Uuid,
) -> Result<(Option<Map<String, Value>>, Option<Map<String, Value>>), DbError> {
    let row = sqlx::query!(
        r#"SELECT draft AS "draft: Json<Map<String, Value>>",
                  published AS "published: Json<Map<String, Value>>"
           FROM documents
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NOT NULL"#,
        DEFAULT_TENANT,
        collection_id,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DbError::NotFound)?;
    Ok((row.draft.map(|d| d.0), row.published.map(|d| d.0)))
}

/// `errors` about `copy` of a document of `c`, saying which copy when `c` has two: a
/// restore checks both.
pub fn copy_errors(c: &Collection, copy: DocCopy, errors: Vec<FieldError>) -> Vec<FieldError> {
    if !c.schema.drafts() {
        return errors;
    }
    let which = match copy {
        DocCopy::Draft => "the draft",
        DocCopy::Published => "the published copy",
    };
    errors
        .into_iter()
        .map(|e| FieldError {
            message: format!("{which}: {}", e.message),
            ..e
        })
        .collect()
}

/// Takes trashed document `id` of `c` out of the trash inside `tx`: each of `prepared`,
/// validated against the current schema, replaces its copy and re-syncs that copy's links,
/// so a link to a document that's gone or in the trash is a `Validation` error at its path
/// (both copies' errors, each saying its copy). A unique clash with a live document is a
/// `UniqueViolation`. The caller has locked it (`query::lock_trash_in`) and passes every
/// copy it has. Returns the copy a write to `c` changes: the draft, with drafts.
pub async fn restore_in(
    tx: &mut PgConnection,
    c: &Collection,
    id: Uuid,
    prepared: &[(DocCopy, Prepared)],
    by: Option<Uuid>,
) -> Result<Document, DbError> {
    // Lock order: the document (by the caller), then every link target in id order, and
    // only then are its link rows replaced. A purge locks its targets FOR UPDATE, then
    // deletes the rows linking them; taking the targets before touching those rows means a
    // restore waits on the purge (and then finds the target gone) instead of deadlocking.
    let mut targets: Vec<Uuid> = prepared
        .iter()
        .flat_map(|(_, p)| p.relations.iter().map(|r| r.id))
        .collect();
    targets.sort_unstable();
    targets.dedup();
    if !targets.is_empty() {
        sqlx::query!(
            "SELECT id FROM documents WHERE tenant_id = $1 AND id = ANY($2) AND deleted_at IS NULL
             ORDER BY id FOR KEY SHARE",
            DEFAULT_TENANT,
            &targets,
        )
        .fetch_all(&mut *tx)
        .await?;
    }
    let data = |copy| {
        prepared
            .iter()
            .find(|(c, _)| *c == copy)
            .map(|(_, p)| Json(&p.data))
    };
    let doc = sqlx::query_as!(
        Document,
        r#"UPDATE documents
           SET published = COALESCE($4::jsonb, published), draft = COALESCE($5::jsonb, draft),
               deleted_at = NULL, updated_at = now(), updated_by = COALESCE($6, updated_by)
           WHERE tenant_id = $1 AND collection_id = $2 AND id = $3 AND deleted_at IS NOT NULL
           RETURNING id, status, created_at, updated_at,
                     CASE WHEN draft IS NOT NULL THEN published_at END AS published_at, deleted_at,
                     CASE WHEN $7 THEN draft ELSE published END AS "data!: Json<Map<String, Value>>""#,
        DEFAULT_TENANT,
        c.id,
        id,
        data(DocCopy::Published) as _,
        data(DocCopy::Draft) as _,
        by,
        c.schema.drafts(),
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_unique)?
    .ok_or(DbError::NotFound)?;
    let mut errors = Vec::new();
    for (copy, p) in prepared {
        match sync_relations(&mut *tx, id, *copy, &p.relations).await {
            Err(DbError::Validation(e)) => errors.extend(copy_errors(c, *copy, e)),
            other => other?,
        }
    }
    if !errors.is_empty() {
        return Err(DbError::Validation(errors));
    }
    Ok(doc)
}

/// Deletes the trashed documents of `collection_id` among `ids` for good inside `tx`,
/// with their versions and jobs, and returns how many went. Links to them from other
/// trashed documents go first; a restore of those then says the link's target is gone.
/// The caller has locked them (`query::lock_trash_in`).
pub async fn purge_in(
    tx: &mut PgConnection,
    collection_id: Uuid,
    ids: &[Uuid],
) -> Result<u64, DbError> {
    // Only trashed documents' links: a live one can't link here (`sync_relations` refuses
    // a trashed target), and if one somehow did, its link's foreign key stops the purge.
    sqlx::query!(
        "DELETE FROM relations r USING documents d, documents f
         WHERE r.to_doc = d.id AND d.tenant_id = $1 AND d.collection_id = $2
           AND d.id = ANY($3) AND d.deleted_at IS NOT NULL
           AND f.id = r.from_doc AND f.deleted_at IS NOT NULL",
        DEFAULT_TENANT,
        collection_id,
        ids,
    )
    .execute(&mut *tx)
    .await?;
    Ok(sqlx::query!(
        "DELETE FROM documents
         WHERE tenant_id = $1 AND collection_id = $2 AND id = ANY($3) AND deleted_at IS NOT NULL",
        DEFAULT_TENANT,
        collection_id,
        ids,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected())
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
