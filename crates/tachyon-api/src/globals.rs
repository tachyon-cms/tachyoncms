//! `/api/globals/{slug}`: singletons such as site settings. A global's single document uses
//! the collection's own ID, so there can only ever be one.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, RawQuery, State};
use serde_json::Value;
use tachyon_core::auth::Action;
use tachyon_core::{CollectionKind, defaults, validate_document};
use tachyon_db::DbError;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Document};
use tachyon_db::query::get_many_readable;

use crate::access::{Access, both};
use crate::documents::{json_body, shaped, unique_error};
use crate::params::read_params;
use crate::populate::{Shapes, fill_virtuals, link_targets, populate, prepare, to_value};
use crate::{ApiError, AppState};

fn global(
    state: &AppState,
    path: Result<Path<String>, PathRejection>,
) -> Result<Arc<Collection>, ApiError> {
    let Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    state
        .schemas
        .get(&slug)
        .filter(|c| c.schema.kind == CollectionKind::Global)
        .ok_or(ApiError::NotFound)
}

async fn respond(
    state: &AppState,
    access: &Access,
    c: Arc<Collection>,
    doc: &Document,
    depth: u8,
) -> Result<Json<Value>, ApiError> {
    let mut items = vec![(to_value(doc)?, c)];
    fill_virtuals(state, access, &mut items, None).await?;
    Ok(Json(
        populate(state, access, items, depth, &Shapes::new())
            .await?
            .remove(0),
    ))
}

/// A global's read rule judges its stored document. One that hasn't been saved yet shows
/// its defaults, unless the rule is a filter: there's nothing yet for it to match.
pub async fn get(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<String>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let c = global(&state, path)?;
    let read = access.permit(Action::Read, &c)?;
    let p = read_params(raw.as_deref(), &c, &access)?;
    let value = match read {
        Some(filter) => {
            let one = HashMap::from([(c.id, Some(filter))]);
            let (_, doc) = get_many_readable(&state.pool, &[c.id], &one)
                .await?
                .pop()
                .ok_or(ApiError::NotFound)?;
            to_value(&doc)?
        }
        None => match documents::get(&state.pool, c.id, c.id).await {
            Ok(doc) => to_value(&doc)?,
            Err(DbError::NotFound) => Value::Object(defaults(c.schema.data())),
            Err(e) => return Err(e.into()),
        },
    };
    let mut items = vec![(value, c)];
    fill_virtuals(&state, &access, &mut items, p.select.as_ref()).await?;
    let (value, c) = items.remove(0);
    let value = shaped(value, p.select.as_ref());
    Ok(Json(
        populate(&state, &access, vec![(value, c)], p.depth, &p.populate)
            .await?
            .remove(0),
    ))
}

/// Changing a global needs its update rule (or the role default) and its read rule. When
/// either is a filter, the stored document must match it, checked under the row lock, and a
/// global that hasn't been saved yet can't be created this way.
pub async fn update(
    State(state): State<AppState>,
    access: Access,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let c = global(&state, path)?;
    let guard = both(
        access.permit(Action::Read, &c)?,
        access.permit(Action::UpdateGlobal, &c)?,
    );
    let who = access.actor(Action::UpdateGlobal, &c);
    let patch = json_body(body)?;
    let targets = link_targets(&state, &c).await?;
    // Two first saves can race; the loser's insert hits the primary key and retries as an update.
    for _ in 0..2 {
        let updated = documents::update_as(
            &state.pool,
            c.id,
            c.schema_version,
            c.id,
            who,
            guard.as_ref(),
            |current| {
                let data = validate_document(c.schema.data(), &patch, Some(current))
                    .map_err(ApiError::Validation)?;
                prepare(&targets, &c, data)
            },
        )
        .await;
        match updated {
            Ok(doc) => return respond(&state, &access, c.clone(), &doc, 1).await,
            Err(ApiError::NotFound) if guard.is_some() => return Err(DbError::Forbidden.into()),
            Err(ApiError::NotFound) => {}
            Err(e) => return Err(unique_error(&c, e)),
        }
        let data =
            validate_document(c.schema.data(), &patch, None).map_err(ApiError::Validation)?;
        match documents::insert_as(
            &state.pool,
            c.id,
            c.schema_version,
            Some(c.id),
            &prepare(&targets, &c, data)?,
            who.map(|a| a.id),
        )
        .await
        {
            Ok(doc) => return respond(&state, &access, c.clone(), &doc, 1).await,
            Err(DbError::UniqueViolation(ref name)) if name == "documents_pkey" => continue,
            Err(e) => return Err(unique_error(&c, e.into())),
        }
    }
    Err(ApiError::Internal(
        "global kept changing during save".into(),
    ))
}
