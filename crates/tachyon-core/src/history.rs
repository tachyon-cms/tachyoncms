//! What changed between two copies of a document, field by field, for history views.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::{Field, FieldKind};

/// One difference, at a dotted path in the style errors use (`seo.title`, `layout.1.heading`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldChange {
    pub path: String,
    pub op: Op,
    pub before: Option<Value>,
    pub after: Option<Value>,
    /// The `id` of the array or block row the change is in, or is.
    pub row_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Added,
    Removed,
    Changed,
}

/// The changes from `before` to `after`, both documents of `fields` (data fields, as
/// `CollectionSchema::data` gives them), in schema order. Groups are compared field by
/// field, and array and block rows by their `id`: a row's path uses its index in `after`
/// (in `before` for a removed row), and rows that only moved show as one change to the
/// list's order, `before` and `after` holding the row IDs. Rich text, points, `hasMany`
/// values and everything else compare whole. A `null` counts as no value. Strip what the
/// reader can't see from both sides first: this shows whatever it's given.
pub fn diff_documents(
    fields: &[Field],
    before: &Map<String, Value>,
    after: &Map<String, Value>,
) -> Vec<FieldChange> {
    let mut out = Vec::new();
    diff_fields(fields, Some(before), Some(after), "", None, &mut out);
    out
}

fn present<'v>(doc: Option<&'v Map<String, Value>>, name: &str) -> Option<&'v Value> {
    doc?.get(name).filter(|v| !v.is_null())
}

fn diff_fields(
    fields: &[Field],
    before: Option<&Map<String, Value>>,
    after: Option<&Map<String, Value>>,
    prefix: &str,
    row_id: Option<&str>,
    out: &mut Vec<FieldChange>,
) {
    for f in fields {
        let path = format!("{prefix}{}", f.name);
        let (b, a) = (present(before, &f.name), present(after, &f.name));
        match (&f.kind, b, a) {
            (FieldKind::Group { fields }, _, _)
                if b.is_none_or(Value::is_object) && a.is_none_or(Value::is_object) =>
            {
                let (b, a) = (b.and_then(Value::as_object), a.and_then(Value::as_object));
                diff_fields(fields, b, a, &format!("{path}."), row_id, out);
            }
            (FieldKind::Array { .. } | FieldKind::Blocks { .. }, _, _)
                if b.is_none_or(Value::is_array) && a.is_none_or(Value::is_array) =>
            {
                let none = Vec::new();
                let b = b.and_then(Value::as_array).unwrap_or(&none);
                let a = a.and_then(Value::as_array).unwrap_or(&none);
                diff_rows(f, b, a, &path, out);
            }
            _ => push(out, path, b, a, row_id),
        }
    }
}

/// A change, when `before` and `after` differ.
fn push(
    out: &mut Vec<FieldChange>,
    path: String,
    before: Option<&Value>,
    after: Option<&Value>,
    row_id: Option<&str>,
) {
    let op = match (before, after) {
        (None, None) => return,
        (Some(b), Some(a)) if b == a => return,
        (None, Some(_)) => Op::Added,
        (Some(_), None) => Op::Removed,
        (Some(_), Some(_)) => Op::Changed,
    };
    out.push(FieldChange {
        path,
        op,
        before: before.cloned(),
        after: after.cloned(),
        row_id: row_id.map(str::to_owned),
    });
}

fn row_id(row: &Value) -> Option<&str> {
    row.get("id").and_then(Value::as_str)
}

/// The fields a row of list field `f` holds: its own, or its block type's. `None` for a
/// block type the schema doesn't know, whose rows compare whole.
fn row_fields<'f>(f: &'f Field, row: &Value) -> Option<&'f [Field]> {
    match &f.kind {
        FieldKind::Array { fields, .. } => Some(fields),
        _ => {
            let slug = row.get("blockType").and_then(Value::as_str)?;
            f.kind.block(slug).map(|b| b.fields.as_slice())
        }
    }
}

fn diff_rows(f: &Field, before: &[Value], after: &[Value], path: &str, out: &mut Vec<FieldChange>) {
    // A row without an ID can't be followed, so it's matched by its place in the list.
    let find = |row: &Value, i: usize| match row_id(row) {
        Some(id) => before.iter().position(|r| row_id(r) == Some(id)),
        None => (i < before.len() && row_id(&before[i]).is_none()).then_some(i),
    };
    let mut matched = vec![false; before.len()];
    let mut order = Vec::new();
    for (i, row) in after.iter().enumerate() {
        let at = format!("{path}.{i}");
        let id = row_id(row);
        let Some(j) = find(row, i) else {
            push(out, at, None, Some(row), id);
            continue;
        };
        matched[j] = true;
        order.push(j);
        let old = &before[j];
        let same_type = old.get("blockType") == row.get("blockType");
        match (row_fields(f, row), old.as_object(), row.as_object()) {
            (Some(fields), Some(b), Some(a)) if same_type => {
                diff_fields(fields, Some(b), Some(a), &format!("{at}."), id, out)
            }
            _ => push(out, at, Some(old), Some(row), id),
        }
    }
    for (j, row) in before.iter().enumerate().filter(|(j, _)| !matched[*j]) {
        push(out, format!("{path}.{j}"), Some(row), None, row_id(row));
    }
    // Rows kept, in a new order.
    if order.windows(2).any(|w| w[0] > w[1]) {
        let ids = |rows: Vec<&Value>| -> Value {
            rows.into_iter()
                .map(|r| row_id(r).map_or(Value::Null, |id| id.into()))
                .collect()
        };
        let mut kept: Vec<usize> = order.clone();
        kept.sort_unstable();
        out.push(FieldChange {
            path: path.to_owned(),
            op: Op::Changed,
            before: Some(ids(kept.iter().map(|&j| &before[j]).collect())),
            after: Some(ids(order.iter().map(|&j| &before[j]).collect())),
            row_id: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields() -> Vec<Field> {
        serde_json::from_value(json!([
            {"name": "title", "type": "text"},
            {"name": "seo", "type": "group", "fields": [
                {"name": "title", "type": "text"},
                {"name": "image", "type": "group", "fields": [{"name": "alt", "type": "text"}]}
            ]},
            {"name": "body", "type": "richText"},
            {"name": "tags", "type": "text", "hasMany": true},
            {"name": "layout", "type": "blocks", "blocks": [
                {"slug": "hero", "fields": [{"name": "heading", "type": "text"}]},
                {"slug": "quote", "fields": [{"name": "text", "type": "text"}]}
            ]},
            {"name": "links", "type": "array", "fields": [{"name": "url", "type": "text"}]}
        ]))
        .unwrap()
    }

    fn doc(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn change(path: &str, op: Op, before: Value, after: Value, row: Option<&str>) -> FieldChange {
        let some = |v: Value| (!v.is_null()).then_some(v);
        FieldChange {
            path: path.into(),
            op,
            before: some(before),
            after: some(after),
            row_id: row.map(str::to_owned),
        }
    }

    #[test]
    fn diff_documents_cases() {
        let fields = fields();
        let text = |t: &str| {
            json!({"type": "doc", "content": [{"type": "paragraph",
                                    "content": [{"type": "text", "text": t}]}]})
        };
        let base = json!({
            "title": "A",
            "seo": {"title": "S", "image": {"alt": "x"}},
            "body": text("one"),
            "tags": ["a", "b"],
            "layout": [
                {"id": "h1", "blockType": "hero", "heading": "Hi"},
                {"id": "q1", "blockType": "quote", "text": "Q"}
            ],
            "links": [{"id": "l1", "url": "a"}, {"id": "l2", "url": "b"}]
        });

        // Equal documents: nothing.
        assert_eq!(
            diff_documents(&fields, &doc(base.clone()), &doc(base.clone())),
            []
        );

        // A scalar change, and nesting through groups.
        let mut next = base.clone();
        next["title"] = "B".into();
        next["seo"]["image"]["alt"] = "y".into();
        next["seo"]["title"] = Value::Null;
        assert_eq!(
            diff_documents(&fields, &doc(base.clone()), &doc(next)),
            [
                change("title", Op::Changed, json!("A"), json!("B"), None),
                change("seo.title", Op::Removed, json!("S"), Value::Null, None),
                change("seo.image.alt", Op::Changed, json!("x"), json!("y"), None),
            ]
        );

        // Rich text and hasMany values compare whole.
        let mut next = base.clone();
        next["body"] = text("two");
        next["tags"] = json!(["a", "c"]);
        assert_eq!(
            diff_documents(&fields, &doc(base.clone()), &doc(next)),
            [
                change("body", Op::Changed, text("one"), text("two"), None),
                change(
                    "tags",
                    Op::Changed,
                    json!(["a", "b"]),
                    json!(["a", "c"]),
                    None
                ),
            ]
        );

        // A row added in front: the others are matched by id, at their new index.
        let mut next = base.clone();
        next["layout"] = json!([
            {"id": "q0", "blockType": "quote", "text": "New"},
            {"id": "h1", "blockType": "hero", "heading": "Hello"},
            {"id": "q1", "blockType": "quote", "text": "Q"}
        ]);
        let added = json!({"id": "q0", "blockType": "quote", "text": "New"});
        assert_eq!(
            diff_documents(&fields, &doc(base.clone()), &doc(next)),
            [
                change("layout.0", Op::Added, Value::Null, added, Some("q0")),
                change(
                    "layout.1.heading",
                    Op::Changed,
                    json!("Hi"),
                    json!("Hello"),
                    Some("h1")
                ),
            ]
        );

        // A row removed: named by its index before.
        let mut next = base.clone();
        next["links"] = json!([{"id": "l2", "url": "b"}]);
        assert_eq!(
            diff_documents(&fields, &doc(base.clone()), &doc(next)),
            [change(
                "links.0",
                Op::Removed,
                json!({"id": "l1", "url": "a"}),
                Value::Null,
                Some("l1")
            )]
        );

        // Rows that only moved: one change to the order.
        let mut next = base.clone();
        next["links"] = json!([{"id": "l2", "url": "b"}, {"id": "l1", "url": "a"}]);
        assert_eq!(
            diff_documents(&fields, &doc(base.clone()), &doc(next)),
            [change(
                "links",
                Op::Changed,
                json!(["l1", "l2"]),
                json!(["l2", "l1"]),
                None
            )]
        );

        // A row that changed block type is replaced whole; a field appears from nothing.
        let mut next = base.clone();
        next["layout"][1] = json!({"id": "q1", "blockType": "hero", "heading": "Q"});
        let before = doc(json!({"title": "A"}));
        let after = doc(json!({"title": "A", "seo": {"title": "S"}}));
        assert_eq!(
            diff_documents(&fields, &doc(base.clone()), &doc(next.clone())),
            [change(
                "layout.1",
                Op::Changed,
                base["layout"][1].clone(),
                next["layout"][1].clone(),
                Some("q1")
            )]
        );
        assert_eq!(
            diff_documents(&fields, &before, &after),
            [change(
                "seo.title",
                Op::Added,
                Value::Null,
                json!("S"),
                None
            )]
        );
    }
}
