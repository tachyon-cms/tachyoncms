//! Stored files and their lifetime.
//!
//! An upload first `reserve`s a `pending` row, with a `file_cleanup` job due after
//! `PENDING_GRACE` that removes it if nothing ever takes it; the document write that takes
//! it makes it `live` (`go_live_in`) and cancels that job. A file stays live while any of
//! three references points at it: `documents.published_file`, `documents.draft_file` or
//! `versions.file_id`, trashed documents included. Each write that drops a reference passes
//! the old file to `release_in`, which turns it `gone` and queues its cleanup once nothing
//! points at it.

use std::time::Duration;

use serde_json::{Map, Value, json};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::jobs::{self, NewJob};
use crate::{DEFAULT_TENANT, DbError, DocCopy};

/// The job that deletes a file from storage. Its payload is `{"file": id}`, or
/// `{"keys": [..], "storage": name}` for objects no row records; its dedupe key
/// `file_cleanup:<id>`.
pub const CLEANUP: &str = "file_cleanup";
/// Storage can be down for a while; a cleanup keeps trying.
pub const CLEANUP_ATTEMPTS: i32 = 20;
/// How long a reserved file waits for a document to take it before it's cleaned up.
pub const PENDING_GRACE: Duration = Duration::from_secs(3600);

/// A stored original, with its sizes.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FileRow {
    pub id: Uuid,
    pub collection_id: Uuid,
    pub storage: String,
    pub key: String,
    pub filename: String,
    pub mime_type: String,
    pub filesize: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub focal_x: f32,
    pub focal_y: f32,
    pub blurhash: Option<String>,
    pub sizes: Json<Map<String, Value>>,
    /// Every name it's served under: the original's, then each size's.
    pub names: Vec<String>,
    /// `pending`, `live` or `gone`.
    pub state: String,
    pub created_at: OffsetDateTime,
    /// Who may claim it while it's pending, for an upload straight to the bucket.
    pub issued_to: Option<Uuid>,
    pub issued_key: Option<Uuid>,
}

/// A file to `reserve`.
pub struct NewFile<'a> {
    pub collection_id: Uuid,
    pub storage: &'a str,
    /// Sanitised; numbered on a clash.
    pub filename: &'a str,
    /// The `filenameCompoundIndex` values as JSON, or `""`.
    pub scope: &'a str,
    pub mime_type: &'a str,
    pub filesize: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub focal: (f32, f32),
    pub blurhash: Option<&'a str>,
    /// The user, and their API key, a client upload is issued to.
    pub issued_to: Option<(Uuid, Option<Uuid>)>,
    /// The document being written; its own files don't count as clashes.
    pub document: Option<Uuid>,
}

/// The cleanup of file `id`, due at `run_at`.
fn cleanup(id: Uuid, run_at: OffsetDateTime) -> NewJob<'static> {
    NewJob {
        kind: CLEANUP,
        document_id: None,
        collection_id: None,
        payload: json!({ "file": id }),
        run_at,
        dedupe_key: Some(format!("{CLEANUP}:{id}")),
        created_by: None,
        key_id: None,
        max_attempts: Some(CLEANUP_ATTEMPTS),
    }
}

/// Records a `pending` file in its own transaction, under the first free name: `a.png`,
/// then `a-1.png`, `a-2.png`… A name is taken by the current file of another live document
/// (either copy) or by another pending file, in the same collection and `scope`; a version's
/// file and `f.document`'s own don't count, so an edit or a replacement keeps its name. Its
/// cleanup is queued for `PENDING_GRACE` from now. `key_of` makes the storage key from the
/// new ID and the name; its error is `Refused`, and nothing is kept.
pub async fn reserve(
    pool: &PgPool,
    f: &NewFile<'_>,
    key_of: impl Fn(Uuid, &str) -> Result<String, String>,
) -> Result<FileRow, DbError> {
    let mut tx = pool.begin().await?;
    // One numbering at a time per collection and scope, so two uploads can't pick one name.
    sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        format!("files:{}:{}", f.collection_id, f.scope),
    )
    .execute(&mut *tx)
    .await?;
    let mut filename = f.filename.to_owned();
    for n in 1.. {
        let taken = sqlx::query_scalar!(
            r#"SELECT EXISTS (
                   SELECT 1 FROM files fl
                   WHERE fl.tenant_id = $1 AND fl.collection_id = $2 AND fl.scope = $3
                     AND fl.filename = $5
                     AND (fl.state = 'pending' OR EXISTS (
                           SELECT 1 FROM documents d
                           WHERE d.tenant_id = $1 AND d.collection_id = $2
                             AND d.deleted_at IS NULL AND d.id IS DISTINCT FROM $4
                             AND (d.published_file = fl.id OR d.draft_file = fl.id)))
               ) AS "taken!""#,
            DEFAULT_TENANT,
            f.collection_id,
            f.scope,
            f.document,
            filename,
        )
        .fetch_one(&mut *tx)
        .await?;
        if !taken {
            break;
        }
        filename = tachyon_core::uploads::numbered(f.filename, n);
    }
    let id = Uuid::now_v7();
    let key = key_of(id, &filename).map_err(DbError::Refused)?;
    let row = sqlx::query_as!(
        FileRow,
        r#"INSERT INTO files (id, tenant_id, collection_id, storage, key, filename, scope,
                              mime_type, filesize, width, height, focal_x, focal_y, blurhash,
                              names, state, issued_to, issued_key)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                   ARRAY[$6], 'pending', $15, $16)
           RETURNING id, collection_id, storage, key, filename, mime_type, filesize, width,
                     height, focal_x, focal_y, blurhash,
                     sizes AS "sizes: Json<Map<String, Value>>", names, state, created_at,
                     issued_to, issued_key"#,
        id,
        DEFAULT_TENANT,
        f.collection_id,
        f.storage,
        key,
        filename,
        f.scope,
        f.mime_type,
        f.filesize,
        f.width,
        f.height,
        f.focal.0,
        f.focal.1,
        f.blurhash,
        f.issued_to.map(|(user, _)| user),
        f.issued_to.and_then(|(_, key)| key),
    )
    .fetch_one(&mut *tx)
    .await?;
    let grace = time::Duration::try_from(PENDING_GRACE).expect("an hour fits");
    jobs::enqueue(&mut tx, &cleanup(id, OffsetDateTime::now_utc() + grace)).await?;
    tx.commit().await?;
    Ok(row)
}

/// Marks file `id` live, as a document takes it, and cancels a pending file's cleanup. A
/// file that's already gone can't be taken again: `Refused`.
pub async fn go_live_in(tx: &mut PgConnection, id: Uuid) -> Result<(), DbError> {
    // The row lock waits out a `release_in` deciding on it, so this sees what it decided.
    let was = sqlx::query_scalar!(
        "SELECT state FROM files WHERE tenant_id = $1 AND id = $2 FOR NO KEY UPDATE",
        DEFAULT_TENANT,
        id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .filter(|s| s != "gone")
    .ok_or_else(|| DbError::Refused(format!("the file `{id}` was deleted meanwhile")))?;
    sqlx::query!("UPDATE files SET state = 'live' WHERE id = $1", id)
        .execute(&mut *tx)
        .await?;
    if was == "pending" {
        jobs::cancel(tx, &format!("{CLEANUP}:{id}")).await?;
    }
    Ok(())
}

/// Turns each of `candidates` that nothing points at any more `gone`, queuing its cleanup
/// in the caller's transaction, so the job only exists once the change dropping the last
/// reference commits. Returns those released.
pub async fn release_in(tx: &mut PgConnection, candidates: &[Uuid]) -> Result<Vec<Uuid>, DbError> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    // Locked first, in its own statement: a write adding a reference holds the row (its
    // foreign key check) until it commits, and the check below then sees it.
    sqlx::query!(
        "SELECT id FROM files WHERE tenant_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE",
        DEFAULT_TENANT,
        candidates,
    )
    .fetch_all(&mut *tx)
    .await?;
    let gone = sqlx::query_scalar!(
        r#"UPDATE files f SET state = 'gone', released_at = now()
           WHERE f.tenant_id = $1 AND f.id = ANY($2) AND f.state = 'live'
             AND NOT EXISTS (SELECT 1 FROM documents d
                             WHERE d.published_file = f.id OR d.draft_file = f.id)
             AND NOT EXISTS (SELECT 1 FROM versions v WHERE v.file_id = f.id)
           RETURNING f.id"#,
        DEFAULT_TENANT,
        candidates,
    )
    .fetch_all(&mut *tx)
    .await?;
    let now = OffsetDateTime::now_utc();
    for id in &gone {
        jobs::enqueue(tx, &cleanup(*id, now)).await?;
    }
    Ok(gone)
}

/// File `id`, whatever its state.
pub async fn get(pool: &PgPool, id: Uuid) -> Result<FileRow, DbError> {
    sqlx::query_as!(
        FileRow,
        r#"SELECT id, collection_id, storage, key, filename, mime_type, filesize, width,
                  height, focal_x, focal_y, blurhash,
                  sizes AS "sizes: Json<Map<String, Value>>", names, state, created_at,
                  issued_to, issued_key
           FROM files WHERE tenant_id = $1 AND id = $2"#,
        DEFAULT_TENANT,
        id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

/// Live files of `collection_id` served under `name`, the original's or a size's. More than
/// one when scopes share a name; `holders` tells them apart.
pub async fn by_name(
    pool: &PgPool,
    collection_id: Uuid,
    name: &str,
) -> Result<Vec<FileRow>, DbError> {
    Ok(sqlx::query_as!(
        FileRow,
        r#"SELECT id, collection_id, storage, key, filename, mime_type, filesize, width,
                  height, focal_x, focal_y, blurhash,
                  sizes AS "sizes: Json<Map<String, Value>>", names, state, created_at,
                  issued_to, issued_key
           FROM files
           WHERE tenant_id = $1 AND collection_id = $2 AND names @> ARRAY[$3]
             AND state = 'live'
           ORDER BY id"#,
        DEFAULT_TENANT,
        collection_id,
        name,
    )
    .fetch_all(pool)
    .await?)
}

/// Live documents whose copy holds `file` as its current file, once per copy.
pub async fn holders(pool: &PgPool, file: Uuid) -> Result<Vec<(Uuid, DocCopy)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT id, published_file IS NOT DISTINCT FROM $2 AS "published!",
                  draft_file IS NOT DISTINCT FROM $2 AS "draft!"
           FROM documents
           WHERE tenant_id = $1 AND deleted_at IS NULL
             AND (published_file = $2 OR draft_file = $2)
           ORDER BY id"#,
        DEFAULT_TENANT,
        file,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .flat_map(|r| {
            [
                r.published.then_some((r.id, DocCopy::Published)),
                r.draft.then_some((r.id, DocCopy::Draft)),
            ]
        })
        .flatten()
        .collect())
}

/// The pending file of `collection_id` named `filename` that was issued to `user` with
/// `key` (none for a session), locked for the document write that takes it.
pub async fn claim_pending(
    tx: &mut PgConnection,
    collection_id: Uuid,
    filename: &str,
    user: Uuid,
    key: Option<Uuid>,
) -> Result<Option<FileRow>, DbError> {
    Ok(sqlx::query_as!(
        FileRow,
        r#"SELECT id, collection_id, storage, key, filename, mime_type, filesize, width,
                  height, focal_x, focal_y, blurhash,
                  sizes AS "sizes: Json<Map<String, Value>>", names, state, created_at,
                  issued_to, issued_key
           FROM files
           WHERE tenant_id = $1 AND collection_id = $2 AND filename = $3 AND state = 'pending'
             AND issued_to = $4 AND issued_key IS NOT DISTINCT FROM $5
           ORDER BY id
           LIMIT 1
           FOR UPDATE"#,
        DEFAULT_TENANT,
        collection_id,
        filename,
        user,
        key,
    )
    .fetch_optional(&mut *tx)
    .await?)
}
