//! Listing with filters, sorting and cursors.
//!
//! The only text spliced into SQL here is static, a schema-validated field name (checked
//! again by `assert_identifier`), or a `Uuid`. Every user-supplied value is bound.

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
    path.iter().for_each(|s| assert_identifier(s));
    let path = path.join(",");
    match kind {
        ValueKind::Text | ValueKind::Date => format!("(published #>> '{{{path}}}')"),
        ValueKind::Number => format!("((published #>> '{{{path}}}'))::numeric"),
        ValueKind::Boolean => format!("((published #>> '{{{path}}}'))::boolean"),
        ValueKind::List => format!("(published #> '{{{path}}}')"),
    }
}

fn expr(target: &Target) -> String {
    match target {
        Target::Id => "id".into(),
        Target::CreatedAt => "created_at".into(),
        Target::UpdatedAt => "updated_at".into(),
        Target::Field { path, kind } => field_expr(path, *kind),
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
        (_, Scalar::Number(n)) => {
            qb.push_bind(*n).push("::numeric");
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
            let numbers: Vec<f64> = values
                .iter()
                .filter_map(|v| match v {
                    Scalar::Number(n) => Some(*n),
                    _ => None,
                })
                .collect();
            qb.push_bind(numbers).push("::numeric[]");
        }
        Target::Field { .. } => {
            qb.push_bind(texts().collect::<Vec<_>>());
        }
    }
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn push_filter(qb: &mut QueryBuilder<Postgres>, f: &Filter) {
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
                push_filter(qb, part);
            }
            qb.push(")");
        }
        Filter::Cond(c) => push_cond(qb, c),
    }
}

fn push_cond(qb: &mut QueryBuilder<Postgres>, c: &Cond) {
    let e = expr(&c.target);
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
            qb.push_bind(Json(json!([s])));
        }
        (op, value) => unreachable!("the parser never builds {op:?} with {value:?}"),
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
    let direction = if descending { "DESC" } else { "ASC" };

    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT id, status, created_at, updated_at, published AS data FROM documents",
    );
    push_where(&mut qb, collection_id, q.filter);
    if let Some(cursor) = q.after {
        qb.push(" AND ");
        push_after(&mut qb, q.sort, &sort_expr, descending, cursor);
    }
    qb.push(format!(
        " ORDER BY {sort_expr} {direction} NULLS LAST, id DESC LIMIT "
    ));
    qb.push_bind(q.limit).push(" OFFSET ").push_bind(q.offset);
    let docs: Vec<Document> = qb.build_query_as().fetch_all(pool).await?;

    let total = if q.count {
        let mut qb = QueryBuilder::<Postgres>::new("SELECT count(*) FROM documents");
        push_where(&mut qb, collection_id, q.filter);
        Some(qb.build_query_scalar::<i64>().fetch_one(pool).await?)
    } else {
        None
    };

    let full = q.limit > 0 && i64::try_from(docs.len()).is_ok_and(|n| n == q.limit);
    let next = if full {
        docs.last().map(|last| Cursor {
            value: sort_value(q.sort, last),
            id: last.id,
        })
    } else {
        None
    };
    Ok(Page { docs, total, next })
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
        Target::Id => Some(Scalar::Text(doc.id.to_string())),
        Target::CreatedAt => doc.created_at.format(&Rfc3339).ok().map(Scalar::Text),
        Target::UpdatedAt => doc.updated_at.format(&Rfc3339).ok().map(Scalar::Text),
        Target::Field { path, kind } => {
            let mut v = Value::Object(doc.data.0.clone());
            for segment in path {
                v = v.get(segment)?.clone();
            }
            match (kind, &v) {
                (ValueKind::Number, Value::Number(n)) => n.as_f64().map(Scalar::Number),
                (ValueKind::Boolean, Value::Bool(b)) => Some(Scalar::Bool(*b)),
                (_, Value::String(s)) => Some(Scalar::Text(s.clone())),
                _ => None,
            }
        }
    }
}
