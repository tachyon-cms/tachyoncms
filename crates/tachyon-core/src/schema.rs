use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Keys the API adds to every document, so top-level fields can't use them.
pub const RESERVED_FIELD_NAMES: &[&str] = &["id", "createdAt", "updatedAt", "status"];

/// Paths under `/api` that belong to the server rather than to a collection.
pub const RESERVED_SLUGS: &[&str] = &[
    "admin", "auth", "globals", "media", "search", "graphql", "plugins",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionSchema {
    pub slug: String,
    #[serde(default)]
    pub kind: CollectionKind,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectionKind {
    #[default]
    Collection,
    Global,
}

impl CollectionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Collection => "collection",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    /// No two live documents in the collection may share this value.
    #[serde(default)]
    pub unique: bool,
    /// Index this field so filters and sorts on it stay fast.
    #[serde(default)]
    pub filterable: bool,
    #[serde(default)]
    pub admin: Admin,
    #[serde(flatten)]
    pub kind: FieldKind,
}

/// How the admin shows a field. Presentation only: the API stores these but never enforces them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Admin {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub position: Position,
    /// Such as `50%` or `240px`.
    #[serde(default)]
    pub width: Option<String>,
    /// Show the field only when another field matches.
    #[serde(default)]
    pub condition: Option<Condition>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Main,
    Sidebar,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    /// Dotted path from the top of the document, such as `kind` or `seo.enabled`.
    pub field: String,
    #[serde(flatten)]
    pub test: ConditionTest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConditionTest {
    Equals(Value),
    NotEquals(Value),
    In(Vec<Value>),
    Exists(bool),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FieldKind {
    Text {
        #[serde(default)]
        min_length: Option<usize>,
        #[serde(default)]
        max_length: Option<usize>,
    },
    Textarea {
        #[serde(default)]
        max_length: Option<usize>,
    },
    Number {
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
        #[serde(default)]
        integer: bool,
    },
    Boolean,
    Date {
        #[serde(default)]
        date_only: bool,
    },
    Email,
    Url,
    Select {
        options: Vec<String>,
        #[serde(default)]
        multiple: bool,
    },
    Slug {
        /// A text field to generate the slug from when none is given.
        #[serde(default)]
        from: Option<String>,
    },
    Relationship {
        relation_to: String,
        #[serde(default)]
        has_many: bool,
    },
    Json,
    Group {
        fields: Vec<Field>,
    },
    Array {
        fields: Vec<Field>,
        #[serde(default)]
        min_rows: Option<usize>,
        #[serde(default)]
        max_rows: Option<usize>,
    },
}

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum SchemaError {
    #[error(
        "collection slug `{0}` must start with a lowercase letter and use only lowercase letters, digits and underscores (64 at most)"
    )]
    InvalidSlug(String),
    #[error("collection slug `{0}` is reserved")]
    ReservedSlug(String),
    #[error(
        "field name `{0}` must start with a letter and use only letters, digits and underscores (64 at most)"
    )]
    InvalidFieldName(String),
    #[error("field name `{0}` is reserved")]
    ReservedFieldName(String),
    #[error("field `{0}` is defined more than once")]
    DuplicateField(String),
    #[error("field `{field}`: {message}")]
    InvalidField { field: String, message: String },
    #[error("{0}")]
    InvalidChange(String),
    #[error("field `{field}`: unknown option `{option}`")]
    UnknownOption { field: String, option: String },
}

impl FieldKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Textarea { .. } => "textarea",
            Self::Number { .. } => "number",
            Self::Boolean => "boolean",
            Self::Date { .. } => "date",
            Self::Email => "email",
            Self::Url => "url",
            Self::Select { .. } => "select",
            Self::Slug { .. } => "slug",
            Self::Relationship { .. } => "relationship",
            Self::Json => "json",
            Self::Group { .. } => "group",
            Self::Array { .. } => "array",
        }
    }

    /// A single comparable value, so it can carry a B-tree index.
    pub fn is_indexable(&self) -> bool {
        match self {
            Self::Select { multiple, .. } => !multiple,
            Self::Relationship { has_many, .. } => !has_many,
            Self::Json | Self::Group { .. } | Self::Array { .. } => false,
            _ => true,
        }
    }
}

impl Field {
    pub fn is_unique(&self) -> bool {
        self.unique || matches!(self.kind, FieldKind::Slug { .. })
    }
}

impl CollectionSchema {
    /// Every collection slug a relationship field points at, nested ones included.
    pub fn relation_targets(&self) -> Vec<&str> {
        fn walk<'a>(fields: &'a [Field], out: &mut Vec<&'a str>) {
            for f in fields {
                match &f.kind {
                    FieldKind::Relationship { relation_to, .. } => out.push(relation_to),
                    FieldKind::Group { fields } | FieldKind::Array { fields, .. } => {
                        walk(fields, out)
                    }
                    _ => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.fields, &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Checks the schema is usable before it's stored.
    pub fn check(&self) -> Result<(), SchemaError> {
        if !is_identifier(&self.slug, false) {
            return Err(SchemaError::InvalidSlug(self.slug.clone()));
        }
        if RESERVED_SLUGS.contains(&self.slug.as_str()) {
            return Err(SchemaError::ReservedSlug(self.slug.clone()));
        }
        check_fields(&self.fields, true, &self.fields)
    }

    /// Rejects keys in the original JSON that parsing would silently drop, such as a
    /// misspelt `maxLenght`. Every option this model understands survives a round trip,
    /// so anything missing after re-serialising is unknown.
    pub fn reject_unknown_options(&self, input: &Value) -> Result<(), SchemaError> {
        let known = serde_json::to_value(self).expect("schemas always serialize");
        unknown_key(input, &known, "")?;
        unknown_in_fields(&self.fields, input.get("fields"))
    }
}

fn unknown_key(input: &Value, known: &Value, field: &str) -> Result<(), SchemaError> {
    if let (Value::Object(input), Value::Object(known)) = (input, known)
        && let Some(option) = input.keys().find(|k| !known.contains_key(*k))
    {
        return Err(SchemaError::UnknownOption {
            field: field.into(),
            option: option.clone(),
        });
    }
    Ok(())
}

fn unknown_in_fields(fields: &[Field], input: Option<&Value>) -> Result<(), SchemaError> {
    let Some(Value::Array(raw)) = input else {
        return Ok(());
    };
    for (f, raw) in fields.iter().zip(raw) {
        let known = serde_json::to_value(f).expect("fields always serialize");
        unknown_key(raw, &known, &f.name)?;
        if let (Some(Value::Object(raw_admin)), Some(Value::Object(known_admin))) =
            (raw.get("admin"), known.get("admin"))
            && let Some(option) = raw_admin.keys().find(|k| !known_admin.contains_key(*k))
        {
            return Err(SchemaError::UnknownOption {
                field: f.name.clone(),
                option: format!("admin.{option}"),
            });
        }
        if let FieldKind::Group { fields } | FieldKind::Array { fields, .. } = &f.kind {
            unknown_in_fields(fields, raw.get("fields"))?;
        }
    }
    Ok(())
}

/// The field at a dotted path, looking through groups and arrays.
pub(crate) fn find_field<'a>(fields: &'a [Field], path: &str) -> Option<&'a Field> {
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

fn is_identifier(s: &str, allow_upper: bool) -> bool {
    let ok = |c: char| c.is_ascii_lowercase() || (allow_upper && c.is_ascii_uppercase());
    let mut chars = s.chars();
    s.len() <= 64
        && chars.next().is_some_and(ok)
        && chars.all(|c| ok(c) || c.is_ascii_digit() || c == '_')
}

fn check_fields(fields: &[Field], top_level: bool, root: &[Field]) -> Result<(), SchemaError> {
    let mut seen = HashSet::new();
    for f in fields {
        if !is_identifier(&f.name, true) {
            return Err(SchemaError::InvalidFieldName(f.name.clone()));
        }
        if top_level && RESERVED_FIELD_NAMES.contains(&f.name.as_str()) {
            return Err(SchemaError::ReservedFieldName(f.name.clone()));
        }
        if !seen.insert(f.name.as_str()) {
            return Err(SchemaError::DuplicateField(f.name.clone()));
        }
        let invalid = |message: String| SchemaError::InvalidField {
            field: f.name.clone(),
            message,
        };
        let indexed = f.unique || f.filterable || matches!(f.kind, FieldKind::Slug { .. });
        if indexed && !top_level {
            return Err(invalid(
                "unique, filterable and slug fields must be at the top level".into(),
            ));
        }
        if (f.unique || f.filterable) && !f.kind.is_indexable() {
            let what = match f.kind {
                FieldKind::Select { .. } => "select fields with multiple values".to_owned(),
                FieldKind::Relationship { .. } => {
                    "relationship fields with multiple values".to_owned()
                }
                _ => format!("{} fields", f.kind.name()),
            };
            return Err(invalid(format!("{what} can't be unique or filterable")));
        }
        match &f.kind {
            FieldKind::Text {
                min_length: Some(a),
                max_length: Some(b),
            } if a > b => {
                return Err(invalid("minLength is greater than maxLength".into()));
            }
            FieldKind::Number {
                min: Some(a),
                max: Some(b),
                ..
            } if a > b => {
                return Err(invalid("min is greater than max".into()));
            }
            FieldKind::Select { options, .. } if options.is_empty() => {
                return Err(invalid("select needs at least one option".into()));
            }
            FieldKind::Array {
                min_rows: Some(a),
                max_rows: Some(b),
                ..
            } if a > b => {
                return Err(invalid("minRows is greater than maxRows".into()));
            }
            FieldKind::Slug { from: Some(source) }
                if !fields
                    .iter()
                    .any(|s| &s.name == source && matches!(s.kind, FieldKind::Text { .. })) =>
            {
                return Err(invalid(format!(
                    "from must name a top-level text field, not `{source}`"
                )));
            }
            FieldKind::Relationship { relation_to, .. } if !is_identifier(relation_to, false) => {
                return Err(invalid(format!(
                    "relationTo `{relation_to}` isn't a valid collection slug"
                )));
            }
            FieldKind::Group { fields } | FieldKind::Array { fields, .. } => {
                check_fields(fields, false, root)?
            }
            _ => {}
        }
        if let Some(condition) = &f.admin.condition
            && find_field(root, &condition.field).is_none()
        {
            return Err(invalid(format!(
                "admin.condition refers to `{}`, which isn't a field",
                condition.field
            )));
        }
        if let Some(width) = &f.admin.width
            && !(width.ends_with('%') || width.ends_with("px"))
        {
            return Err(invalid("admin.width must look like 50% or 240px".into()));
        }
        if let Some(default) = &f.default {
            let mut errors = Vec::new();
            crate::validate::check_value(f, default, &f.name, &mut errors);
            if let Some(e) = errors.first() {
                return Err(invalid(format!("default: {}", e.message)));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn schema(v: Value) -> CollectionSchema {
        serde_json::from_value(v).expect("schema JSON should parse")
    }

    #[test]
    fn parses_and_round_trips() {
        let s = schema(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text", "required": true, "maxLength": 120},
            {"name": "tags", "type": "select", "options": ["news", "guides"], "multiple": true},
            {"name": "seo", "type": "group", "fields": [{"name": "description", "type": "textarea"}]}
        ]}));
        assert_eq!(s.kind, CollectionKind::Collection);
        assert_eq!(
            s.fields[0].kind,
            FieldKind::Text {
                min_length: None,
                max_length: Some(120)
            }
        );
        assert!(s.fields[0].required);
        assert_eq!(s.check(), Ok(()));
        let again: CollectionSchema =
            serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(again, s);
    }

    #[test]
    fn rejects_bad_slugs() {
        let long = "a".repeat(65);
        for slug in ["Posts", "1posts", "", "my-posts", long.as_str()] {
            let s = schema(json!({"slug": slug, "fields": []}));
            assert_eq!(
                s.check(),
                Err(SchemaError::InvalidSlug(slug.to_owned())),
                "{slug}"
            );
        }
    }

    #[test]
    fn rejects_reserved_slug() {
        let s = schema(json!({"slug": "admin", "fields": []}));
        assert_eq!(s.check(), Err(SchemaError::ReservedSlug("admin".into())));
    }

    #[test]
    fn reserved_field_names_only_apply_at_top_level() {
        let top = schema(json!({"slug": "posts", "fields": [{"name": "id", "type": "text"}]}));
        assert_eq!(
            top.check(),
            Err(SchemaError::ReservedFieldName("id".into()))
        );
        let nested = schema(json!({"slug": "posts", "fields": [
            {"name": "meta", "type": "group", "fields": [{"name": "id", "type": "text"}]}
        ]}));
        assert_eq!(nested.check(), Ok(()));
    }

    #[test]
    fn rejects_bad_and_duplicate_field_names() {
        let bad =
            schema(json!({"slug": "posts", "fields": [{"name": "my-title", "type": "text"}]}));
        assert_eq!(
            bad.check(),
            Err(SchemaError::InvalidFieldName("my-title".into()))
        );
        let dup = schema(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"}, {"name": "title", "type": "textarea"}
        ]}));
        assert_eq!(
            dup.check(),
            Err(SchemaError::DuplicateField("title".into()))
        );
    }

    #[test]
    fn rejects_unknown_options() {
        let input = json!({"slug": "posts", "fields": [
            {"name": "seo", "type": "group", "fields": [{"name": "t", "type": "text", "maxLenght": 3}]}
        ]});
        let s = schema(input.clone());
        assert_eq!(
            s.reject_unknown_options(&input),
            Err(SchemaError::UnknownOption {
                field: "t".into(),
                option: "maxLenght".into()
            })
        );
        let top = json!({"slug": "posts", "labels": {}, "fields": []});
        assert_eq!(
            schema(top.clone()).reject_unknown_options(&top),
            Err(SchemaError::UnknownOption {
                field: String::new(),
                option: "labels".into()
            })
        );
        let fine =
            json!({"slug": "posts", "fields": [{"name": "t", "type": "text", "maxLength": 3}]});
        assert_eq!(schema(fine.clone()).reject_unknown_options(&fine), Ok(()));
    }

    #[test]
    fn rejects_inconsistent_field_options() {
        let cases = [
            (
                json!({"name": "t", "type": "text", "minLength": 5, "maxLength": 2}),
                "minLength is greater than maxLength",
            ),
            (
                json!({"name": "n", "type": "number", "min": 10, "max": 1}),
                "min is greater than max",
            ),
            (
                json!({"name": "s", "type": "select", "options": []}),
                "select needs at least one option",
            ),
            (
                json!({"name": "a", "type": "array", "fields": [], "minRows": 3, "maxRows": 1}),
                "minRows is greater than maxRows",
            ),
            (
                json!({"name": "n", "type": "number", "default": "ten"}),
                "default: expected a number",
            ),
        ];
        for (field, message) in cases {
            let name = field["name"].as_str().unwrap().to_owned();
            let s = schema(json!({"slug": "posts", "fields": [field]}));
            assert_eq!(
                s.check(),
                Err(SchemaError::InvalidField {
                    field: name,
                    message: message.into()
                })
            );
        }
    }

    #[test]
    fn parses_slug_relationship_and_index_flags() {
        let input = json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text", "filterable": true},
            {"name": "slug", "type": "slug", "from": "title"},
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "related", "type": "relationship", "relationTo": "posts", "hasMany": true},
            {"name": "meta", "type": "group", "fields": [
                {"name": "editor", "type": "relationship", "relationTo": "authors"}
            ]}
        ]});
        let s = schema(input.clone());
        assert_eq!(s.check(), Ok(()));
        assert_eq!(s.reject_unknown_options(&input), Ok(()));
        assert!(s.fields[0].filterable);
        assert!(s.fields[1].is_unique(), "slugs are always unique");
        assert_eq!(
            s.fields[2].kind,
            FieldKind::Relationship {
                relation_to: "authors".into(),
                has_many: false
            }
        );
        assert_eq!(s.relation_targets(), ["authors", "posts"]);
    }

    #[test]
    fn index_flags_need_a_top_level_scalar() {
        let cases = [
            (
                json!({"name": "data", "type": "json", "unique": true}),
                "json fields can't be unique or filterable",
            ),
            (
                json!({"name": "tags", "type": "select", "options": ["a"], "multiple": true, "filterable": true}),
                "select fields with multiple values can't be unique or filterable",
            ),
        ];
        for (field, message) in cases {
            let name = field["name"].as_str().unwrap().to_owned();
            let s = schema(json!({"slug": "posts", "fields": [field]}));
            assert_eq!(
                s.check(),
                Err(SchemaError::InvalidField {
                    field: name,
                    message: message.into()
                })
            );
        }
        let nested = schema(json!({"slug": "posts", "fields": [
            {"name": "seo", "type": "group", "fields": [{"name": "t", "type": "text", "unique": true}]}
        ]}));
        assert_eq!(
            nested.check(),
            Err(SchemaError::InvalidField {
                field: "t".into(),
                message: "unique, filterable and slug fields must be at the top level".into(),
            })
        );
    }

    #[test]
    fn slug_and_relationship_options_are_checked() {
        let bad_from = schema(
            json!({"slug": "posts", "fields": [{"name": "slug", "type": "slug", "from": "nope"}]}),
        );
        assert_eq!(
            bad_from.check(),
            Err(SchemaError::InvalidField {
                field: "slug".into(),
                message: "from must name a top-level text field, not `nope`".into(),
            })
        );
        let bad_target = schema(json!({"slug": "posts", "fields": [
            {"name": "author", "type": "relationship", "relationTo": "Authors!"}
        ]}));
        assert_eq!(
            bad_target.check(),
            Err(SchemaError::InvalidField {
                field: "author".into(),
                message: "relationTo `Authors!` isn't a valid collection slug".into(),
            })
        );
    }
    #[test]
    fn admin_options_round_trip_and_are_checked() {
        let input = json!({"slug": "posts", "fields": [
            {"name": "kind", "type": "select", "options": ["article", "video"]},
            {"name": "videoUrl", "type": "url", "admin": {
                "description": "Paste the YouTube link",
                "position": "sidebar",
                "width": "50%",
                "condition": {"field": "kind", "equals": "video"}
            }}
        ]});
        let s = schema(input.clone());
        assert_eq!(s.check(), Ok(()));
        assert_eq!(s.reject_unknown_options(&input), Ok(()));
        let admin = &s.fields[1].admin;
        assert_eq!(admin.position, Position::Sidebar);
        assert_eq!(
            admin.condition,
            Some(Condition {
                field: "kind".into(),
                test: ConditionTest::Equals(json!("video"))
            })
        );
        assert_eq!(s.fields[0].admin, Admin::default(), "admin is optional");

        let typo = json!({"slug": "posts", "fields": [{"name": "t", "type": "text", "admin": {"descripton": "x"}}]});
        assert_eq!(
            schema(typo.clone()).reject_unknown_options(&typo),
            Err(SchemaError::UnknownOption {
                field: "t".into(),
                option: "admin.descripton".into()
            })
        );

        let dangling = schema(json!({"slug": "posts", "fields": [
            {"name": "t", "type": "text", "admin": {"condition": {"field": "nope", "exists": true}}}
        ]}));
        assert_eq!(
            dangling.check(),
            Err(SchemaError::InvalidField {
                field: "t".into(),
                message: "admin.condition refers to `nope`, which isn't a field".into()
            })
        );
        let bad_width = schema(json!({"slug": "posts", "fields": [
            {"name": "t", "type": "text", "admin": {"width": "half"}}
        ]}));
        assert_eq!(
            bad_width.check(),
            Err(SchemaError::InvalidField {
                field: "t".into(),
                message: "admin.width must look like 50% or 240px".into()
            })
        );
    }
}
