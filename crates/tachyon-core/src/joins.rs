//! Join fields: the documents of another collection that link to this one, found through
//! the `relations` table on read and never stored.

use crate::schema::{Field, FieldKind};

pub use crate::schema::JoinField;

/// An anchored Postgres regular expression matching the `relations.field_path` values a
/// link at `on` produces: `links.target` is stored as `links.0.target`, a list of links
/// as `tags.0`, `tags.1`. Inside blocks, each block type that has the path adds its own
/// shape. `None` when `on` doesn't end at a relationship.
pub fn on_pattern(target: &[Field], on: &str) -> Option<String> {
    pattern(target, on).map(|p| format!("^{p}$"))
}

fn pattern(fields: &[Field], path: &str) -> Option<String> {
    let (head, rest) = path
        .split_once('.')
        .map_or((path, None), |(h, r)| (h, Some(r)));
    let f = fields.iter().find(|f| f.name == head)?;
    // Field names are identifiers, so they need no escaping.
    match (&f.kind, rest) {
        (FieldKind::Relationship { has_many, .. }, None) => Some(if *has_many {
            format!(r"{head}(\.\d+)?")
        } else {
            head.to_owned()
        }),
        (FieldKind::Group { fields }, Some(rest)) => {
            Some(format!(r"{head}\.{}", pattern(fields, rest)?))
        }
        (FieldKind::Array { fields, .. }, Some(rest)) => {
            Some(format!(r"{head}\.\d+\.{}", pattern(fields, rest)?))
        }
        (FieldKind::Blocks { blocks, .. }, Some(rest)) => {
            let mut shapes: Vec<String> = Vec::new();
            for shape in blocks.iter().filter_map(|b| pattern(&b.fields, rest)) {
                if !shapes.contains(&shape) {
                    shapes.push(shape);
                }
            }
            match shapes.as_slice() {
                [] => None,
                [one] => Some(format!(r"{head}\.\d+\.{one}")),
                _ => Some(format!(r"{head}\.\d+\.(?:{})", shapes.join("|"))),
            }
        }
        _ => None,
    }
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
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "layout", "type": "blocks", "blocks": [
                {"slug": "card", "fields": [{"name": "who", "type": "relationship", "relationTo": "categories"}]},
                {"slug": "list", "fields": [
                    {"name": "who", "type": "relationship", "relationTo": "categories", "hasMany": true},
                    {"name": "rows", "type": "array", "fields": [
                        {"name": "who", "type": "relationship", "relationTo": "categories"}
                    ]}
                ]}
            ]}
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
        for on in ["links.target", "layout.who", "layout.rows.who"] {
            let ok = categories(
                json!({"name": "posts", "type": "join", "collection": "posts", "on": on}),
            );
            assert_eq!(ok.check_join_targets(lookup), Ok(()), "{on}");
        }
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
            // Each block type's own shape: one link in `card`, several in `list`.
            ("layout.who", r"^layout\.\d+\.(?:who|who(\.\d+)?)$"),
            ("layout.rows.who", r"^layout\.\d+\.rows\.\d+\.who$"),
        ] {
            assert_eq!(
                super::on_pattern(p.data(), on).as_deref(),
                Some(pattern),
                "{on}"
            );
        }
        assert_eq!(super::on_pattern(p.data(), "title"), None);
        assert_eq!(super::on_pattern(p.data(), "layout.nope"), None);
        assert_eq!(super::on_pattern(p.data(), "layout"), None);
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

    #[test]
    fn turning_a_join_into_a_stored_field_loses_nothing() {
        let with = categories(
            json!({"name": "posts", "type": "join", "collection": "posts", "on": "category"}),
        );
        let text = categories(json!({"name": "posts", "type": "text"}));
        let d = diff(&with, &text, &Renames::new()).unwrap();
        assert!(!d.is_destructive(), "{:?}", d.changes);
        assert_eq!(
            d.changes,
            vec![
                Change::VirtualRemoved {
                    path: "posts".into()
                },
                Change::Added {
                    path: "posts".into(),
                    kind: "text"
                }
            ]
        );
    }

    #[test]
    fn a_join_where_must_be_an_object() {
        let raw = json!({"slug": "categories", "fields": [
            {"name": "posts", "type": "join", "collection": "posts", "on": "category", "where": "draft"}
        ]});
        let e = serde_json::from_value::<CollectionSchema>(raw.clone()).unwrap_err();
        assert_eq!(
            crate::explain_parse_error(&raw, &e),
            "field `posts`: where must be an object like {\"draft\": {\"equals\": \"false\"}}"
        );
    }

    #[test]
    fn joins_cant_filter_by_distance() {
        let places = schema(json!({"slug": "posts", "fields": [
            {"name": "category", "type": "relationship", "relationTo": "categories"},
            {"name": "spot", "type": "point"}
        ]}));
        let s = categories(
            json!({"name": "posts", "type": "join", "collection": "posts", "on": "category",
                                  "where": {"spot": {"near": "0,0,1000"}}}),
        );
        assert_eq!(
            s.check_join_targets(|slug| (slug == "posts").then(|| places.clone()))
                .unwrap_err()
                .to_string(),
            "field `posts`: joins can't be ordered by distance yet, so `near` isn't allowed in their where"
        );
    }
}
