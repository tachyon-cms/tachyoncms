//! What changes between two versions of a collection's schema, and how stored documents
//! are reshaped to match.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::schema::{
    CollectionSchema, Field, FieldKind, RelationTo, SchemaError, data_fields, find_field,
};

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
    /// Only the admin layout moved (rows, collapsibles, tabs); stored data is untouched.
    LayoutChanged,
    /// A collection-wide option such as `defaultPopulate` changed; stored data is untouched.
    CollectionOptionsChanged,
    /// A relationship's targets widened without losing any (one collection to several that
    /// include it); existing links are kept and reshaped.
    Relinked {
        path: String,
        from: String,
        to: String,
    },
    /// A relationship's targets changed so that some links can't be kept; those are cleared,
    /// the rest are reshaped.
    Retargeted {
        path: String,
        from: String,
        to: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SchemaDiff {
    pub changes: Vec<Change>,
    /// How each relationship whose targets changed must be converted. Internal to applying
    /// the change, so not serialised.
    #[serde(skip)]
    pub links: Vec<LinkChange>,
}

/// A relationship (at a path in the new schema) whose allowed targets changed.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkChange {
    pub path: String,
    pub from: RelationTo,
    pub to: RelationTo,
    /// Whether the field held a list of links before, and holds one after.
    pub from_many: bool,
    pub to_many: bool,
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
    // Compare what's stored, not how the admin arranges it.
    let old_data = data_fields(&old.fields);
    let new_data = data_fields(&new.fields);
    for from in renames.keys() {
        if find_field(&old_data, from).is_none() {
            return Err(invalid(format!(
                "can't rename `{from}`: there's no such field"
            )));
        }
    }
    let mut changes = Vec::new();
    let mut links = Vec::new();
    compare(
        &old_data,
        &new_data,
        "",
        "",
        renames,
        &mut changes,
        &mut links,
    )?;
    if changes.is_empty() && old.fields != new.fields {
        changes.push(Change::LayoutChanged);
    }
    if old.default_populate != new.default_populate {
        changes.push(Change::CollectionOptionsChanged);
    }
    Ok(SchemaDiff { changes, links })
}

fn compare(
    old: &[Field],
    new: &[Field],
    old_prefix: &str,
    new_prefix: &str,
    renames: &Renames,
    out: &mut Vec<Change>,
    links: &mut Vec<LinkChange>,
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
        if renamed
            && let Some(other) = old
                .iter()
                .find(|s| s.name != o.name && s.name != name && new_name(s) == name)
        {
            let mut both = [old_path.clone(), join(old_prefix, &other.name)];
            both.sort();
            return Err(invalid(format!(
                "can't rename both `{}` and `{}` to `{name}`",
                both[0], both[1]
            )));
        }
        // Another field is renamed onto this one's name, so this one is removed.
        let displaced = !renamed
            && old
                .iter()
                .any(|s| s.name != o.name && new_name(s) == o.name);
        let Some(n) = new.iter().find(|n| n.name == name && !displaced) else {
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
                compare(a, b, &old_path, &new_path, renames, out, links)?;
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
            ) if from != to || h1 != h2 => {
                let keeps_every_target = from.targets().iter().all(|t| to.allows(t));
                let shrinks_to_one = *h1 && !*h2;
                // Several → more, with the same single/list shape: every stored link already fits.
                if from.is_polymorphic() && to.is_polymorphic() && h1 == h2 && keeps_every_target {
                    out.push(Change::OptionsChanged { path: new_path });
                } else {
                    let describe = |r: &RelationTo, many: bool| {
                        if many {
                            format!("a list of links to {r}")
                        } else {
                            r.to_string()
                        }
                    };
                    let (from_text, to_text) = (describe(from, *h1), describe(to, *h2));
                    out.push(if keeps_every_target && !shrinks_to_one {
                        Change::Relinked {
                            path: new_path.clone(),
                            from: from_text,
                            to: to_text,
                        }
                    } else {
                        Change::Retargeted {
                            path: new_path.clone(),
                            from: from_text,
                            to: to_text,
                        }
                    });
                    links.push(LinkChange {
                        path: new_path.clone(),
                        from: from.clone(),
                        to: to.clone(),
                        from_many: *h1,
                        to_many: *h2,
                    });
                    // Other option changes on the same field are still worth showing.
                    if options_ignoring_targets(o) != options_ignoring_targets(n) {
                        out.push(Change::OptionsChanged { path: new_path });
                    }
                }
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

/// `options`, but also blind to a relationship's targets and list setting, which `compare`
/// reports separately.
fn options_ignoring_targets(f: &Field) -> Field {
    let mut f = options(f);
    if let FieldKind::Relationship {
        relation_to,
        has_many,
    } = &mut f.kind
    {
        *relation_to = RelationTo::One(String::new());
        *has_many = false;
    }
    f
}

fn same_type(a: &FieldKind, b: &FieldKind) -> bool {
    use FieldKind::{Code, Number, Radio, Select, Text, Textarea};
    match (a, b) {
        // Turning `hasMany` on or off changes the stored shape.
        (Text { has_many: x, .. }, Text { has_many: y, .. }) => x == y,
        (Number { has_many: x, .. }, Number { has_many: y, .. }) => x == y,
        // Same stored shape, so a change between them is an option change.
        (
            Text {
                has_many: false, ..
            }
            | Textarea { .. }
            | Code { .. },
            Text {
                has_many: false, ..
            }
            | Textarea { .. }
            | Code { .. },
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
/// schema has no field for, and fills in new fields' defaults at every depth. Values whose type changed
/// are left alone; validation reports them.
pub fn migrate_document(
    new: &[Field],
    renames: &Renames,
    doc: &Map<String, Value>,
) -> Map<String, Value> {
    let new: &[Field] = &data_fields(new);
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
    crate::validate::apply_defaults(new, &mut doc);
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
            // A target's old value belonged to a field this change removes.
            for (_, to) in batch {
                obj.remove(*to);
            }
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

/// Converts stored links for relationships whose targets changed: wraps plain IDs as
/// `{relationTo, value}` when a field gains targets, unwraps links to the one remaining
/// target, and drops links whose collection is no longer allowed. Paths are in the new
/// schema, so call this after `migrate_document`. Returns whether any link was dropped.
pub fn relink(doc: &mut Map<String, Value>, links: &[LinkChange]) -> bool {
    let mut dropped = false;
    for change in links {
        let segments: Vec<&str> = change.path.split('.').collect();
        dropped |= relink_at(doc, &segments, change);
    }
    dropped
}

fn relink_at(obj: &mut Map<String, Value>, segments: &[&str], change: &LinkChange) -> bool {
    match segments {
        [name] => {
            let Some(value) = obj.get(*name) else {
                return false;
            };
            if value.is_null() {
                return false;
            }
            let links: Vec<Value> = match (change.from_many, value) {
                (true, Value::Array(items)) => items.clone(),
                // A list field whose value isn't a list: leave it for validation.
                (true, _) => return false,
                (false, single) => vec![single.clone()],
            };
            let before = links.len();
            let mut kept: Vec<Value> = links
                .iter()
                .filter_map(|v| convert_link(v, change))
                .collect();
            let mut dropped = kept.len() < before;
            if change.to_many {
                obj.insert((*name).to_owned(), Value::Array(kept));
            } else {
                dropped |= kept.len() > 1;
                kept.truncate(1);
                match kept.pop() {
                    Some(first) => {
                        obj.insert((*name).to_owned(), first);
                    }
                    None => {
                        obj.remove(*name);
                    }
                }
            }
            dropped
        }
        [head, rest @ ..] => match obj.get_mut(*head) {
            Some(Value::Object(inner)) => relink_at(inner, rest, change),
            Some(Value::Array(rows)) => rows.iter_mut().fold(false, |any, row| match row {
                Value::Object(inner) => relink_at(inner, rest, change) | any,
                _ => any,
            }),
            _ => false,
        },
        [] => false,
    }
}

/// One link in its new shape, or `None` when it's a real link whose collection is no longer
/// allowed. Values in any other shape are left alone for validation to report.
fn convert_link(v: &Value, change: &LinkChange) -> Option<Value> {
    let linked_collection = |o: &Map<String, Value>| {
        o.get("relationTo")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match (&change.from, &change.to, v) {
        (RelationTo::One(old), RelationTo::Many(allowed), Value::String(id)) => allowed
            .contains(old)
            .then(|| serde_json::json!({ "relationTo": old, "value": id })),
        (RelationTo::One(old), RelationTo::One(new), Value::String(_)) => {
            (old == new).then(|| v.clone())
        }
        (RelationTo::Many(_), RelationTo::One(only), Value::Object(o)) => {
            match linked_collection(o) {
                Some(t) if t == *only => Some(o.get("value").cloned().unwrap_or_else(|| v.clone())),
                Some(_) => None,
                None => Some(v.clone()),
            }
        }
        (RelationTo::Many(_), RelationTo::Many(allowed), Value::Object(o)) => {
            match linked_collection(o) {
                Some(t) if !allowed.contains(&t) => None,
                _ => Some(v.clone()),
            }
        }
        _ => Some(v.clone()),
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
                json!({"slug": "posts", "fields": [{"name": "label", "type": "text"}]}),
                vec![("title", "label"), ("name", "label")],
                "can't rename both `name` and `title` to `label`",
            ),
        ];
        for (new, pairs, message) in cases {
            let err = diff(&old, &schema(new), &renames(&pairs)).unwrap_err();
            assert_eq!(err.to_string(), message);
        }
    }
    #[test]
    fn changing_default_populate_is_a_change() {
        let old = schema(json!({"slug": "authors", "fields": [{"name": "name", "type": "text"}]}));
        let new = schema(json!({"slug": "authors", "defaultPopulate": {"name": true},
                                "fields": [{"name": "name", "type": "text"}]}));
        let d = diff(&old, &new, &Renames::new()).unwrap();
        assert_eq!(d.changes, vec![Change::CollectionOptionsChanged]);
        assert!(!d.is_destructive());
    }

    #[test]
    fn schema_changes_fill_nested_defaults() {
        let new = schema(json!({"slug": "posts", "fields": [
            {"name": "seo", "type": "group", "fields": [
                {"name": "title", "type": "text"},
                {"name": "index", "type": "boolean", "default": true}
            ]}
        ]}));
        let doc = json!({"seo": {"title": "T"}});
        assert_eq!(
            Value::Object(migrate_document(
                &new.fields,
                &Renames::new(),
                doc.as_object().unwrap()
            )),
            json!({"seo": {"title": "T", "index": true}})
        );
    }

    #[test]
    fn renaming_onto_a_removed_fields_name() {
        let old = schema(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"}, {"name": "name", "type": "text"}
        ]}));
        let new = schema(json!({"slug": "posts", "fields": [{"name": "name", "type": "text"}]}));
        let r = renames(&[("title", "name")]);
        let d = diff(&old, &new, &r).unwrap();
        assert_eq!(
            d.changes,
            vec![
                Change::Renamed {
                    from: "title".into(),
                    to: "name".into()
                },
                Change::Removed {
                    path: "name".into(),
                    kind: "text"
                },
            ]
        );
        assert!(d.is_destructive());
        let doc = json!({"title": "T", "name": "N"});
        assert_eq!(
            Value::Object(migrate_document(&new.fields, &r, doc.as_object().unwrap())),
            json!({"name": "T"})
        );
        let doc = json!({"name": "N"});
        assert_eq!(
            Value::Object(migrate_document(&new.fields, &r, doc.as_object().unwrap())),
            json!({}),
            "the removed field's value goes even when there's nothing to move in"
        );
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
    #[test]
    fn toggling_has_many_is_a_type_change() {
        let old = schema(json!({"slug": "posts", "fields": [{"name": "tags", "type": "text"}]}));
        let new = schema(
            json!({"slug": "posts", "fields": [{"name": "tags", "type": "text", "hasMany": true}]}),
        );
        let d = diff(&old, &new, &Renames::new()).unwrap();
        assert!(d.is_destructive(), "{:?}", d.changes);
    }
    #[test]
    fn moving_fields_between_layout_containers_is_a_layout_change() {
        let old = schema(
            json!({"slug": "posts", "fields": [{"name": "title", "type": "text"}, {"name": "body", "type": "textarea"}]}),
        );
        let new = schema(json!({"slug": "posts", "fields": [
            {"type": "row", "fields": [{"name": "title", "type": "text"}]},
            {"type": "collapsible", "label": "Body", "fields": [{"name": "body", "type": "textarea"}]}
        ]}));
        let d = diff(&old, &new, &Renames::new()).unwrap();
        assert_eq!(d.changes, vec![Change::LayoutChanged]);
        assert!(!d.is_destructive());

        let named = schema(json!({"slug": "posts", "fields": [
            {"type": "tabs", "tabs": [{"name": "content", "label": "Content", "fields": [
                {"name": "title", "type": "text"}, {"name": "body", "type": "textarea"}
            ]}]}
        ]}));
        let d = diff(&old, &named, &Renames::new()).unwrap();
        assert!(
            d.is_destructive(),
            "a named tab nests data, so title/body move: {:?}",
            d.changes
        );
    }
    #[test]
    fn relationship_target_changes_carry_links_across() {
        let id = "0199d5c4-0000-7000-8000-00000000000a";
        let rel = |to: Value, many: bool| {
            schema(
                json!({"slug": "m", "fields": [{"name": "l", "type": "relationship", "relationTo": to, "hasMany": many}]}),
            )
        };
        let one = rel(json!("posts"), false);
        let both = rel(json!(["posts", "pages"]), false);
        let pages_only = rel(json!(["pages"]), false);

        // One → several including the old target: every link survives, wrapped.
        let d = diff(&one, &both, &Renames::new()).unwrap();
        assert_eq!(
            d.changes,
            vec![Change::Relinked {
                path: "l".into(),
                from: "posts".into(),
                to: "posts, pages".into()
            }]
        );
        assert!(!d.is_destructive());
        let mut doc = json!({"l": id}).as_object().unwrap().clone();
        assert!(!relink(&mut doc, &d.links), "nothing is lost");
        assert_eq!(doc["l"], json!({"relationTo": "posts", "value": id}));

        // Several → one: links to the remaining target unwrap, others are cleared.
        let d = diff(&both, &one, &Renames::new()).unwrap();
        assert!(d.is_destructive());
        let mut kept = json!({"l": {"relationTo": "posts", "value": id}})
            .as_object()
            .unwrap()
            .clone();
        assert!(!relink(&mut kept, &d.links));
        assert_eq!(kept["l"], id);
        let mut dropped = json!({"l": {"relationTo": "pages", "value": id}})
            .as_object()
            .unwrap()
            .clone();
        assert!(relink(&mut dropped, &d.links));
        assert!(dropped.get("l").is_none());

        // Several → fewer: only links to the removed collection go.
        let d = diff(&both, &pages_only, &Renames::new()).unwrap();
        assert!(d.is_destructive());
        let mut doc = json!({"l": {"relationTo": "posts", "value": id}})
            .as_object()
            .unwrap()
            .clone();
        assert!(relink(&mut doc, &d.links));

        // Several → more is an option change with nothing to convert.
        let d = diff(&pages_only, &both, &Renames::new()).unwrap();
        assert_eq!(d.changes, vec![Change::OptionsChanged { path: "l".into() }]);
        assert!(d.links.is_empty());

        // Lists convert element by element.
        let many_one = rel(json!("posts"), true);
        let many_both = rel(json!(["posts", "pages"]), true);
        let d = diff(&many_both, &many_one, &Renames::new()).unwrap();
        let mut doc = json!({"l": [{"relationTo": "posts", "value": id}, {"relationTo": "pages", "value": id}]}).as_object().unwrap().clone();
        assert!(relink(&mut doc, &d.links));
        assert_eq!(doc["l"], json!([id]));
    }

    #[test]
    fn has_many_toggles_and_option_changes_on_relationships() {
        let id = "0199d5c4-0000-7000-8000-00000000000a";
        let id2 = "0199d5c4-0000-7000-8000-00000000000b";
        let rel = |to: Value, many: bool, required: bool| {
            schema(json!({"slug": "m", "fields": [
                {"name": "l", "type": "relationship", "relationTo": to, "hasMany": many, "required": required}
            ]}))
        };
        // One link → a list: lossless, the value is wrapped in a list.
        let d = diff(
            &rel(json!("posts"), false, false),
            &rel(json!("posts"), true, false),
            &Renames::new(),
        )
        .unwrap();
        assert!(
            matches!(d.changes.as_slice(), [Change::Relinked { .. }]),
            "{:?}",
            d.changes
        );
        let mut doc = json!({"l": id}).as_object().unwrap().clone();
        assert!(!relink(&mut doc, &d.links));
        assert_eq!(doc["l"], json!([id]));

        // A list → one link: the first link is kept, the rest are cleared.
        let d = diff(
            &rel(json!("posts"), true, false),
            &rel(json!("posts"), false, false),
            &Renames::new(),
        )
        .unwrap();
        assert!(d.is_destructive());
        let mut doc = json!({"l": [id, id2]}).as_object().unwrap().clone();
        assert!(relink(&mut doc, &d.links));
        assert_eq!(doc["l"], id);

        // A target change and an option change are both reported.
        let d = diff(
            &rel(json!("posts"), false, false),
            &rel(json!("pages"), false, true),
            &Renames::new(),
        )
        .unwrap();
        assert!(
            d.changes
                .contains(&Change::OptionsChanged { path: "l".into() }),
            "{:?}",
            d.changes
        );
        assert!(
            d.changes
                .iter()
                .any(|c| matches!(c, Change::Retargeted { .. }))
        );

        // Malformed values are left for validation, and aren't counted as cleared links.
        let d = diff(
            &rel(json!(["posts", "pages"]), false, false),
            &rel(json!("posts"), false, false),
            &Renames::new(),
        )
        .unwrap();
        let mut doc = json!({"l": {"value": id}}).as_object().unwrap().clone();
        assert!(!relink(&mut doc, &d.links));
        assert_eq!(doc["l"], json!({"value": id}));
        let d = diff(
            &rel(json!("posts"), false, false),
            &rel(json!("pages"), false, false),
            &Renames::new(),
        )
        .unwrap();
        let mut doc = json!({"l": null}).as_object().unwrap().clone();
        assert!(!relink(&mut doc, &d.links), "null isn't a link");
    }
}
