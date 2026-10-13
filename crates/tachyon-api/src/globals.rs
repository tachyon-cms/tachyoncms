//! `/api/globals/{slug}`: singletons such as site settings. A global's single document uses
//! the collection's own ID, so there can only ever be one.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, RawQuery, State};
use serde_json::Value;
use tachyon_core::{CollectionKind, defaults, validate_document};
use tachyon_db::DbError;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Document};

use crate::documents::{json_body, shaped, unique_error};
use crate::params::read_params;
use crate::populate::{link_targets, populate, prepare, to_value};
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
    c: Arc<Collection>,
    doc: &Document,
    depth: u8,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        populate(state, vec![(to_value(doc)?, c)], depth)
            .await?
            .remove(0),
    ))
}

pub async fn get(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let c = global(&state, path)?;
    let p = read_params(raw.as_deref(), c.schema.data())?;
    let value = match documents::get(&state.pool, c.id, c.id).await {
        Ok(doc) => to_value(&doc)?,
        Err(DbError::NotFound) => Value::Object(defaults(c.schema.data())),
        Err(e) => return Err(e.into()),
    };
    let value = shaped(value, p.select.as_ref());
    Ok(Json(
        populate(&state, vec![(value, c)], p.depth).await?.remove(0),
    ))
}

pub async fn update(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let c = global(&state, path)?;
    let patch = json_body(body)?;
    let targets = link_targets(&state, &c).await?;
    // Two first saves can race; the loser's insert hits the primary key and retries as an update.
    for _ in 0..2 {
        let updated = documents::update(&state.pool, c.id, c.schema_version, c.id, |current| {
            let data = validate_document(c.schema.data(), &patch, Some(current))
                .map_err(ApiError::Validation)?;
            prepare(&targets, &c, data)
        })
        .await;
        match updated {
            Ok(doc) => return respond(&state, c.clone(), &doc, 1).await,
            Err(ApiError::NotFound) => {}
            Err(e) => return Err(unique_error(&c, e)),
        }
        let data =
            validate_document(c.schema.data(), &patch, None).map_err(ApiError::Validation)?;
        match documents::insert(
            &state.pool,
            c.id,
            c.schema_version,
            Some(c.id),
            &prepare(&targets, &c, data)?,
        )
        .await
        {
            Ok(doc) => return respond(&state, c.clone(), &doc, 1).await,
            Err(DbError::UniqueViolation(ref name)) if name == "documents_pkey" => continue,
            Err(e) => return Err(unique_error(&c, e.into())),
        }
    }
    Err(ApiError::Internal(
        "global kept changing during save".into(),
    ))
}
