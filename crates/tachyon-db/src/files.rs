//! Stored files and their lifetime.
//!
//! An upload first `reserve`s a `pending` row, with a `file_cleanup` job due after
//! `PENDING_GRACE` that removes it if nothing ever takes it; the document write that takes
//! it makes it `live` (`go_live_in`) and cancels that job. A file stays live while any of
//! three references points at it: `documents.published_file`, `documents.draft_file` or
//! `versions.file_id`, trashed documents included. Each write that drops a reference passes
//! the old file to `release_in`, which turns it `gone` and queues its cleanup once nothing
//! points at it.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::{Map, Value, json};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use tachyon_core::uploads::{numbered, numbered_bounds};
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

/// The first free name for `filename` in `collection_id` and `scope`, inside `conn`'s
/// transaction: `a.png`, then `a-1.png`, `a-2.png`… A name is taken by the current file of
/// another live document (either copy) or by a pending file not uploaded for `document`. A
/// version's file and `document`'s own don't count, so an edit or a replacement keeps its
/// name. Names in `also` count as taken too. Holds the numbering lock of the collection and
/// scope until the transaction ends.
pub(crate) async fn free_name(
    conn: &mut PgConnection,
    collection_id: Uuid,
    scope: &str,
    filename: &str,
    document: Option<Uuid>,
    also: &HashSet<String>,
) -> Result<String, DbError> {
    // One numbering at a time per collection and scope, so two uploads can't pick one name.
    sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        format!("files:{collection_id}:{scope}"),
    )
    .execute(&mut *conn)
    .await?;
    // Every name the series could reach, in one query; the first free one is picked here.
    let (head, tail) = numbered_bounds(filename);
    let mut taken: HashSet<String> = sqlx::query_scalar!(
        r#"SELECT fl.filename FROM files fl
           WHERE fl.tenant_id = $1 AND fl.collection_id = $2 AND fl.scope = $3
             AND (fl.filename = $5
                  OR (starts_with(fl.filename, $6) AND right(fl.filename, length($7)) = $7))
             AND ((fl.state = 'pending' AND (fl.document_id IS DISTINCT FROM $4 OR $4 IS NULL))
                  OR EXISTS (
                      SELECT 1 FROM documents d
                      WHERE d.tenant_id = $1 AND d.collection_id = $2
                        AND d.deleted_at IS NULL AND d.id IS DISTINCT FROM $4
                        AND (d.published_file = fl.id OR d.draft_file = fl.id)))"#,
        DEFAULT_TENANT,
        collection_id,
        scope,
        document,
        filename,
        head,
        tail,
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    taken.extend(also.iter().cloned());
    if !taken.contains(filename) {
        return Ok(filename.to_owned());
    }
    // At most `taken.len()` numbers are taken, so this ends within that many tries.
    Ok((1..)
        .map(|n| numbered(filename, n))
        .find(|name| !taken.contains(name))
        .expect("more numbers than names taken"))
}

/// Records a `pending` file in its own transaction, under the first free name (see
/// `free_name`). Its cleanup is queued for `PENDING_GRACE` from now. `key_of` makes the
/// storage key from the new ID and the name; its error is `Refused`, and nothing is kept.
pub async fn reserve(
    pool: &PgPool,
    f: &NewFile<'_>,
    key_of: impl Fn(Uuid, &str) -> Result<String, String>,
) -> Result<FileRow, DbError> {
    let mut tx = pool.begin().await?;
    let filename = free_name(
        &mut tx,
        f.collection_id,
        f.scope,
        f.filename,
        f.document,
        &HashSet::new(),
    )
    .await?;
    let id = Uuid::now_v7();
    let key = key_of(id, &filename).map_err(DbError::Refused)?;
    let row = sqlx::query_as!(
        FileRow,
        r#"INSERT INTO files (id, tenant_id, collection_id, storage, key, filename, scope,
                              mime_type, filesize, width, height, focal_x, focal_y, blurhash,
                              names, state, issued_to, issued_key, document_id)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                   ARRAY[$6], 'pending', $15, $16, $17)
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
        f.document,
    )
    .fetch_one(&mut *tx)
    .await?;
    let grace = time::Duration::try_from(PENDING_GRACE).expect("an hour fits");
    jobs::enqueue(&mut tx, &cleanup(id, OffsetDateTime::now_utc() + grace)).await?;
    tx.commit().await?;
    Ok(row)
}

/// Locks `ids` (`None`s skipped) for a write that moves a reference between them, in ID
/// order, so two writes trading files wait for each other instead of deadlocking.
pub(crate) async fn lock_in(tx: &mut PgConnection, ids: &[Option<Uuid>]) -> Result<(), DbError> {
    let ids: Vec<Uuid> = ids.iter().flatten().copied().collect();
    if !ids.is_empty() {
        sqlx::query!(
            "SELECT id FROM files WHERE tenant_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE",
            DEFAULT_TENANT,
            &ids,
        )
        .fetch_all(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Marks file `id` of `collection_id` live, as a document of that collection takes it, and
/// cancels a pending file's cleanup. A file that's gone, or another collection's, can't be
/// taken: `Refused`.
pub async fn go_live_in(
    tx: &mut PgConnection,
    collection_id: Uuid,
    id: Uuid,
) -> Result<(), DbError> {
    // The row lock waits out a `release_in` deciding on it, so this sees what it decided.
    let row = sqlx::query!(
        "SELECT state, collection_id FROM files WHERE tenant_id = $1 AND id = $2 FOR NO KEY UPDATE",
        DEFAULT_TENANT,
        id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .filter(|r| r.state != "gone")
    .ok_or_else(|| DbError::Refused(format!("the file `{id}` was deleted meanwhile")))?;
    if row.collection_id != collection_id {
        return Err(DbError::Refused(format!(
            "the file `{id}` belongs to another collection"
        )));
    }
    sqlx::query!(
        "UPDATE files SET state = 'live' WHERE tenant_id = $1 AND id = $2",
        DEFAULT_TENANT,
        id,
    )
    .execute(&mut *tx)
    .await?;
    if row.state == "pending" {
        jobs::cancel(tx, &format!("{CLEANUP}:{id}")).await?;
    }
    Ok(())
}

/// Gives the files document `doc` holds (either copy), just taken out of the trash, a free
/// name again: another live document may have taken theirs meanwhile. A renamed file's
/// `filename` changes in both copies that hold it, and never to a name another of the
/// document's files ends with. Numbering locks are taken in scope order (`reserve` takes
/// one), so two restores can't deadlock on them. True when one was renamed.
pub(crate) async fn renumber_in(tx: &mut PgConnection, doc: Uuid) -> Result<bool, DbError> {
    let held = sqlx::query!(
        r#"SELECT f.id, f.collection_id, f.scope, f.filename FROM files f
           JOIN documents d ON f.id IN (d.published_file, d.draft_file)
           WHERE d.tenant_id = $1 AND d.id = $2
           ORDER BY f.scope, f.id"#,
        DEFAULT_TENANT,
        doc,
    )
    .fetch_all(&mut *tx)
    .await?;
    // First which clash at all, so a kept name is known before a renamed one is picked.
    let none = HashSet::new();
    let mut clashing = Vec::new();
    for (i, f) in held.iter().enumerate() {
        let name = free_name(
            &mut *tx,
            f.collection_id,
            &f.scope,
            &f.filename,
            Some(doc),
            &none,
        )
        .await?;
        if name != f.filename {
            clashing.push(i);
        }
    }
    let mut names: Vec<String> = held.iter().map(|f| f.filename.clone()).collect();
    for &i in &clashing {
        let f = &held[i];
        // Every other file of the document in this scope keeps, or already got, its name.
        let also: HashSet<String> = held
            .iter()
            .enumerate()
            .filter(|&(j, g)| j != i && g.scope == f.scope && (!clashing.contains(&j) || j < i))
            .map(|(j, _)| names[j].clone())
            .collect();
        let name = free_name(
            &mut *tx,
            f.collection_id,
            &f.scope,
            &f.filename,
            Some(doc),
            &also,
        )
        .await?;
        sqlx::query!(
            "UPDATE files SET filename = $3, names = array_replace(names, filename, $3)
             WHERE tenant_id = $1 AND id = $2",
            DEFAULT_TENANT,
            f.id,
            name,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"UPDATE documents SET
                   published = CASE WHEN published_file = $3 AND published ? 'filename'
                                    THEN jsonb_set(published, '{filename}', to_jsonb($4::text))
                                    ELSE published END,
                   draft = CASE WHEN draft_file = $3 AND draft ? 'filename'
                                THEN jsonb_set(draft, '{filename}', to_jsonb($4::text))
                                ELSE draft END
               WHERE tenant_id = $1 AND id = $2"#,
            DEFAULT_TENANT,
            doc,
            f.id,
            name,
        )
        .execute(&mut *tx)
        .await?;
        names[i] = name;
    }
    Ok(!clashing.is_empty())
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

/// Turns pending file `id` `gone` once `PENDING_GRACE` has passed since it was reserved, so
/// no document can take it while its objects are deleted. False when it isn't pending or
/// its grace hasn't run out.
pub async fn expire(pool: &PgPool, id: Uuid) -> Result<bool, DbError> {
    let done = sqlx::query!(
        r#"UPDATE files SET state = 'gone', released_at = now()
           WHERE tenant_id = $1 AND id = $2 AND state = 'pending'
             AND created_at + make_interval(secs => $3) <= now()"#,
        DEFAULT_TENANT,
        id,
        PENDING_GRACE.as_secs_f64(),
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Deletes gone file `id`'s row, its objects deleted, unless something points at it again.
/// The delete locks the row: call it before the cleanup job's `complete_in` (a file before
/// its jobs, the order `release_in` and `go_live_in` take them).
pub async fn forget_in(tx: &mut PgConnection, id: Uuid) -> Result<bool, DbError> {
    let done = sqlx::query!(
        r#"DELETE FROM files f
           WHERE f.tenant_id = $1 AND f.id = $2 AND f.state = 'gone'
             AND NOT EXISTS (SELECT 1 FROM documents d
                             WHERE d.published_file = f.id OR d.draft_file = f.id)
             AND NOT EXISTS (SELECT 1 FROM versions v WHERE v.file_id = f.id)"#,
        DEFAULT_TENANT,
        id,
    )
    .execute(&mut *tx)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Queues again, with fresh attempts, each failed cleanup still worth trying on one of
/// `storages` (the configured ones): a file's whose file is still gone or pending past its
/// grace, and a `{"keys"}` one that ran out of attempts (one refused outright, for a bad
/// payload, would only be refused again). Storage that was down for longer than the retries is
/// tried again. The newest failed job per dedupe key, and none whose key already has a
/// pending one. Returns how many.
pub async fn requeue_cleanups_in(
    tx: &mut PgConnection,
    storages: &[String],
) -> Result<u64, DbError> {
    let done = sqlx::query!(
        r#"UPDATE jobs SET state = 'queued', attempts = 0, run_at = now(), locked_until = NULL,
               finished_at = NULL, result = NULL
           WHERE id IN (
               SELECT DISTINCT ON (coalesce(j.dedupe_key, j.id::text)) j.id FROM jobs j
               LEFT JOIN files f ON f.tenant_id = j.tenant_id
                   AND f.id = CASE WHEN j.payload->>'file' ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                                   THEN (j.payload->>'file')::uuid END
               WHERE j.tenant_id = $1 AND j.kind = $2 AND j.state = 'failed'
                 AND CASE WHEN j.payload ? 'file' THEN
                          f.storage = ANY($4)
                          AND (f.state = 'gone'
                               OR (f.state = 'pending'
                                   AND f.created_at + make_interval(secs => $3) <= now()))
                     ELSE j.payload->>'storage' = ANY($4) AND j.attempts >= j.max_attempts END
                 AND NOT EXISTS (SELECT 1 FROM jobs o
                                 WHERE o.tenant_id = $1 AND o.dedupe_key = j.dedupe_key
                                   AND o.state IN ('queued', 'running'))
               ORDER BY coalesce(j.dedupe_key, j.id::text), j.id DESC)"#,
        DEFAULT_TENANT,
        CLEANUP,
        PENDING_GRACE.as_secs_f64(),
        storages,
    )
    .execute(&mut *tx)
    .await?;
    Ok(done.rows_affected())
}

/// How many gone or pending files sit on a storage not among `storages`: their cleanups
/// can't run until it's configured again.
pub async fn on_unknown_storage(pool: &PgPool, storages: &[String]) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM files
           WHERE tenant_id = $1 AND state <> 'live' AND NOT storage = ANY($2)"#,
        DEFAULT_TENANT,
        storages,
    )
    .fetch_one(pool)
    .await?)
}

/// Whether `key` on `storage` is still served: a file that isn't gone sits in the key's
/// directory and lists its last part among its names, as a size remade under a name a
/// cleanup was queued for does.
pub async fn key_in_use(pool: &PgPool, storage: &str, key: &str) -> Result<bool, DbError> {
    let Some((dir, name)) = key.rsplit_once('/') else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM files
               WHERE tenant_id = $1 AND storage = $2 AND state <> 'gone'
                 AND starts_with(key, $3 || '/') AND strpos(substr(key, length($3) + 2), '/') = 0
                 AND $4 = ANY(names)) AS "used!""#,
        DEFAULT_TENANT,
        storage,
        dir,
        name,
    )
    .fetch_one(pool)
    .await?)
}
