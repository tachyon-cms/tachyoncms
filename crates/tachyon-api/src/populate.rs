//! Replaces relationship IDs with the documents they point at, one query per depth level.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Map, Value};
use tachyon_core::select::{Select, parse_select, project};
use tachyon_core::virtuals::{self, VirtualField};
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

/// Fills each document's virtual fields (those `select` keeps) with one query for all of them.
pub(crate) async fn fill_virtuals(
    state: &AppState,
    items: &mut [(Value, Arc<Collection>)],
    select: Option<&Select>,
) -> Result<(), ApiError> {
    let selects = vec![select; items.len()];
    fill_virtuals_each(state, items, &selects).await
}

/// `fill_virtuals` where each document has its own `select`, still in one query.
pub(crate) async fn fill_virtuals_each(
    state: &AppState,
    items: &mut [(Value, Arc<Collection>)],
    selects: &[Option<&Select>],
) -> Result<(), ApiError> {
    let wanted: Vec<Vec<VirtualField>> = items
        .iter()
        .zip(selects)
        .map(|((_, c), select)| {
            c.schema
                .virtual_fields()
                .into_iter()
                .filter(|v| select.is_none_or(|s| s.keeps(&v.path)))
                .collect()
        })
        .collect();
    let mut ids: Vec<Uuid> = items
        .iter()
        .zip(&wanted)
        .filter_map(|((v, _), wanted)| Some((v.as_object()?, wanted)))
        .flat_map(|(doc, wanted)| wanted.iter().flat_map(|v| virtuals::linked_ids(v, doc)))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(());
    }
    let linked: HashMap<Uuid, Value> = documents::get_many(&state.pool, &ids)
        .await?
        .into_iter()
        .map(|(_, doc)| (doc.id, Value::Object(doc.data.0)))
        .collect();
    for ((value, _), wanted) in items.iter_mut().zip(&wanted) {
        if let Some(doc) = value.as_object_mut() {
            for v in wanted {
                virtuals::fill(v, doc, |id| linked.get(&id).cloned());
            }
        }
    }
    Ok(())
}

/// `populate[collection]` choices: how documents of each collection look when populated.
pub(crate) type Shapes = HashMap<String, Select>;

/// How a populated document of `c` is cut down: `populate[its collection]`, or else its
/// collection's `defaultPopulate`, or not at all.
// ponytail: defaultPopulate is parsed per populated document; cache the parsed Select on the
// collection if it shows up in a profile.
pub(crate) fn child_shape(c: &Collection, shapes: &Shapes) -> Option<Select> {
    if let Some(select) = shapes.get(&c.schema.slug) {
        return Some(select.clone());
    }
    let dp = c.schema.default_populate.as_ref()?;
    // Checked when the schema was saved, so this only fails on a hand-edited row.
    parse_select(
        c.schema.data(),
        &Value::Object(dp.clone()),
        "defaultPopulate",
    )
    .ok()
}

pub(crate) fn populate<'a>(
    state: &'a AppState,
    docs: Vec<(Value, Arc<Collection>)>,
    depth: u8,
    shapes: &'a Shapes,
) -> Populated<'a> {
    let items = docs.into_iter().map(|(v, c)| (v, c, depth)).collect();
    populate_each(state, items, shapes)
}

/// Each document carries its own remaining depth, because a field's `maxDepth` can cut it
/// short. One query per level for every distinct link, whatever the depths.
fn populate_each<'a>(
    state: &'a AppState,
    items: Vec<(Value, Arc<Collection>, u8)>,
    shapes: &'a Shapes,
) -> Populated<'a> {
    Box::pin(async move {
        // For each document: the link at each path, and how deep to populate it.
        let wanted: Vec<HashMap<String, (Uuid, u8)>> = items
            .iter()
            .map(|(v, c, depth)| {
                let Some(obj) = v.as_object().filter(|_| *depth > 0) else {
                    return HashMap::new();
                };
                relations(c.schema.data(), obj)
                    .into_iter()
                    .filter_map(|r| {
                        let reach = r.max_depth.map_or(*depth, |cap| cap.min(*depth));
                        (reach > 0).then(|| (r.path, (r.id, reach - 1)))
                    })
                    .collect()
            })
            .collect();
        let mut ids: Vec<Uuid> = wanted
            .iter()
            .flat_map(|w| w.values().map(|(id, _)| *id))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(items.into_iter().map(|(v, _, _)| v).collect());
        }

        let found: HashMap<Uuid, (Uuid, Document)> = documents::get_many(&state.pool, &ids)
            .await?
            .into_iter()
            .map(|(collection_id, doc)| (doc.id, (collection_id, doc)))
            .collect();
        // The same document wanted at two depths is populated once for each.
        let mut keys: Vec<(Uuid, u8)> = wanted.iter().flat_map(|w| w.values().copied()).collect();
        keys.sort_unstable();
        keys.dedup();
        let mut fetched = Vec::new();
        let mut child_keys = Vec::new();
        for key @ (id, remaining) in keys {
            let Some((collection_id, doc)) = found.get(&id) else {
                continue;
            };
            if let Some(c) = state.schemas.get_by_id(*collection_id) {
                fetched.push((to_value(doc)?, c, remaining));
                child_keys.push(key);
            }
        }
        // Populated documents show their virtual fields too (those their shape keeps),
        // then take that shape.
        let shapes_of: Vec<Option<Select>> = fetched
            .iter()
            .map(|(_, c, _)| child_shape(c, shapes))
            .collect();
        let mut pairs = Vec::with_capacity(fetched.len());
        let mut depths = Vec::with_capacity(fetched.len());
        for (value, c, remaining) in fetched {
            pairs.push((value, c));
            depths.push(remaining);
        }
        let refs: Vec<Option<&Select>> = shapes_of.iter().map(Option::as_ref).collect();
        fill_virtuals_each(state, &mut pairs, &refs).await?;
        let children: Vec<(Value, Arc<Collection>, u8)> = pairs
            .into_iter()
            .zip(shapes_of)
            .zip(depths)
            .map(|(((mut value, c), shape), remaining)| {
                if let (Some(select), Value::Object(doc)) = (&shape, &mut value) {
                    project(select, doc);
                }
                (value, c, remaining)
            })
            .collect();
        let children = populate_each(state, children, shapes).await?;
        let by_key: HashMap<(Uuid, u8), Value> = child_keys.into_iter().zip(children).collect();

        Ok(items
            .into_iter()
            .zip(wanted)
            .map(|((mut v, c, _), wanted)| {
                if let Some(obj) = v.as_object_mut() {
                    map_relations(c.schema.data(), obj, &mut |path, _| {
                        wanted.get(path).and_then(|key| by_key.get(key)).cloned()
                    });
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
            auth: std::sync::Arc::new(crate::auth::AuthState::new(Default::default()).unwrap()),
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
