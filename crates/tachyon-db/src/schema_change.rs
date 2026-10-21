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
use crate::documents::{Relation, sync_relations};
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
    data: Map<String, Value>,
}

async fn batch(
    conn: &mut PgConnection,
    collection_id: Uuid,
    after: Option<Uuid>,
) -> Result<Vec<Row>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT id, deleted_at IS NULL AS "live!", published AS "data!: Json<Map<String, Value>>"
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
            data: r.data.0,
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
        if let Some(field) = rewrite(&mut tx, part, &targets, &previews[k].diff.links).await? {
            previews[k].duplicates = vec![field];
            return Err(ChangeError::Duplicates(Box::new(entry(
                part.owners[0],
                &previews,
            ))));
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
            let mut migrated = migrate_document(data, &part.renames, &row.data);
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
    p.needs_confirmation = p.diff.is_destructive() || p.invalid > 0;
    Ok(p)
}

/// Pass 2 for one part: rewrites every document, deleted ones included so Trash restores
/// cleanly, then builds its new unique indexes. Returns a unique field whose index found
/// duplicate values.
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
            let mut migrated = migrate_document(new.data(), &part.renames, &row.data);
            relink(new.data(), &mut migrated, links);
            // Valid documents are stored normalized; invalid ones as migrated, to fix on next edit.
            let stored = validate_document(new.data(), &Value::Object(Map::new()), Some(&migrated))
                .unwrap_or(migrated);
            if stored != row.data {
                sqlx::query!(
                    "UPDATE documents SET published = $2 WHERE id = $1",
                    row.id,
                    Json(&stored) as _
                )
                .execute(&mut *conn)
                .await?;
            }
            // Deleted documents keep their old link rows; they re-sync when restored and saved (M4).
            if row.live {
                let links: Vec<Relation> = relations(new.data(), &stored)
                    .into_iter()
                    .filter_map(|r| {
                        Some(Relation {
                            path: r.path,
                            collection_id: *targets.get(&r.relation_to)?,
                            id: r.id,
                        })
                    })
                    .collect();
                sync_relations(&mut *conn, row.id, &links).await?;
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
