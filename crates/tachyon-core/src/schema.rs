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
    #[serde(default)]
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
    #[serde(default, deserialize_with = "null_as_default")]
    pub admin: Admin,
    #[serde(flatten)]
    pub kind: FieldKind,
}

/// `null` reads as the default, like a missing key.
fn null_as_default<'de, D: serde::Deserializer<'de>, T: Default + Deserialize<'de>>(
    d: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
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
        /// Store a list of values instead of one.
        #[serde(default)]
        has_many: bool,
        #[serde(default)]
        min_rows: Option<usize>,
        #[serde(default)]
        max_rows: Option<usize>,
    },
    Textarea {
        #[serde(default)]
        min_length: Option<usize>,
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
        /// Store a list of values instead of one.
        #[serde(default)]
        has_many: bool,
        #[serde(default)]
        min_rows: Option<usize>,
        #[serde(default)]
        max_rows: Option<usize>,
    },
    #[serde(alias = "checkbox")]
    Boolean,
    Date {
        #[serde(default)]
        date_only: bool,
    },
    Email,
    Url,
    Select {
        options: Vec<SelectOption>,
        #[serde(default)]
        multiple: bool,
    },
    Radio {
        options: Vec<SelectOption>,
    },
    Code {
        /// For the admin's editor highlighting, such as `rust` or `html`.
        #[serde(default)]
        language: Option<String>,
    },
    /// A location, stored as `[longitude, latitude]`.
    Point,
    Slug {
        /// A text field to generate the slug from when none is given.
        #[serde(default)]
        from: Option<String>,
    },
    Relationship {
        relation_to: RelationTo,
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
    /// Puts its fields side by side in the admin. No effect on data.
    Row {
        fields: Vec<Field>,
    },
    /// A titled section the editor can fold away (titled by the field's `label`). No effect on data.
    Collapsible {
        #[serde(default)]
        initially_collapsed: bool,
        fields: Vec<Field>,
    },
    /// Tabs in the admin. A tab with a `name` nests its fields like a group; one without
    /// a name has no effect on data.
    Tabs {
        tabs: Vec<Tab>,
    },
}

/// A choice in a `select` or `radio`: a bare value, or a label shown to editors plus the stored value.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SelectOption {
    Value(String),
    Labeled { label: String, value: String },
}

impl<'de> Deserialize<'de> for SelectOption {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
        match Value::deserialize(d)? {
            Value::String(value) => Ok(Self::Value(value)),
            // Extra keys are left for `reject_unknown_options`, which names them.
            Value::Object(o) => match (text(o.get("label")), text(o.get("value"))) {
                (Some(label), Some(value)) => Ok(Self::Labeled { label, value }),
                _ => Err(serde::de::Error::custom(OPTION_SHAPE)),
            },
            _ => Err(serde::de::Error::custom(OPTION_SHAPE)),
        }
    }
}

const OPTION_SHAPE: &str = "each option must be text, or {\"label\": \"…\", \"value\": \"…\"}";

impl SelectOption {
    pub fn value(&self) -> &str {
        match self {
            Self::Value(v) | Self::Labeled { value: v, .. } => v,
        }
    }
}

/// Which collections a relationship may link to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RelationTo {
    One(String),
    Many(Vec<String>),
}

impl RelationTo {
    pub fn targets(&self) -> Vec<&str> {
        match self {
            Self::One(s) => vec![s.as_str()],
            Self::Many(list) => list.iter().map(String::as_str).collect(),
        }
    }

    /// Several possible targets, so each stored link names its collection.
    pub fn is_polymorphic(&self) -> bool {
        matches!(self, Self::Many(_))
    }

    pub fn allows(&self, slug: &str) -> bool {
        self.targets().contains(&slug)
    }
}

impl std::fmt::Display for RelationTo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.targets().join(", "))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tab {
    #[serde(default)]
    pub name: Option<String>,
    pub label: String,
    pub fields: Vec<Field>,
}

/// The fields as stored data sees them: rows, collapsibles and unnamed tabs dissolve into
/// their parent, and named tabs become groups. Applies at every level.
// NOTE: allocates on every call; fine for schemas of a few dozen fields.
pub fn data_fields(fields: &[Field]) -> Vec<Field> {
    let mut out = Vec::new();
    for f in fields {
        match &f.kind {
            FieldKind::Row { fields } | FieldKind::Collapsible { fields, .. } => {
                out.extend(data_fields(fields))
            }
            FieldKind::Tabs { tabs } => {
                for tab in tabs {
                    match &tab.name {
                        Some(name) => out.push(Field {
                            name: name.clone(),
                            label: Some(tab.label.clone()),
                            required: false,
                            default: None,
                            unique: false,
                            filterable: false,
                            admin: Admin::default(),
                            kind: FieldKind::Group {
                                fields: data_fields(&tab.fields),
                            },
                        }),
                        None => out.extend(data_fields(&tab.fields)),
                    }
                }
            }
            FieldKind::Group { fields } => out.push(Field {
                kind: FieldKind::Group {
                    fields: data_fields(fields),
                },
                ..f.clone()
            }),
            FieldKind::Array {
                fields,
                min_rows,
                max_rows,
            } => out.push(Field {
                kind: FieldKind::Array {
                    fields: data_fields(fields),
                    min_rows: *min_rows,
                    max_rows: *max_rows,
                },
                ..f.clone()
            }),
            _ => out.push(f.clone()),
        }
    }
    out
}

/// Admin options any field can carry, data or layout. `root` is the data shape; `inside`
/// holds the data paths the field itself covers, which its condition can't depend on.
fn check_admin(
    admin: &Admin,
    root: &[Field],
    inside: &[String],
    layout: bool,
) -> Result<(), String> {
    if let Some(condition) = &admin.condition {
        let target = &condition.field;
        let Some(f) = find_field(root, target) else {
            return Err(format!(
                "admin.condition refers to `{target}`, which isn't a field"
            ));
        };
        if inside
            .iter()
            .any(|p| target == p || target.starts_with(&format!("{p}.")))
        {
            return Err(if layout {
                "admin.condition can't depend on a field inside it".into()
            } else {
                "admin.condition can't depend on the field itself".into()
            });
        }
        match (&f.kind, &condition.test) {
            (FieldKind::Group { .. }, _) => {
                return Err(format!(
                    "admin.condition must point at a field with a value, not the group `{target}`"
                ));
            }
            (FieldKind::Array { .. }, ConditionTest::Exists(_)) => {}
            (FieldKind::Array { .. }, _) => {
                return Err(format!(
                    "admin.condition on the array `{target}` can only test exists"
                ));
            }
            _ => {}
        }
    }
    if let Some(width) = &admin.width
        && !is_size(width)
    {
        return Err("admin.width must be a size like 50% or 240px".into());
    }
    Ok(())
}

/// A positive width in pixels, or a percentage up to 100.
fn is_size(width: &str) -> bool {
    let (number, max) = if let Some(n) = width.strip_suffix('%') {
        (n, 100.0)
    } else if let Some(n) = width.strip_suffix("px") {
        (n, f64::MAX)
    } else {
        return false;
    };
    number
        .parse::<f64>()
        .is_ok_and(|v| v.is_finite() && v > 0.0 && v <= max)
}

/// Rules for layout fields themselves, on the raw tree. Data rules run on `data_fields`.
/// `loc` is where these fields sit in the JSON (`fields.0.fields`), which names a layout
/// field in errors since it has no name; `prefix` is their data path.
fn check_layout(
    fields: &[Field],
    root: &[Field],
    loc: &str,
    prefix: &str,
) -> Result<(), SchemaError> {
    for (i, f) in fields.iter().enumerate() {
        let here = format!("{loc}.{i}");
        if f.kind.is_layout() {
            let invalid = |message: String| SchemaError::InvalidField {
                field: here.clone(),
                message,
            };
            let inside: Vec<String> = data_fields(std::slice::from_ref(f))
                .iter()
                .map(|d| join(prefix, &d.name))
                .collect();
            check_admin(&f.admin, root, &inside, true).map_err(invalid)?;
            let kind = f.kind.name();
            if !f.name.is_empty() {
                return Err(invalid(format!("{kind} fields don't take a name")));
            }
            if f.required || f.default.is_some() || f.unique || f.filterable {
                return Err(invalid(format!(
                    "{kind} fields can't be required, unique, filterable or have a default"
                )));
            }
            match &f.kind {
                FieldKind::Collapsible { .. }
                    if f.label.as_deref().is_none_or(|l| l.trim().is_empty()) =>
                {
                    return Err(invalid("collapsible fields need a label".into()));
                }
                FieldKind::Row { .. } | FieldKind::Tabs { .. } if f.label.is_some() => {
                    return Err(invalid(format!("{kind} fields don't take a label")));
                }
                FieldKind::Tabs { tabs } if tabs.is_empty() => {
                    return Err(invalid("tabs needs at least one tab".into()));
                }
                FieldKind::Tabs { tabs } if tabs.iter().any(|t| t.label.trim().is_empty()) => {
                    return Err(invalid("every tab needs a label".into()));
                }
                _ => {}
            }
        }
        match &f.kind {
            FieldKind::Row { fields } | FieldKind::Collapsible { fields, .. } => {
                check_layout(fields, root, &format!("{here}.fields"), prefix)?
            }
            FieldKind::Group { fields } | FieldKind::Array { fields, .. } => check_layout(
                fields,
                root,
                &format!("{here}.fields"),
                &join(prefix, &f.name),
            )?,
            FieldKind::Tabs { tabs } => {
                for (t, tab) in tabs.iter().enumerate() {
                    let prefix = match &tab.name {
                        Some(name) => join(prefix, name),
                        None => prefix.to_owned(),
                    };
                    check_layout(
                        &tab.fields,
                        root,
                        &format!("{here}.tabs.{t}.fields"),
                        &prefix,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

/// Turns a failure to read a schema into a message naming the field at fault, such as
/// "field `seo.kind`: each option must be text…". Falls back to serde's own message.
pub fn explain_parse_error(raw: &Value, e: &serde_json::Error) -> String {
    fn locate(fields: Option<&Value>, loc: &str, names: &str) -> Option<String> {
        let Some(Value::Array(items)) = fields else {
            return None;
        };
        for (i, item) in items.iter().enumerate() {
            let here = format!("{loc}.{i}");
            let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
            let label = if name.is_empty() {
                here.clone()
            } else {
                join(names, name)
            };
            // Children first, so the deepest field at fault is named.
            let inner_names = if name.is_empty() { names } else { &label };
            if let Some(found) = locate(item.get("fields"), &format!("{here}.fields"), inner_names)
            {
                return Some(found);
            }
            if let Some(Value::Array(tabs)) = item.get("tabs") {
                for (t, tab) in tabs.iter().enumerate() {
                    let tab_names = match tab.get("name").and_then(Value::as_str) {
                        Some(n) => join(names, n),
                        None => names.to_owned(),
                    };
                    if let Some(found) = locate(
                        tab.get("fields"),
                        &format!("{here}.tabs.{t}.fields"),
                        &tab_names,
                    ) {
                        return Some(found);
                    }
                }
            }
            if let Err(e) = Field::deserialize(item) {
                return Some(format!("field `{label}`: {e}"));
            }
        }
        None
    }
    locate(raw.get("fields"), "fields", "").unwrap_or_else(|| e.to_string())
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
            Self::Radio { .. } => "radio",
            Self::Code { .. } => "code",
            Self::Point => "point",
            Self::Slug { .. } => "slug",
            Self::Relationship { .. } => "relationship",
            Self::Json => "json",
            Self::Group { .. } => "group",
            Self::Array { .. } => "array",
            Self::Row { .. } => "row",
            Self::Collapsible { .. } => "collapsible",
            Self::Tabs { .. } => "tabs",
        }
    }

    pub fn is_layout(&self) -> bool {
        matches!(
            self,
            Self::Row { .. } | Self::Collapsible { .. } | Self::Tabs { .. }
        )
    }

    /// A single comparable value, so it can carry a B-tree index.
    pub fn is_indexable(&self) -> bool {
        match self {
            Self::Select { multiple, .. } => !multiple,
            Self::Relationship {
                relation_to,
                has_many,
            } => !has_many && !relation_to.is_polymorphic(),
            Self::Text { has_many, .. } | Self::Number { has_many, .. } => !has_many,
            Self::Json
            | Self::Group { .. }
            | Self::Array { .. }
            | Self::Row { .. }
            | Self::Collapsible { .. }
            | Self::Tabs { .. }
            | Self::Point => false,
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
                    FieldKind::Relationship { relation_to, .. } => {
                        out.extend(relation_to.targets())
                    }
                    FieldKind::Group { fields }
                    | FieldKind::Array { fields, .. }
                    | FieldKind::Row { fields }
                    | FieldKind::Collapsible { fields, .. } => walk(fields, out),
                    FieldKind::Tabs { tabs } => {
                        for tab in tabs {
                            walk(&tab.fields, out);
                        }
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
        let data = data_fields(&self.fields);
        check_layout(&self.fields, &data, "fields", "")?;
        check_fields(&data, true, &data, "")
    }

    /// Rejects keys in the original JSON that parsing would silently drop, such as a
    /// misspelt `maxLenght`. Every option this model understands survives a round trip,
    /// so anything missing after re-serialising is unknown.
    pub fn reject_unknown_options(&self, input: &Value) -> Result<(), SchemaError> {
        let known = serde_json::to_value(self).expect("schemas always serialize");
        unknown_key(input, &known, "")?;
        unknown_in_fields(&self.fields, input.get("fields"), "fields")
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

fn unknown_in_fields(
    fields: &[Field],
    input: Option<&Value>,
    loc: &str,
) -> Result<(), SchemaError> {
    let Some(Value::Array(raw)) = input else {
        return Ok(());
    };
    for (i, (f, raw)) in fields.iter().zip(raw).enumerate() {
        let here = format!("{loc}.{i}");
        // Layout fields have no name, so they're named by where they sit.
        let label = if f.name.is_empty() {
            here.clone()
        } else {
            f.name.clone()
        };
        let known = serde_json::to_value(f).expect("fields always serialize");
        unknown_key(raw, &known, &label)?;
        // Nested objects that parsing could also drop keys from: admin, its condition
        // (which holds exactly one test), and labelled options.
        let mut nested = vec![
            ("admin".to_owned(), raw.get("admin"), known.get("admin")),
            (
                "admin.condition".to_owned(),
                raw.pointer("/admin/condition"),
                known.pointer("/admin/condition"),
            ),
        ];
        if let (Some(Value::Array(raw_options)), Some(Value::Array(known_options))) =
            (raw.get("options"), known.get("options"))
        {
            for (i, (r, k)) in raw_options.iter().zip(known_options).enumerate() {
                nested.push((format!("options.{i}"), Some(r), Some(k)));
            }
        }
        for (prefix, raw_part, known_part) in nested {
            if let (Some(Value::Object(r)), Some(Value::Object(k))) = (raw_part, known_part)
                && let Some(option) = r.keys().find(|key| !k.contains_key(*key))
            {
                return Err(SchemaError::UnknownOption {
                    field: label.clone(),
                    option: format!("{prefix}.{option}"),
                });
            }
        }
        match &f.kind {
            FieldKind::Group { fields }
            | FieldKind::Array { fields, .. }
            | FieldKind::Row { fields }
            | FieldKind::Collapsible { fields, .. } => {
                unknown_in_fields(fields, raw.get("fields"), &format!("{here}.fields"))?
            }
            FieldKind::Tabs { tabs } => {
                if let (Some(Value::Array(raw_tabs)), Some(Value::Array(known_tabs))) =
                    (raw.get("tabs"), known.get("tabs"))
                {
                    for (t, ((tab, raw_tab), known_tab)) in
                        tabs.iter().zip(raw_tabs).zip(known_tabs).enumerate()
                    {
                        if let (Value::Object(r), Value::Object(k)) = (raw_tab, known_tab)
                            && let Some(option) = r.keys().find(|key| !k.contains_key(*key))
                        {
                            return Err(SchemaError::UnknownOption {
                                field: label.clone(),
                                option: format!("tabs.{t}.{option}"),
                            });
                        }
                        unknown_in_fields(
                            &tab.fields,
                            raw_tab.get("fields"),
                            &format!("{here}.tabs.{t}.fields"),
                        )?;
                    }
                }
            }
            _ => {}
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

fn check_fields(
    fields: &[Field],
    top_level: bool,
    root: &[Field],
    prefix: &str,
) -> Result<(), SchemaError> {
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
            let what = match &f.kind {
                FieldKind::Relationship { relation_to, .. } if relation_to.is_polymorphic() => {
                    "relationship fields with several targets".to_owned()
                }
                FieldKind::Select { .. } => "select fields with multiple values".to_owned(),
                FieldKind::Relationship { .. }
                | FieldKind::Text { .. }
                | FieldKind::Number { .. } => {
                    format!("{} fields with multiple values", f.kind.name())
                }
                _ => format!("{} fields", f.kind.name()),
            };
            return Err(invalid(format!("{what} can't be unique or filterable")));
        }
        match &f.kind {
            FieldKind::Text {
                min_length: Some(a),
                max_length: Some(b),
                ..
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
            FieldKind::Text {
                has_many,
                min_rows,
                max_rows,
                ..
            }
            | FieldKind::Number {
                has_many,
                min_rows,
                max_rows,
                ..
            } if (min_rows.is_some() || max_rows.is_some())
                && (!has_many || min_rows > max_rows && max_rows.is_some()) =>
            {
                return Err(invalid(if *has_many {
                    "minRows is greater than maxRows".into()
                } else {
                    "minRows and maxRows only apply with hasMany".into()
                }));
            }
            FieldKind::Select { options, .. } | FieldKind::Radio { options } => {
                if options.is_empty() {
                    return Err(invalid(format!(
                        "{} needs at least one option",
                        f.kind.name()
                    )));
                }
                let mut values = HashSet::new();
                if let Some(dup) = options
                    .iter()
                    .map(SelectOption::value)
                    .find(|v| !values.insert(*v))
                {
                    return Err(invalid(format!(
                        "option values must be unique; `{dup}` appears twice"
                    )));
                }
            }
            FieldKind::Textarea {
                min_length: Some(a),
                max_length: Some(b),
            } if a > b => {
                return Err(invalid("minLength is greater than maxLength".into()));
            }
            FieldKind::Array {
                min_rows: Some(a),
                max_rows: Some(b),
                ..
            } if a > b => {
                return Err(invalid("minRows is greater than maxRows".into()));
            }
            FieldKind::Slug { from: Some(source) }
                if !fields.iter().any(|s| {
                    &s.name == source
                        && matches!(
                            s.kind,
                            FieldKind::Text {
                                has_many: false,
                                ..
                            }
                        )
                }) =>
            {
                return Err(invalid(format!(
                    "from must name a top-level text field, not `{source}`"
                )));
            }
            FieldKind::Relationship { relation_to, .. } => {
                let targets = relation_to.targets();
                if targets.is_empty() {
                    return Err(invalid("relationTo needs at least one collection".into()));
                }
                let mut seen = HashSet::new();
                for t in targets {
                    if !is_identifier(t, false) {
                        return Err(invalid(format!(
                            "relationTo `{t}` isn't a valid collection slug"
                        )));
                    }
                    if !seen.insert(t) {
                        return Err(invalid(format!("relationTo lists `{t}` twice")));
                    }
                }
            }
            FieldKind::Group { fields } | FieldKind::Array { fields, .. } => {
                check_fields(fields, false, root, &join(prefix, &f.name))?
            }
            _ => {}
        }
        check_admin(&f.admin, root, &[join(prefix, &f.name)], false).map_err(invalid)?;
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
                max_length: Some(120),
                has_many: false,
                min_rows: None,
                max_rows: None,
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
                relation_to: RelationTo::One("authors".into()),
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
                message: "admin.width must be a size like 50% or 240px".into()
            })
        );
    }
    #[test]
    fn labelled_options_radio_checkbox_and_code_parse() {
        let input = json!({"slug": "posts", "fields": [
            {"name": "stage", "type": "select", "options": ["draft", {"label": "Ready to go", "value": "ready"}]},
            {"name": "size", "type": "radio", "options": [{"label": "Small", "value": "s"}, {"label": "Large", "value": "l"}]},
            {"name": "featured", "type": "checkbox"},
            {"name": "snippet", "type": "code", "language": "rust"},
            {"name": "intro", "type": "textarea", "minLength": 10}
        ]});
        let s = schema(input.clone());
        assert_eq!(s.check(), Ok(()));
        assert_eq!(
            s.fields[2].kind,
            FieldKind::Boolean,
            "checkbox is stored as boolean"
        );
        assert_eq!(
            s.fields[3].kind,
            FieldKind::Code {
                language: Some("rust".into())
            }
        );
        let FieldKind::Select { options, .. } = &s.fields[0].kind else {
            panic!()
        };
        assert_eq!(
            options.iter().map(SelectOption::value).collect::<Vec<_>>(),
            ["draft", "ready"]
        );

        let dup = schema(
            json!({"slug": "posts", "fields": [{"name": "r", "type": "radio", "options": ["a", {"label": "A", "value": "a"}]}]}),
        );
        assert_eq!(
            dup.check(),
            Err(SchemaError::InvalidField {
                field: "r".into(),
                message: "option values must be unique; `a` appears twice".into()
            })
        );
    }
    #[test]
    fn has_many_fields_cant_be_indexed() {
        let s = schema(
            json!({"slug": "posts", "fields": [{"name": "tags", "type": "text", "hasMany": true, "filterable": true}]}),
        );
        assert_eq!(
            s.check(),
            Err(SchemaError::InvalidField {
                field: "tags".into(),
                message: "text fields with multiple values can't be unique or filterable".into()
            })
        );
    }

    #[test]
    fn stray_keys_in_conditions_and_options_are_rejected() {
        let cases = [
            (
                json!({"name": "t", "type": "text", "admin": {"condition": {"field": "t", "equals": "a", "bogus": 1}}}),
                "admin.condition.bogus",
            ),
            (
                json!({"name": "t", "type": "text", "admin": {"condition": {"field": "t", "equals": "a", "in": ["b"]}}}),
                "admin.condition.in",
            ),
            (
                json!({"name": "c", "type": "select", "options": [{"label": "A", "value": "a", "color": "red"}]}),
                "options.0.color",
            ),
        ];
        for (field, option) in cases {
            let name = field["name"].as_str().unwrap().to_owned();
            let input = json!({"slug": "posts", "fields": [field]});
            assert_eq!(
                schema(input.clone()).reject_unknown_options(&input),
                Err(SchemaError::UnknownOption {
                    field: name,
                    option: option.into()
                })
            );
        }
    }

    #[test]
    fn row_limits_need_has_many_and_must_be_ordered() {
        for (field, message) in [
            (
                json!({"name": "t", "type": "text", "hasMany": true, "minRows": 3, "maxRows": 1}),
                "minRows is greater than maxRows",
            ),
            (
                json!({"name": "t", "type": "number", "minRows": 2}),
                "minRows and maxRows only apply with hasMany",
            ),
        ] {
            let s = schema(json!({"slug": "posts", "fields": [field]}));
            assert_eq!(
                s.check(),
                Err(SchemaError::InvalidField {
                    field: "t".into(),
                    message: message.into()
                })
            );
        }
        let slug_from_list = schema(json!({"slug": "posts", "fields": [
            {"name": "tags", "type": "text", "hasMany": true},
            {"name": "slug", "type": "slug", "from": "tags"}
        ]}));
        assert_eq!(
            slug_from_list.check(),
            Err(SchemaError::InvalidField {
                field: "slug".into(),
                message: "from must name a top-level text field, not `tags`".into()
            })
        );
    }
    fn layout_schema() -> Value {
        json!({"slug": "posts", "fields": [
            {"type": "row", "fields": [
                {"name": "title", "type": "text"},
                {"name": "slug", "type": "slug", "from": "title"}
            ]},
            {"type": "tabs", "tabs": [
                {"label": "Content", "fields": [{"name": "body", "type": "textarea"}]},
                {"name": "meta", "label": "Meta", "fields": [
                    {"type": "collapsible", "label": "SEO", "fields": [{"name": "title", "type": "text"}]}
                ]}
            ]}
        ]})
    }

    #[test]
    fn layout_dissolves_into_the_data_shape() {
        let input = layout_schema();
        let s = schema(input.clone());
        assert_eq!(s.check(), Ok(()));
        assert_eq!(s.reject_unknown_options(&input), Ok(()));
        let data: Vec<(String, &'static str)> = data_fields(&s.fields)
            .iter()
            .map(|f| (f.name.clone(), f.kind.name()))
            .collect();
        assert_eq!(
            data,
            [
                ("title".into(), "text"),
                ("slug".into(), "slug"),
                ("body".into(), "textarea"),
                ("meta".into(), "group")
            ]
        );
        let meta = &data_fields(&s.fields)[3];
        let FieldKind::Group { fields } = &meta.kind else {
            panic!()
        };
        assert_eq!(
            fields[0].name, "title",
            "named tabs nest their fields; collapsibles inside them dissolve"
        );
    }

    #[test]
    fn layout_rules_are_checked() {
        let cases = [
            (
                json!([{"type": "row", "name": "r", "fields": []}]),
                "fields.0",
                "row fields don't take a name",
            ),
            (
                json!([{"type": "row", "required": true, "fields": []}]),
                "fields.0",
                "row fields can't be required, unique, filterable or have a default",
            ),
            (
                json!([{"type": "tabs", "tabs": []}]),
                "fields.0",
                "tabs needs at least one tab",
            ),
            (
                json!([{"type": "tabs", "tabs": [{"label": "", "fields": []}]}]),
                "fields.0",
                "every tab needs a label",
            ),
            (
                json!([{"type": "row", "label": "R", "fields": []}]),
                "fields.0",
                "row fields don't take a label",
            ),
            (
                json!([{"type": "tabs", "label": "T", "tabs": [{"label": "A", "fields": []}]}]),
                "fields.0",
                "tabs fields don't take a label",
            ),
            (
                json!([{"name": "seo", "type": "group", "fields": [
                    {"name": "title", "type": "text"},
                    {"type": "collapsible", "label": "More", "fields": [
                        {"type": "row", "name": "oops", "fields": []}
                    ]}
                ]}]),
                "fields.0.fields.1.fields.0",
                "row fields don't take a name",
            ),
            (
                json!([{"type": "tabs", "tabs": [{"label": "A", "fields": [
                    {"type": "row", "unique": true, "fields": []}
                ]}]}]),
                "fields.0.tabs.0.fields.0",
                "row fields can't be required, unique, filterable or have a default",
            ),
        ];
        for (fields, field, message) in cases {
            let s = schema(json!({"slug": "posts", "fields": fields}));
            assert_eq!(
                s.check(),
                Err(SchemaError::InvalidField {
                    field: field.into(),
                    message: message.into()
                }),
                "{message}"
            );
        }
        let clash = schema(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"type": "row", "fields": [{"name": "title", "type": "text"}]}
        ]}));
        assert_eq!(
            clash.check(),
            Err(SchemaError::DuplicateField("title".into()))
        );
        let same_name_in_named_tabs = schema(
            json!({"slug": "posts", "fields": [{"type": "tabs", "tabs": [
                {"name": "a", "label": "A", "fields": [{"name": "title", "type": "text"}]},
                {"name": "b", "label": "B", "fields": [{"name": "title", "type": "text"}]}
            ]}]}),
        );
        assert_eq!(same_name_in_named_tabs.check(), Ok(()));
    }

    #[test]
    fn unknown_keys_inside_layout_are_rejected() {
        let cases = [
            (
                json!({"type": "row", "fields": [{"name": "t", "type": "text", "maxLenght": 3}]}),
                "t",
                "maxLenght",
            ),
            (
                json!({"type": "tabs", "tabs": [{"label": "A", "fields": [], "icon": "x"}]}),
                "fields.0",
                "tabs.0.icon",
            ),
            (
                json!({"type": "tabs", "tabs": [{"label": "A", "fields": [{"name": "t", "type": "text", "bogus": 1}]}]}),
                "t",
                "bogus",
            ),
        ];
        for (field, name, option) in cases {
            let input = json!({"slug": "posts", "fields": [field]});
            assert_eq!(
                schema(input.clone()).reject_unknown_options(&input),
                Err(SchemaError::UnknownOption {
                    field: name.into(),
                    option: option.into()
                })
            );
        }
    }

    #[test]
    fn admin_options_on_layout_fields_are_checked() {
        for (field, message) in [
            (
                json!({"type": "row", "admin": {"condition": {"field": "nope", "equals": 1}}, "fields": []}),
                "admin.condition refers to `nope`, which isn't a field",
            ),
            (
                json!({"type": "collapsible", "label": "C", "admin": {"width": "banana"}, "fields": []}),
                "admin.width must be a size like 50% or 240px",
            ),
        ] {
            let s = schema(json!({"slug": "posts", "fields": [field]}));
            assert_eq!(
                s.check(),
                Err(SchemaError::InvalidField {
                    field: "fields.0".into(),
                    message: message.into()
                })
            );
        }
        let ok = schema(json!({"slug": "posts", "fields": [
            {"name": "kind", "type": "text"},
            {"type": "row", "admin": {"condition": {"field": "kind", "equals": "video"}}, "fields": []}
        ]}));
        assert_eq!(ok.check(), Ok(()));
    }
    #[test]
    fn polymorphic_relationships_parse_and_are_checked() {
        let s = schema(json!({"slug": "menus", "fields": [
            {"name": "link", "type": "relationship", "relationTo": ["posts", "pages"]},
            {"name": "author", "type": "relationship", "relationTo": "authors"}
        ]}));
        assert_eq!(s.check(), Ok(()));
        assert_eq!(s.relation_targets(), ["authors", "pages", "posts"]);
        let FieldKind::Relationship { relation_to, .. } = &s.fields[0].kind else {
            panic!()
        };
        assert!(
            relation_to.is_polymorphic()
                && relation_to.allows("pages")
                && !relation_to.allows("authors")
        );

        for (rel, message) in [
            (json!([]), "relationTo needs at least one collection"),
            (json!(["posts", "posts"]), "relationTo lists `posts` twice"),
        ] {
            let bad = schema(
                json!({"slug": "menus", "fields": [{"name": "link", "type": "relationship", "relationTo": rel}]}),
            );
            assert_eq!(
                bad.check(),
                Err(SchemaError::InvalidField {
                    field: "link".into(),
                    message: message.into()
                })
            );
        }
        let indexed = schema(json!({"slug": "menus", "fields": [
            {"name": "link", "type": "relationship", "relationTo": ["posts", "pages"], "filterable": true}
        ]}));
        assert_eq!(
            indexed.check(),
            Err(SchemaError::InvalidField {
                field: "link".into(),
                message: "relationship fields with several targets can't be unique or filterable"
                    .into()
            })
        );
    }

    fn check_err(fields: Value) -> String {
        match schema(json!({"slug": "posts", "fields": fields})).check() {
            Err(e) => e.to_string(),
            Ok(()) => "ok".into(),
        }
    }

    #[test]
    fn option_shape_errors_name_the_field() {
        let message =
            "field `kind`: each option must be text, or {\"label\": \"…\", \"value\": \"…\"}";
        for options in [
            json!([1]),
            json!([{"value": "a"}]),
            json!([{"label": "A", "value": 2}]),
        ] {
            let raw = json!({"slug": "posts", "fields": [
                {"name": "title", "type": "text"},
                {"name": "seo", "type": "group", "fields": [{"name": "kind", "type": "select", "options": options}]}
            ]});
            let e = CollectionSchema::deserialize(&raw).unwrap_err();
            assert_eq!(
                explain_parse_error(&raw, &e),
                message.replace("`kind`", "`seo.kind`")
            );
        }
        let raw = json!({"slug": "posts", "fields": [
            {"type": "row", "fields": [{"name": "kind", "type": "radio", "options": [true]}]}
        ]});
        let e = CollectionSchema::deserialize(&raw).unwrap_err();
        assert_eq!(explain_parse_error(&raw, &e), message);
        let raw = json!({"slug": "posts", "fields": [{"name": "x", "type": "bogus"}]});
        let e = CollectionSchema::deserialize(&raw).unwrap_err();
        assert!(explain_parse_error(&raw, &e).starts_with("field `x`: unknown variant `bogus`"));
        let raw = json!({"slug": "posts", "fields": [{"type": "row", "fields": "nope"}]});
        let e = CollectionSchema::deserialize(&raw).unwrap_err();
        assert!(explain_parse_error(&raw, &e).starts_with("field `fields.0`: "));
    }

    #[test]
    fn admin_null_means_defaults() {
        let input =
            json!({"slug": "posts", "fields": [{"name": "t", "type": "text", "admin": null}]});
        let s = schema(input.clone());
        assert_eq!(s.fields[0].admin, Admin::default());
        assert_eq!(s.reject_unknown_options(&input), Ok(()));
    }

    #[test]
    fn width_must_be_a_real_size() {
        for (width, ok) in [
            ("50%", true),
            ("240px", true),
            ("33.3%", true),
            ("100%", true),
            ("half%", false),
            ("0px", false),
            ("150%", false),
            ("-5%", false),
            ("%", false),
            ("NaNpx", false),
            ("infpx", false),
        ] {
            let result =
                check_err(json!([{"name": "t", "type": "text", "admin": {"width": width}}]));
            assert_eq!(result == "ok", ok, "{width}: {result}");
        }
    }

    #[test]
    fn conditions_cant_point_at_themselves_or_groups() {
        let cases = [
            (
                json!([{"name": "t", "type": "text", "admin": {"condition": {"field": "t", "exists": true}}}]),
                "field `t`: admin.condition can't depend on the field itself",
            ),
            (
                json!([{"name": "seo", "type": "group", "admin": {"condition": {"field": "seo.on", "equals": true}},
                        "fields": [{"name": "on", "type": "boolean"}]}]),
                "field `seo`: admin.condition can't depend on the field itself",
            ),
            (
                json!([{"name": "seo", "type": "group", "fields": [{"name": "on", "type": "boolean"}]},
                       {"name": "t", "type": "text", "admin": {"condition": {"field": "seo", "exists": true}}}]),
                "field `t`: admin.condition must point at a field with a value, not the group `seo`",
            ),
            (
                json!([{"name": "links", "type": "array", "fields": [{"name": "url", "type": "url"}]},
                       {"name": "t", "type": "text", "admin": {"condition": {"field": "links", "equals": 1}}}]),
                "field `t`: admin.condition on the array `links` can only test exists",
            ),
            (
                json!([{"type": "row", "admin": {"condition": {"field": "a", "exists": true}},
                        "fields": [{"name": "a", "type": "text"}]}]),
                "field `fields.0`: admin.condition can't depend on a field inside it",
            ),
        ];
        for (fields, message) in cases {
            assert_eq!(check_err(fields), message);
        }
        assert_eq!(
            check_err(json!([
                {"name": "links", "type": "array", "fields": [{"name": "url", "type": "url"}]},
                {"name": "t", "type": "text", "admin": {"condition": {"field": "links", "exists": true}}},
                {"name": "seo", "type": "group", "fields": [
                    {"name": "on", "type": "boolean"},
                    {"name": "title", "type": "text", "admin": {"condition": {"field": "seo.on", "equals": true}}}
                ]}
            ])),
            "ok"
        );
    }
}
