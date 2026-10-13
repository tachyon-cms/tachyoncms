//! Join fields: the documents of another collection that link to this one, found through
//! the `relations` table on read and never stored.

use crate::schema::{Field, FieldKind};

pub use crate::schema::JoinField;

/// An anchored Postgres regular expression matching the `relations.field_path` values a
/// link at `on` produces: `links.target` is stored as `links.0.target`, a list of links
/// as `tags.0`, `tags.1`. `None` when `on` doesn't end at a relationship.
pub fn on_pattern(target: &[Field], on: &str) -> Option<String> {
    let segments: Vec<&str> = on.split('.').collect();
    let mut current = target;
    let mut out = String::from("^");
    for (i, segment) in segments.iter().enumerate() {
        let f = current.iter().find(|f| f.name == *segment)?;
        // Field names are identifiers, so they need no escaping.
        out.push_str(segment);
        let last = i + 1 == segments.len();
        match (&f.kind, last) {
            (FieldKind::Relationship { has_many, .. }, true) => {
                if *has_many {
                    out.push_str(r"(\.\d+)?");
                }
                out.push('$');
                return Some(out);
            }
            (FieldKind::Group { fields }, false) => {
                out.push_str(r"\.");
                current = fields;
            }
            (FieldKind::Array { fields, .. }, false) => {
                out.push_str(r"\.\d+\.");
                current = fields;
            }
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::diff::{Change, Renames, diff};
    use crate::query::{parse_filter, parse_query, parse_sort};
    use crate::{CollectionSchema, validate_document};

    fn schema(v: Value) -> CollectionSchema {
        serde_json::from_value(v).unwrap()
    }

    fn categories(join: Value) -> CollectionSchema {
        schema(json!({"slug": "categories", "fields": [{"name": "name", "type": "text"}, join]}))
    }

    fn posts() -> CollectionSchema {
        schema(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "category", "type": "relationship", "relationTo": "categories"},
            {"name": "tags", "type": "relationship", "relationTo": "categories", "hasMany": true},
            {"name": "meta", "type": "group", "fields": [
                {"name": "main", "type": "relationship", "relationTo": ["categories", "posts"]}
            ]},
            {"name": "links", "type": "array", "fields": [
                {"name": "target", "type": "relationship", "relationTo": "categories"},
                {"name": "many", "type": "relationship", "relationTo": "categories", "hasMany": true}
            ]},
            {"name": "author", "type": "relationship", "relationTo": "authors"}
        ]}))
    }

    fn check(join: Value) -> String {
        match categories(join).check() {
            Ok(()) => "ok".into(),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn join_fields_are_checked() {
        assert_eq!(
            check(
                json!({"name": "posts", "type": "join", "collection": "posts", "on": "category"})
            ),
            "ok"
        );
        assert_eq!(
            check(
                json!({"name": "posts", "type": "join", "collection": "posts", "on": "category",
                         "defaultLimit": 5, "defaultSort": "-title", "where": {"title": {"exists": "true"}}})
            ),
            "ok"
        );
        for (join, message) in [
            (
                json!({"name": "posts", "type": "join", "collection": "posts", "on": "category", "defaultLimit": 0}),
                "field `posts`: defaultLimit must be between 1 and 100",
            ),
            (
                json!({"name": "posts", "type": "join", "collection": "posts", "on": "category", "required": true}),
                "field `posts`: join fields can't be required, unique, filterable, virtual or have a default",
            ),
            (
                json!({"name": "posts", "type": "join", "collection": "Posts!", "on": "category"}),
                "field `posts`: collection `Posts!` isn't a valid collection slug",
            ),
            (
                json!({"name": "rows", "type": "array", "fields": [
                    {"name": "posts", "type": "join", "collection": "posts", "on": "category"}
                ]}),
                "field `posts`: join fields can't be inside a list",
            ),
        ] {
            assert_eq!(check(join), message);
        }
        let global = schema(json!({"slug": "settings", "kind": "global", "fields": [
            {"name": "posts", "type": "join", "collection": "posts", "on": "category"}
        ]}));
        assert_eq!(
            global.check().unwrap_err().to_string(),
            "field `posts`: globals can't have join fields, since nothing links to a global"
        );
    }

    #[test]
    fn join_targets_are_checked() {
        let lookup = |slug: &str| (slug == "posts").then(posts);
        let ok = categories(
            json!({"name": "posts", "type": "join", "collection": "posts", "on": "links.target"}),
        );
        assert_eq!(ok.check_join_targets(lookup), Ok(()));
        for (join, message) in [
            (
                json!({"name": "j", "type": "join", "collection": "nope", "on": "category"}),
                "field `j`: collection `nope` doesn't exist",
            ),
            (
                json!({"name": "j", "type": "join", "collection": "posts", "on": "title"}),
                "field `j`: posts.title isn't a relationship field",
            ),
            (
                json!({"name": "j", "type": "join", "collection": "posts", "on": "missing"}),
                "field `j`: posts has no field `missing`",
            ),
            (
                json!({"name": "j", "type": "join", "collection": "posts", "on": "author"}),
                "field `j`: posts.author links to authors, not categories",
            ),
            (
                json!({"name": "j", "type": "join", "collection": "posts", "on": "category",
                       "where": {"nope": {"equals": "x"}}}),
                "field `j`: where[nope][equals]: unknown field `nope`",
            ),
            (
                json!({"name": "j", "type": "join", "collection": "posts", "on": "category", "defaultSort": "nope"}),
                "field `j`: sort: unknown field `nope`",
            ),
        ] {
            let s = categories(join);
            assert_eq!(s.check(), Ok(()));
            assert_eq!(
                s.check_join_targets(lookup).unwrap_err().to_string(),
                message
            );
        }
    }

    #[test]
    fn on_patterns_match_the_paths_relations_store() {
        let p = posts();
        for (on, pattern) in [
            ("category", r"^category$"),
            ("tags", r"^tags(\.\d+)?$"),
            ("meta.main", r"^meta\.main$"),
            ("links.target", r"^links\.\d+\.target$"),
            ("links.many", r"^links\.\d+\.many(\.\d+)?$"),
        ] {
            assert_eq!(
                super::on_pattern(p.data(), on).as_deref(),
                Some(pattern),
                "{on}"
            );
        }
        assert_eq!(super::on_pattern(p.data(), "title"), None);
    }

    #[test]
    fn joins_cant_be_written_filtered_or_sorted() {
        let s = categories(
            json!({"name": "posts", "type": "join", "collection": "posts", "on": "category"}),
        );
        let errors = validate_document(s.data(), &json!({"posts": []}), None).unwrap_err();
        assert_eq!(errors[0].path, "posts");
        assert_eq!(
            errors[0].message,
            "is filled from posts.category; it can't be set"
        );
        let params = parse_query("where[posts][exists]=true").unwrap();
        assert_eq!(
            parse_filter(s.data(), &params["where"])
                .unwrap_err()
                .to_string(),
            "where[posts][exists]: `posts` is filled from other documents and can't be filtered"
        );
        assert_eq!(
            parse_sort(s.data(), "posts").unwrap_err().to_string(),
            "sort: `posts` is filled from other documents and can't be sorted"
        );
    }

    #[test]
    fn removing_a_join_isnt_destructive() {
        let with = categories(
            json!({"name": "posts", "type": "join", "collection": "posts", "on": "category"}),
        );
        let without = categories(json!({"name": "extra", "type": "text"}));
        let d = diff(&with, &without, &Renames::new()).unwrap();
        assert!(d.changes.contains(&Change::VirtualRemoved {
            path: "posts".into()
        }));
        assert!(!d.is_destructive());
        let fields = with.join_fields();
        assert_eq!(fields.len(), 1);
        assert_eq!(
            (
                fields[0].collection.as_str(),
                fields[0].on.as_str(),
                fields[0].limit
            ),
            ("posts", "category", 10)
        );
    }
}
