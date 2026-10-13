//! Virtual fields: values read through a relationship (`"virtual": "author.name"`), never stored.

use serde_json::{Map, Value};
use uuid::Uuid;

pub use crate::schema::VirtualField;

fn at<'a>(obj: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut value = obj.get(parts.next()?)?;
    for part in parts {
        value = value.as_object()?.get(part)?;
    }
    Some(value)
}

/// A stored link's document ID: a plain ID, or a polymorphic `{relationTo, value}`.
fn link_id(link: &Value) -> Option<Uuid> {
    let id = match link {
        Value::Object(o) => o.get("value")?,
        other => other,
    };
    Uuid::try_parse(id.as_str()?).ok()
}

/// The documents `v` reads from in `doc`, in link order.
pub fn linked_ids(v: &VirtualField, doc: &Map<String, Value>) -> Vec<Uuid> {
    match at(doc, &v.link) {
        Some(Value::Array(links)) if v.many => links.iter().filter_map(link_id).collect(),
        Some(link) if !v.many => link_id(link).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Sets `v`'s value in `doc` from the documents `linked` finds: one value, or a list of
/// the values found when the link is a list. Nothing found, nothing set.
pub fn fill(
    v: &VirtualField,
    doc: &mut Map<String, Value>,
    linked: impl Fn(Uuid) -> Option<Value>,
) {
    let values: Vec<Value> = linked_ids(v, doc)
        .into_iter()
        .filter_map(|id| {
            linked(id)?
                .as_object()
                .and_then(|d| at(d, &v.read))
                .cloned()
        })
        .collect();
    let value = if v.many {
        (!values.is_empty()).then_some(Value::Array(values))
    } else {
        values.into_iter().next()
    };
    let Some(value) = value else { return };
    let mut parts: Vec<&str> = v.path.split('.').collect();
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::diff::{Change, Renames, diff, migrate_document};
    use crate::query::{parse_filter, parse_query, parse_sort};
    use crate::{CollectionSchema, validate_document};

    fn schema(v: Value) -> CollectionSchema {
        serde_json::from_value(v).unwrap()
    }

    fn posts(extra: Value) -> CollectionSchema {
        let mut fields = vec![
            json!({"name": "title", "type": "text"}),
            json!({"name": "author", "type": "relationship", "relationTo": "authors"}),
            json!({"name": "meta", "type": "group", "fields": [
                {"name": "editor", "type": "relationship", "relationTo": ["authors", "guests"], "hasMany": true}
            ]}),
        ];
        fields.push(extra);
        schema(json!({"slug": "posts", "fields": fields}))
    }

    fn authors() -> CollectionSchema {
        schema(json!({"slug": "authors", "fields": [
            {"name": "name", "type": "text"},
            {"name": "profile", "type": "group", "fields": [{"name": "city", "type": "text"}]},
            {"name": "age", "type": "number"}
        ]}))
    }

    fn guests() -> CollectionSchema {
        schema(json!({"slug": "guests", "fields": [{"name": "name", "type": "text"}]}))
    }

    fn check(extra: Value) -> String {
        match posts(extra).check() {
            Ok(()) => "ok".into(),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn virtual_fields_are_checked() {
        assert_eq!(
            check(json!({"name": "authorName", "type": "text", "virtual": "author.name"})),
            "ok"
        );
        assert_eq!(
            check(json!({"name": "editorNames", "type": "text", "virtual": "meta.editor.name"})),
            "ok"
        );
        for (field, message) in [
            (
                json!({"name": "v", "type": "text", "virtual": "title"}),
                "field `v`: virtual must name a relationship field, then a field in the linked document, like author.name",
            ),
            (
                json!({"name": "v", "type": "text", "virtual": "author"}),
                "field `v`: virtual must name a relationship field, then a field in the linked document, like author.name",
            ),
            (
                json!({"name": "v", "type": "text", "virtual": "nope.name"}),
                "field `v`: virtual must name a relationship field, then a field in the linked document, like author.name",
            ),
            (
                json!({"name": "v", "type": "group", "virtual": "author.profile", "fields": []}),
                "field `v`: group fields can't be virtual; a virtual field holds one value",
            ),
            (
                json!({"name": "v", "type": "text", "virtual": "author.name", "required": true}),
                "field `v`: virtual fields can't be required, unique, filterable or have a default",
            ),
            (
                json!({"name": "rows", "type": "array", "fields": [
                    {"name": "v", "type": "text", "virtual": "author.name"}
                ]}),
                "field `v`: virtual fields can't be inside a list yet",
            ),
        ] {
            assert_eq!(check(field), message);
        }
    }

    #[test]
    fn virtual_targets_must_have_a_matching_field() {
        let lookup = |slug: &str| match slug {
            "authors" => Some(authors()),
            "guests" => Some(guests()),
            _ => None,
        };
        let ok = posts(json!({"name": "city", "type": "text", "virtual": "author.profile.city"}));
        assert_eq!(ok.check_virtual_targets(|s| lookup(s)), Ok(()));
        for (field, message) in [
            (
                json!({"name": "v", "type": "text", "virtual": "author.nope"}),
                "field `v`: authors has no field `nope` to read",
            ),
            (
                json!({"name": "v", "type": "text", "virtual": "author.age"}),
                "field `v`: authors.age is a number field, but v is a text field",
            ),
            (
                json!({"name": "v", "type": "number", "virtual": "meta.editor.age"}),
                "field `v`: guests has no field `age` to read",
            ),
        ] {
            let s = posts(field);
            assert_eq!(s.check(), Ok(()));
            assert_eq!(
                s.check_virtual_targets(|s| lookup(s))
                    .unwrap_err()
                    .to_string(),
                message
            );
        }
    }

    #[test]
    fn virtual_fields_cant_be_written_filtered_or_sorted() {
        let s = posts(json!({"name": "authorName", "type": "text", "virtual": "author.name"}));
        let errors = validate_document(s.data(), &json!({"authorName": "x"}), None).unwrap_err();
        assert_eq!(errors[0].path, "authorName");
        assert_eq!(
            errors[0].message,
            "is read from author.name; set that instead"
        );
        let params = parse_query("where[authorName][equals]=x").unwrap();
        assert_eq!(
            parse_filter(s.data(), &params["where"])
                .unwrap_err()
                .to_string(),
            "where[authorName][equals]: `authorName` is read through a relationship and can't be filtered yet"
        );
        assert_eq!(
            parse_sort(s.data(), "authorName").unwrap_err().to_string(),
            "sort: `authorName` is read through a relationship and can't be sorted yet"
        );
    }

    #[test]
    fn virtual_fields_in_schema_changes() {
        let plain = posts(json!({"name": "extra", "type": "text"}));
        let with_virtual =
            posts(json!({"name": "authorName", "type": "text", "virtual": "author.name"}));
        let removed = diff(&with_virtual, &plain, &Renames::new()).unwrap();
        assert!(removed.changes.contains(&Change::VirtualRemoved {
            path: "authorName".into()
        }));
        assert!(!removed.is_destructive(), "nothing stored, nothing lost");

        // A stored field turned virtual: its stored values go.
        let stored = posts(json!({"name": "authorName", "type": "text"}));
        let d = diff(&stored, &with_virtual, &Renames::new()).unwrap();
        assert_eq!(
            d.changes,
            vec![Change::TypeChanged {
                path: "authorName".into(),
                from: "text",
                to: "virtual"
            }]
        );
        assert!(d.is_destructive());
        let doc = json!({"title": "T", "authorName": "old"});
        assert_eq!(
            Value::Object(migrate_document(
                &with_virtual.fields,
                &Renames::new(),
                doc.as_object().unwrap()
            )),
            json!({"title": "T"})
        );
    }

    fn orgs() -> CollectionSchema {
        schema(json!({"slug": "authors", "fields": [
            {"name": "name", "type": "text"},
            {"name": "tags", "type": "text", "hasMany": true},
            {"name": "colour", "type": "select", "options": ["red"], "multiple": true},
            {"name": "rows", "type": "array", "fields": [{"name": "label", "type": "text"}]},
            {"name": "about", "type": "textarea"}
        ]}))
    }

    #[test]
    fn slugs_cant_be_virtual() {
        assert_eq!(
            check(json!({"name": "v", "type": "slug", "from": "title", "virtual": "author.name"})),
            "field `v`: slug fields can't be virtual; a slug is stored so it can stay unique"
        );
    }

    #[test]
    fn virtual_targets_must_hold_the_same_shape() {
        let lookup = |slug: &str| (slug == "authors").then(orgs);
        let ok = posts(json!({"name": "v", "type": "text", "virtual": "author.about"}));
        assert_eq!(
            ok.check_virtual_targets(lookup),
            Ok(()),
            "text reads textarea: same stored shape"
        );
        for (field, message) in [
            (
                json!({"name": "v", "type": "text", "virtual": "author.tags"}),
                "field `v`: authors.tags is a list of text, which a text field can't hold",
            ),
            (
                json!({"name": "v", "type": "select", "options": ["red"], "virtual": "author.colour"}),
                "field `v`: authors.colour is a list of select values, which a select field can't hold",
            ),
            (
                json!({"name": "v", "type": "text", "virtual": "author.rows.label"}),
                "field `v`: authors.rows.label is inside a list, so it can't be read",
            ),
        ] {
            let s = posts(field);
            assert_eq!(s.check(), Ok(()));
            assert_eq!(
                s.check_virtual_targets(lookup).unwrap_err().to_string(),
                message
            );
        }
    }

    #[test]
    fn null_virtual_and_default_populate_mean_none() {
        let input = json!({"slug": "posts", "defaultPopulate": null, "fields": [
            {"name": "title", "type": "text", "virtual": null}
        ]});
        let s = schema(input.clone());
        assert_eq!(s.reject_unknown_options(&input), Ok(()));
        assert_eq!(s.fields[0].virtual_path, None);
    }

    #[test]
    fn renaming_a_field_while_making_it_virtual_reports_both() {
        let stored = posts(json!({"name": "old", "type": "text"}));
        let virt = posts(json!({"name": "authorName", "type": "text", "virtual": "author.name"}));
        let renames: Renames = [("old".to_owned(), "authorName".to_owned())]
            .into_iter()
            .collect();
        let d = diff(&stored, &virt, &renames).unwrap();
        assert_eq!(
            d.changes,
            vec![
                Change::Renamed {
                    from: "old".into(),
                    to: "authorName".into()
                },
                Change::TypeChanged {
                    path: "authorName".into(),
                    from: "text",
                    to: "virtual"
                },
            ]
        );
    }

    #[test]
    fn globals_cant_have_default_populate() {
        let g = schema(
            json!({"slug": "settings", "kind": "global", "defaultPopulate": {"name": true},
                             "fields": [{"name": "name", "type": "text"}]}),
        );
        assert_eq!(
            g.check().unwrap_err().to_string(),
            "defaultPopulate: globals are never populated, so they can't have one"
        );
    }
}
