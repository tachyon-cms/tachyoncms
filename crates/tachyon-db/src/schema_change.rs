//! Changing a collection's fields after it has documents.
//!
//! One code path runs in two modes. Preview scans every document against the new fields
//! and rolls back. Apply does the same scan, decides, then rewrites every document,
//! re-syncs links, and stores the new schema, all in one transaction that holds the
//! collection row `FOR UPDATE`, so no document write can interleave.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use tachyon_core::diff::{Renames, SchemaDiff, diff, migrate_document, relink};
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
    pending: &[&str],
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

// NOTE: the whole change runs in one transaction; very large collections should move to a
// background job once the M4 job queue exists.
async fn run(
    pool: &PgPool,
    slug: &str,
    new: &CollectionSchema,
    renames: &Renames,
    pending: &[&str],
    write: Option<bool>,
) -> Result<(Collection, Preview), ChangeError> {
    new.check().map_err(ChangeError::Invalid)?;
    crate::auth::check_users_schema(new).map_err(ChangeError::Invalid)?;
    let mut tx = pool.begin().await?;
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
    let changes = diff(&current.schema.0, new, renames).map_err(ChangeError::Invalid)?;

    // Every relationship must point at an existing collection (not a global).
    let others = sqlx::query!(
        "SELECT id, slug, kind FROM collections WHERE tenant_id = $1",
        DEFAULT_TENANT
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut targets: HashMap<String, Uuid> = others
        .iter()
        .filter(|c| c.kind == CollectionKind::Collection.as_str())
        .map(|c| (c.slug.clone(), c.id))
        .collect();
    targets.insert(slug.to_owned(), current.id);
    for target in new.relation_targets() {
        if !targets.contains_key(target) && !pending.contains(&target) {
            return Err(ChangeError::Invalid(SchemaError::InvalidChange(format!(
                "relationTo `{target}` doesn't match an existing collection"
            ))));
        }
    }

    // Pass 1: what would happen to every live document.
    let mut p = Preview {
        diff: changes,
        documents: 0,
        invalid: 0,
        examples: Vec::new(),
        duplicates: Vec::new(),
        needs_confirmation: false,
        cleared: 0,
        indexes: index_changes(current.id, &current.schema.0, new),
    };
    let data = new.data();
    let unique: Vec<&str> = data
        .iter()
        .filter(|f| f.is_unique())
        .map(|f| f.name.as_str())
        .collect();
    // Relationships whose targets changed: links are reshaped, and dropped where their
    // collection is no longer allowed.
    let links = p.diff.links.clone();
    let mut seen: HashMap<&str, HashSet<String>> = HashMap::new();
    let mut duplicates = BTreeSet::new();
    let mut after = None;
    loop {
        let rows = batch(&mut tx, current.id, after).await?;
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        for row in rows.iter().filter(|r| r.live) {
            p.documents += 1;
            let mut migrated = migrate_document(new.data(), renames, &row.data);
            if relink(&mut migrated, &links) {
                p.cleared += 1;
            }
            if let Err(errors) =
                validate_document(new.data(), &Value::Object(Map::new()), Some(&migrated))
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

    // Pass 2: rewrite every document, deleted ones included so Trash restores cleanly.
    let mut after = None;
    loop {
        let rows = batch(&mut tx, current.id, after).await?;
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        for row in rows {
            let mut migrated = migrate_document(new.data(), renames, &row.data);
            relink(&mut migrated, &links);
            // Valid documents are stored normalized; invalid ones as migrated, to fix on next edit.
            let stored = validate_document(new.data(), &Value::Object(Map::new()), Some(&migrated))
                .unwrap_or(migrated);
            if stored != row.data {
                sqlx::query!(
                    "UPDATE documents SET published = $2 WHERE id = $1",
                    row.id,
                    Json(&stored) as _
                )
                .execute(&mut *tx)
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
                sync_relations(&mut tx, row.id, &links).await?;
            }
        }
    }

    // Build new unique indexes inside the transaction: a value that slipped past the scan
    // (or arrived in between) rolls the whole change back instead of leaving a unique field
    // without its constraint.
    // NOTE: a plain CREATE INDEX blocks writes to `documents` while it builds; fine at
    // today's sizes, revisit with the M4 job queue.
    let existing = existing_indexes(&mut tx, current.id).await?;
    for w in wanted_indexes(current.id, new)
        .into_iter()
        .filter(|w| w.unique)
    {
        if existing.iter().any(|e| e.name == w.name && e.valid) {
            continue;
        }
        if existing.iter().any(|e| e.name == w.name) {
            ddl(&mut tx, format!("DROP INDEX {}", w.name)).await?;
        }
        match ddl(
            &mut tx,
            format!("CREATE UNIQUE INDEX {} {}", w.name, w.definition),
        )
        .await
        {
            Err(DbError::Sqlx(sqlx::Error::Database(e))) if e.is_unique_violation() => {
                p.duplicates = vec![w.field];
                return Err(ChangeError::Duplicates(Box::new(p)));
            }
            result => result?,
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

/// How a unique index sees a value: numbers by numeric value, so 1 and 1.0 collide.
fn unique_key(value: &Value) -> String {
    match value {
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), |f| f.to_string()),
        other => other.to_string(),
    }
}
