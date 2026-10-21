//! Changing a collection's fields after it has documents.
//!
//! One code path runs in two modes. Preview scans every document against the new fields
//! and rolls back. Apply does the same scan, decides, then rewrites every document,
//! re-syncs links, and stores the new schema, all in one transaction that holds the
//! collection row `FOR UPDATE`, so no document write can interleave.
//!
//! A shared block has no documents. Changing one changes every schema that names it, so
//! each of those runs the same passes, in the same transaction.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use tachyon_core::diff::{Change, Renames, SchemaDiff, diff, migrate_document, relink};
use tachyon_core::{
    CollectionKind, CollectionSchema, FieldError, SchemaError, relations, validate_document,
};
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

pub async fn preview(
    pool: &PgPool,
    slug: &str,
    new: &CollectionSchema,
    renames: &Renames,
    pending: &[&CollectionSchema],
) -> Result<Preview, ChangeError> {
    run(pool, slug, new, renames, pending, None)
        .await
        .map(|(_, p)| p)
}

pub async fn apply(
    pool: &PgPool,
    slug: &str,
    new: &CollectionSchema,
    renames: &Renames,
    confirm: bool,
) -> Result<(Collection, Preview), ChangeError> {
    run(pool, slug, new, renames, &[], Some(confirm)).await
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

/// One schema's part of a change, expanded before and after. A shared block's change has
/// one part per schema that names it; any other change has one, its own.
struct Part {
    id: Uuid,
    slug: String,
    old: CollectionSchema,
    new: CollectionSchema,
    /// A shared block's renames name fields within it (`note`); here they're the paths
    /// into this schema (`layout.cta.note`).
    renames: Renames,
}

// NOTE: the whole change runs in one transaction; very large collections should move to a
// background job once the M4 job queue exists.
async fn run(
    pool: &PgPool,
    slug: &str,
    new: &CollectionSchema,
    renames: &Renames,
    pending: &[&CollectionSchema],
    write: Option<bool>,
) -> Result<(Collection, Preview), ChangeError> {
    crate::auth::check_users_schema(new).map_err(ChangeError::Invalid)?;
    let mut tx = pool.begin().await?;
    if write.is_some() {
        // Every schema save holds R42's lock to its commit, so a shared block's save and the
        // saves of schemas naming it each see the other's result, never half of it.
        crate::auth::lock_rule_roles(&mut tx).await?;
    }
    // Only a real change locks the collection; a preview reads without blocking saves.
    let current = if write.is_some() {
        sqlx::query_as!(
            Collection,
            r#"SELECT id, schema AS "schema: Json<CollectionSchema>", schema_version, created_at, updated_at
               FROM collections WHERE tenant_id = $1 AND slug = $2
               FOR UPDATE"#,
            DEFAULT_TENANT,
            slug,
        )
        .fetch_optional(&mut *tx)
        .await?
    } else {
        sqlx::query_as!(
            Collection,
            r#"SELECT id, schema AS "schema: Json<CollectionSchema>", schema_version, created_at, updated_at
               FROM collections WHERE tenant_id = $1 AND slug = $2"#,
            DEFAULT_TENANT,
            slug,
        )
        .fetch_optional(&mut *tx)
        .await?
    }
    .ok_or(ChangeError::NotFound)?;
    let stored = sqlx::query!(
        r#"SELECT id, schema AS "schema: Json<CollectionSchema>" FROM collections
           WHERE tenant_id = $1 ORDER BY slug"#,
        DEFAULT_TENANT
    )
    .fetch_all(&mut *tx)
    .await?;

    // Every relationship must point at an existing collection (not a global or a block).
    let mut targets: HashMap<String, Uuid> = stored
        .iter()
        .filter(|c| c.schema.kind == CollectionKind::Collection)
        .map(|c| (c.schema.slug.clone(), c.id))
        .collect();
    if new.kind != CollectionKind::Block {
        targets.insert(slug.to_owned(), current.id);
    }

    // Shared blocks as stored, and as they'll be: with this change, and for a preview, with
    // those a schema file is about to create.
    let before = |s: &str| stored.iter().map(|c| &c.schema.0).find(|c| c.slug == s);
    let after = |s: &str| {
        if new.kind == CollectionKind::Block && s == new.slug {
            return Some(new);
        }
        before(s).or_else(|| pending.iter().copied().find(|p| p.slug == s))
    };
    let is_block =
        new.kind == CollectionKind::Block || current.schema.kind == CollectionKind::Block;
    let mut parts = Vec::new();
    // The block's own change. Also checks the kind stays and every rename names a field.
    let mut block_diff = None;
    if is_block {
        block_diff = Some(diff(&current.schema.0, new, renames).map_err(ChangeError::Invalid)?);
        new.check().map_err(ChangeError::Invalid)?;
        for c in stored
            .iter()
            .filter(|c| c.schema.block_refs().contains(&slug))
        {
            let breaks = |e: SchemaError| {
                ChangeError::Invalid(SchemaError::InvalidChange(format!(
                    "this change would break {}: {e}",
                    c.schema.slug
                )))
            };
            let paths = c.schema.block_ref_paths(slug);
            let part = Part {
                id: c.id,
                slug: c.schema.slug.clone(),
                old: c.schema.expand(before).map_err(breaks)?,
                new: c.schema.expand(after).map_err(breaks)?,
                renames: paths
                    .iter()
                    .flat_map(|p| {
                        renames
                            .iter()
                            .map(move |(from, to)| (format!("{p}.{slug}.{from}"), to.clone()))
                    })
                    .collect(),
            };
            part.new.check().map_err(breaks)?;
            parts.push(part);
        }
    } else {
        let new = new.expand(after).map_err(ChangeError::Invalid)?;
        new.check().map_err(ChangeError::Invalid)?;
        parts.push(Part {
            id: current.id,
            slug: slug.to_owned(),
            old: current
                .schema
                .expand(before)
                .map_err(ChangeError::Invalid)?,
            new,
            renames: renames.clone(),
        });
    }
    for target in parts
        .iter()
        .flat_map(|p| p.new.relation_targets())
        .chain(new.relation_targets())
    {
        if !targets.contains_key(target) && !pending.iter().any(|p| p.slug == target) {
            return Err(ChangeError::Invalid(SchemaError::InvalidChange(format!(
                "relationTo `{target}` doesn't match an existing collection"
            ))));
        }
    }

    // Pass 1: what would happen to every live document.
    let mut previews = Vec::new();
    for part in &parts {
        if write.is_some() && part.id != current.id {
            // Document writes hold this row FOR KEY SHARE, so none interleaves from here on.
            sqlx::query!(
                "SELECT id FROM collections WHERE id = $1 FOR UPDATE",
                part.id
            )
            .fetch_one(&mut *tx)
            .await?;
        }
        previews.push(scan(&mut tx, part).await?);
    }
    let p = if let Some(changes) = block_diff {
        let mut duplicates: Vec<String> = previews
            .iter()
            .flat_map(|p| p.duplicates.iter().cloned())
            .collect();
        duplicates.sort();
        duplicates.dedup();
        Preview {
            needs_confirmation: changes.is_destructive()
                || previews.iter().any(|p| p.needs_confirmation),
            diff: changes,
            documents: previews.iter().map(|p| p.documents).sum(),
            invalid: previews.iter().map(|p| p.invalid).sum(),
            examples: Vec::new(),
            duplicates,
            cleared: previews.iter().map(|p| p.cleared).sum(),
            indexes: IndexChanges::default(),
            collections: parts
                .iter()
                .map(|part| part.slug.clone())
                .zip(previews.iter().cloned())
                .collect(),
        }
    } else {
        let mut p = previews[0].clone();
        // Swapping an inline block for a reference to the same definition stores nothing
        // new, but is still a change to save.
        if p.diff.is_empty() && current.schema.0 != *new {
            p.diff.changes.push(Change::LayoutChanged);
        }
        p
    };

    let Some(confirm) = write else {
        return Ok((current, p));
    };
    if p.diff.is_empty() {
        return Ok((current, p));
    }
    if !p.duplicates.is_empty() {
        return Err(ChangeError::Duplicates(Box::new(p)));
    }
    if p.needs_confirmation && !confirm {
        return Err(ChangeError::NeedsConfirmation(Box::new(p)));
    }
    crate::auth::check_rule_roles(&mut tx, new).await?;

    for (part, preview) in parts.iter().zip(&mut previews) {
        rewrite(&mut tx, part, &targets, preview).await?;
        if part.id != current.id {
            // Its fields changed with the block, so writes validated against the old ones
            // must retry (see `documents::check_schema`).
            sqlx::query!(
                "UPDATE collections SET schema_version = schema_version + 1, updated_at = now() WHERE id = $1",
                part.id
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    let updated = sqlx::query_as!(
        Collection,
        r#"UPDATE collections
           SET schema = $2, schema_version = schema_version + 1, updated_at = now()
           WHERE id = $1
           RETURNING id, schema AS "schema: Json<CollectionSchema>", schema_version, created_at, updated_at"#,
        current.id,
        Json(new) as _,
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!("SELECT pg_notify('tachyon_schema', $1)", slug)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((updated, p))
}

/// Pass 1 for one part: its diff, and what would happen to every live document.
async fn scan(conn: &mut PgConnection, part: &Part) -> Result<Preview, ChangeError> {
    let (old, new) = (&part.old, &part.new);
    let changes = diff(old, new, &part.renames).map_err(ChangeError::Invalid)?;
    let mut p = Preview {
        diff: changes,
        documents: 0,
        invalid: 0,
        examples: Vec::new(),
        duplicates: Vec::new(),
        needs_confirmation: false,
        cleared: 0,
        indexes: index_changes(part.id, old, new),
        collections: BTreeMap::new(),
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
/// cleanly, then builds its new unique indexes.
async fn rewrite(
    conn: &mut PgConnection,
    part: &Part,
    targets: &HashMap<String, Uuid>,
    p: &mut Preview,
) -> Result<(), ChangeError> {
    let new = &part.new;
    let mut after = None;
    loop {
        let rows = batch(&mut *conn, part.id, after).await?;
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        for row in rows {
            let mut migrated = migrate_document(new.data(), &part.renames, &row.data);
            relink(new.data(), &mut migrated, &p.diff.links);
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
                p.duplicates = vec![w.field];
                return Err(ChangeError::Duplicates(Box::new(p.clone())));
            }
            result => result?,
        }
    }
    Ok(())
}

/// How a unique index sees a value: numbers by numeric value, so 1 and 1.0 collide.
fn unique_key(value: &Value) -> String {
    match value {
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), |f| f.to_string()),
        other => other.to_string(),
    }
}
