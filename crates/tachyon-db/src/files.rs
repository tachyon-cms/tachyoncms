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
use tachyon_core::uploads::{numbered, numbered_bounds, rename_sizes};
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
/// name. Names in `also` count as taken too, and file `except`'s doesn't. Holds the
/// numbering lock of the collection and scope until the transaction ends.
pub(crate) async fn free_name(
    conn: &mut PgConnection,
    collection_id: Uuid,
    scope: &str,
    filename: &str,
    document: Option<Uuid>,
    also: &HashSet<String>,
    except: Option<Uuid>,
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
             AND fl.id IS DISTINCT FROM $8
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
        except,
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
    let row = reserve_in(&mut tx, f, &HashSet::new(), key_of).await?;
    tx.commit().await?;
    Ok(row)
}

/// `reserve` inside `tx`.
async fn reserve_in(
    tx: &mut PgConnection,
    f: &NewFile<'_>,
    also: &HashSet<String>,
    key_of: impl Fn(Uuid, &str) -> Result<String, String>,
) -> Result<FileRow, DbError> {
    let filename = free_name(
        &mut *tx,
        f.collection_id,
        f.scope,
        f.filename,
        f.document,
        also,
        None,
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
    jobs::enqueue(&mut *tx, &cleanup(id, OffsetDateTime::now_utc() + grace)).await?;
    Ok(row)
}

/// How long a client's signed `PUT` URL works.
pub const ISSUED_PUT: Duration = Duration::from_secs(15 * 60);
/// How long after the claim's grace a staging key is swept, for a claim just made.
const CLAIM_SKEW: Duration = Duration::from_secs(60);

/// The most files one user and key may be issued in one collection while their `PUT` URLs
/// still work, whatever became of them since.
pub const ISSUED_CAP: i64 = 100;

/// What `issue` did.
pub enum Issued {
    Reserved(Box<FileRow>),
    /// `ISSUED_CAP` URLs still work; the oldest runs out in `retry_after`.
    Full {
        retry_after: Duration,
    },
}

/// `reserve`s `f` for the client it's issued to (`f.issued_to`, which it needs), to put at
/// `staging` in its storage, unless `ISSUED_CAP` files issued to them in its collection
/// still have working URLs. Each issue queues the sweep of its staging key for when the
/// claim's grace is over, whatever happens to the file. That sweep is what's counted,
/// while the URL works, so a file given up or cleaned up early still counts. Its name is
/// numbered against the client's other waiting files in every scope, so a claim by name
/// finds one. A file issued earlier for the same document under the same name, in any
/// scope, not yet claimed, is given up: only the newest can be claimed.
pub async fn issue(
    pool: &PgPool,
    f: &NewFile<'_>,
    staging: &str,
    key_of: impl Fn(Uuid, &str) -> Result<String, String>,
) -> Result<Issued, DbError> {
    let Some((user, key)) = f.issued_to else {
        return Err(DbError::Refused(
            "an issued file needs whom it's issued to".into(),
        ));
    };
    let mut tx = pool.begin().await?;
    // One count at a time per client, so two requests can't both take the last place.
    sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        format!("files:issued:{}:{user}:{key:?}", f.collection_id),
    )
    .execute(&mut *tx)
    .await?;
    let window = ISSUED_PUT.as_secs_f64();
    let working = sqlx::query!(
        r#"SELECT count(*) AS "n!",
                  ceil(extract(epoch FROM min(created_at) + make_interval(secs => $5) - now()))::bigint AS wait
           FROM jobs
           WHERE tenant_id = $1 AND kind = 'file_cleanup' AND collection_id = $2
             AND payload ? 'staging' AND created_by = $3 AND key_id IS NOT DISTINCT FROM $4
             AND created_at > now() - make_interval(secs => $5)"#,
        DEFAULT_TENANT,
        f.collection_id,
        user,
        key,
        window,
    )
    .fetch_one(&mut *tx)
    .await?;
    if working.n >= ISSUED_CAP {
        let secs = working.wait.unwrap_or(1).max(1);
        return Ok(Issued::Full {
            retry_after: Duration::from_secs(secs.unsigned_abs()),
        });
    }
    let waiting: HashSet<String> = sqlx::query_scalar!(
        r#"SELECT filename FROM files
           WHERE tenant_id = $1 AND collection_id = $2 AND state = 'pending'
             AND issued_to = $3 AND issued_key IS NOT DISTINCT FROM $4
             AND (document_id IS DISTINCT FROM $5 OR $5 IS NULL)"#,
        DEFAULT_TENANT,
        f.collection_id,
        user,
        key,
        f.document,
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let row = reserve_in(&mut tx, f, &waiting, key_of).await?;
    sqlx::query!(
        "UPDATE files SET staging_key = $3 WHERE tenant_id = $1 AND id = $2",
        DEFAULT_TENANT,
        row.id,
        staging,
    )
    .execute(&mut *tx)
    .await?;
    let sweep = time::Duration::try_from(PENDING_GRACE + CLAIM_SKEW).expect("an hour fits");
    jobs::enqueue(
        &mut tx,
        &NewJob {
            kind: CLEANUP,
            document_id: None,
            collection_id: Some(f.collection_id),
            payload: json!({ "keys": [staging], "storage": f.storage, "staging": true }),
            run_at: OffsetDateTime::now_utc() + sweep,
            dedupe_key: None,
            created_by: Some(user),
            key_id: key,
            max_attempts: Some(CLEANUP_ATTEMPTS),
        },
    )
    .await?;
    if let Some(document) = f.document {
        let earlier = sqlx::query_scalar!(
            "UPDATE files SET state = 'gone', released_at = now()
             WHERE tenant_id = $1 AND collection_id = $2 AND document_id = $3 AND filename = $4
               AND state = 'pending' AND issued_to IS NOT NULL AND id <> $5
             RETURNING id",
            DEFAULT_TENANT,
            f.collection_id,
            document,
            row.filename,
            row.id,
        )
        .fetch_all(&mut *tx)
        .await?;
        for id in earlier {
            jobs::enqueue(&mut tx, &cleanup(id, OffsetDateTime::now_utc())).await?;
        }
    }
    tx.commit().await?;
    Ok(Issued::Reserved(Box::new(row)))
}

/// Which of `staging` (keys on `storage`) a pending file waits on.
pub async fn staging_claimed(
    pool: &PgPool,
    storage: &str,
    staging: &[String],
) -> Result<HashSet<String>, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT staging_key AS "key!" FROM files
           WHERE tenant_id = $1 AND storage = $2 AND staging_key = ANY($3)
             AND state = 'pending'"#,
        DEFAULT_TENANT,
        storage,
        staging,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect())
}

/// Whether a claim is under way on what a client put at `staging` on `storage`: its file is
/// pending and taken. The row is locked as a claim locks it, until `tx` ends, so the answer
/// holds while the caller acts on it.
pub async fn staging_in_claim_in(
    tx: &mut PgConnection,
    storage: &str,
    staging: &str,
) -> Result<bool, DbError> {
    let rows = sqlx::query!(
        "SELECT state, issued_to FROM files
         WHERE tenant_id = $1 AND storage = $2 AND staging_key = $3
         FOR UPDATE",
        DEFAULT_TENANT,
        storage,
        staging,
    )
    .fetch_all(&mut *tx)
    .await?;
    Ok(rows
        .iter()
        .any(|r| r.state == "pending" && r.issued_to.is_none()))
}

/// The key a client puts an issued file at, which only the server reads.
pub async fn staging_key(
    conn: impl sqlx::PgExecutor<'_>,
    id: Uuid,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT staging_key FROM files WHERE tenant_id = $1 AND id = $2",
        DEFAULT_TENANT,
        id,
    )
    .fetch_optional(conn)
    .await?
    .flatten())
}

/// Records what pending file `id`, put by a client and claimed, turned out to be once it
/// was checked: `f`'s type, size, dimensions, placeholder and focal point, and its name
/// numbered again in `f.scope`, where `f.document`'s own files don't count, as `reserve`
/// numbers. A new name moves its key (`key_of`); nothing was stored under the old one,
/// since only the server writes a file's key, after this. Only a pending file: `Refused`
/// otherwise.
pub async fn settle_in(
    tx: &mut PgConnection,
    id: Uuid,
    f: &NewFile<'_>,
    key_of: impl Fn(Uuid, &str) -> Result<String, String>,
) -> Result<FileRow, DbError> {
    sqlx::query!(
        "SELECT id FROM files
         WHERE tenant_id = $1 AND id = $2 AND state = 'pending' FOR UPDATE",
        DEFAULT_TENANT,
        id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| DbError::Refused(format!("the file `{id}` was deleted meanwhile")))?;
    let filename = free_name(
        &mut *tx,
        f.collection_id,
        f.scope,
        f.filename,
        f.document,
        &HashSet::new(),
        Some(id),
    )
    .await?;
    let key = key_of(id, &filename).map_err(DbError::Refused)?;
    let row = sqlx::query_as!(
        FileRow,
        r#"UPDATE files SET filename = $3, key = $4, names = ARRAY[$3], scope = $5,
                  mime_type = $6, filesize = $7, width = $8, height = $9, focal_x = $10,
                  focal_y = $11, blurhash = $12, document_id = $13
           WHERE tenant_id = $1 AND id = $2
           RETURNING id, collection_id, storage, key, filename, mime_type, filesize, width,
                     height, focal_x, focal_y, blurhash,
                     sizes AS "sizes: Json<Map<String, Value>>", names, state, created_at,
                     issued_to, issued_key"#,
        DEFAULT_TENANT,
        id,
        filename,
        key,
        f.scope,
        f.mime_type,
        f.filesize,
        f.width,
        f.height,
        f.focal.0,
        f.focal.1,
        f.blurhash,
        f.document,
    )
    .fetch_one(&mut *tx)
    .await?;
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

/// A file `renumber_in` renamed: its sizes' objects, in the directory of `key` on
/// `storage`, must move from each old name to its new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renamed {
    pub file: Uuid,
    /// The original's new name.
    pub filename: String,
    pub storage: String,
    pub key: String,
    /// (old, new) for each size and copy, the original's own name not among them: its
    /// object keeps the key it was stored under.
    pub sizes: Vec<(String, String)>,
}

/// Gives the files document `doc` holds (either copy), just taken out of the trash, a free
/// name again: another live document may have taken theirs meanwhile. A renamed file's
/// `filename` changes in both copies that hold it and in every version holding it, and
/// never to a name another of the document's files ends with; its sizes are renamed after
/// it everywhere they're described, and the caller moves their objects (`Renamed`).
/// Numbering locks are taken in scope order (`reserve` takes one), so two restores can't
/// deadlock on them. Returns those renamed.
pub(crate) async fn renumber_in(tx: &mut PgConnection, doc: Uuid) -> Result<Vec<Renamed>, DbError> {
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
            None,
        )
        .await?;
        if name != f.filename {
            clashing.push(i);
        }
    }
    let mut names: Vec<String> = held.iter().map(|f| f.filename.clone()).collect();
    let mut renamed = Vec::new();
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
            None,
        )
        .await?;
        // Locked before its sizes are read, so a size being written waits or is seen.
        let row = sqlx::query!(
            r#"SELECT storage, key, names, sizes AS "sizes: Json<Map<String, Value>>"
               FROM files WHERE tenant_id = $1 AND id = $2 FOR UPDATE"#,
            DEFAULT_TENANT,
            f.id,
        )
        .fetch_one(&mut *tx)
        .await?;
        let mut sizes = row.sizes.0;
        let moved = rename_sizes(&mut sizes, &name);
        let served: Vec<String> = row
            .names
            .iter()
            .map(|n| match moved.iter().find(|(old, _)| old == n) {
                _ if *n == f.filename => name.clone(),
                Some((_, new)) => new.clone(),
                None => n.clone(),
            })
            .collect();
        let sizes = Value::Object(sizes);
        sqlx::query!(
            "UPDATE files SET filename = $3, names = $4, sizes = $5 WHERE tenant_id = $1 AND id = $2",
            DEFAULT_TENANT,
            f.id,
            name,
            &served,
            &sizes,
        )
        .execute(&mut *tx)
        .await?;
        // Sizes no job has filled yet stay as each copy has them.
        sqlx::query!(
            r#"UPDATE documents SET
                   published = CASE WHEN published_file = $2 AND published ? 'filename'
                                    THEN jsonb_set(published, '{filename}', to_jsonb($3::text))
                                         || CASE WHEN jsonb_typeof(published->'sizes') = 'object'
                                                 THEN jsonb_build_object('sizes', published->'sizes' || $4::jsonb)
                                                 ELSE '{}' END
                                    ELSE published END,
                   draft = CASE WHEN draft_file = $2 AND draft ? 'filename'
                                THEN jsonb_set(draft, '{filename}', to_jsonb($3::text))
                                     || CASE WHEN jsonb_typeof(draft->'sizes') = 'object'
                                             THEN jsonb_build_object('sizes', draft->'sizes' || $4::jsonb)
                                             ELSE '{}' END
                                ELSE draft END
               WHERE tenant_id = $1 AND (published_file = $2 OR draft_file = $2)"#,
            DEFAULT_TENANT,
            f.id,
            name,
            &sizes,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"UPDATE versions SET data = jsonb_set(data, '{filename}', to_jsonb($3::text))
                   || CASE WHEN jsonb_typeof(data->'sizes') = 'object'
                           THEN jsonb_build_object('sizes', data->'sizes' || $4::jsonb)
                           ELSE '{}' END
               WHERE tenant_id = $1 AND file_id = $2 AND data ? 'filename'"#,
            DEFAULT_TENANT,
            f.id,
            name,
            &sizes,
        )
        .execute(&mut *tx)
        .await?;
        renamed.push(Renamed {
            file: f.id,
            filename: name.clone(),
            storage: row.storage,
            key: row.key,
            sizes: moved,
        });
        names[i] = name;
    }
    Ok(renamed)
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
pub async fn get<'e>(conn: impl sqlx::PgExecutor<'e>, id: Uuid) -> Result<FileRow, DbError> {
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
    .fetch_optional(conn)
    .await?
    .ok_or(DbError::NotFound)
}

/// What `lock_for_size_in` found.
#[derive(Debug)]
pub enum SizeLock {
    /// The file is live and locked, with everything that describes it; the collection
    /// was at `schema_version`.
    Live {
        file: Box<FileRow>,
        schema_version: i32,
    },
    /// Pending, gone or deleted: no size may be written.
    Released,
    /// A document took the file between the locks: roll back and lock again later.
    Moved,
    /// A user's write holds the file or one of its versions: roll back, so it can go on,
    /// and lock again later.
    Busy,
}

/// Whether `e` is Postgres refusing a `NOWAIT` lock someone else holds.
fn lock_taken(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("55P03"))
}

/// Locks file `id` of `collection_id`, and everything a size written to it changes, in the
/// order every write takes them: the collection `FOR KEY SHARE` (a schema change, which
/// rewrites documents whole, waits or is waited for), the documents holding it in ID
/// order, the versions holding it, then the file `FOR NO KEY UPDATE`, which a `{"keys"}`
/// cleanup's `FOR SHARE` waits on. Should a document have taken the file before its lock,
/// it's `Moved`. The versions and the file are taken `NOWAIT`: a document write may hold
/// the file before its versions (`lock_in`, then a prune) or after (a prune, then
/// `release_in`), so the job never waits on either while it holds what that write needs;
/// it's `Busy`, and the transaction must be rolled back.
pub async fn lock_for_size_in(
    tx: &mut PgConnection,
    collection_id: Uuid,
    id: Uuid,
) -> Result<SizeLock, DbError> {
    let Some(schema_version) = sqlx::query_scalar!(
        "SELECT schema_version FROM collections WHERE id = $1 FOR KEY SHARE",
        collection_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(SizeLock::Released);
    };
    let holders = sqlx::query_scalar!(
        "SELECT id FROM documents
         WHERE tenant_id = $1 AND (published_file = $2 OR draft_file = $2)
         ORDER BY id FOR NO KEY UPDATE",
        DEFAULT_TENANT,
        id,
    )
    .fetch_all(&mut *tx)
    .await?;
    let versions = sqlx::query!(
        "SELECT id FROM versions WHERE tenant_id = $1 AND file_id = $2
         ORDER BY id FOR UPDATE NOWAIT",
        DEFAULT_TENANT,
        id,
    )
    .fetch_all(&mut *tx)
    .await;
    match versions {
        Err(e) if lock_taken(&e) => return Ok(SizeLock::Busy),
        other => other?,
    };
    let file = sqlx::query_as!(
        FileRow,
        r#"SELECT id, collection_id, storage, key, filename, mime_type, filesize, width,
                  height, focal_x, focal_y, blurhash,
                  sizes AS "sizes: Json<Map<String, Value>>", names, state, created_at,
                  issued_to, issued_key
           FROM files WHERE tenant_id = $1 AND id = $2 FOR NO KEY UPDATE NOWAIT"#,
        DEFAULT_TENANT,
        id,
    )
    .fetch_optional(&mut *tx)
    .await;
    let file = match file {
        Err(e) if lock_taken(&e) => return Ok(SizeLock::Busy),
        other => other?,
    };
    let Some(file) = file.filter(|f| f.state == "live") else {
        return Ok(SizeLock::Released);
    };
    // With the file locked no other document can take it, so this set is final.
    let now = sqlx::query_scalar!(
        "SELECT id FROM documents
         WHERE tenant_id = $1 AND (published_file = $2 OR draft_file = $2) ORDER BY id",
        DEFAULT_TENANT,
        id,
    )
    .fetch_all(&mut *tx)
    .await?;
    if now.iter().any(|d| !holders.contains(d)) {
        return Ok(SizeLock::Moved);
    }
    Ok(SizeLock::Live {
        file: Box::new(file),
        schema_version,
    })
}

/// Writes size `name` of file `id` as `entry` everywhere the file's described: its row,
/// where `new_names` join the names it's served under, each document copy holding it,
/// trashed ones too, and each version holding it (call 1). Matched by file ID, so a size
/// made for a file a document has since replaced lands only where that file still is.
/// Call it under `lock_for_size_in`. Returns how many documents and versions hold it.
pub async fn fill_size_in(
    tx: &mut PgConnection,
    id: Uuid,
    name: &str,
    entry: &Value,
    new_names: &[String],
) -> Result<u64, DbError> {
    sqlx::query!(
        r#"UPDATE files SET sizes = sizes || jsonb_build_object($3::text, $4::jsonb),
               names = names || ARRAY(SELECT n FROM unnest($5::text[]) n WHERE n <> ALL(names))
           WHERE tenant_id = $1 AND id = $2"#,
        DEFAULT_TENANT,
        id,
        name,
        entry,
        new_names,
    )
    .execute(&mut *tx)
    .await?;
    let docs = sqlx::query!(
        r#"UPDATE documents SET
               published = CASE WHEN published_file = $2 AND published ? 'sizes'
                                THEN jsonb_set(published, '{sizes}',
                                     CASE WHEN jsonb_typeof(published->'sizes') = 'object'
                                          THEN published->'sizes' ELSE '{}' END
                                     || jsonb_build_object($3::text, $4::jsonb))
                                ELSE published END,
               draft = CASE WHEN draft_file = $2 AND draft ? 'sizes'
                            THEN jsonb_set(draft, '{sizes}',
                                 CASE WHEN jsonb_typeof(draft->'sizes') = 'object'
                                      THEN draft->'sizes' ELSE '{}' END
                                 || jsonb_build_object($3::text, $4::jsonb))
                            ELSE draft END
           WHERE tenant_id = $1 AND (published_file = $2 OR draft_file = $2)"#,
        DEFAULT_TENANT,
        id,
        name,
        entry,
    )
    .execute(&mut *tx)
    .await?;
    let versions = sqlx::query!(
        r#"UPDATE versions SET data = jsonb_set(data, '{sizes}',
               CASE WHEN jsonb_typeof(data->'sizes') = 'object' THEN data->'sizes' ELSE '{}' END
               || jsonb_build_object($3::text, $4::jsonb))
           WHERE tenant_id = $1 AND file_id = $2 AND data ? 'sizes'"#,
        DEFAULT_TENANT,
        id,
        name,
        entry,
    )
    .execute(&mut *tx)
    .await?;
    Ok(docs.rows_affected() + versions.rows_affected())
}

/// Queues the deletion of `keys` on `storage`, objects no row may record yet, due at
/// `run_at`; each is kept if a file serves it by the time the cleanup runs
/// (`key_in_use_in`). Queued for `PENDING_GRACE` ahead before objects are stored, it's what
/// removes them should the write that was to record them never commit.
pub async fn queue_key_cleanup(
    conn: &mut PgConnection,
    storage: &str,
    keys: &[String],
    run_at: OffsetDateTime,
) -> Result<Uuid, DbError> {
    jobs::enqueue(
        conn,
        &NewJob {
            kind: CLEANUP,
            document_id: None,
            collection_id: None,
            payload: json!({ "keys": keys, "storage": storage }),
            run_at,
            dedupe_key: None,
            created_by: None,
            key_id: None,
            max_attempts: Some(CLEANUP_ATTEMPTS),
        },
    )
    .await
}

/// Live files of `collection_id` served under `name`, the original's or a size's, each with
/// the live documents whose copy holds it now, once per copy. More than one file when
/// scopes share a name; the holders tell them apart. One query, so a name only a hidden
/// file has costs about what a missing one does.
pub async fn by_name(
    pool: &PgPool,
    collection_id: Uuid,
    name: &str,
) -> Result<Vec<(FileRow, Vec<(Uuid, DocCopy)>)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT f.id, f.collection_id, f.storage, f.key, f.filename, f.mime_type,
                  f.filesize, f.width, f.height, f.focal_x, f.focal_y, f.blurhash,
                  f.sizes AS "sizes: Json<Map<String, Value>>", f.names, f.state,
                  f.created_at, f.issued_to, f.issued_key,
                  COALESCE(h.docs, '{}') AS "docs!: Vec<Uuid>",
                  COALESCE(h.published, '{}') AS "published!: Vec<bool>",
                  COALESCE(h.drafts, '{}') AS "drafts!: Vec<bool>"
           FROM files f
           LEFT JOIN LATERAL (
               SELECT array_agg(d.id ORDER BY d.id) AS docs,
                      array_agg(d.published_file IS NOT DISTINCT FROM f.id ORDER BY d.id)
                          AS published,
                      array_agg(d.draft_file IS NOT DISTINCT FROM f.id ORDER BY d.id)
                          AS drafts
               FROM documents d
               WHERE d.tenant_id = $1 AND d.deleted_at IS NULL
                 AND (d.published_file = f.id OR d.draft_file = f.id)
           ) h ON true
           WHERE f.tenant_id = $1 AND f.collection_id = $2 AND f.names @> ARRAY[$3]
             AND f.state = 'live'
           ORDER BY f.id"#,
        DEFAULT_TENANT,
        collection_id,
        name,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let holders = r
                .docs
                .iter()
                .zip(r.published.iter().zip(&r.drafts))
                .flat_map(|(&doc, (&published, &draft))| {
                    [
                        published.then_some((doc, DocCopy::Published)),
                        draft.then_some((doc, DocCopy::Draft)),
                    ]
                })
                .flatten()
                .collect();
            let file = FileRow {
                id: r.id,
                collection_id: r.collection_id,
                storage: r.storage,
                key: r.key,
                filename: r.filename,
                mime_type: r.mime_type,
                filesize: r.filesize,
                width: r.width,
                height: r.height,
                focal_x: r.focal_x,
                focal_y: r.focal_y,
                blurhash: r.blurhash,
                sizes: r.sizes,
                names: r.names,
                state: r.state,
                created_at: r.created_at,
                issued_to: r.issued_to,
                issued_key: r.issued_key,
            };
            (file, holders)
        })
        .collect())
}

/// The storage key of the file each of `docs` holds in its published copy, by document;
/// trashed documents left out.
pub async fn published_keys(
    pool: &PgPool,
    docs: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, String>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT d.id, f.key FROM documents d JOIN files f ON f.id = d.published_file
           WHERE d.tenant_id = $1 AND d.id = ANY($2) AND d.deleted_at IS NULL"#,
        DEFAULT_TENANT,
        docs,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.key)).collect())
}

/// Takes the pending file of `collection_id` named `filename` that was issued to `user`
/// with `key` (none for a session), within its grace, for `document`: one issued
/// for no document is taken by any write, one issued for a document only by that
/// document's update. Once taken it's issued to nobody, so no other request can take it:
/// of two at once, the second waits for the first's row lock and then finds nothing. It
/// stays pending, for the write that takes it to make live or give up, or for its cleanup.
pub async fn claim_pending(
    tx: &mut PgConnection,
    collection_id: Uuid,
    filename: &str,
    user: Uuid,
    key: Option<Uuid>,
    document: Option<Uuid>,
) -> Result<Option<FileRow>, DbError> {
    // The row locked is the row taken: every condition is in the lock's query, and checked
    // again once the lock is had.
    Ok(sqlx::query_as!(
        FileRow,
        r#"UPDATE files SET issued_to = NULL, issued_key = NULL
           WHERE tenant_id = $1 AND state = 'pending' AND issued_to = $4
             AND id = (SELECT id FROM files
                       WHERE tenant_id = $1 AND collection_id = $2 AND filename = $3
                         AND state = 'pending' AND issued_to = $4
                         AND issued_key IS NOT DISTINCT FROM $5
                         AND created_at + make_interval(secs => $6) > now()
                         AND (document_id IS NULL OR document_id = $7)
                       ORDER BY id LIMIT 1 FOR UPDATE)
           RETURNING id, collection_id, storage, key, filename, mime_type, filesize, width,
                     height, focal_x, focal_y, blurhash,
                     sizes AS "sizes: Json<Map<String, Value>>", names, state, created_at,
                     issued_to, issued_key"#,
        DEFAULT_TENANT,
        collection_id,
        filename,
        user,
        key,
        PENDING_GRACE.as_secs_f64(),
        document,
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

/// Gives up pending file `id`, reserved by a write that then failed: it turns `gone`, so
/// nothing can take it, and its cleanup is due now instead of after the grace. False when
/// it wasn't pending.
pub async fn abandon(pool: &PgPool, id: Uuid) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let done = sqlx::query!(
        "UPDATE files SET state = 'gone', released_at = now()
         WHERE tenant_id = $1 AND id = $2 AND state = 'pending'",
        DEFAULT_TENANT,
        id,
    )
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(false);
    }
    // `enqueue` replaces the queued cleanup that `reserve` made.
    jobs::enqueue(&mut tx, &cleanup(id, OffsetDateTime::now_utc())).await?;
    tx.commit().await?;
    Ok(true)
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
/// cleanup was queued for does. The file is locked `FOR SHARE` until the caller's
/// transaction ends, so a size can't be written in (`lock_for_size_in`) between this
/// answer and a delete it leads to; a size job whose object such a delete took finds it
/// missing once committed and stores it again.
pub async fn key_in_use_in(
    tx: &mut PgConnection,
    storage: &str,
    key: &str,
) -> Result<bool, DbError> {
    let Some((dir, name)) = key.rsplit_once('/') else {
        return Ok(false);
    };
    let names = sqlx::query_scalar!(
        r#"SELECT names FROM files
           WHERE tenant_id = $1 AND storage = $2 AND state <> 'gone'
             AND starts_with(key, $3 || '/') AND strpos(substr(key, length($3) + 2), '/') = 0
           ORDER BY id FOR SHARE"#,
        DEFAULT_TENANT,
        storage,
        dir,
    )
    .fetch_all(&mut *tx)
    .await?;
    Ok(names.iter().any(|n| n.iter().any(|n| n == name)))
}

/// The job that makes every live file's sizes in a collection again, after a schema change
/// to what they're made with. Its payload is `{"after": file id | null}`, the batch going on
/// from that file; its dedupe key `regenerate_sizes:<collection id>`, so a newer change's walk
/// replaces an older one, from the start.
pub const REGENERATE: &str = "regenerate_sizes";

/// The batch of `collection_id`'s walk after file `after`, or its first.
pub fn regenerate_job(collection_id: Uuid, after: Option<Uuid>) -> NewJob<'static> {
    NewJob {
        kind: REGENERATE,
        document_id: None,
        collection_id: Some(collection_id),
        payload: json!({ "after": after }),
        run_at: OffsetDateTime::now_utc(),
        dedupe_key: Some(format!("{REGENERATE}:{collection_id}")),
        created_by: None,
        key_id: None,
        // A walk cut short leaves stale sizes and objects: it keeps trying as a cleanup
        // does, and the hourly sweep revives it should it run out (`requeue_walks_in`).
        max_attempts: Some(CLEANUP_ATTEMPTS),
    }
}

/// Queues again, with fresh attempts, each failed walk that's still its collection's
/// latest: no newer walk, pending or finished, has its key, nor is one pending under it
/// (newer or not: job ids come from each server's clock). It goes on from its batch.
/// Returns how many.
pub async fn requeue_walks_in(tx: &mut PgConnection) -> Result<u64, DbError> {
    let done = sqlx::query!(
        r#"UPDATE jobs SET state = 'queued', attempts = 0, run_at = now(), locked_until = NULL,
               finished_at = NULL, result = NULL
           WHERE id IN (
               SELECT id FROM (
                   SELECT DISTINCT ON (dedupe_key) id, state, dedupe_key FROM jobs
                   WHERE tenant_id = $1 AND kind = $2
                   ORDER BY dedupe_key, created_at DESC, id DESC
               ) latest
               WHERE state = 'failed'
                 AND NOT EXISTS (SELECT 1 FROM jobs o
                                 WHERE o.tenant_id = $1 AND o.dedupe_key = latest.dedupe_key
                                   AND o.state IN ('queued', 'running')))"#,
        DEFAULT_TENANT,
        REGENERATE,
    )
    .execute(&mut *tx)
    .await?;
    Ok(done.rows_affected())
}

/// One batch of `collection_id`'s walk, in `tx`: up to `limit` of its live files after file
/// `after` (from the first without one), in ID order, each given the job `job_for` makes
/// from its ID and type (none for a file it skips), all queued in one statement
/// (`jobs::enqueue_all`). Returns the batch's files. The collection is taken `FOR KEY SHARE`
/// first, so a schema change under way, a removal of its files say, is waited out and its
/// outcome read: the lock order a removal takes too.
pub async fn regenerate_batch_in(
    tx: &mut PgConnection,
    collection_id: Uuid,
    after: Option<Uuid>,
    limit: i64,
    job_for: impl Fn(Uuid, &str) -> Option<NewJob<'static>>,
) -> Result<Vec<Uuid>, DbError> {
    let here = sqlx::query_scalar!(
        "SELECT id FROM collections WHERE tenant_id = $1 AND id = $2 FOR KEY SHARE",
        DEFAULT_TENANT,
        collection_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if here.is_none() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!(
        r#"SELECT id, mime_type FROM files
           WHERE tenant_id = $1 AND collection_id = $2 AND state = 'live'
             AND ($3::uuid IS NULL OR id > $3)
           ORDER BY id LIMIT $4"#,
        DEFAULT_TENANT,
        collection_id,
        after,
        limit,
    )
    .fetch_all(&mut *tx)
    .await?;
    let queued: Vec<NewJob<'static>> = rows
        .iter()
        .filter_map(|r| job_for(r.id, &r.mime_type))
        .collect();
    jobs::enqueue_all(&mut *tx, &queued).await?;
    Ok(rows.into_iter().map(|r| r.id).collect())
}

/// The focal point file `id`'s sizes centre on: the one of the document copy holding it,
/// its draft's before its published copy's (the newer edit, and what a publish takes), then
/// the newest version's holding it; 50/50 where none sets one.
pub async fn focal_of(pool: &PgPool, id: Uuid) -> Result<(f32, f32), DbError> {
    let row = sqlx::query!(
        r#"SELECT x, y FROM (
               SELECT 0 AS rank, id, draft->'focalX' AS x, draft->'focalY' AS y
               FROM documents WHERE tenant_id = $1 AND draft_file = $2
               UNION ALL
               SELECT 1, id, published->'focalX', published->'focalY'
               FROM documents WHERE tenant_id = $1 AND published_file = $2
               UNION ALL
               SELECT 2, id, data->'focalX', data->'focalY'
               FROM versions WHERE tenant_id = $1 AND file_id = $2
           ) holders
           ORDER BY rank, id DESC LIMIT 1"#,
        DEFAULT_TENANT,
        id,
    )
    .fetch_optional(pool)
    .await?;
    let of = |v: Option<Value>| v.and_then(|v| v.as_f64()).map_or(50.0, |v| v as f32);
    Ok(row.map_or((50.0, 50.0), |r| (of(r.x), of(r.y))))
}

/// Drops from file `id` the sizes not named in `keep`, and every name that neither its
/// original nor a size it keeps is stored under; returns those names, whose objects are
/// then no longer served. Call it under `lock_for_size_in`, once each kept size is written.
pub async fn drop_stale_sizes_in(
    tx: &mut PgConnection,
    id: Uuid,
    keep: &[&str],
) -> Result<Vec<String>, DbError> {
    let row = sqlx::query!(
        r#"SELECT filename, names, sizes AS "sizes: Json<Map<String, Value>>"
           FROM files WHERE tenant_id = $1 AND id = $2"#,
        DEFAULT_TENANT,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    let mut sizes = row.sizes.0;
    sizes.retain(|name, _| keep.contains(&name.as_str()));
    let mut used = HashSet::from([row.filename.as_str()]);
    for entry in sizes.values() {
        let formats = entry.get("formats").and_then(Value::as_object);
        for e in std::iter::once(entry).chain(formats.into_iter().flat_map(Map::values)) {
            used.extend(e.get("filename").and_then(Value::as_str));
        }
    }
    let (kept, stale): (Vec<String>, Vec<String>) = row
        .names
        .into_iter()
        .partition(|n| used.contains(n.as_str()));
    let sizes = Value::Object(sizes);
    sqlx::query!(
        "UPDATE files SET sizes = $3, names = $4 WHERE tenant_id = $1 AND id = $2",
        DEFAULT_TENANT,
        id,
        &sizes,
        &kept,
    )
    .execute(&mut *tx)
    .await?;
    Ok(stale)
}

/// How many documents of `collection_id`, trashed ones included, hold a file: in either
/// copy, or in one of their versions.
pub async fn holding_documents(
    conn: &mut PgConnection,
    collection_id: Uuid,
) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM documents d
           WHERE d.tenant_id = $1 AND d.collection_id = $2
             AND (d.published_file IS NOT NULL OR d.draft_file IS NOT NULL
                  OR EXISTS (SELECT 1 FROM versions v
                             WHERE v.document_id = d.id AND v.file_id IS NOT NULL))"#,
        DEFAULT_TENANT,
        collection_id,
    )
    .fetch_one(&mut *conn)
    .await?)
}

/// Takes every file of `collection_id` from whatever holds it, both copies of each document
/// (trashed ones too) and every version, and releases every live one, held or not, each
/// with its cleanup due now; for a collection that stops holding files. Its pending files
/// (issued ones too) turn `gone` as well, so nothing can take them, keeping the cleanup each
/// was reserved with: due once an upload that may still be storing it is done (and a
/// client's staging object keeps the sweep its issue queued). The collection's size jobs
/// and walk are cancelled: there's nothing left for them to make. All of it set-based, a
/// statement per step whatever the collection holds, with no savepoint per file.
pub async fn drop_collection_in(tx: &mut PgConnection, collection_id: Uuid) -> Result<(), DbError> {
    // Documents, then versions, then files, then jobs: the order every write locks them in.
    sqlx::query!(
        r#"WITH old AS (SELECT id FROM documents
                        WHERE tenant_id = $1 AND collection_id = $2
                          AND (published_file IS NOT NULL OR draft_file IS NOT NULL)
                        ORDER BY id FOR NO KEY UPDATE)
           UPDATE documents d SET published_file = NULL, draft_file = NULL FROM old
           WHERE d.id = old.id"#,
        DEFAULT_TENANT,
        collection_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"WITH old AS (SELECT id FROM versions
                        WHERE tenant_id = $1 AND collection_id = $2 AND file_id IS NOT NULL
                        ORDER BY id FOR UPDATE)
           UPDATE versions v SET file_id = NULL FROM old WHERE v.id = old.id"#,
        DEFAULT_TENANT,
        collection_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "SELECT id FROM files WHERE tenant_id = $1 AND collection_id = $2 AND state <> 'gone'
         ORDER BY id FOR UPDATE",
        DEFAULT_TENANT,
        collection_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    let gone = sqlx::query_scalar!(
        r#"UPDATE files f SET state = 'gone', released_at = now()
           WHERE f.tenant_id = $1 AND f.collection_id = $2 AND f.state = 'live'
             AND NOT EXISTS (SELECT 1 FROM documents d
                             WHERE d.published_file = f.id OR d.draft_file = f.id)
             AND NOT EXISTS (SELECT 1 FROM versions v WHERE v.file_id = f.id)
           RETURNING f.id"#,
        DEFAULT_TENANT,
        collection_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE files SET state = 'gone', released_at = now()
         WHERE tenant_id = $1 AND collection_id = $2 AND state = 'pending'",
        DEFAULT_TENANT,
        collection_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"UPDATE jobs SET state = 'cancelled', locked_until = NULL, finished_at = clock_timestamp()
           WHERE tenant_id = $1 AND collection_id = $2 AND kind IN ('image_sizes', $3)
             AND state IN ('queued', 'running')"#,
        DEFAULT_TENANT,
        collection_id,
        REGENERATE,
    )
    .execute(&mut *tx)
    .await?;
    // Their rows are locked above, so no other caller can queue these keys meanwhile.
    let now = OffsetDateTime::now_utc();
    let cleanups: Vec<NewJob<'static>> = gone.into_iter().map(|id| cleanup(id, now)).collect();
    jobs::enqueue_all(&mut *tx, &cleanups).await
}
