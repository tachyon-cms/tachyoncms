//! Writes to the folders collection: where a folder sits, and deleting one with its
//! contents moved (`?moveContentsTo`). Who may read a folder is `Access::folder_reads`.

use std::collections::HashMap;

use serde_json::{Value, json};
use sqlx::PgConnection;
use tachyon_core::access::Compiled;
use tachyon_core::auth::Action;
use tachyon_core::query::Filter;
use tachyon_core::{FOLDERS, FieldError, merged, validate_document_in};
use tachyon_db::DbError;
use tachyon_db::DocCopy;
use tachyon_db::auth::USERS;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Actor};
use tachyon_db::folders::{self, Content};
use uuid::Uuid;

use crate::access::{Access, both};
use crate::documents::{delete_error, mode_for};
use crate::populate::{link_targets, prepare};
use crate::{ApiError, AppState};

/// Whether a write of `body` to `c` may move a folder, so must be checked under the lock.
pub(crate) fn moves(c: &Collection, body: &Value) -> bool {
    c.schema.slug == FOLDERS && body.get("folder").is_some()
}

/// An empty `collections` is stored as none: both mean any collection with folders, and
/// the read filter can only tell a missing value apart.
pub(crate) fn normalise(c: &Collection, body: &mut Value) {
    if c.schema.slug == FOLDERS && body.get("collections") == Some(&json!([])) {
        body["collections"] = Value::Null;
    }
}

/// Takes the lock every change of where folders sit takes, before the write.
pub(crate) async fn lock(tx: &mut PgConnection) -> Result<(), ApiError> {
    Ok(folders::lock_tree_in(tx).await?)
}

/// Refuses folder `id`, as just written in `tx`, inside itself or too deep.
pub(crate) async fn check_place(tx: &mut PgConnection, id: Uuid) -> Result<(), ApiError> {
    Ok(folders::check_place_in(tx, id).await?)
}

fn bad_move(message: &str) -> ApiError {
    ApiError::Validation(vec![FieldError {
        path: "moveContentsTo".into(),
        message: message.into(),
    }])
}

/// Judges a write of `copy` of document `id` of `c` that moved it from folder `before` to
/// `after` (the stored values, `None` for none): filed into a folder that lists others but
/// not `c`, it's refused. Unchanged, it isn't judged, so a folder whose list narrowed keeps
/// what's already in it editable.
pub(crate) async fn check_filed(
    tx: &mut PgConnection,
    c: &Collection,
    id: Uuid,
    copy: DocCopy,
    before: Option<&Value>,
    after: Option<&Value>,
) -> Result<(), ApiError> {
    let after = after.filter(|v| !v.is_null());
    if !c.schema.folders || c.schema.slug == FOLDERS || after.is_none() || before == after {
        return Ok(());
    }
    Ok(folders::check_filed_in(tx, id, copy).await?)
}

/// Deletes folder `id` of `c` (under `guard`, judging `copy`), first moving what files into
/// it, documents and folders, in the trash or not, to `to` (`None`: the root). One
/// transaction, on one connection: any refusal or failure leaves everything as it was.
/// Each moved copy is an update, judged by its collection's rules and the `folder` field's,
/// and a live copy with drafts beside it by the right to publish too; versions aren't
/// touched.
// shortcut: one guard check and update per document, all under the tree lock; move a
// folder of many thousands on the jobs worker, in batches, when that's needed.
#[allow(clippy::too_many_arguments)] // the delete's, plus where to
pub(crate) async fn delete_moving(
    state: &AppState,
    access: &Access,
    c: &Collection,
    id: Uuid,
    to: Option<Uuid>,
    actor: Option<Actor>,
    guard: Option<&Filter>,
    copy: DocCopy,
) -> Result<(), ApiError> {
    // A folder the caller can't read is told apart from none no more than elsewhere.
    if let Some(t) = to
        && access.read_one(&state.pool, c, t).await.is_err()
    {
        return Err(bad_move("doesn't match any folder"));
    }
    // What each collection with a `folder` may link to, worked out before the lock.
    let mut targets = HashMap::new();
    for dc in state.schemas.all() {
        if dc.schema.folders || dc.schema.slug == FOLDERS {
            targets.insert(dc.id, link_targets(state, &dc).await?);
        }
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock(&mut tx).await?;
    if let Some(t) = to
        && folders::ancestors_in(&mut tx, t).await?.contains(&id)
    {
        return Err(bad_move(
            "can't be the folder being deleted or one inside it",
        ));
    }
    for item in folders::contents_in(&mut tx, id).await? {
        let (dc, links) = match (
            state.schemas.get_by_id(item.collection_id),
            targets.get(&item.collection_id),
        ) {
            (Some(dc), Some(links)) => (dc, links),
            _ => return Err(ApiError::Conflict(crate::error::TRY_AGAIN.into())),
        };
        move_one(&mut tx, access, &dc, links, &item, to)
            .await
            .map_err(|e| match e {
                // Hidden from them is still a document they may not change.
                ApiError::NotFound => access.refuse(),
                // Named by the document that can't move, at the parameter that moved it.
                ApiError::Validation(errors) => ApiError::Validation(
                    errors
                        .into_iter()
                        .map(|f| FieldError {
                            path: "moveContentsTo".into(),
                            message: format!(
                                "{} {} can't be moved: {}{}",
                                dc.schema.slug,
                                item.id,
                                match f.path.is_empty() {
                                    true => String::new(),
                                    false => format!("{} ", f.path),
                                },
                                f.message
                            ),
                        })
                        .collect(),
                ),
                e => e,
            })?;
    }
    documents::delete_in(&mut tx, c.id, id, actor, guard, copy)
        .await
        .map_err(|e| delete_error(access, e))?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

/// Rewrites `item`'s copy, of a document of `dc`, to sit in `to`, as an update by the
/// caller, on `tx` alone.
async fn move_one(
    tx: &mut PgConnection,
    access: &Access,
    dc: &Collection,
    targets: &HashMap<String, Uuid>,
    item: &Content,
    to: Option<Uuid>,
) -> Result<(), ApiError> {
    // Accounts change through their own routes, which only those who manage users pass.
    let (actor, mut guard) = if dc.schema.slug == USERS {
        if !access.manages_users() {
            return Err(access.refuse());
        }
        (None, None)
    } else {
        // Judged on the locked row: the read and update rules, and the creator where
        // either right is the caller's own documents only.
        let (read, write) = access.guard_rules(Action::Update, dc)?;
        let mut own = read.owner.is_some();
        let mut guard = both(read.filter, write);
        // Everything in the trash takes `delete` too (plan call 9).
        if item.trashed {
            let trash = access.trash_rules(dc)?;
            own |= trash.owner.is_some();
            guard = both(guard, trash.filter);
        }
        let actor = access.actor(Action::Update, dc).map(|a| Actor {
            own_only: a.own_only || own,
            ..a
        });
        (actor, guard)
    };
    // A live copy with a draft beside it changes only by publishing.
    if item.copy == DocCopy::Published && dc.schema.drafts() {
        match access.allows(Action::Publish, dc) {
            Compiled::Deny => return Err(access.refuse()),
            Compiled::Allow => {}
            Compiled::Filter(f) => guard = both(guard, Some(f)),
        }
    }
    // Each copy holding the link is its own item, and `update_any_in` judges the guard on
    // that copy as it's stored: the live rewrite by the live copy, the draft's by the draft.
    // Both run in the move's transaction, so either refusing moves nothing.
    let patch = json!({ "folder": to });
    let doc = documents::update_any_in(
        tx,
        dc.id,
        dc.schema_version,
        item.id,
        item.copy,
        item.trashed,
        actor,
        guard.as_ref(),
        |current| {
            access.writes(dc, &patch, Action::Update, Some(current))?;
            validate_document_in(mode_for(item.copy), dc.schema.data(), &patch, Some(current))
                .map_err(ApiError::Validation)
                .and_then(|data| prepare(targets, dc, data))
                .map_err(|e| {
                    let doc = merged(dc.schema.data(), &patch, Some(current));
                    access.hide(dc, e, Some(&doc))
                })
        },
    )
    .await?;
    // A folder takes what's inside it along, which must still fit; a move is a filing.
    if dc.schema.slug == FOLDERS {
        check_place(tx, item.id).await?;
    }
    check_filed(tx, dc, item.id, item.copy, None, doc.data.0.get("folder")).await
}
