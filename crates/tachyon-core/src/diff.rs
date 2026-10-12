//! What changes between two versions of a collection's schema, and how stored documents
//! are reshaped to match.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::schema::{CollectionSchema, Field, FieldKind, SchemaError, find_field};

/// Field renames: old dotted path to new name, such as `{"seo.title": "headline"}`.
pub type Renames = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Change {
    Added {
        path: String,
        kind: &'static str,
    },
    Removed {
        path: String,
        kind: &'static str,
    },
    Renamed {
        from: String,
        to: String,
    },
    TypeChanged {
        path: String,
        from: &'static str,
        to: &'static str,
    },
    OptionsChanged {
        path: String,
    },
    /// A relationship now points at another collection; its existing links are cleared.
    Retargeted {
        path: String,
        from: String,
        to: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SchemaDiff {
    pub changes: Vec<Change>,
}

impl SchemaDiff {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Removing or retyping a field can lose data.
    pub fn is_destructive(&self) -> bool {
        self.changes.iter().any(|c| {
            matches!(
                c,
                Change::Removed { .. } | Change::TypeChanged { .. } | Change::Retargeted { .. }
            )
        })
    }
}

fn invalid(message: String) -> SchemaError {
    SchemaError::InvalidChange(message)
}

pub fn diff(
    old: &CollectionSchema,
    new: &CollectionSchema,
    renames: &Renames,
) -> Result<SchemaDiff, SchemaError> {
    if old.slug != new.slug {
        return Err(invalid("renaming a collection isn't supported yet".into()));
    }
    if old.kind != new.kind {
        return Err(invalid(
            "a collection can't become a global, or the other way round".into(),
        ));
    }
    for from in renames.keys() {
        if find_field(&old.fields, from).is_none() {
            return Err(invalid(format!(
                "can't rename `{from}`: there's no such field"
            )));
        }
    }
    let mut changes = Vec::new();
    compare(&old.fields, &new.fields, "", "", renames, &mut changes)?;
    Ok(SchemaDiff { changes })
}

fn compare(
    old: &[Field],
    new: &[Field],
    old_prefix: &str,
    new_prefix: &str,
    renames: &Renames,
    out: &mut Vec<Change>,
) -> Result<(), SchemaError> {
    let new_name = |f: &Field| {
        renames
            .get(&join(old_prefix, &f.name))
            .cloned()
            .unwrap_or_else(|| f.name.clone())
    };
    for o in old {
        let old_path = join(old_prefix, &o.name);
        let name = new_name(o);
        let new_path = join(new_prefix, &name);
        let renamed = name != o.name;
        if renamed && old.iter().any(|s| s.name == name && new_name(s) == name) {
            return Err(invalid(format!(
                "can't rename `{old_path}` to `{name}`: a field with that name already exists"
            )));
        }
        let Some(n) = new.iter().find(|n| n.name == name) else {
            if renamed {
                return Err(invalid(format!(
                    "can't rename `{old_path}` to `{name}`: there's no `{new_path}` in the new fields"
                )));
            }
            out.push(Change::Removed {
                path: old_path,
                kind: o.kind.name(),
            });
            continue;
        };
        if renamed {
            out.push(Change::Renamed {
                from: old_path.clone(),
                to: new_path.clone(),
            });
        }
        match (&o.kind, &n.kind) {
            (FieldKind::Group { fields: a }, FieldKind::Group { fields: b })
            | (FieldKind::Array { fields: a, .. }, FieldKind::Array { fields: b, .. }) => {
                if options(o) != options(n) {
                    out.push(Change::OptionsChanged {
                        path: new_path.clone(),
                    });
                }
                compare(a, b, &old_path, &new_path, renames, out)?;
            }
            (
                FieldKind::Relationship {
                    relation_to: from,
                    has_many: h1,
                },
                FieldKind::Relationship {
                    relation_to: to,
                    has_many: h2,
                },
            ) if h1 == h2 && from != to => out.push(Change::Retargeted {
                path: new_path,
                from: from.clone(),
                to: to.clone(),
            }),
            _ if !same_type(&o.kind, &n.kind) => out.push(Change::TypeChanged {
                path: new_path,
                from: o.kind.name(),
                to: n.kind.name(),
            }),
            _ => {
                if options(o) != options(n) {
                    out.push(Change::OptionsChanged { path: new_path });
                }
            }
        }
    }
    for n in new {
        if !old.iter().any(|o| new_name(o) == n.name) {
            out.push(Change::Added {
                path: join(new_prefix, &n.name),
                kind: n.kind.name(),
            });
        }
    }
    Ok(())
}

/// A field with its name and nested fields blanked, so two can be compared on options alone.
fn options(f: &Field) -> Field {
    let mut f = f.clone();
    f.name.clear();
    if let FieldKind::Group { fields } | FieldKind::Array { fields, .. } = &mut f.kind {
        fields.clear();
    }
    f
}

fn same_type(a: &FieldKind, b: &FieldKind) -> bool {
    use FieldKind::{Code, Radio, Select, Text, Textarea};
    match (a, b) {
        // Same stored shape, so a change between them is an option change.
        (
            Text { .. } | Textarea { .. } | Code { .. },
            Text { .. } | Textarea { .. } | Code { .. },
        ) => true,
        (
            Radio { .. }
            | Select {
                multiple: false, ..
            },
            Radio { .. }
            | Select {
                multiple: false, ..
            },
        ) => true,
        (FieldKind::Select { multiple: x, .. }, FieldKind::Select { multiple: y, .. }) => x == y,
        (FieldKind::Date { date_only: x }, FieldKind::Date { date_only: y }) => x == y,
        (
            FieldKind::Relationship {
                relation_to: r1,
                has_many: h1,
            },
            FieldKind::Relationship {
                relation_to: r2,
                has_many: h2,
            },
        ) => r1 == r2 && h1 == h2,
        _ => a.name() == b.name(),
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

/// Reshapes a stored document for a new schema: applies renames, drops values the new
/// schema has no field for, and fills in new fields' defaults. Values whose type changed
/// are left alone; validation reports them.
pub fn migrate_document(
    new: &[Field],
    renames: &Renames,
    doc: &Map<String, Value>,
) -> Map<String, Value> {
    let mut doc = doc.clone();
    // Renames are grouped by the object they happen in and applied deepest first, so a
    // renamed group's children are still found under its old name. Within one object every
    // source is taken out before any target goes in, so swaps and chains keep their data.
    let mut by_parent: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (from, to) in renames {
        let (parent, name) = from.rsplit_once('.').unwrap_or(("", from.as_str()));
        by_parent
            .entry(parent)
            .or_default()
            .push((name, to.as_str()));
    }
    let mut parents: Vec<(&str, Vec<(&str, &str)>)> = by_parent.into_iter().collect();
    parents.sort_by_key(|(parent, _)| std::cmp::Reverse(depth(parent)));
    for (parent, batch) in parents {
        let segments: Vec<&str> = if parent.is_empty() {
            Vec::new()
        } else {
            parent.split('.').collect()
        };
        rename_batch(&mut doc, &segments, &batch);
    }
    prune(new, &mut doc);
    for f in new {
        if let Some(default) = &f.default
            && !doc.contains_key(&f.name)
        {
            doc.insert(f.name.clone(), default.clone());
        }
    }
    doc
}

fn depth(path: &str) -> usize {
    if path.is_empty() {
        0
    } else {
        path.matches('.').count() + 1
    }
}

fn rename_batch(obj: &mut Map<String, Value>, segments: &[&str], batch: &[(&str, &str)]) {
    match segments {
        [] => {
            let taken: Vec<(&str, Value)> = batch
                .iter()
                .filter_map(|(from, to)| obj.remove(*from).map(|value| (*to, value)))
                .collect();
            for (to, value) in taken {
                obj.insert(to.to_owned(), value);
            }
        }
        [head, rest @ ..] => match obj.get_mut(*head) {
            Some(Value::Object(inner)) => rename_batch(inner, rest, batch),
            Some(Value::Array(rows)) => {
                for row in rows {
                    if let Value::Object(inner) = row {
                        rename_batch(inner, rest, batch);
                    }
                }
            }
            _ => {}
        },
    }
}

/// Removes the value at a dotted path, in every array row along the way.
/// Returns whether anything was removed.
pub fn clear_at(doc: &mut Map<String, Value>, path: &str) -> bool {
    fn walk(obj: &mut Map<String, Value>, segments: &[&str]) -> bool {
        match segments {
            [name] => obj.remove(*name).is_some(),
            [head, rest @ ..] => match obj.get_mut(*head) {
                Some(Value::Object(inner)) => walk(inner, rest),
                Some(Value::Array(rows)) => rows.iter_mut().fold(false, |any, row| match row {
                    Value::Object(inner) => walk(inner, rest) | any,
                    _ => any,
                }),
                _ => false,
            },
            [] => false,
        }
    }
    walk(doc, &path.split('.').collect::<Vec<_>>())
}

fn prune(fields: &[Field], obj: &mut Map<String, Value>) {
    obj.retain(|key, _| fields.iter().any(|f| &f.name == key));
    for f in fields {
        match (&f.kind, obj.get_mut(&f.name)) {
            (FieldKind::Group { fields }, Some(Value::Object(inner))) => prune(fields, inner),
            (FieldKind::Array { fields, .. }, Some(Value::Array(rows))) => {
                for row in rows {
                    if let Value::Object(inner) = row {
                        prune(fields, inner);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(v: Value) -> CollectionSchema {
        serde_json::from_value(v).unwrap()
    }

    fn renames(pairs: &[(&str, &str)]) -> Renames {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    #[test]
    fn reports_every_kind_of_change() {
        let old = schema(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "summary", "type": "textarea"},
            {"name": "price", "type": "number"},
            {"name": "seo", "type": "group", "fields": [{"name": "title", "type": "text"}]}
        ]}));
        let new = schema(json!({"slug": "posts", "fields": [
            {"name": "headline", "type": "text", "required": true},
            {"name": "price", "type": "text"},
            {"name": "seo", "type": "group", "fields": [
                {"name": "title", "type": "text"},
                {"name": "description", "type": "textarea"}
            ]}
        ]}));
        let d = diff(&old, &new, &renames(&[("title", "headline")])).unwrap();
        assert_eq!(
            d.changes,
            vec![
                Change::Renamed {
                    from: "title".into(),
                    to: "headline".into()
                },
                Change::OptionsChanged {
                    path: "headline".into()
                },
                Change::Removed {
                    path: "summary".into(),
                    kind: "textarea"
                },
                Change::TypeChanged {
                    path: "price".into(),
                    from: "number",
                    to: "text"
                },
                Change::Added {
                    path: "seo.description".into(),
                    kind: "textarea"
                },
            ]
        );
        assert!(d.is_destructive());
        assert_eq!(
            serde_json::to_value(&d.changes[0]).unwrap(),
            json!({"type": "renamed", "from": "title", "to": "headline"})
        );
    }

    #[test]
    fn renames_a_group_and_its_children_together() {
        let old = schema(json!({"slug": "posts", "fields": [
            {"name": "seo", "type": "group", "fields": [{"name": "title", "type": "text"}]}
        ]}));
        let new = schema(json!({"slug": "posts", "fields": [
            {"name": "meta", "type": "group", "fields": [{"name": "headline", "type": "text"}]}
        ]}));
        let d = diff(
            &old,
            &new,
            &renames(&[("seo", "meta"), ("seo.title", "headline")]),
        )
        .unwrap();
        assert_eq!(
            d.changes,
            vec![
                Change::Renamed {
                    from: "seo".into(),
                    to: "meta".into()
                },
                Change::Renamed {
                    from: "seo.title".into(),
                    to: "meta.headline".into()
                },
            ]
        );
        assert!(!d.is_destructive());
    }

    #[test]
    fn unchanged_schemas_have_no_changes() {
        let s = schema(json!({"slug": "posts", "fields": [{"name": "title", "type": "text"}]}));
        assert!(diff(&s, &s, &Renames::new()).unwrap().is_empty());
    }

    #[test]
    fn refuses_changes_it_cant_carry_out() {
        let old = schema(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"}, {"name": "name", "type": "text"}
        ]}));
        let cases = [
            (
                json!({"slug": "articles", "fields": []}),
                vec![],
                "renaming a collection isn't supported yet",
            ),
            (
                json!({"slug": "posts", "kind": "global", "fields": []}),
                vec![],
                "a collection can't become a global, or the other way round",
            ),
            (
                json!({"slug": "posts", "fields": []}),
                vec![("nope", "x")],
                "can't rename `nope`: there's no such field",
            ),
            (
                json!({"slug": "posts", "fields": [{"name": "name", "type": "text"}]}),
                vec![("title", "heading")],
                "can't rename `title` to `heading`: there's no `heading` in the new fields",
            ),
            (
                json!({"slug": "posts", "fields": [{"name": "name", "type": "text"}]}),
                vec![("title", "name")],
                "can't rename `title` to `name`: a field with that name already exists",
            ),
        ];
        for (new, pairs, message) in cases {
            let err = diff(&old, &schema(new), &renames(&pairs)).unwrap_err();
            assert_eq!(err.to_string(), message);
        }
    }
    #[test]
    fn migrate_document_renames_prunes_and_fills_defaults() {
        let new = schema(json!({"slug": "posts", "fields": [
            {"name": "headline", "type": "text"},
            {"name": "meta", "type": "group", "fields": [{"name": "headline", "type": "text"}]},
            {"name": "links", "type": "array", "fields": [{"name": "href", "type": "url"}]},
            {"name": "featured", "type": "boolean", "default": false}
        ]}));
        let doc = json!({
            "title": "T",
            "seo": {"title": "S", "junk": 1},
            "links": [{"url": "https://a.example", "old": 1}, {"url": "https://b.example"}],
            "summary": "gone"
        });
        let r = renames(&[
            ("title", "headline"),
            ("seo", "meta"),
            ("seo.title", "headline"),
            ("links.url", "href"),
        ]);
        let out = migrate_document(&new.fields, &r, doc.as_object().unwrap());
        assert_eq!(
            Value::Object(out),
            json!({
                "headline": "T",
                "meta": {"headline": "S"},
                "links": [{"href": "https://a.example"}, {"href": "https://b.example"}],
                "featured": false
            })
        );
    }

    #[test]
    fn swapped_and_chained_renames_keep_every_value() {
        let new = schema(json!({"slug": "posts", "fields": [
            {"name": "a", "type": "text"}, {"name": "b", "type": "text"}, {"name": "c", "type": "text"}
        ]}));
        let doc = json!({"a": "A", "b": "B"});
        let swap = migrate_document(
            &new.fields,
            &renames(&[("a", "b"), ("b", "a")]),
            doc.as_object().unwrap(),
        );
        assert_eq!(Value::Object(swap), json!({"a": "B", "b": "A"}));
        let chain = migrate_document(
            &new.fields,
            &renames(&[("a", "b"), ("b", "c")]),
            doc.as_object().unwrap(),
        );
        assert_eq!(Value::Object(chain), json!({"b": "A", "c": "B"}));

        let rows = schema(
            json!({"slug": "p", "fields": [{"name": "rows", "type": "array", "fields": [
                {"name": "x", "type": "text"}, {"name": "y", "type": "text"}
            ]}]}),
        );
        let doc = json!({"rows": [{"x": "1", "y": "2"}]});
        let out = migrate_document(
            &rows.fields,
            &renames(&[("rows.x", "y"), ("rows.y", "x")]),
            doc.as_object().unwrap(),
        );
        assert_eq!(Value::Object(out), json!({"rows": [{"x": "2", "y": "1"}]}));

        let old = schema(
            json!({"slug": "posts", "fields": [{"name": "a", "type": "text"}, {"name": "b", "type": "text"}]}),
        );
        assert_eq!(
            diff(&old, &old, &renames(&[("a", "b"), ("b", "a")]))
                .unwrap()
                .changes
                .len(),
            2
        );
    }

    #[test]
    fn clear_at_removes_values_everywhere_on_a_path() {
        let mut doc = json!({"who": "x", "rows": [{"who": "y"}, {}]})
            .as_object()
            .unwrap()
            .clone();
        assert!(clear_at(&mut doc, "who"));
        assert!(clear_at(&mut doc, "rows.who"));
        assert!(!clear_at(&mut doc, "missing"));
        assert_eq!(Value::Object(doc), json!({"rows": [{}, {}]}));
    }
    #[test]
    fn same_shaped_types_change_options_not_type() {
        let old = schema(json!({"slug": "posts", "fields": [
            {"name": "body", "type": "text"},
            {"name": "size", "type": "select", "options": ["s", "l"]}
        ]}));
        let new = schema(json!({"slug": "posts", "fields": [
            {"name": "body", "type": "code"},
            {"name": "size", "type": "radio", "options": ["s", "l"]}
        ]}));
        let d = diff(&old, &new, &Renames::new()).unwrap();
        assert_eq!(
            d.changes,
            vec![
                Change::OptionsChanged {
                    path: "body".into()
                },
                Change::OptionsChanged {
                    path: "size".into()
                },
            ]
        );
        assert!(!d.is_destructive());
    }
}
