//! The folders system collection: documents of any collection with `folders` on link to
//! one through their `folder` field, and folders nest through theirs.

use serde_json::{Map, Value, json};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use tachyon_core::{CollectionKind, CollectionSchema, FieldKind, SchemaError, SelectOption};
use uuid::Uuid;

use crate::collections::{self, Collection};
use crate::{DEFAULT_TENANT, DbError, DocCopy};

pub use tachyon_core::FOLDERS;

/// How many folders may sit above one.
pub const MAX_DEPTH: i32 = 64;

/// The `folders` collection, created the first time it's needed, listing the collections
/// that file into folders now.
pub async fn ensure_folders_collection(pool: &PgPool) -> Result<Collection, DbError> {
    match collections::get_by_slug(pool, FOLDERS).await {
        Ok(c) => return Ok(c),
        Err(DbError::NotFound) => {}
        Err(e) => return Err(e),
    }
    let enabled: Vec<String> = collections::list(pool)
        .await?
        .iter()
        .map(|c| &c.schema.0)
        .filter(|s| s.folders && s.kind == CollectionKind::Collection)
        .map(|s| s.slug.clone())
        .collect();
    let schema: CollectionSchema = serde_json::from_value(json!({
        "slug": FOLDERS,
        "fields": [
            {"name": "name", "type": "text", "required": true},
            {"name": "folder", "type": "relationship", "relationTo": FOLDERS},
            {"name": "collections", "type": "select", "multiple": true, "options": enabled},
        ]
    }))
    .expect("the folders schema is valid");
    match collections::create(pool, &schema).await {
        Ok(c) => Ok(c),
        // Another server got there first.
        Err(DbError::Conflict(_)) => collections::get_by_slug(pool, FOLDERS).await,
        Err(e) => Err(e),
    }
}

/// The folders collection is the system's: a schema can't create one (`new`), and a change
/// keeps its `name`, `folder` and `collections` as they are made. It keeps no versions,
/// since restoring one could put a folder inside itself.
pub fn check_folders_schema(schema: &CollectionSchema, new: bool) -> Result<(), SchemaError> {
    if schema.slug != FOLDERS {
        return Ok(());
    }
    if new {
        return Err(SchemaError::At {
            path: "slug".into(),
            message:
                "folders is built in; change its fields with PUT /api/admin/collections/folders"
                    .into(),
        });
    }
    if schema.versions.is_some() {
        return Err(SchemaError::InvalidOption(
            "versions: folders can't have versions or drafts, since restoring one could put a folder inside itself".into(),
        ));
    }
    let top = |name: &str| {
        schema
            .data()
            .iter()
            .find(|f| f.name == name && f.virtual_path.is_none())
    };
    let kept = [
        (
            "name",
            "folders need their text field called name",
            top("name").is_some_and(|f| {
                matches!(
                    f.kind,
                    FieldKind::Text {
                        has_many: false,
                        ..
                    }
                )
            }),
        ),
        (
            "folder",
            "folders need their folder field, a relationship to one folder",
            top("folder").is_some_and(|f| {
                f.kind
                    .link()
                    .is_some_and(|l| !l.has_many && l.relation_to.targets() == [FOLDERS])
                    && f.kind.name() == "relationship"
            }),
        ),
        (
            "collections",
            "folders need their collections field, a select of several",
            top("collections")
                .is_some_and(|f| matches!(f.kind, FieldKind::Select { multiple: true, .. })),
        ),
    ];
    match kept.iter().find(|(_, _, ok)| !ok) {
        Some((field, message, _)) => Err(SchemaError::InvalidField {
            field: (*field).into(),
            message: (*message).into(),
        }),
        None => Ok(()),
    }
}

/// Lists `enabled`, the collections that file into folders, as the options of the folders'
/// `collections`, and drops stored values naming any other. Inside a schema change, which
/// holds the schema lock; the folders' writes retry against the new options.
pub(crate) async fn refresh_in(
    conn: &mut PgConnection,
    mut enabled: Vec<String>,
) -> Result<(), DbError> {
    let Some(row) = sqlx::query!(
        r#"SELECT id, schema AS "schema: Json<CollectionSchema>" FROM collections
           WHERE tenant_id = $1 AND slug = $2 FOR UPDATE"#,
        DEFAULT_TENANT,
        FOLDERS,
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(());
    };
    enabled.sort();
    let mut schema = row.schema.0;
    let Some(FieldKind::Select { options, .. }) = schema
        .fields
        .iter_mut()
        .find(|f| f.name == "collections")
        .map(|f| &mut f.kind)
    else {
        return Ok(());
    };
    if options
        .iter()
        .map(SelectOption::value)
        .eq(enabled.iter().map(String::as_str))
    {
        return Ok(());
    }
    *options = enabled.iter().cloned().map(SelectOption::Value).collect();
    sqlx::query!(
        "UPDATE collections SET schema = $2, schema_version = schema_version + 1, updated_at = now() WHERE id = $1",
        row.id,
        Json(&schema) as _,
    )
    .execute(&mut *conn)
    .await?;
    let docs = sqlx::query!(
        r#"SELECT id, published AS "published: Json<Map<String, Value>>",
                  draft AS "draft: Json<Map<String, Value>>"
           FROM documents WHERE tenant_id = $1 AND collection_id = $2
             AND (published ? 'collections' OR draft ? 'collections')"#,
        DEFAULT_TENANT,
        row.id,
    )
    .fetch_all(&mut *conn)
    .await?;
    let keep = |copy: Option<Json<Map<String, Value>>>| {
        copy.map(|Json(mut doc)| {
            if let Some(Value::Array(listed)) = doc.get_mut("collections") {
                listed.retain(|v| v.as_str().is_some_and(|s| enabled.iter().any(|e| e == s)));
                // None left is no restriction, as a folder made without any.
                if listed.is_empty() {
                    doc.remove("collections");
                }
            }
            Json(doc)
        })
    };
    for d in docs {
        sqlx::query!(
            "UPDATE documents SET published = $2, draft = $3 WHERE id = $1",
            d.id,
            keep(d.published) as _,
            keep(d.draft) as _,
        )
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query!("SELECT pg_notify('tachyon_schema', $1)", FOLDERS)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Holds every change to where folders sit until `conn`'s transaction ends, so two moves
/// at once can't each pass `check_place_in` and make a loop between them.
pub async fn lock_tree_in(conn: &mut PgConnection) -> Result<(), DbError> {
    sqlx::query!("SELECT pg_advisory_xact_lock(hashtext('tachyon_folders'))")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `id` and the folders above it, nearest first, as `conn` sees them: up to `MAX_DEPTH`
/// above it and one more, so a deeper chain shows, and stopping where a loop comes back.
pub async fn ancestors_in(conn: &mut PgConnection, id: Uuid) -> Result<Vec<Uuid>, DbError> {
    let up = sqlx::query_scalar!(
        r#"WITH RECURSIVE up(id, depth) AS (
               SELECT $1::uuid, 0
               UNION ALL
               SELECT r.to_doc, up.depth + 1 FROM up
               JOIN relations r ON r.from_doc = up.id AND r.tenant_id = $2
                 AND r.copy = 'published' AND r.field_path = 'folder'
               WHERE up.depth <= $3
           )
           SELECT id AS "id!" FROM up ORDER BY depth"#,
        id,
        DEFAULT_TENANT,
        MAX_DEPTH,
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for f in up {
        if out.contains(&f) {
            out.push(f);
            break;
        }
        out.push(f);
    }
    Ok(out)
}

/// Why folder `id`, as just written in `conn`, can't sit where it is: inside itself, or
/// under more than `MAX_DEPTH` folders. Run under `lock_tree_in`.
pub async fn check_place_in(conn: &mut PgConnection, id: Uuid) -> Result<(), DbError> {
    let up = ancestors_in(conn, id).await?;
    let message = if up[1..].contains(&id) {
        "can't be this folder or one inside it"
    } else if up.len() > MAX_DEPTH as usize + 1 {
        "is too deep; folders nest 64 levels at most"
    } else {
        return Ok(());
    };
    Err(DbError::Validation(vec![tachyon_core::FieldError {
        path: "folder".into(),
        message: message.into(),
    }]))
}

/// Locks live folder `id` for deleting, and returns what files into it: each document
/// (collection and ID) and copy whose `folder` names it.
pub async fn contents_in(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Vec<(Uuid, Uuid, DocCopy)>, DbError> {
    sqlx::query_scalar!(
        "SELECT id FROM documents WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL FOR UPDATE",
        DEFAULT_TENANT,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DbError::NotFound)?;
    let rows = sqlx::query!(
        r#"SELECT d.collection_id, d.id, r.copy FROM relations r
           JOIN documents d ON d.id = r.from_doc
           WHERE r.tenant_id = $1 AND r.to_doc = $2 AND r.field_path = 'folder'
             AND r.from_doc <> $2 AND d.deleted_at IS NULL
           ORDER BY d.id, r.copy"#,
        DEFAULT_TENANT,
        id,
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            let copy = DocCopy::parse(&r.copy).ok_or_else(|| {
                DbError::Sqlx(sqlx::Error::Decode(
                    format!("unknown document copy `{}`", r.copy).into(),
                ))
            })?;
            Ok((r.collection_id, r.id, copy))
        })
        .collect()
}
