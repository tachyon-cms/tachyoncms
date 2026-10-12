//! What changes between two versions of a collection's schema, and how stored documents
//! are reshaped to match.

use std::collections::BTreeMap;

use serde::Serialize;

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

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
}
