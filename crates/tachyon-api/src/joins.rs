//! Filling join fields: the documents of another collection that link to each document read.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use tachyon_core::CollectionKind;
use tachyon_core::access::Compiled;
use tachyon_core::auth::Action;
use tachyon_core::joins::{JoinField, on_pattern};
use tachyon_core::query::{Filter, parse_filter_with, parse_sort};
use tachyon_core::select::{Select, project};
use tachyon_db::collections::Collection;
use tachyon_db::query::{self, JoinQuery};
use uuid::Uuid;

use crate::access::{Access, both};
use crate::params::{JoinRequest, Joins};
use crate::populate::{Shapes, child_shape, fill_virtuals_each, populate, to_value};
use crate::{ApiError, AppState};

#[cfg(test)]
tokio::task_local! {
    /// How many join queries a request ran, so tests can check the batching.
    pub(crate) static JOIN_QUERIES: std::cell::Cell<usize>;
}

/// Writes `value` at a dotted path, making any group objects along the way.
fn set_at(doc: &mut Map<String, Value>, path: &str, value: Value) {
    let mut parts: Vec<&str> = path.split('.').collect();
    let last = parts.pop().expect("a path has a name");
    let mut obj = doc;
    for part in parts {
        let slot = obj
            .entry(part.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(inner) = slot else { return };
        obj = inner;
    }
    obj.insert(last.to_owned(), value);
}

fn empty(count: bool) -> Value {
    let mut v = json!({"docs": [], "hasNextPage": false});
    if count {
        v["totalDocs"] = 0.into();
    }
    v
}

/// What a join reads from, or `None` when its collection or linking field has gone since the
/// schema was saved, or the caller can't read that collection: then it comes back empty
/// rather than failing the read.
struct Plan {
    target: Arc<Collection>,
    pattern: String,
    filter: Option<Filter>,
    sort: Option<tachyon_core::query::Sort>,
}

// ponytail: the schema's `where` and `defaultSort` are parsed on every read; cache them on
// the collection if that shows up in a profile.
fn plan(state: &AppState, access: &Access, j: &JoinField, req: &mut JoinRequest) -> Option<Plan> {
    let target = state
        .schemas
        .get(&j.collection)
        .filter(|t| t.schema.kind == CollectionKind::Collection)?;
    let read = match access.allows(Action::Read, &target) {
        Compiled::Deny => return None,
        Compiled::Allow => None,
        Compiled::Filter(f) => Some(f),
    };
    // Which documents link here through a field the caller can't read is hidden too.
    if !access.reads(&target, &j.on) {
        return None;
    }
    let pattern = on_pattern(target.schema.data(), &j.on)?;
    let schema_filter = match &j.r#where {
        Some(w) => {
            let lookup = |slug: &str| state.schemas.lookup(slug);
            Some(parse_filter_with(target.schema.data(), &Value::Object(w.clone()), &lookup).ok()?)
        }
        None => None,
    };
    // The schema's filter and the caller's read rule always apply; a request can only
    // narrow them.
    let filter = both(both(schema_filter, read), req.filter.take());
    // A default sort on a field the caller can't read would rank what they can't see, so
    // they get the usual order instead.
    let sort = req.sort.take().or_else(|| {
        j.sort
            .as_deref()
            .and_then(|s| parse_sort(&access.readable_fields(&target), s).ok())
    });
    Some(Plan {
        target,
        pattern,
        filter,
        sort,
    })
}

/// Fills every join field `select` keeps, on documents of one or more collections: one
/// query per join field for the whole batch. Joined documents are populated objects,
/// shaped by `populate`/`defaultPopulate`, their own links filled to `depth - 1`; at depth 0
/// they're IDs.
pub(crate) async fn fill_joins(
    state: &AppState,
    access: &Access,
    items: &mut [(Value, Arc<Collection>)],
    select: Option<&Select>,
    joins: &Joins,
    depth: u8,
    shapes: &Shapes,
) -> Result<(), ApiError> {
    let Joins::On(requests) = joins else {
        return Ok(());
    };
    let mut collections: Vec<Arc<Collection>> = Vec::new();
    for (_, c) in items.iter() {
        if !collections.iter().any(|seen| seen.id == c.id) {
            collections.push(c.clone());
        }
    }
    for c in collections {
        for j in c.schema.join_fields() {
            if select.is_some_and(|s| !s.keeps(&j.path)) || !access.reads(&c, &j.path) {
                continue;
            }
            let mut req = requests.get(&j.path).cloned().unwrap_or_default();
            // Parents of this collection, by position, with their IDs.
            let parents: Vec<(usize, Uuid)> = items
                .iter()
                .enumerate()
                .filter(|(_, (_, ic))| ic.id == c.id)
                .filter_map(|(i, (v, _))| Some((i, Uuid::try_parse(v.get("id")?.as_str()?).ok()?)))
                .collect();
            let count = req.count;
            let Some(plan) = plan(state, access, &j, &mut req) else {
                for (i, _) in &parents {
                    if let Some(doc) = items[*i].0.as_object_mut() {
                        set_at(doc, &j.path, empty(count));
                    }
                }
                continue;
            };
            let limit = req.limit.unwrap_or(i64::from(j.limit)).clamp(1, 100);
            let offset = (req.page.unwrap_or(1) - 1).saturating_mul(limit);
            let ids: Vec<Uuid> = parents.iter().map(|(_, id)| *id).collect();
            let q = JoinQuery {
                collection_id: plan.target.id,
                pattern: &plan.pattern,
                filter: plan.filter.as_ref(),
                sort: plan.sort.as_ref(),
                limit,
                offset,
                count,
            };
            #[cfg(test)]
            let _ = JOIN_QUERIES.try_with(|n| n.set(n.get() + 1));
            let mut pages = query::joined(&state.pool, &ids, &q).await?;

            // Every joined document across all parents, filled and populated together.
            let mut owners = Vec::new();
            let mut joined = Vec::new();
            let mut meta = HashMap::new();
            for (i, id) in &parents {
                let page = pages.remove(id).unwrap_or(query::Page {
                    docs: Vec::new(),
                    total: Some(0),
                    next: None,
                    has_more: false,
                });
                meta.insert(*i, (page.has_more, page.total));
                for doc in &page.docs {
                    owners.push(*i);
                    joined.push(if depth == 0 {
                        Value::String(doc.id.to_string())
                    } else {
                        let mut value = to_value(doc)?;
                        access.strip(&plan.target, &mut value);
                        value
                    });
                }
            }
            let values: Vec<Value> = if depth == 0 {
                joined
            } else {
                let shape = child_shape(&plan.target, shapes);
                let mut pairs: Vec<(Value, Arc<Collection>)> = joined
                    .into_iter()
                    .map(|v| (v, plan.target.clone()))
                    .collect();
                let selects = vec![shape.as_ref(); pairs.len()];
                fill_virtuals_each(state, access, &mut pairs, &selects).await?;
                for (v, _) in &mut pairs {
                    if let (Some(s), Value::Object(doc)) = (&shape, v) {
                        project(s, doc);
                    }
                }
                populate(state, access, pairs, depth - 1, shapes).await?
            };
            let mut by_owner: HashMap<usize, Vec<Value>> = HashMap::new();
            for (owner, v) in owners.into_iter().zip(values) {
                by_owner.entry(owner).or_default().push(v);
            }
            for (i, _) in &parents {
                let (has_more, total) = meta.get(i).copied().unwrap_or((false, None));
                let mut value = json!({
                    "docs": by_owner.remove(i).unwrap_or_default(),
                    "hasNextPage": has_more,
                });
                if let Some(total) = total.filter(|_| count) {
                    value["totalDocs"] = total.into();
                }
                if let Some(doc) = items[*i].0.as_object_mut() {
                    set_at(doc, &j.path, value);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::JOIN_QUERIES;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use sqlx::PgPool;
    use std::cell::Cell;
    use tower::ServiceExt;

    async fn call(app: &axum::Router, method: &str, uri: &str, body: serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert!(res.status().is_success(), "{uri}: {}", res.status());
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn one_join_query_for_a_whole_page(pool: PgPool) {
        let app = crate::auth::admin_router(pool).await;
        call(
            &app,
            "POST",
            "/api/admin/collections",
            json!({"slug": "categories", "fields": [
                {"name": "name", "type": "text"}
            ]}),
        )
        .await;
        call(
            &app,
            "POST",
            "/api/admin/collections",
            json!({"slug": "posts", "fields": [
                {"name": "category", "type": "relationship", "relationTo": "categories"}
            ]}),
        )
        .await;
        call(
            &app,
            "PUT",
            "/api/admin/collections/categories",
            json!({"schema": {"slug": "categories", "fields": [
                {"name": "name", "type": "text"},
                {"name": "posts", "type": "join", "collection": "posts", "on": "category"}
            ]}}),
        )
        .await;
        for name in ["a", "b", "c", "d", "e"] {
            call(&app, "POST", "/api/categories", json!({"name": name})).await;
        }
        let queries = JOIN_QUERIES
            .scope(Cell::new(0), async {
                let req = Request::get("/api/categories").body(Body::empty()).unwrap();
                let res = app.clone().oneshot(req).await.unwrap();
                assert!(res.status().is_success());
                JOIN_QUERIES.with(Cell::get)
            })
            .await;
        assert_eq!(queries, 1, "five categories, one join field, one query");
    }
}
