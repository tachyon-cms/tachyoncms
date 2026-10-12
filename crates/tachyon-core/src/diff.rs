//! What changes between two versions of a collection's schema, and how stored documents
//! are reshaped to match.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::schema::{CollectionSchema, Field, FieldKind, SchemaError};

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
        self.changes
            .iter()
            .any(|c| matches!(c, Change::Removed { .. } | Change::TypeChanged { .. }))
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
        if find(&old.fields, from).is_none() {
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
    match (a, b) {
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

fn find<'a>(fields: &'a [Field], path: &str) -> Option<&'a Field> {
    let mut current = fields;
    let mut found = None;
    for segment in path.split('.') {
        let f = current.iter().find(|f| f.name == segment)?;
        current = match &f.kind {
            FieldKind::Group { fields } | FieldKind::Array { fields, .. } => fields,
            _ => &[],
        };
        found = Some(f);
    }
    found
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
    // Deepest first, so a renamed group's children are still found under its old name.
    let mut ordered: Vec<(&String, &String)> = renames.iter().collect();
    ordered.sort_by_key(|(from, _)| std::cmp::Reverse(from.matches('.').count()));
    for (from, to) in ordered {
        let segments: Vec<&str> = from.split('.').collect();
        rename_in(&mut doc, &segments, to);
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

fn rename_in(obj: &mut Map<String, Value>, segments: &[&str], to: &str) {
    match segments {
        [name] => {
            if let Some(value) = obj.remove(*name) {
                obj.insert(to.to_owned(), value);
            }
        }
        [head, rest @ ..] => match obj.get_mut(*head) {
            Some(Value::Object(inner)) => rename_in(inner, rest, to),
            Some(Value::Array(rows)) => {
                for row in rows {
                    if let Value::Object(inner) = row {
                        rename_in(inner, rest, to);
                    }
                }
            }
            _ => {}
        },
        [] => {}
    }
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
}
