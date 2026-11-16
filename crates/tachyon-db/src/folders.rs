//! The folders system collection: documents of any collection with `folders` on link to
//! one through their `folder` field, and folders nest through theirs.

use serde_json::{Value, json};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use tachyon_core::{CollectionKind, CollectionSchema, FieldKind, SchemaError, SelectOption};
use uuid::Uuid;

use crate::collections::{self, Collection};
use crate::{DEFAULT_TENANT, DbError, DocCopy};

pub use tachyon_core::FOLDERS;

/// How many folders may sit above one.
pub const MAX_DEPTH: i32 = 64;

/// The slugs a folder's `collections` may name: every collection, `users` too, whether or
/// not it has folders on, except the folders themselves. A slug never leaves the list when
/// folders are turned off, so no folder's list is ever rewritten wider.
pub fn listable(
    schemas: impl IntoIterator<Item = impl std::borrow::Borrow<CollectionSchema>>,
) -> Vec<String> {
    let mut out: Vec<String> = schemas
        .into_iter()
        .filter(|s| {
            let s = s.borrow();
            s.kind == CollectionKind::Collection && s.slug != FOLDERS
        })
        .map(|s| s.borrow().slug.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The folders collection as the system makes it, its `collections` listing `options`.
pub fn system_schema(options: Vec<String>) -> CollectionSchema {
    serde_json::from_value(json!({
        "slug": FOLDERS,
        "fields": [
            {"name": "name", "type": "text", "required": true},
            {"name": "folder", "type": "relationship", "relationTo": FOLDERS},
            {"name": "collections", "type": "select", "multiple": true, "options": options},
        ]
    }))
    .expect("the folders schema is valid")
}

/// The `folders` collection, created the first time it's needed.
pub async fn ensure_folders_collection(pool: &PgPool) -> Result<Collection, DbError> {
    match collections::get_by_slug(pool, FOLDERS).await {
        Ok(c) => return Ok(c),
        Err(DbError::NotFound) => {}
        Err(e) => return Err(e),
    }
    let all = collections::list(pool).await?;
    let schema = system_schema(listable(all.iter().map(|c| &c.schema.0)));
    match collections::create(pool, &schema).await {
        Ok(c) => Ok(c),
        // Another server got there first.
        Err(DbError::Conflict(_)) => collections::get_by_slug(pool, FOLDERS).await,
        Err(e) => Err(e),
    }
}

/// `ensure_folders_collection` inside a schema change, which holds the schema lock (or, for
/// a preview, rolls back): so a change that turns folders on before any server has made
/// them works, and a preview of it creates nothing.
pub(crate) async fn ensure_in(conn: &mut PgConnection) -> Result<(), DbError> {
    let found = sqlx::query_scalar!(
        "SELECT id FROM collections WHERE tenant_id = $1 AND slug = $2",
        DEFAULT_TENANT,
        FOLDERS,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if found.is_none() {
        collections::insert(conn, Uuid::now_v7(), &system_schema(Vec::new())).await?;
    }
    Ok(())
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
    if schema.upload.is_some() {
        return Err(SchemaError::At {
            path: "upload".into(),
            message: "folders can't hold files, since every folder would then need one".into(),
        });
    }
    // At the top, not in a row or tab, where the refresh and the read filter find them.
    let top = |name: &str| {
        schema
            .fields
            .iter()
            .find(|f| f.name == name && f.virtual_path.is_none())
    };
    let kept = [
        (
            "name",
            "folders need their text field called name, at the top level",
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
            "folders need their folder field, a relationship to one folder, at the top level",
            top("folder").is_some_and(|f| {
                f.kind
                    .link()
                    .is_some_and(|l| !l.has_many && l.relation_to.targets() == [FOLDERS])
                    && f.kind.name() == "relationship"
            }),
        ),
        (
            "collections",
            "folders need their collections field, a select of several, at the top level",
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

/// Lists `listable`, the collections a folder may name, as the options of the folders'
/// `collections`, and returns the folders collection when that changed it. Inside a schema
/// change, which holds the schema lock; the folders' writes retry against the new options.
/// Stored lists are never rewritten: no slug leaves the options while collections can't
/// be deleted.
pub(crate) async fn refresh_in(
    conn: &mut PgConnection,
    listable: Vec<String>,
) -> Result<Option<Collection>, DbError> {
    let Some(row) = sqlx::query!(
        r#"SELECT id, schema AS "schema: Json<CollectionSchema>" FROM collections
           WHERE tenant_id = $1 AND slug = $2 FOR UPDATE"#,
        DEFAULT_TENANT,
        FOLDERS,
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let mut schema = row.schema.0;
    let Some(FieldKind::Select { options, .. }) = schema
        .fields
        .iter_mut()
        .find(|f| f.name == "collections")
        .map(|f| &mut f.kind)
    else {
        return Ok(None);
    };
    if options
        .iter()
        .map(SelectOption::value)
        .eq(listable.iter().map(String::as_str))
    {
        return Ok(None);
    }
    *options = listable.into_iter().map(SelectOption::Value).collect();
    let saved = sqlx::query_as!(
        Collection,
        r#"UPDATE collections
           SET schema = $2, schema_version = schema_version + 1, updated_at = now()
           WHERE id = $1
           RETURNING id, schema AS "schema: Json<CollectionSchema>", schema_version, created_at, updated_at"#,
        row.id,
        Json(&schema) as _,
    )
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query!("SELECT pg_notify('tachyon_schema', $1)", FOLDERS)
        .execute(&mut *conn)
        .await?;
    Ok(Some(saved))
}

/// Holds every change to where folders sit until `conn`'s transaction ends, so two moves
/// at once can't each pass `check_place_in` and make a loop between them.
pub async fn lock_tree_in(conn: &mut PgConnection) -> Result<(), DbError> {
    sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        format!("folders:{DEFAULT_TENANT}"),
    )
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

/// How many levels of folders sit below live folder `id`: 0 with none. Counted past
/// `MAX_DEPTH` only by one, enough to tell it's too many.
async fn height_in(conn: &mut PgConnection, id: Uuid) -> Result<i32, DbError> {
    Ok(sqlx::query_scalar!(
        r#"WITH RECURSIVE down(id, depth) AS (
               SELECT $1::uuid, 0
               UNION ALL
               SELECT r.from_doc, down.depth + 1 FROM down
               JOIN relations r ON r.to_doc = down.id AND r.tenant_id = $2
                 AND r.copy = 'published' AND r.field_path = 'folder'
               JOIN documents d ON d.id = r.from_doc AND d.deleted_at IS NULL
                 AND d.collection_id = (SELECT collection_id FROM documents WHERE id = $1)
               WHERE down.depth <= $3
           )
           SELECT max(depth) AS "height!" FROM down"#,
        id,
        DEFAULT_TENANT,
        MAX_DEPTH,
    )
    .fetch_one(&mut *conn)
    .await?)
}

/// Why folder `id`, as just written in `conn`, can't sit where it is: inside itself, or
/// with its deepest folder (itself, or one inside it) under more than `MAX_DEPTH` others.
/// Run under `lock_tree_in`.
pub async fn check_place_in(conn: &mut PgConnection, id: Uuid) -> Result<(), DbError> {
    let up = ancestors_in(conn, id).await?;
    let message = if up[1..].contains(&id) {
        "can't be this folder or one inside it"
    } else if up.len() - 1 + height_in(conn, id).await? as usize > MAX_DEPTH as usize {
        "would put a folder too deep; folders nest 64 levels at most"
    } else {
        return Ok(());
    };
    Err(DbError::Validation(vec![tachyon_core::FieldError {
        path: "folder".into(),
        message: message.into(),
    }]))
}

/// What files into a folder: a document (collection and ID), the copy whose `folder`
/// names it, and whether the document is in the trash.
pub struct Content {
    pub collection_id: Uuid,
    pub id: Uuid,
    pub copy: DocCopy,
    pub trashed: bool,
}

/// Locks live folder `id` for deleting, and returns what files into it, trashed documents
/// included: they'd otherwise be left linking to a folder in the trash.
pub async fn contents_in(conn: &mut PgConnection, id: Uuid) -> Result<Vec<Content>, DbError> {
    sqlx::query_scalar!(
        "SELECT id FROM documents WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL FOR UPDATE",
        DEFAULT_TENANT,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DbError::NotFound)?;
    let rows = sqlx::query!(
        r#"SELECT d.collection_id, d.id, r.copy, d.deleted_at IS NOT NULL AS "trashed!"
           FROM relations r
           JOIN documents d ON d.id = r.from_doc
           WHERE r.tenant_id = $1 AND r.to_doc = $2 AND r.field_path = 'folder'
             AND r.from_doc <> $2
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
            Ok(Content {
                collection_id: r.collection_id,
                id: r.id,
                copy,
                trashed: r.trashed,
            })
        })
        .collect()
}

/// The collections folder `id` lists, as `conn` sees it: empty when it lists none (any
/// collection), `None` when there's no such live folder.
pub async fn lists_in(conn: &mut PgConnection, id: Uuid) -> Result<Option<Vec<String>>, DbError> {
    let listed = sqlx::query_scalar!(
        r#"SELECT published -> 'collections' AS "listed: Json<Value>" FROM documents
           WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL"#,
        DEFAULT_TENANT,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(listed.map(|l| match l {
        Some(Json(Value::Array(slugs))) => slugs
            .iter()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }))
}

/// Refuses `copy` of document `from_doc`, its links just stored, filed in a folder whose
/// `collections` lists others but not its own collection; one that lists none holds any.
/// Only a write that changes the copy's folder is judged: a create, an edit or a move into
/// one. A publish or a restore keeps what was judged when it was filed, and a folder whose
/// list narrows keeps what's already inside. Folders nest whatever they list.
pub async fn check_filed_in(
    conn: &mut PgConnection,
    from_doc: Uuid,
    copy: DocCopy,
) -> Result<(), DbError> {
    let refused = sqlx::query_scalar!(
        r#"SELECT fc.slug FROM relations r
           JOIN documents f ON f.id = r.from_doc
           JOIN collections fc ON fc.id = f.collection_id AND fc.slug <> $3
           JOIN documents t ON t.id = r.to_doc
           JOIN collections tc ON tc.id = t.collection_id AND tc.slug = $3
           WHERE r.tenant_id = $4 AND r.from_doc = $1 AND r.copy = $2 AND r.field_path = 'folder'
             AND jsonb_typeof(t.published -> 'collections') = 'array'
             AND jsonb_array_length(t.published -> 'collections') > 0
             AND NOT (t.published -> 'collections') ? fc.slug"#,
        from_doc,
        copy.as_str(),
        FOLDERS,
        DEFAULT_TENANT,
    )
    .fetch_optional(&mut *conn)
    .await?;
    match refused {
        Some(slug) => Err(DbError::Validation(vec![tachyon_core::FieldError {
            path: "folder".into(),
            message: format!("this folder doesn't hold {slug}"),
        }])),
        None => Ok(()),
    }
}
