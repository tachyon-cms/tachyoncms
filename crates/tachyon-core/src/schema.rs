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
    #[serde(flatten)]
    pub kind: FieldKind,
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
}

impl CollectionSchema {
    /// Checks the schema is usable before it's stored.
    pub fn check(&self) -> Result<(), SchemaError> {
        if !is_identifier(&self.slug, false) {
            return Err(SchemaError::InvalidSlug(self.slug.clone()));
        }
        if RESERVED_SLUGS.contains(&self.slug.as_str()) {
            return Err(SchemaError::ReservedSlug(self.slug.clone()));
        }
        check_fields(&self.fields, true)
    }
}

fn is_identifier(s: &str, allow_upper: bool) -> bool {
    let ok = |c: char| c.is_ascii_lowercase() || (allow_upper && c.is_ascii_uppercase());
    let mut chars = s.chars();
    s.len() <= 64
        && chars.next().is_some_and(ok)
        && chars.all(|c| ok(c) || c.is_ascii_digit() || c == '_')
}

fn check_fields(fields: &[Field], top_level: bool) -> Result<(), SchemaError> {
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
            FieldKind::Group { fields } | FieldKind::Array { fields, .. } => {
                check_fields(fields, false)?
            }
            _ => {}
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
}
