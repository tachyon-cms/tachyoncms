//! Replaces relationship IDs with the documents they point at, one query per depth level.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Map, Value};
use tachyon_core::{CollectionKind, FieldError, map_relations, relations};
use tachyon_db::DbError;
use tachyon_db::collections::{self, Collection};
use tachyon_db::documents::{self, Document, Prepared, Relation};
use uuid::Uuid;

use crate::{ApiError, AppState};

type Populated<'a> = Pin<Box<dyn Future<Output = Result<Vec<Value>, ApiError>> + Send + 'a>>;

/// The collection IDs a collection's relationship fields may link to, by slug. Reads the
/// database when the cache hasn't caught up (another server may have just created the
/// target). Globals and unknown slugs are left out, so `prepare` refuses links to them.
pub(crate) async fn link_targets(
    state: &AppState,
    c: &Collection,
) -> Result<HashMap<String, Uuid>, ApiError> {
    let mut out = HashMap::new();
    for slug in c.schema.relation_targets() {
        let found = if slug == c.schema.slug {
            Some((c.id, c.schema.kind))
        } else if let Some(t) = state.schemas.get(slug) {
            Some((t.id, t.schema.kind))
        } else {
            match collections::get_by_slug(&state.pool, slug).await {
                Ok(t) => Some((t.id, t.schema.kind)),
                Err(DbError::NotFound) => None,
                Err(e) => return Err(e.into()),
            }
        };
        if let Some((id, CollectionKind::Collection)) = found {
            out.insert(slug.to_owned(), id);
        }
    }
    Ok(out)
}

/// Pairs validated data with the links it holds, resolved to collection IDs from `targets`.
pub(crate) fn prepare(
    targets: &HashMap<String, Uuid>,
    c: &Collection,
    data: Map<String, Value>,
) -> Result<Prepared, ApiError> {
    let mut errors = Vec::new();
    let mut found = Vec::new();
    for r in relations(c.schema.data(), &data) {
        match targets.get(&r.relation_to) {
            Some(&collection_id) => found.push(Relation {
                path: r.path,
                collection_id,
                id: r.id,
            }),
            None => errors.push(FieldError {
                message: format!("links to `{}`, which isn't a collection", r.relation_to),
                path: r.path,
            }),
        }
    }
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    Ok(Prepared {
        data,
        relations: found,
    })
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
            .filter_map(|(v, c)| v.as_object().map(|o| relations(c.schema.data(), o)))
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
                    map_relations(c.schema.data(), obj, &mut |id| by_id.get(id).cloned());
                }
                v
            })
            .collect())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::Schemas;
    use serde_json::json;
    use sqlx::PgPool;
    use tachyon_core::CollectionSchema;
    use tachyon_db::collections;

    fn schema(v: Value) -> CollectionSchema {
        serde_json::from_value(v).unwrap()
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn links_to_collections_the_cache_hasnt_seen_are_kept(pool: PgPool) {
        // Loaded before the collections exist, as on a server that hasn't heard the news yet.
        let state = AppState {
            pool: pool.clone(),
            schemas: Arc::new(Schemas::load(&pool).await.unwrap()),
        };
        let b = collections::create(&pool, &schema(json!({"slug": "b", "fields": []})))
            .await
            .unwrap();
        collections::create(
            &pool,
            &schema(json!({"slug": "settings", "kind": "global", "fields": []})),
        )
        .await
        .unwrap();
        let a = collections::create(
            &pool,
            &schema(json!({"slug": "a", "fields": [
                {"name": "to_b", "type": "relationship", "relationTo": "b"},
                {"name": "to_global", "type": "relationship", "relationTo": "settings"}
            ]})),
        )
        .await
        .unwrap();
        let id = uuid::Uuid::now_v7();

        let targets = link_targets(&state, &a).await.unwrap();
        let data = json!({"to_b": id.to_string()});
        let prepared = prepare(&targets, &a, data.as_object().unwrap().clone()).unwrap();
        assert_eq!(prepared.relations.len(), 1);
        assert_eq!(prepared.relations[0].collection_id, b.id);

        let data = json!({"to_global": id.to_string()});
        let Err(ApiError::Validation(errors)) =
            prepare(&targets, &a, data.as_object().unwrap().clone())
        else {
            panic!("a link to a global must be refused");
        };
        assert_eq!(errors[0].path, "to_global");
        assert_eq!(
            errors[0].message,
            "links to `settings`, which isn't a collection"
        );
    }
}
