//! Listing with filters, sorting and cursors.
//!
//! The only text spliced into SQL here is static, a schema-validated field name (checked
//! again by `assert_identifier`), or a `Uuid`. Every user-supplied value is bound.

use std::collections::HashMap;

use serde_json::{Value, json};
use sqlx::types::Json;
use sqlx::{PgPool, Postgres, QueryBuilder};
use tachyon_core::query::{Cond, Cursor, Filter, Op, Operand, Scalar, Sort, Target, ValueKind};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::documents::Document;
use crate::{DEFAULT_TENANT, DbError};

pub struct ListQuery<'a> {
    pub filter: Option<&'a Filter>,
    pub sort: Option<&'a Sort>,
    pub limit: i64,
    pub offset: i64,
    pub after: Option<&'a Cursor>,
    pub count: bool,
}

#[derive(Debug)]
pub struct Page {
    pub docs: Vec<Document>,
    /// Only counted when asked for, because counting a large collection is slow.
    pub total: Option<i64>,
    /// Where the next page starts, when this one came back full.
    pub next: Option<Cursor>,
    /// Whether at least one more document follows this page.
    pub has_more: bool,
}

fn assert_identifier(segment: &str) {
    assert!(
        !segment.is_empty()
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "`{segment}` is not a safe identifier"
    );
}

/// The SQL expression for a field. Index DDL uses this too, so indexes always match queries.
pub fn field_expr(path: &[String], kind: ValueKind) -> String {
    field_expr_on("published", path, kind)
}

/// `field_expr` read from another JSON expression: a linked document's `t1.published`, or a
/// list row's `r1.v`.
fn field_expr_on(base: &str, path: &[String], kind: ValueKind) -> String {
    path.iter().for_each(|s| assert_identifier(s));
    let path = path.join(",");
    match kind {
        ValueKind::Text | ValueKind::Date => format!("({base} #>> '{{{path}}}')"),
        // Guarded casts: values left behind by a retype read as NULL instead of failing the query.
        ValueKind::Number => format!(
            "(CASE WHEN jsonb_typeof({base} #> '{{{path}}}') = 'number' THEN ({base} #>> '{{{path}}}')::numeric END)"
        ),
        ValueKind::Boolean => format!(
            "(CASE WHEN jsonb_typeof({base} #> '{{{path}}}') = 'boolean' THEN ({base} #>> '{{{path}}}')::boolean END)"
        ),
        ValueKind::List | ValueKind::LinkList | ValueKind::Point => {
            format!("({base} #> '{{{path}}}')")
        }
    }
}

/// One coordinate of a stored `[lng, lat]`, or NULL when the stored value isn't a number.
/// Only a two-number array whose numbers are in range counts. The CASEs are nested
/// because Postgres doesn't promise to evaluate AND left to right, and
/// `jsonb_array_length` fails on anything that isn't an array.
fn coordinate(path: &[String], index: usize) -> String {
    path.iter().for_each(|s| assert_identifier(s));
    let path = path.join(",");
    let limit = if index == 0 { 180 } else { 90 };
    format!(
        "(CASE WHEN jsonb_typeof(published #> '{{{path}}}') = 'array' THEN \
           CASE WHEN jsonb_array_length(published #> '{{{path}}}') = 2 \
                 AND jsonb_typeof(published #> '{{{path},{index}}}') = 'number' THEN \
             CASE WHEN (published #>> '{{{path},{index}}}')::numeric BETWEEN -{limit} AND {limit} \
               THEN (published #>> '{{{path},{index}}}')::float8 END \
           END \
         END)"
    )
}

/// Metres from a stored point to (`lng`, `lat`): haversine on a sphere of the mean earth
/// radius, accurate to well under 1% for finding nearby places.
fn push_distance(qb: &mut QueryBuilder<Postgres>, path: &[String], lng: f64, lat: f64) {
    let (x, y) = (coordinate(path, 0), coordinate(path, 1));
    qb.push("(6371008.8 * 2 * asin(least(1, sqrt(power(sin(radians((")
        .push(&y)
        .push(" - ");
    qb.push_bind(lat);
    qb.push(") / 2)), 2) + cos(radians(");
    qb.push_bind(lat);
    qb.push(")) * cos(radians(")
        .push(&y)
        .push(")) * power(sin(radians((")
        .push(&x)
        .push(" - ");
    qb.push_bind(lng);
    qb.push(") / 2)), 2)))))");
}

/// What a condition reads from: the document's own JSON and columns at the top, a linked
/// document's (`t1.published`, `t1.id`) or a list row's (`r1.v`) further in. `depth` keeps
/// each nested alias distinct.
struct Base {
    json: String,
    columns: String,
    depth: u8,
}

impl Base {
    fn top() -> Self {
        Self {
            json: "published".into(),
            columns: String::new(),
            depth: 0,
        }
    }
}

fn expr(target: &Target) -> String {
    expr_in(target, &Base::top())
}

fn expr_in(target: &Target, base: &Base) -> String {
    let columns = &base.columns;
    match target {
        Target::Id => format!("{columns}id"),
        Target::CreatedAt => format!("{columns}created_at"),
        Target::UpdatedAt => format!("{columns}updated_at"),
        Target::Field { path, kind } => field_expr_on(&base.json, path, *kind),
        Target::Via { .. } | Target::Row { .. } => {
            unreachable!("nested targets compile to EXISTS, never to an expression")
        }
    }
}

fn bind(qb: &mut QueryBuilder<Postgres>, target: &Target, value: &Scalar) {
    match (target, value) {
        (Target::Id, Scalar::Text(s)) => {
            qb.push_bind(Uuid::try_parse(s).expect("parser validated the ID"));
        }
        (Target::CreatedAt | Target::UpdatedAt, Scalar::Text(s)) => {
            qb.push_bind(OffsetDateTime::parse(s, &Rfc3339).expect("parser normalized the date"));
        }
        // Bound as text, which Postgres reads exactly; float8→numeric would keep 15 digits.
        (_, Scalar::Number(n)) => {
            qb.push_bind(n.to_string()).push("::numeric");
        }
        (_, Scalar::Bool(b)) => {
            qb.push_bind(*b);
        }
        (_, Scalar::Text(s)) => {
            qb.push_bind(s.clone());
        }
    }
}

fn bind_list(qb: &mut QueryBuilder<Postgres>, target: &Target, values: &[Scalar]) {
    let texts = || {
        values.iter().filter_map(|v| match v {
            Scalar::Text(s) => Some(s.clone()),
            _ => None,
        })
    };
    match target {
        Target::Id => {
            qb.push_bind(
                texts()
                    .filter_map(|s| Uuid::try_parse(&s).ok())
                    .collect::<Vec<_>>(),
            );
        }
        Target::CreatedAt | Target::UpdatedAt => {
            let times: Vec<OffsetDateTime> = texts()
                .filter_map(|s| OffsetDateTime::parse(&s, &Rfc3339).ok())
                .collect();
            qb.push_bind(times);
        }
        Target::Field {
            kind: ValueKind::Number,
            ..
        } => {
            let numbers: Vec<String> = values
                .iter()
                .filter_map(|v| match v {
                    Scalar::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect();
            qb.push_bind(numbers).push("::numeric[]");
        }
        Target::Field { .. } => {
            qb.push_bind(texts().collect::<Vec<_>>());
        }
        Target::Via { .. } | Target::Row { .. } => {
            unreachable!("values bind against the innermost target")
        }
    }
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn push_filter(qb: &mut QueryBuilder<Postgres>, f: &Filter) {
    push_filter_in(qb, f, &Base::top());
}

fn push_filter_in(qb: &mut QueryBuilder<Postgres>, f: &Filter, base: &Base) {
    match f {
        Filter::All(parts) | Filter::Any(parts) => {
            let all = matches!(f, Filter::All(_));
            if parts.is_empty() {
                qb.push(if all { "TRUE" } else { "FALSE" });
                return;
            }
            qb.push("(");
            for (i, part) in parts.iter().enumerate() {
                if i > 0 {
                    qb.push(if all { " AND " } else { " OR " });
                }
                push_filter_in(qb, part, base);
            }
            qb.push(")");
        }
        Filter::Cond(c) if c.target.is_nested() => push_nested(qb, c, base),
        Filter::Cond(c) => push_cond(qb, c, base),
    }
}

/// A condition through a relationship or into a list: `EXISTS` some linked document or
/// row matching it. The negative operators mean "none matches", so they become `NOT EXISTS`
/// of the positive form, the way a missing value already satisfies them on plain fields.
fn push_nested(qb: &mut QueryBuilder<Postgres>, c: &Cond, base: &Base) {
    let (negate, op, value) = match (c.op, &c.value) {
        (Op::NotEquals, v) => (true, Op::Equals, v.clone()),
        (Op::NotIn, v) => (true, Op::In, v.clone()),
        (Op::Exists, Operand::One(Scalar::Bool(false))) => {
            (true, Op::Exists, Operand::One(Scalar::Bool(true)))
        }
        (op, v) => (false, op, v.clone()),
    };
    if negate {
        qb.push("NOT ");
    }
    push_exists(qb, &c.target, op, &value, base);
}

/// A stored link's text, as a `uuid` only when it looks like one, so bad data never errors.
fn guarded_uuid(text: &str) -> String {
    format!(
        "(CASE WHEN {text} ~* '^[0-9a-f]{{8}}-[0-9a-f]{{4}}-[0-9a-f]{{4}}-[0-9a-f]{{4}}-[0-9a-f]{{12}}$' THEN ({text})::uuid END)"
    )
}

/// `jsonb` at `path` when it's an array, else an empty one, so `jsonb_array_elements` never fails.
fn array_at(base: &str, path: &[String]) -> String {
    path.iter().for_each(|s| assert_identifier(s));
    let at = format!("({base} #> '{{{}}}')", path.join(","));
    format!("(CASE WHEN jsonb_typeof({at}) = 'array' THEN {at} ELSE '[]'::jsonb END)")
}

// ponytail: the EXISTS runs per candidate document (a primary-key probe each), so a very
// selective filter through a link on a huge collection still visits every row. If that shows
// up, drive top-level links from `relations` (matching linked documents first, then
// `relations_to_doc_from_doc`) and keep this form for links inside list rows.
fn push_exists(
    qb: &mut QueryBuilder<Postgres>,
    target: &Target,
    op: Op,
    value: &Operand,
    base: &Base,
) {
    let depth = base.depth + 1;
    // Inside the subquery a bare `published` would mean the linked document's, so the
    // outer document's JSON is named by its table.
    let outer = if base.depth == 0 {
        "documents.published".to_owned()
    } else {
        base.json.clone()
    };
    let inner = match target {
        Target::Row {
            list,
            inner,
            blocks,
        } => {
            let alias = format!("r{depth}");
            qb.push(format!(
                "EXISTS (SELECT 1 FROM jsonb_array_elements({}) AS {alias}(v) \
                 WHERE jsonb_typeof({alias}.v) = 'object' AND ",
                array_at(&outer, list)
            ));
            // Only rows of the block types that have the field: another type's field of the
            // same name may hold something else entirely.
            if let Some(types) = blocks {
                qb.push(format!("({alias}.v ->> 'blockType') = ANY("))
                    .push_bind(types.clone())
                    .push(") AND ");
            }
            let row = Base {
                json: format!("{alias}.v"),
                columns: String::new(),
                depth,
            };
            (inner, row)
        }
        Target::Via {
            link,
            many,
            polymorphic,
            collections,
            inner,
            guards,
        } => {
            let alias = format!("t{depth}");
            link.iter().for_each(|s| assert_identifier(s));
            let path = link.join(",");
            let ids = match (many, polymorphic) {
                (false, false) => format!(
                    "ARRAY[{}]",
                    guarded_uuid(&format!("({outer} #>> '{{{path}}}')"))
                ),
                (false, true) => format!(
                    "ARRAY[{}]",
                    guarded_uuid(&format!("({outer} #>> '{{{path},value}}')"))
                ),
                (true, false) => format!(
                    "ARRAY(SELECT {} FROM jsonb_array_elements({}) AS x WHERE jsonb_typeof(x) = 'string')",
                    guarded_uuid("(x #>> '{}')"),
                    array_at(&outer, link)
                ),
                (true, true) => format!(
                    "ARRAY(SELECT {} FROM jsonb_array_elements({}) AS x WHERE jsonb_typeof(x) = 'object')",
                    guarded_uuid("(x ->> 'value')"),
                    array_at(&outer, link)
                ),
            };
            qb.push(format!(
                "EXISTS (SELECT 1 FROM documents {alias} WHERE {alias}.tenant_id = "
            ))
            .push_bind(DEFAULT_TENANT)
            .push(format!(
                " AND {alias}.deleted_at IS NULL AND {alias}.id = ANY({ids}) \
                     AND {alias}.collection_id IN (SELECT id FROM collections WHERE tenant_id = "
            ))
            .push_bind(DEFAULT_TENANT)
            .push(" AND slug = ANY(")
            .push_bind(collections.clone())
            .push(")) AND ");
            let linked = Base {
                json: format!("{alias}.published"),
                columns: format!("{alias}."),
                depth,
            };
            if !guards.is_empty() {
                push_guards(qb, &alias, collections, guards, &linked);
            }
            (inner, linked)
        }
        _ => unreachable!("push_exists is only called on nested targets"),
    };
    let (inner, next) = inner;
    if inner.is_nested() {
        push_exists(qb, inner, op, value, &next);
    } else {
        let leaf = Cond {
            target: (**inner).clone(),
            op,
            value: value.clone(),
        };
        push_cond(qb, &leaf, &next);
    }
    qb.push(")");
}

/// `(…) AND `: a linked document must be in a collection the caller may read, and match
/// that collection's read filter. A collection with no guard entry reads nothing.
fn push_guards(
    qb: &mut QueryBuilder<Postgres>,
    alias: &str,
    collections: &[String],
    guards: &[(String, Option<Box<Filter>>)],
    linked: &Base,
) {
    qb.push("(FALSE");
    for slug in collections {
        let Some((_, guard)) = guards.iter().find(|(s, _)| s == slug) else {
            continue;
        };
        qb.push(format!(
            " OR ({alias}.collection_id = (SELECT id FROM collections WHERE tenant_id = "
        ))
        .push_bind(DEFAULT_TENANT)
        .push(" AND slug = ")
        .push_bind(slug.clone())
        .push(")");
        if let Some(f) = guard {
            qb.push(" AND ");
            push_filter_in(qb, f, linked);
        }
        qb.push(")");
    }
    qb.push(") AND ");
}

fn push_cond(qb: &mut QueryBuilder<Postgres>, c: &Cond, base: &Base) {
    let e = expr_in(&c.target, base);
    let compare = |qb: &mut QueryBuilder<Postgres>, sql_op: &str, v: &Scalar| {
        qb.push(&e).push(sql_op);
        bind(qb, &c.target, v);
    };
    match (c.op, &c.value) {
        (Op::Exists, Operand::One(Scalar::Bool(present))) => {
            qb.push(&e)
                .push(if *present { " IS NOT NULL" } else { " IS NULL" });
        }
        (Op::Equals, Operand::One(v)) => compare(qb, " = ", v),
        (Op::NotEquals, Operand::One(v)) => compare(qb, " IS DISTINCT FROM ", v),
        (Op::Gt, Operand::One(v)) => compare(qb, " > ", v),
        (Op::Gte, Operand::One(v)) => compare(qb, " >= ", v),
        (Op::Lt, Operand::One(v)) => compare(qb, " < ", v),
        (Op::Lte, Operand::One(v)) => compare(qb, " <= ", v),
        (Op::In, Operand::Many(vs)) => {
            qb.push(&e).push(" = ANY(");
            bind_list(qb, &c.target, vs);
            qb.push(")");
        }
        (Op::NotIn, Operand::Many(vs)) => {
            qb.push("(")
                .push(&e)
                .push(" IS NULL OR ")
                .push(&e)
                .push(" <> ALL(");
            bind_list(qb, &c.target, vs);
            qb.push("))");
        }
        (Op::Like, Operand::One(Scalar::Text(s))) => {
            qb.push(&e).push(" ILIKE ");
            qb.push_bind(format!("%{}%", escape_like(s)));
        }
        (Op::Contains, Operand::One(Scalar::Text(s))) => {
            qb.push(&e).push(" @> ");
            let element = if c.target.kind() == ValueKind::LinkList {
                json!({ "value": s })
            } else {
                json!(s)
            };
            qb.push_bind(Json(json!([element])));
        }
        (Op::Contains, Operand::One(Scalar::Number(n))) => {
            // jsonb containment compares numbers by value, so 3 matches 3.0.
            qb.push(&e).push(" @> ");
            qb.push_bind(Json(json!([n])));
        }
        (Op::Near, Operand::Near { lng, lat, max, min }) => {
            let Target::Field { path, .. } = &c.target else {
                unreachable!("near only targets fields")
            };
            // Both coordinates must be real numbers: `least()` ignores NULLs, so a broken
            // value would otherwise come out as a distance.
            qb.push("(")
                .push(coordinate(path, 0))
                .push(" IS NOT NULL AND ")
                .push(coordinate(path, 1))
                .push(" IS NOT NULL");
            if let Some(max) = max {
                qb.push(" AND ");
                push_distance(qb, path, *lng, *lat);
                qb.push(" <= ").push_bind(*max);
            }
            if let Some(min) = min {
                qb.push(" AND ");
                push_distance(qb, path, *lng, *lat);
                qb.push(" >= ").push_bind(*min);
            }
            qb.push(")");
        }
        (op, value) => unreachable!("the parser never builds {op:?} with {value:?}"),
    }
}

/// Built-in columns are NOT NULL, so they skip NULLS LAST and can use the plain indexes.
fn order_by(sort: Option<&Sort>) -> String {
    let Some(s) = sort else {
        return "id DESC".into();
    };
    let direction = if s.descending { "DESC" } else { "ASC" };
    match &s.target {
        Target::Id => format!("id {direction}"),
        Target::Field { .. } => format!("{} {direction} NULLS LAST, id DESC", expr(&s.target)),
        _ => format!("{} {direction}, id DESC", expr(&s.target)),
    }
}

fn push_where(qb: &mut QueryBuilder<Postgres>, collection_id: Uuid, filter: Option<&Filter>) {
    qb.push(" WHERE tenant_id = ").push_bind(DEFAULT_TENANT);
    // A literal (not a bind) so the planner can match per-collection partial indexes.
    qb.push(format!(
        " AND collection_id = '{collection_id}' AND deleted_at IS NULL"
    ));
    if let Some(f) = filter {
        qb.push(" AND ");
        push_filter(qb, f);
    }
}

pub async fn list(pool: &PgPool, collection_id: Uuid, q: &ListQuery<'_>) -> Result<Page, DbError> {
    let (sort_expr, descending) = match q.sort {
        Some(s) => (expr(&s.target), s.descending),
        None => ("id".to_owned(), true),
    };

    // With `near` and no explicit sort, nearest first. Distance isn't a stored value a cursor
    // can resume from, so those results page by number only.
    let near = q
        .sort
        .is_none()
        .then(|| q.filter.and_then(Filter::near))
        .flatten();
    if near.is_some() && q.after.is_some() {
        return Err(DbError::InvalidQuery(
            "results near a point are ordered by distance; page through them with page, not after",
        ));
    }

    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT id, status, created_at, updated_at, published AS data FROM documents",
    );
    push_where(&mut qb, collection_id, q.filter);
    if let Some(cursor) = q.after {
        qb.push(" AND ");
        push_after(&mut qb, q.sort, &sort_expr, descending, cursor);
    }
    match near {
        Some((path, lng, lat)) => {
            qb.push(" ORDER BY ");
            push_distance(&mut qb, path, lng, lat);
            qb.push(" ASC NULLS LAST, id DESC LIMIT ");
        }
        None => {
            qb.push(format!(" ORDER BY {} LIMIT ", order_by(q.sort)));
        }
    }
    // One extra row says whether another page follows, without a separate count.
    qb.push_bind(q.limit.saturating_add(1))
        .push(" OFFSET ")
        .push_bind(q.offset);
    let mut docs: Vec<Document> = qb.build_query_as().fetch_all(pool).await?;
    let has_more = usize::try_from(q.limit).is_ok_and(|limit| docs.len() > limit);
    if has_more {
        docs.truncate(usize::try_from(q.limit).unwrap_or(docs.len()));
    }

    let total = if q.count {
        let mut qb = QueryBuilder::<Postgres>::new("SELECT count(*) FROM documents");
        push_where(&mut qb, collection_id, q.filter);
        Some(qb.build_query_scalar::<i64>().fetch_one(pool).await?)
    } else {
        None
    };

    let next = if has_more && near.is_none() {
        docs.last().map(|last| Cursor {
            value: sort_value(q.sort, last),
            id: last.id,
        })
    } else {
        None
    };
    Ok(Page {
        docs,
        total,
        next,
        has_more,
    })
}

/// The documents of one collection that link to each of several parents: a join field.
pub struct JoinQuery<'a> {
    pub collection_id: Uuid,
    /// Matches `relations.field_path` for the linking field (see `tachyon_core::joins::on_pattern`).
    pub pattern: &'a str,
    pub filter: Option<&'a Filter>,
    pub sort: Option<&'a Sort>,
    pub limit: i64,
    pub offset: i64,
    pub count: bool,
}

#[derive(sqlx::FromRow)]
struct JoinedRow {
    parent: Uuid,
    #[sqlx(flatten)]
    doc: Document,
}

/// ` AND` the document links to `p.parent` through a path `pattern` matches. `EXISTS`, so a
/// document linking twice still counts once.
fn push_linked(qb: &mut QueryBuilder<Postgres>, pattern: &str) {
    qb.push(
        " AND EXISTS (SELECT 1 FROM relations r WHERE r.from_doc = documents.id \
         AND r.to_doc = p.parent AND r.tenant_id = ",
    )
    .push_bind(DEFAULT_TENANT)
    .push(" AND r.field_path ~ ")
    .push_bind(pattern.to_owned())
    .push(")");
}

/// One page of linking documents for every parent, in one query: a `LATERAL` subquery
/// pages each parent's documents on its own. Every parent gets an entry, empty or not.
pub async fn joined(
    pool: &PgPool,
    parents: &[Uuid],
    q: &JoinQuery<'_>,
) -> Result<HashMap<Uuid, Page>, DbError> {
    // Each parent once, or `unnest` would answer it twice into the same page.
    let mut unique = parents.to_vec();
    unique.sort_unstable();
    unique.dedup();
    let parents = unique.as_slice();
    let mut out: HashMap<Uuid, Page> = parents
        .iter()
        .map(|p| {
            let page = Page {
                docs: Vec::new(),
                total: None,
                next: None,
                has_more: false,
            };
            (*p, page)
        })
        .collect();
    if parents.is_empty() {
        return Ok(out);
    }
    let order = order_by(q.sort);
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT p.parent, d.id, d.status, d.created_at, d.updated_at, d.data FROM unnest(",
    );
    qb.push_bind(parents.to_vec())
        .push("::uuid[]) AS p(parent) CROSS JOIN LATERAL (SELECT id, status, created_at, updated_at, published AS data, ")
        .push(format!("row_number() OVER (ORDER BY {order}) AS n FROM documents"));
    push_where(&mut qb, q.collection_id, q.filter);
    push_linked(&mut qb, q.pattern);
    qb.push(format!(" ORDER BY {order} LIMIT "))
        .push_bind(q.limit.saturating_add(1))
        .push(" OFFSET ")
        .push_bind(q.offset)
        .push(") AS d ORDER BY p.parent, d.n");
    let rows: Vec<JoinedRow> = qb.build_query_as().fetch_all(pool).await?;
    let limit = usize::try_from(q.limit).unwrap_or(usize::MAX);
    for row in rows {
        if let Some(page) = out.get_mut(&row.parent) {
            if page.docs.len() < limit {
                page.docs.push(row.doc);
            } else {
                page.has_more = true;
            }
        }
    }

    if q.count {
        let mut qb =
            QueryBuilder::<Postgres>::new("SELECT p.parent, (SELECT count(*) FROM documents");
        push_where(&mut qb, q.collection_id, q.filter);
        push_linked(&mut qb, q.pattern);
        qb.push(") FROM unnest(")
            .push_bind(parents.to_vec())
            .push("::uuid[]) AS p(parent)");
        let counts: Vec<(Uuid, i64)> = qb.build_query_as().fetch_all(pool).await?;
        for (parent, total) in counts {
            if let Some(page) = out.get_mut(&parent) {
                page.total = Some(total);
            }
        }
    }
    Ok(out)
}

#[derive(sqlx::FromRow)]
struct ReadableRow {
    collection_id: Uuid,
    #[sqlx(flatten)]
    doc: Document,
}

/// `documents::get_many`, keeping only what the caller may read. `read` maps each readable
/// collection's ID to its read filter (`None`: all of it); documents in any other
/// collection don't come back.
pub async fn get_many_readable(
    pool: &PgPool,
    ids: &[Uuid],
    read: &HashMap<Uuid, Option<Filter>>,
) -> Result<Vec<(Uuid, Document)>, DbError> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT id, collection_id, status, created_at, updated_at, published AS data \
         FROM documents WHERE tenant_id = ",
    );
    qb.push_bind(DEFAULT_TENANT)
        .push(" AND id = ANY(")
        .push_bind(ids.to_vec())
        .push(") AND deleted_at IS NULL AND (FALSE");
    // Sorted, so the same map always builds the same SQL text (and reuses its statement).
    let mut read: Vec<_> = read.iter().collect();
    read.sort_unstable_by_key(|(id, _)| **id);
    for (collection_id, filter) in read {
        qb.push(" OR (collection_id = ").push_bind(*collection_id);
        if let Some(f) = filter {
            qb.push(" AND ");
            push_filter(&mut qb, f);
        }
        qb.push(")");
    }
    qb.push(")");
    let rows: Vec<ReadableRow> = qb.build_query_as().fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| (r.collection_id, r.doc)).collect())
}

/// Whether live document `id` matches `filter`. Run after locking the row, this judges the
/// version the rest of the transaction writes over.
pub(crate) async fn matches(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    filter: &Filter,
) -> Result<bool, DbError> {
    let mut qb =
        QueryBuilder::<Postgres>::new("SELECT EXISTS (SELECT 1 FROM documents WHERE tenant_id = ");
    qb.push_bind(DEFAULT_TENANT)
        .push(" AND deleted_at IS NULL AND id = ")
        .push_bind(id)
        .push(" AND ");
    push_filter(&mut qb, filter);
    qb.push(")");
    Ok(qb.build_query_scalar().fetch_one(conn).await?)
}

/// Rows after `cursor` in `ORDER BY sort_expr <dir> NULLS LAST, id DESC` order.
fn push_after(
    qb: &mut QueryBuilder<Postgres>,
    sort: Option<&Sort>,
    e: &str,
    descending: bool,
    cursor: &Cursor,
) {
    let Some(sort) = sort else {
        qb.push("id < ").push_bind(cursor.id);
        return;
    };
    match &cursor.value {
        Some(v) => {
            qb.push("(")
                .push(e)
                .push(if descending { " < " } else { " > " });
            bind(qb, &sort.target, v);
            qb.push(" OR (").push(e).push(" = ");
            bind(qb, &sort.target, v);
            qb.push(" AND id < ").push_bind(cursor.id);
            qb.push(") OR ").push(e).push(" IS NULL)");
        }
        None => {
            qb.push("(")
                .push(e)
                .push(" IS NULL AND id < ")
                .push_bind(cursor.id)
                .push(")");
        }
    }
}

fn sort_value(sort: Option<&Sort>, doc: &Document) -> Option<Scalar> {
    match &sort?.target {
        Target::Via { .. } | Target::Row { .. } => None,
        Target::Id => Some(Scalar::Text(doc.id.to_string())),
        Target::CreatedAt => doc.created_at.format(&Rfc3339).ok().map(Scalar::Text),
        Target::UpdatedAt => doc.updated_at.format(&Rfc3339).ok().map(Scalar::Text),
        Target::Field { path, kind } => {
            let mut v = Value::Object(doc.data.0.clone());
            for segment in path {
                v = v.get(segment)?.clone();
            }
            match (kind, &v) {
                (ValueKind::Number, Value::Number(n)) => Some(Scalar::Number(n.clone())),
                (ValueKind::Boolean, Value::Bool(b)) => Some(Scalar::Bool(*b)),
                (_, Value::String(s)) => Some(Scalar::Text(s.clone())),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_sorts_match_their_indexes() {
        // NOT NULL columns get no NULLS LAST, so Postgres can walk the (…, id DESC) index.
        assert_eq!(order_by(None), "id DESC");
        let created = Sort {
            target: Target::CreatedAt,
            descending: false,
        };
        assert_eq!(order_by(Some(&created)), "created_at ASC, id DESC");
        let price = Sort {
            target: Target::Field {
                path: vec!["price".into()],
                kind: ValueKind::Number,
            },
            descending: true,
        };
        assert_eq!(
            order_by(Some(&price)),
            "(CASE WHEN jsonb_typeof(published #> '{price}') = 'number' THEN (published #>> '{price}')::numeric END) DESC NULLS LAST, id DESC"
        );
    }
}
