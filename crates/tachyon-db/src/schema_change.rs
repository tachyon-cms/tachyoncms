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
use tachyon_core::diff::{Renames, SchemaDiff, diff, migrate_document};
use tachyon_core::{
    CollectionKind, CollectionSchema, FieldError, SchemaError, relations, validate_document,
};
use uuid::Uuid;

use crate::collections::Collection;
use crate::documents::{Relation, sync_relations};
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
    Duplicates(Preview),
    #[error("the change needs confirmation")]
    NeedsConfirmation(Preview),
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
) -> Result<Preview, ChangeError> {
    run(pool, slug, new, renames, None).await.map(|(_, p)| p)
}

pub async fn apply(
    pool: &PgPool,
    slug: &str,
    new: &CollectionSchema,
    renames: &Renames,
    confirm: bool,
) -> Result<(Collection, Preview), ChangeError> {
    run(pool, slug, new, renames, Some(confirm)).await
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
    write: Option<bool>,
) -> Result<(Collection, Preview), ChangeError> {
    new.check().map_err(ChangeError::Invalid)?;
    let mut tx = pool.begin().await?;
    let current = sqlx::query_as!(
        Collection,
        r#"SELECT id, schema AS "schema: Json<CollectionSchema>", schema_version, created_at, updated_at
           FROM collections WHERE tenant_id = $1 AND slug = $2
           FOR UPDATE"#,
        DEFAULT_TENANT,
        slug,
    )
    .fetch_optional(&mut *tx)
    .await?
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
        if !targets.contains_key(target) {
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
    };
    let unique: Vec<&str> = new
        .fields
        .iter()
        .filter(|f| f.is_unique())
        .map(|f| f.name.as_str())
        .collect();
    let mut seen: HashMap<&str, HashSet<String>> = HashMap::new();
    let mut duplicates = BTreeSet::new();
    let mut after = None;
    loop {
        let rows = batch(&mut tx, current.id, after).await?;
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        for row in rows.iter().filter(|r| r.live) {
            p.documents += 1;
            let migrated = migrate_document(&new.fields, renames, &row.data);
            if let Err(errors) =
                validate_document(&new.fields, &Value::Object(Map::new()), Some(&migrated))
            {
                p.invalid += 1;
                if p.examples.len() < MAX_EXAMPLES {
                    p.examples.push(InvalidDocument { id: row.id, errors });
                }
            }
            for field in &unique {
                if let Some(value) = migrated.get(*field)
                    && !seen.entry(*field).or_default().insert(value.to_string())
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
        return Err(ChangeError::Duplicates(p));
    }
    if p.needs_confirmation && !confirm {
        return Err(ChangeError::NeedsConfirmation(p));
    }

    // Pass 2: rewrite every document, deleted ones included so Trash restores cleanly.
    let mut after = None;
    loop {
        let rows = batch(&mut tx, current.id, after).await?;
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        for row in rows {
            let migrated = migrate_document(&new.fields, renames, &row.data);
            // Valid documents are stored normalized; invalid ones as migrated, to fix on next edit.
            let stored =
                validate_document(&new.fields, &Value::Object(Map::new()), Some(&migrated))
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
                let links: Vec<Relation> = relations(&new.fields, &stored)
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
