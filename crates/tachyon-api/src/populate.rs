//! Replaces relationship IDs with the documents they point at, one query per depth level.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Map, Value};
use tachyon_core::{map_relations, relations};
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Document, Prepared, Relation};

use crate::{ApiError, AppState};

type Populated<'a> = Pin<Box<dyn Future<Output = Result<Vec<Value>, ApiError>> + Send + 'a>>;

/// Pairs validated data with the links it holds, resolved to collection IDs.
pub(crate) fn prepare(state: &AppState, c: &Collection, data: Map<String, Value>) -> Prepared {
    let relations = relations(&c.schema.fields, &data)
        .into_iter()
        .filter_map(|r| {
            // Targets were checked when the collection was created.
            let collection_id = if r.relation_to == c.schema.slug {
                Some(c.id)
            } else {
                state.schemas.get(&r.relation_to).map(|t| t.id)
            }?;
            Some(Relation {
                path: r.path,
                collection_id,
                id: r.id,
            })
        })
        .collect();
    Prepared { data, relations }
}

pub(crate) fn to_value(doc: &Document) -> Result<Value, ApiError> {
    serde_json::to_value(doc).map_err(|e| ApiError::Internal(e.to_string()))
}

pub(crate) fn populate(
    state: &AppState,
    docs: Vec<(Value, Arc<Collection>)>,
    depth: u8,
) -> Populated<'_> {
    Box::pin(async move {
        if depth == 0 {
            return Ok(docs.into_iter().map(|(v, _)| v).collect());
        }
        let mut ids: Vec<_> = docs
            .iter()
            .filter_map(|(v, c)| v.as_object().map(|o| relations(&c.schema.fields, o)))
            .flatten()
            .map(|r| r.id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(docs.into_iter().map(|(v, _)| v).collect());
        }

        let mut children = Vec::new();
        for (collection_id, doc) in documents::get_many(&state.pool, &ids).await? {
            if let Some(c) = state.schemas.get_by_id(collection_id) {
                children.push((to_value(&doc)?, c));
            }
        }
        let children = populate(state, children, depth - 1).await?;
        let by_id: HashMap<String, Value> = children
            .into_iter()
            .filter_map(|v| Some((v.get("id")?.as_str()?.to_owned(), v)))
            .collect();

        Ok(docs
            .into_iter()
            .map(|(mut v, c)| {
                if let Some(obj) = v.as_object_mut() {
                    map_relations(&c.schema.fields, obj, &mut |id| by_id.get(id).cloned());
                }
                v
            })
            .collect())
    })
}
