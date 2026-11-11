//! Writes to the folders collection: where a folder sits, and deleting one with its
//! contents moved (`?moveContentsTo`). Who may read a folder is `Access::folder_reads`.

use serde_json::{Value, json};
use sqlx::PgConnection;
use tachyon_core::auth::Action;
use tachyon_core::query::Filter;
use tachyon_core::{FOLDERS, FieldError, validate_document_in};
use tachyon_db::DbError;
use tachyon_db::DocCopy;
use tachyon_db::auth::USERS;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Actor};
use tachyon_db::folders;
use uuid::Uuid;

use crate::access::Access;
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

/// Deletes folder `id` of `c` (under `guard`, judging `copy`), first moving what files into
/// it, documents and folders, to `to` (`None`: the root). One transaction: any refusal or
/// failure leaves everything as it was. Each moved copy is an update, judged by its
/// collection's rules and the `folder` field's; versions aren't touched.
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
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock(&mut tx).await?;
    if let Some(t) = to
        && folders::ancestors_in(&mut tx, t).await?.contains(&id)
    {
        return Err(bad_move(
            "can't be the folder being deleted or one inside it",
        ));
    }
    for (collection_id, doc, held) in folders::contents_in(&mut tx, id).await? {
        let dc = state
            .schemas
            .get_by_id(collection_id)
            .ok_or_else(|| ApiError::Conflict(crate::error::TRY_AGAIN.into()))?;
        move_one(&mut tx, state, access, &dc, doc, held, to)
            .await
            .map_err(|e| match e {
                // Hidden from them is still a document they may not change.
                ApiError::NotFound => access.refuse(),
                e => e,
            })?;
    }
    documents::delete_in(&mut tx, c.id, id, actor, guard, copy)
        .await
        .map_err(|e| delete_error(access, e))?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

/// Rewrites `copy` of document `doc` of `dc` to sit in `to`, as an update by the caller.
async fn move_one(
    tx: &mut PgConnection,
    state: &AppState,
    access: &Access,
    dc: &Collection,
    doc: Uuid,
    copy: DocCopy,
    to: Option<Uuid>,
) -> Result<(), ApiError> {
    // Accounts change through their own routes, which only those who manage users pass.
    let (actor, guard) = if dc.schema.slug == USERS {
        if !access.manages_users() {
            return Err(access.refuse());
        }
        (None, None)
    } else {
        let guard = access
            .write_guard(&state.pool, Action::Update, dc, doc)
            .await?;
        (access.actor(Action::Update, dc), guard)
    };
    let targets = link_targets(state, dc).await?;
    let patch = json!({ "folder": to });
    documents::update_in(
        tx,
        dc.id,
        dc.schema_version,
        doc,
        copy,
        actor,
        guard.as_ref(),
        |current| {
            access.writes(dc, &patch, Action::Update, Some(current))?;
            validate_document_in(mode_for(copy), dc.schema.data(), &patch, Some(current))
                .map_err(ApiError::Validation)
                .and_then(|data| prepare(&targets, dc, data))
        },
    )
    .await?;
    Ok(())
}
