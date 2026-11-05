//! Changing a collection's fields after it has documents.
//!
//! One code path runs in two modes. Preview scans every document against the new fields
//! and rolls back. Apply does the same scan, decides, then rewrites every document,
//! re-syncs links, and stores the new schema, all in one transaction that holds the
//! collection row `FOR UPDATE`, so no document write can interleave.
//!
//! A shared block has no documents. Changing one changes every schema that reaches it,
//! directly or through other shared blocks, so each of those runs the same passes in the
//! same transaction. Several schemas can be saved as one change (`apply_all`), which a
//! schema file uses: all of it goes in, or none.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use tachyon_core::access::check_rules;
use tachyon_core::diff::{Change, LinkChange, Renames, SchemaDiff, diff, migrate_document, relink};
use tachyon_core::{
    CollectionKind, CollectionSchema, FieldError, SchemaError, relations, validate_document,
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::collections::Collection;
use crate::documents::{DocCopy, Relation, sync_relations};
use crate::indexes::{Wanted, ddl, existing_indexes, wanted_indexes};
use crate::{DEFAULT_TENANT, DbError};

const BATCH: i64 = 500;
const MAX_EXAMPLES: usize = 5;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub diff: SchemaDiff,
    /// Live documents in the collection.
    pub documents: i64,
    /// Live documents that won't validate against the new fields.
    pub invalid: i64,
    pub examples: Vec<InvalidDocument>,
    /// Unique fields whose values aren't unique across live documents.
    pub duplicates: Vec<String>,
    pub needs_confirmation: bool,
    /// Live documents that lose a link because its collection is no longer allowed.
    pub cleared: i64,
    pub indexes: IndexChanges,
    /// Versions the change deletes: those past a lowered `maxVersions`, or every one when
    /// `versions` goes. A version a pending job holds is kept.
    pub pruned: i64,
    /// For a shared block: the change in each schema that names it, by slug.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub collections: BTreeMap<String, Preview>,
}

/// Indexes the change builds and drops, labelled like `title (unique)`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexChanges {
    pub create: Vec<String>,
    pub drop: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InvalidDocument {
    pub id: Uuid,
    pub errors: Vec<FieldError>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChangeError {
    #[error("collection not found")]
    NotFound,
    #[error("{0}")]
    Invalid(SchemaError),
    #[error("unique fields have duplicate values")]
    Duplicates(Box<Preview>),
    #[error("the change needs confirmation")]
    NeedsConfirmation(Box<Preview>),
    /// Turning drafts off would lose work that isn't published; no `confirm` overrides it.
    #[error("{0}")]
    PendingDrafts(String),
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for ChangeError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e.into())
    }
}

/// One schema to save: a change to a stored one, or a new one when `new`.
pub struct Save<'a> {
    /// The stored schema it changes. A different `schema.slug` would rename it, which isn't
    /// supported.
    pub slug: &'a str,
    pub schema: &'a CollectionSchema,
    pub renames: &'a Renames,
    pub new: bool,
}

/// What a save did, or would do.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub collection: Collection,
    pub created: bool,
    pub preview: Preview,
}

pub async fn preview(
    pool: &PgPool,
    slug: &str,
    new: &CollectionSchema,
    renames: &Renames,
) -> Result<Preview, ChangeError> {
    let save = Save {
        slug,
        schema: new,
        renames,
        new: false,
    };
    let mut out = run(pool, &[save], None).await?;
    Ok(out.remove(0).preview)
}

pub async fn apply(
    pool: &PgPool,
    slug: &str,
    new: &CollectionSchema,
    renames: &Renames,
    confirm: bool,
) -> Result<(Collection, Preview), ChangeError> {
    let save = Save {
        slug,
        schema: new,
        renames,
        new: false,
    };
    let o = run(pool, &[save], Some(confirm)).await?.remove(0);
    Ok((o.collection, o.preview))
}

/// Several saves as one change, such as a schema file's: each sees the others' versions,
/// so a shared block's cascade reaches a schema the file also changes as the file has it.
pub async fn preview_all(pool: &PgPool, saves: &[Save<'_>]) -> Result<Vec<Outcome>, ChangeError> {
    run(pool, saves, None).await
}

/// Creates one schema, with every check a change gets, under the lock: its shared blocks
/// expanded (loops and depth refused) and every join, virtual and rule rechecked against
/// what's stored, not what a server's cache last heard.
pub async fn create(pool: &PgPool, schema: &CollectionSchema) -> Result<Collection, ChangeError> {
    let save = Save {
        slug: &schema.slug,
        schema,
        renames: &Renames::new(),
        new: true,
    };
    Ok(run(pool, &[save], Some(true)).await?.remove(0).collection)
}

/// `preview_all`, applied in one transaction: all of it goes in, or none of it.
pub async fn apply_all(
    pool: &PgPool,
    saves: &[Save<'_>],
    confirm: bool,
) -> Result<Vec<Outcome>, ChangeError> {
    run(pool, saves, Some(confirm)).await
}

fn index_changes(id: Uuid, old: &CollectionSchema, new: &CollectionSchema) -> IndexChanges {
    let (before, after) = (wanted_indexes(id, old), wanted_indexes(id, new));
    let missing_from = |list: &[Wanted], w: &Wanted| !list.iter().any(|o| o.name == w.name);
    IndexChanges {
        create: after
            .iter()
            .filter(|w| missing_from(&before, w))
            .map(Wanted::label)
            .collect(),
        drop: before
            .iter()
            .filter(|w| missing_from(&after, w))
            .map(Wanted::label)
            .collect(),
    }
}

struct Row {
    id: Uuid,
    live: bool,
    /// The published copy, missing on a document never published.
    data: Option<Map<String, Value>>,
    draft: Option<Map<String, Value>>,
}

async fn batch(
    conn: &mut PgConnection,
    collection_id: Uuid,
    after: Option<Uuid>,
) -> Result<Vec<Row>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT id, deleted_at IS NULL AS "live!",
                  published AS "data: Json<Map<String, Value>>",
                  draft AS "draft: Json<Map<String, Value>>"
           FROM documents
           WHERE tenant_id = $1 AND collection_id = $2 AND ($3::uuid IS NULL OR id > $3)
           ORDER BY id
           LIMIT $4"#,
        DEFAULT_TENANT,
        collection_id,
        after,
        BATCH,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Row {
            id: r.id,
            live: r.live,
            data: r.data.map(|d| d.0),
            draft: r.draft.map(|d| d.0),
        })
        .collect())
}

/// One collection's or global's part of a change, expanded before and after: a schema
/// saved, or one a saved shared block reaches. Shared blocks have no documents, so they're
/// never parts.
struct Part {
    id: Uuid,
    slug: String,
    old: CollectionSchema,
    new: CollectionSchema,
    /// Its own renames, and a shared block's: those name fields within the block (`note`),
    /// so here they become paths into this schema (`layout.cta.note`).
    renames: Renames,
    /// The saves it's reported under: its own, or every saved shared block reaching it. A
    /// schema reached by several shows their combined change under each.
    owners: Vec<usize>,
    /// Saved itself, rather than only reached.
    saved: bool,
}

impl Preview {
    fn of(diff: SchemaDiff) -> Self {
        Self {
            diff,
            documents: 0,
            invalid: 0,
            examples: Vec::new(),
            duplicates: Vec::new(),
            needs_confirmation: false,
            cleared: 0,
            indexes: IndexChanges::default(),
            pruned: 0,
            collections: BTreeMap::new(),
        }
    }
}

/// A shared block's preview: its own change, and each schema it reaches. It loses data only
/// through those, so only they can need confirmation.
fn summarize(own: &SchemaDiff, reached: Vec<(String, Preview)>) -> Preview {
    let mut duplicates: Vec<String> = reached
        .iter()
        .flat_map(|(_, p)| p.duplicates.iter().cloned())
        .collect();
    duplicates.sort();
    duplicates.dedup();
    Preview {
        documents: reached.iter().map(|(_, p)| p.documents).sum(),
        invalid: reached.iter().map(|(_, p)| p.invalid).sum(),
        cleared: reached.iter().map(|(_, p)| p.cleared).sum(),
        pruned: reached.iter().map(|(_, p)| p.pruned).sum(),
        needs_confirmation: reached.iter().any(|(_, p)| p.needs_confirmation),
        duplicates,
        collections: reached.into_iter().collect(),
        ..Preview::of(own.clone())
    }
}

// NOTE: the whole change runs in one transaction; very large collections should move to a
// background job once the M4 job queue exists.
async fn run(
    pool: &PgPool,
    saves: &[Save<'_>],
    write: Option<bool>,
) -> Result<Vec<Outcome>, ChangeError> {
    // Several saves name the one at fault; with one, the caller knows which.
    let named = saves.len() > 1;
    let own = |slug: &str, e: SchemaError| {
        ChangeError::Invalid(if named {
            SchemaError::InvalidChange(format!("{slug}: {e}"))
        } else {
            e
        })
    };
    let breaks = |slug: &str, e: SchemaError| {
        ChangeError::Invalid(SchemaError::InvalidChange(format!(
            "this change would break {slug}: {e}"
        )))
    };
    for s in saves {
        if s.slug != s.schema.slug {
            return Err(own(
                s.slug,
                SchemaError::InvalidChange("renaming a collection isn't supported yet".into()),
            ));
        }
        crate::auth::check_users_schema(s.schema).map_err(|e| own(s.slug, e))?;
    }
    let mut tx = pool.begin().await?;
    if write.is_some() {
        // Every schema save holds R42's lock to its commit (W1-R16), so what's read below
        // stays true until then: no other schema, shared block or role changes meanwhile.
        crate::auth::lock_rule_roles(&mut tx).await?;
    }
    let stored = sqlx::query_as!(
        Collection,
        r#"SELECT id, schema AS "schema: Json<CollectionSchema>", schema_version, created_at, updated_at
           FROM collections WHERE tenant_id = $1 ORDER BY slug"#,
        DEFAULT_TENANT,
    )
    .fetch_all(&mut *tx)
    .await?;
    // Each save's stored row, or the one it'll create.
    let mut current = Vec::new();
    for s in saves {
        let found = stored.iter().find(|c| c.schema.slug == s.slug);
        current.push(match (found, s.new) {
            (Some(c), false) => c.clone(),
            (None, false) => return Err(ChangeError::NotFound),
            (Some(_), true) => {
                return Err(DbError::Conflict(format!("collection `{}`", s.slug)).into());
            }
            (None, true) => {
                let now = OffsetDateTime::now_utc();
                Collection {
                    id: Uuid::now_v7(),
                    schema: Json(s.schema.clone()),
                    schema_version: 1,
                    created_at: now,
                    updated_at: now,
                }
            }
        });
    }
    let saved = |slug: &str| saves.iter().position(|s| s.slug == slug);
    let before = |slug: &str| stored.iter().map(|c| &c.schema.0).find(|c| c.slug == slug);
    let after = |slug: &str| {
        saved(slug)
            .map(|i| saves[i].schema)
            .or_else(|| before(slug))
    };

    // Every schema as it'll be, expanded, with where each shared block went.
    let mut world: Vec<(CollectionSchema, Vec<(String, String)>)> = Vec::new();
    let new_slugs = saves.iter().filter(|s| s.new).map(|s| s.slug);
    for slug in stored
        .iter()
        .map(|c| c.schema.slug.as_str())
        .chain(new_slugs)
    {
        let raw = after(slug).expect("every slug listed has a schema");
        world.push(raw.expand_placed(after).map_err(|e| match saved(slug) {
            Some(_) => own(slug, e),
            None => breaks(slug, e),
        })?);
    }
    let blocks: Vec<&str> = saves
        .iter()
        .filter(|s| s.schema.kind == CollectionKind::Block)
        .map(|s| s.slug)
        .collect();
    let reaches = |placed: &[(String, String)]| {
        blocks
            .iter()
            .copied()
            .find(|b| placed.iter().any(|(_, s)| s == b))
    };

    // Rechecked here, under the lock, against what's stored now: a cache can be a change
    // behind, and a join, virtual or rule must never be stored pointing at a field that's
    // gone, such as one a shared block dropped.
    let id_of = |slug: &str| {
        saved(slug)
            .map(|i| current[i].id)
            .or_else(|| stored.iter().find(|c| c.schema.slug == slug).map(|c| c.id))
    };
    let mut targets: HashMap<String, Uuid> = HashMap::new();
    for (schema, _) in &world {
        let kept = schema.kind == CollectionKind::Collection
            || (saved(&schema.slug).is_some() && schema.kind != CollectionKind::Block);
        if kept && let Some(id) = id_of(&schema.slug) {
            targets.insert(schema.slug.clone(), id);
        }
    }
    let roles = sqlx::query_scalar!(
        "SELECT slug FROM roles WHERE tenant_id = $1",
        DEFAULT_TENANT
    )
    .fetch_all(&mut *tx)
    .await?;
    let roles: Vec<&str> = roles.iter().map(String::as_str).collect();
    let lookup = |slug: &str| {
        world
            .iter()
            .map(|(s, _)| s)
            .find(|s| s.slug == slug)
            .cloned()
    };
    let users = lookup(crate::auth::USERS)
        .map(|u| u.fields)
        .unwrap_or_default();
    for (schema, placed) in &world {
        let slug = schema.slug.as_str();
        let fail = |e: SchemaError| match saved(slug) {
            Some(_) => own(slug, e),
            None => breaks(slug, e),
        };
        if saved(slug).is_some() || reaches(placed).is_some() {
            schema.check().map_err(fail)?;
            let own_target = |t: &str| t == slug && schema.kind != CollectionKind::Block;
            if let Some(t) = schema
                .relation_targets()
                .into_iter()
                .find(|t| !targets.contains_key(*t) && !own_target(t))
            {
                return Err(fail(SchemaError::InvalidChange(format!(
                    "relationTo `{t}` doesn't match an existing collection"
                ))));
            }
        }
        schema
            .check_virtual_targets(lookup)
            .and_then(|()| schema.check_join_targets(lookup))
            .and_then(|()| check_rules(schema, &users, &roles, &lookup))
            .map_err(fail)?;
    }

    // The parts, and each saved shared block's own change.
    let mut parts = Vec::new();
    for (schema, placed) in &world {
        let Some(row) = stored.iter().find(|c| c.schema.slug == schema.slug) else {
            continue; // New: no documents yet.
        };
        if schema.kind == CollectionKind::Block {
            continue;
        }
        let (old, old_placed) = row
            .schema
            .expand_placed(before)
            .map_err(|e| breaks(&schema.slug, e))?;
        let owners: Vec<usize> = match saved(&schema.slug) {
            Some(i) => vec![i],
            None => blocks
                .iter()
                .filter(|b| placed.iter().chain(&old_placed).any(|(_, s)| s == *b))
                .map(|b| saved(b).expect("a reaching block is saved"))
                .collect(),
        };
        if owners.is_empty() {
            continue;
        }
        let mut renames = match saved(&schema.slug) {
            Some(i) => saves[i].renames.clone(),
            None => Renames::new(),
        };
        for (path, slug) in &old_placed {
            if let Some(b) = saved(slug).filter(|_| blocks.contains(&slug.as_str())) {
                for (from, to) in saves[b].renames {
                    renames.insert(format!("{path}.{slug}.{from}"), to.clone());
                }
            }
        }
        parts.push(Part {
            id: row.id,
            slug: schema.slug.clone(),
            old,
            new: schema.clone(),
            renames,
            owners,
            saved: saved(&schema.slug).is_some(),
        });
    }
    let mut own_diffs = Vec::new();
    for (s, c) in saves.iter().zip(&current) {
        own_diffs.push(if s.new || s.schema.kind != CollectionKind::Block {
            None
        } else {
            let old = c.schema.expand(before).map_err(|e| own(s.slug, e))?;
            let new = lookup(s.slug).expect("a save is in the world");
            Some(diff(&old, &new, s.renames).map_err(|e| own(s.slug, e))?)
        });
    }

    // Pass 1: what would happen to every live document.
    let mut previews = Vec::new();
    for part in &parts {
        if write.is_some() {
            // Document writes hold this row FOR KEY SHARE, so none interleaves from here on.
            sqlx::query!(
                "SELECT id FROM collections WHERE id = $1 FOR UPDATE",
                part.id
            )
            .fetch_one(&mut *tx)
            .await?;
        }
        let changes = diff(&part.old, &part.new, &part.renames).map_err(|e| match part.saved {
            true => own(&part.slug, e),
            false => breaks(&part.slug, e),
        })?;
        previews.push(scan(&mut tx, part, changes).await?);
    }
    let stored_differs: Vec<bool> = current
        .iter()
        .zip(saves)
        .map(|(c, s)| c.schema.0 != *s.schema)
        .collect();
    let entry = |i: usize, previews: &[Preview]| -> Preview {
        if saves[i].new {
            return Preview::of(SchemaDiff::default());
        }
        let mut p = match &own_diffs[i] {
            None => {
                let k = parts
                    .iter()
                    .position(|p| p.saved && p.owners.contains(&i))
                    .expect("a saved collection or global is a part");
                previews[k].clone()
            }
            Some(own) => summarize(
                own,
                parts
                    .iter()
                    .zip(previews)
                    .filter(|(p, _)| !p.saved && p.owners.contains(&i))
                    .map(|(p, preview)| (p.slug.clone(), preview.clone()))
                    .collect(),
            ),
        };
        // Swapping an inline block for a reference to the same definition stores nothing
        // new, but is still a change to save.
        if p.diff.is_empty() && stored_differs[i] {
            p.diff.changes.push(Change::LayoutChanged);
        }
        p
    };
    let out: Vec<Preview> = (0..saves.len()).map(|i| entry(i, &previews)).collect();
    let outcomes = |current: Vec<Collection>, out: Vec<Preview>| {
        current
            .into_iter()
            .zip(out)
            .zip(saves)
            .map(|((collection, preview), s)| Outcome {
                collection,
                created: s.new,
                preview,
            })
            .collect()
    };

    let Some(confirm) = write else {
        return Ok(outcomes(current, out));
    };
    let changed = |i: usize| saves[i].new || !out[i].diff.is_empty();
    if !(0..saves.len()).any(changed) {
        return Ok(outcomes(current, out));
    }
    if let Some(p) = out.iter().find(|p| !p.duplicates.is_empty()) {
        return Err(ChangeError::Duplicates(Box::new(p.clone())));
    }
    for part in parts.iter().filter(|p| p.old.drafts() && !p.new.drafts()) {
        check_no_pending_drafts(&mut tx, part.id).await?;
    }
    if !confirm && let Some(p) = out.iter().find(|p| p.needs_confirmation) {
        return Err(ChangeError::NeedsConfirmation(Box::new(p.clone())));
    }
    for s in saves {
        crate::auth::check_rule_roles(&mut tx, s.schema).await?;
    }
    // New ones first, so links into them are kept.
    for (i, s) in saves.iter().enumerate().filter(|(_, s)| s.new) {
        current[i] = crate::collections::insert(&mut tx, current[i].id, s.schema).await?;
    }

    // Pass 2.
    for k in 0..parts.len() {
        if previews[k].diff.is_empty() {
            continue;
        }
        let part = &parts[k];
        let drafts = (part.old.drafts(), part.new.drafts());
        if drafts == (true, false) {
            clear_drafts(&mut tx, part.id).await?;
        }
        if part.old.versions != part.new.versions {
            prune(&mut tx, part.id, kept(&part.new), true).await?;
        }
        if let Some(field) = rewrite(&mut tx, part, &targets, &previews[k].diff.links).await? {
            previews[k].duplicates = vec![field];
            return Err(ChangeError::Duplicates(Box::new(entry(
                part.owners[0],
                &previews,
            ))));
        }
        if drafts == (false, true) {
            seed_drafts(&mut tx, part.id).await?;
        }
        if !part.saved {
            // Its fields changed with a shared block, so writes validated against the old
            // ones must retry (see `documents::check_schema`).
            sqlx::query!(
                "UPDATE collections SET schema_version = schema_version + 1, updated_at = now() WHERE id = $1",
                part.id
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    for (i, s) in saves.iter().enumerate() {
        if s.new || !changed(i) {
            continue;
        }
        current[i] = sqlx::query_as!(
            Collection,
            r#"UPDATE collections
               SET schema = $2, schema_version = schema_version + 1, updated_at = now()
               WHERE id = $1
               RETURNING id, schema AS "schema: Json<CollectionSchema>", schema_version, created_at, updated_at"#,
            current[i].id,
            Json(s.schema) as _,
        )
        .fetch_one(&mut *tx)
        .await?;
    }
    for s in saves {
        // Delivered to every listening server when the transaction commits.
        sqlx::query!("SELECT pg_notify('tachyon_schema', $1)", s.slug)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(outcomes(current, out))
}

/// Pass 1 for one part: what its diff would do to every live document.
async fn scan(
    conn: &mut PgConnection,
    part: &Part,
    changes: SchemaDiff,
) -> Result<Preview, ChangeError> {
    let (old, new) = (&part.old, &part.new);
    let mut p = Preview {
        indexes: index_changes(part.id, old, new),
        ..Preview::of(changes)
    };
    let data = new.data();
    let unique: Vec<&str> = data
        .iter()
        .filter(|f| f.is_unique())
        .map(|f| f.name.as_str())
        .collect();
    // Relationships whose targets changed: links are reshaped, and dropped where their
    // collection is no longer allowed.
    let links = &p.diff.links;
    let mut seen: HashMap<&str, HashSet<String>> = HashMap::new();
    let mut duplicates = BTreeSet::new();
    let mut after = None;
    loop {
        let rows = batch(&mut *conn, part.id, after).await?;
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        for row in rows.iter().filter(|r| r.live) {
            p.documents += 1;
            // A draft is checked when it's published, and drafts needn't be unique.
            let Some(published) = &row.data else {
                continue;
            };
            let mut migrated = migrate_document(data, &part.renames, published);
            if relink(data, &mut migrated, links) {
                p.cleared += 1;
            }
            if let Err(errors) =
                validate_document(data, &Value::Object(Map::new()), Some(&migrated))
            {
                p.invalid += 1;
                if p.examples.len() < MAX_EXAMPLES {
                    p.examples.push(InvalidDocument { id: row.id, errors });
                }
            }
            for field in &unique {
                if let Some(value) = migrated.get(*field)
                    && !seen.entry(*field).or_default().insert(unique_key(value))
                {
                    duplicates.insert((*field).to_owned());
                }
            }
        }
    }
    p.duplicates = duplicates.into_iter().collect();
    if old.versions != new.versions {
        p.pruned = prune(&mut *conn, part.id, kept(new), false).await?;
    }
    p.needs_confirmation = p.diff.is_destructive() || p.invalid > 0 || p.pruned > 0;
    Ok(p)
}

/// How many versions a document keeps under `schema`: `None` for every one.
fn kept(schema: &CollectionSchema) -> Option<i64> {
    match &schema.versions {
        None => Some(0),
        Some(v) if v.max_versions == 0 => None,
        Some(v) => Some(v.max_versions.into()),
    }
}

/// Counts, or with `delete` deletes, a collection's versions past the newest `keep` of
/// each document. Never one a queued or running job holds, such as a scheduled publish's.
async fn prune(
    conn: &mut PgConnection,
    collection_id: Uuid,
    keep: Option<i64>,
    delete: bool,
) -> Result<i64, DbError> {
    let Some(keep) = keep else {
        return Ok(0);
    };
    let n = sqlx::query_scalar!(
        r#"WITH extra AS (
               SELECT id FROM (
                   SELECT id, row_number() OVER (
                       PARTITION BY document_id ORDER BY created_at DESC, id DESC) AS n
                   FROM versions WHERE tenant_id = $1 AND collection_id = $2
               ) ranked
               WHERE n > $3 AND NOT EXISTS (
                   SELECT 1 FROM jobs j
                   WHERE j.tenant_id = $1 AND j.state IN ('queued', 'running')
                     AND j.payload->>'version' = ranked.id::text)
           ),
           gone AS (DELETE FROM versions WHERE $4 AND id IN (SELECT id FROM extra) RETURNING 1)
           SELECT CASE WHEN $4 THEN (SELECT count(*) FROM gone)
                       ELSE (SELECT count(*) FROM extra) END AS "n!""#,
        DEFAULT_TENANT,
        collection_id,
        keep,
        delete,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(n)
}

/// Refuses turning drafts off while a document's draft isn't what's published, trashed
/// documents included: there's no way to do it without publishing or losing that work.
async fn check_no_pending_drafts(
    conn: &mut PgConnection,
    collection_id: Uuid,
) -> Result<(), ChangeError> {
    let row = sqlx::query!(
        r#"SELECT count(*) AS "n!", count(*) FILTER (WHERE deleted_at IS NOT NULL) AS "trashed!"
           FROM documents
           WHERE tenant_id = $1 AND collection_id = $2
             AND (draft IS DISTINCT FROM published OR published IS NULL)"#,
        DEFAULT_TENANT,
        collection_id,
    )
    .fetch_one(&mut *conn)
    .await?;
    let trashed = match row.trashed {
        0 => String::new(),
        m => format!(" ({m} in trash)"),
    };
    match row.n {
        0 => Ok(()),
        1 => Err(ChangeError::PendingDrafts(format!(
            "1 document has an unpublished draft{trashed}; publish or delete it first"
        ))),
        n => Err(ChangeError::PendingDrafts(format!(
            "{n} documents have unpublished drafts{trashed}; publish or delete them first"
        ))),
    }
}

/// One copy of a document, or a version, as `part.new` stores it: valid ones normalized,
/// invalid ones as migrated, to fix on next edit.
fn rewritten(part: &Part, links: &[LinkChange], data: &Map<String, Value>) -> Map<String, Value> {
    let fields = part.new.data();
    let mut migrated = migrate_document(fields, &part.renames, data);
    relink(fields, &mut migrated, links);
    validate_document(fields, &Value::Object(Map::new()), Some(&migrated)).unwrap_or(migrated)
}

/// Drafts turned off: every draft goes, with its links. Nothing is lost, since
/// `check_no_pending_drafts` found each one the same as what's published.
async fn clear_drafts(conn: &mut PgConnection, collection_id: Uuid) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM relations r USING documents d
         WHERE r.from_doc = d.id AND r.copy = 'draft' AND d.tenant_id = $1 AND d.collection_id = $2",
        DEFAULT_TENANT,
        collection_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE documents SET draft = NULL WHERE tenant_id = $1 AND collection_id = $2",
        DEFAULT_TENANT,
        collection_id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Drafts turned on: every document's draft starts as its published copy, links included,
/// trashed documents too.
async fn seed_drafts(conn: &mut PgConnection, collection_id: Uuid) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE documents SET draft = published WHERE tenant_id = $1 AND collection_id = $2",
        DEFAULT_TENANT,
        collection_id,
    )
    .execute(&mut *conn)
    .await?;
    // The rewrite synced live drafts from nothing; a trashed one may hold stale rows.
    sqlx::query!(
        "DELETE FROM relations r USING documents d
         WHERE r.from_doc = d.id AND r.copy = 'draft' AND d.tenant_id = $1 AND d.collection_id = $2",
        DEFAULT_TENANT,
        collection_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "INSERT INTO relations (tenant_id, from_doc, copy, field_path, to_doc)
         SELECT r.tenant_id, r.from_doc, 'draft', r.field_path, r.to_doc
         FROM relations r JOIN documents d ON d.id = r.from_doc
         WHERE r.copy = 'published' AND d.tenant_id = $1 AND d.collection_id = $2",
        DEFAULT_TENANT,
        collection_id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Pass 2 for one part: rewrites both copies of every document, deleted ones included so
/// Trash restores cleanly, and every version, then builds its new unique indexes. Returns
/// a unique field whose index found duplicate values.
async fn rewrite(
    conn: &mut PgConnection,
    part: &Part,
    targets: &HashMap<String, Uuid>,
    links: &[LinkChange],
) -> Result<Option<String>, ChangeError> {
    let new = &part.new;
    let mut after = None;
    loop {
        let rows = batch(&mut *conn, part.id, after).await?;
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        for row in rows {
            let published = row.data.as_ref().map(|d| rewritten(part, links, d));
            let draft = row.draft.as_ref().map(|d| rewritten(part, links, d));
            if published != row.data || draft != row.draft {
                sqlx::query!(
                    "UPDATE documents SET published = $2, draft = $3 WHERE id = $1",
                    row.id,
                    published.as_ref().map(Json) as _,
                    draft.as_ref().map(Json) as _,
                )
                .execute(&mut *conn)
                .await?;
            }
            // Deleted documents keep their old link rows; they re-sync when restored and saved (M4).
            if row.live {
                for (copy, data) in [(DocCopy::Published, &published), (DocCopy::Draft, &draft)] {
                    // Without drafts there are no draft rows to clear: skip the DELETE.
                    if copy == DocCopy::Draft && data.is_none() && !new.drafts() {
                        continue;
                    }
                    let links: Vec<Relation> = data
                        .iter()
                        .flat_map(|d| relations(new.data(), d))
                        .filter_map(|r| {
                            Some(Relation {
                                path: r.path,
                                at: r.at,
                                collection_id: *targets.get(&r.relation_to)?,
                                id: r.id,
                            })
                        })
                        .collect();
                    sync_relations(&mut *conn, row.id, copy, &links).await?;
                }
            }
        }
    }
    let mut after: Option<Uuid> = None;
    loop {
        let rows = sqlx::query!(
            r#"SELECT id, data AS "data: Json<Map<String, Value>>" FROM versions
               WHERE tenant_id = $1 AND collection_id = $2 AND ($3::uuid IS NULL OR id > $3)
               ORDER BY id
               LIMIT $4"#,
            DEFAULT_TENANT,
            part.id,
            after,
            BATCH,
        )
        .fetch_all(&mut *conn)
        .await?;
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        for v in rows {
            let data = rewritten(part, links, &v.data.0);
            if data != v.data.0 {
                sqlx::query!(
                    "UPDATE versions SET data = $2 WHERE id = $1",
                    v.id,
                    Json(&data) as _
                )
                .execute(&mut *conn)
                .await?;
            }
        }
    }

    // Build new unique indexes inside the transaction: a value that slipped past the scan
    // (or arrived in between) rolls the whole change back instead of leaving a unique field
    // without its constraint.
    // NOTE: a plain CREATE INDEX blocks writes to `documents` while it builds; fine at
    // today's sizes, revisit with the M4 job queue.
    let existing = existing_indexes(&mut *conn, part.id).await?;
    for w in wanted_indexes(part.id, new)
        .into_iter()
        .filter(|w| w.unique)
    {
        if existing.iter().any(|e| e.name == w.name && e.valid) {
            continue;
        }
        if existing.iter().any(|e| e.name == w.name) {
            ddl(&mut *conn, format!("DROP INDEX {}", w.name)).await?;
        }
        match ddl(
            &mut *conn,
            format!("CREATE UNIQUE INDEX {} {}", w.name, w.definition),
        )
        .await
        {
            Err(DbError::Sqlx(sqlx::Error::Database(e))) if e.is_unique_violation() => {
                return Ok(Some(w.field));
            }
            result => result?,
        }
    }
    Ok(None)
}

/// How a unique index sees a value: numbers by numeric value, so 1 and 1.0 collide.
fn unique_key(value: &Value) -> String {
    match value {
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), |f| f.to_string()),
        other => other.to_string(),
    }
}
