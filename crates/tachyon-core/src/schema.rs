use std::borrow::{Borrow, Cow};
use std::collections::HashSet;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Keys the API adds to every document, so top-level fields can't use them.
pub const RESERVED_FIELD_NAMES: &[&str] = &["id", "createdAt", "updatedAt", "status"];

/// Paths under `/api` that belong to the server rather than to a collection.
pub const RESERVED_SLUGS: &[&str] = &[
    "access", "admin", "auth", "blocks", "globals", "media", "search", "graphql", "plugins",
    "setup",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSchema {
    pub slug: String,
    #[serde(default)]
    pub kind: CollectionKind,
    pub fields: Vec<Field>,
    /// What other collections get when they populate a link to this one, as a `select`.
    #[serde(default, rename = "defaultPopulate")]
    pub default_populate: Option<Map<String, Value>>,
    /// Who may read, create, update and delete its documents; see `access`.
    #[serde(default)]
    pub access: Option<crate::access::CollectionAccess>,
    /// What editors call one row of a shared block, and several. Shared blocks only.
    #[serde(default)]
    pub labels: Option<Labels>,
    /// `data()`'s answer, worked out on first use. Schemas are built once and read many
    /// times; change `fields` only on a schema whose `data()` hasn't been asked for.
    #[serde(skip)]
    data: OnceLock<Vec<Field>>,
}

impl PartialEq for CollectionSchema {
    fn eq(&self, other: &Self) -> bool {
        (
            &self.slug,
            self.kind,
            &self.fields,
            &self.default_populate,
            &self.access,
            &self.labels,
        ) == (
            &other.slug,
            other.kind,
            &other.fields,
            &other.default_populate,
            &other.access,
            &other.labels,
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectionKind {
    #[default]
    Collection,
    Global,
    /// A block type that `blocks` fields share by naming it: `{"blockRef": "cta"}`. It has
    /// no documents of its own.
    Block,
}

impl CollectionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Collection => "collection",
            Self::Global => "global",
            Self::Block => "block",
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
    /// Read through a relationship instead of stored, such as `author.name`.
    #[serde(default, rename = "virtual")]
    pub virtual_path: Option<String>,
    /// Who may read and write this field; see `access`.
    #[serde(default)]
    pub access: Option<crate::access::FieldAccess>,
    #[serde(flatten)]
    pub kind: FieldKind,
}

/// A join's `where`: an object of conditions, or nothing.
fn object_or_none<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Map<String, Value>>, D::Error> {
    match Value::deserialize(d)? {
        Value::Null => Ok(None),
        Value::Object(o) => Ok(Some(o)),
        _ => Err(serde::de::Error::custom(
            "where must be an object like {\"draft\": {\"equals\": \"false\"}}",
        )),
    }
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
        /// Populate through this field at most this deep, whatever `depth` asks for.
        #[serde(default)]
        max_depth: Option<u8>,
    },
    Json,
    /// The documents of `collection` whose `on` relationship links here. Read, not stored.
    Join {
        collection: String,
        on: String,
        /// Always applied, on top of any filter a request adds.
        #[serde(default, deserialize_with = "object_or_none")]
        r#where: Option<Map<String, Value>>,
        #[serde(default)]
        default_limit: Option<u8>,
        #[serde(default)]
        default_sort: Option<String>,
    },
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
    /// An ordered list of rows, each one of several named field sets. A row is stored as
    /// `{"id": "<uuid>", "blockType": "<slug>", ...that block's fields}`.
    Blocks {
        blocks: Vec<BlockSource>,
        #[serde(default)]
        min_rows: Option<u32>,
        #[serde(default)]
        max_rows: Option<u32>,
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

/// One kind of row in a `blocks` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockDef {
    pub slug: String,
    #[serde(default)]
    pub labels: Option<Labels>,
    pub fields: Vec<Field>,
}

/// What editors call one row of a block, and several.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Labels {
    pub singular: String,
    pub plural: String,
}

/// A block type offered by a `blocks` field: defined in place, or a shared block named by
/// slug (`{"blockRef": "cta"}`). The API's schema cache swaps each reference for its
/// definition (`expand`), so code past it sees only definitions.
#[derive(Debug, Clone, PartialEq)]
pub enum BlockSource {
    Inline(BlockDef),
    Ref(String),
}

impl BlockSource {
    pub fn slug(&self) -> &str {
        match self {
            Self::Inline(b) => &b.slug,
            Self::Ref(slug) => slug,
        }
    }

    /// The definition. A reference left unexpanded has none, so every reader treats its
    /// rows as a type the schema doesn't know: refused on write, cut down on read.
    pub fn def(&self) -> Option<&BlockDef> {
        match self {
            Self::Inline(b) => Some(b),
            Self::Ref(_) => None,
        }
    }

    pub fn def_mut(&mut self) -> Option<&mut BlockDef> {
        match self {
            Self::Inline(b) => Some(b),
            Self::Ref(_) => None,
        }
    }
}

impl Serialize for BlockSource {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Inline(b) => b.serialize(s),
            Self::Ref(slug) => serde_json::json!({ "blockRef": slug }).serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for BlockSource {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        // Extra keys beside `blockRef` are left for `reject_unknown_options`, which names them.
        match v.get("blockRef") {
            Some(Value::String(slug)) => Ok(Self::Ref(slug.clone())),
            Some(_) => Err(serde::de::Error::custom(
                "blockRef must be a shared block's slug",
            )),
            None => BlockDef::deserialize(v)
                .map(Self::Inline)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// Keys every block row carries besides its block's fields.
pub const BLOCK_ROW_KEYS: &[&str] = &["id", "blockType"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tab {
    #[serde(default)]
    pub name: Option<String>,
    pub label: String,
    pub fields: Vec<Field>,
}

/// The fields as stored data sees them: rows, collapsibles and unnamed tabs dissolve into
/// their parent, and named tabs become groups. Applies at every level. Borrows when there's
/// no layout to dissolve, so calling it on fields that are already data costs nothing.
pub fn data_fields(fields: &[Field]) -> Cow<'_, [Field]> {
    fn has_layout(fields: &[Field]) -> bool {
        fields.iter().any(|f| match &f.kind {
            FieldKind::Group { fields } | FieldKind::Array { fields, .. } => has_layout(fields),
            FieldKind::Blocks { blocks, .. } => blocks
                .iter()
                .filter_map(BlockSource::def)
                .any(|b| has_layout(&b.fields)),
            kind => kind.is_layout(),
        })
    }
    if has_layout(fields) {
        Cow::Owned(flatten(fields))
    } else {
        Cow::Borrowed(fields)
    }
}

fn flatten(fields: &[Field]) -> Vec<Field> {
    let mut out = Vec::new();
    for f in fields {
        match &f.kind {
            FieldKind::Row { fields } | FieldKind::Collapsible { fields, .. } => {
                out.extend(flatten(fields))
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
                            virtual_path: None,
                            access: None,
                            kind: FieldKind::Group {
                                fields: flatten(&tab.fields),
                            },
                        }),
                        None => out.extend(flatten(&tab.fields)),
                    }
                }
            }
            FieldKind::Group { fields } => out.push(Field {
                kind: FieldKind::Group {
                    fields: flatten(fields),
                },
                ..f.clone()
            }),
            FieldKind::Array {
                fields,
                min_rows,
                max_rows,
            } => out.push(Field {
                kind: FieldKind::Array {
                    fields: flatten(fields),
                    min_rows: *min_rows,
                    max_rows: *max_rows,
                },
                ..f.clone()
            }),
            FieldKind::Blocks {
                blocks,
                min_rows,
                max_rows,
            } => out.push(Field {
                kind: FieldKind::Blocks {
                    blocks: blocks
                        .iter()
                        .map(|b| match b {
                            BlockSource::Inline(b) => BlockSource::Inline(BlockDef {
                                fields: flatten(&b.fields),
                                ..b.clone()
                            }),
                            r => r.clone(),
                        })
                        .collect(),
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

/// Drops keys that hold their default from a serialized field list: `null`, `false`, an
/// empty `name`, admin options left as they were. `default` values and options are data,
/// so they're kept whole.
fn compact_fields(fields: &mut Value) {
    let Value::Array(fields) = fields else { return };
    for field in fields {
        let Value::Object(f) = field else { continue };
        f.retain(|key, v| match key.as_str() {
            "default" | "options" => !v.is_null(),
            "name" => v.as_str().is_some_and(|n| !n.is_empty()),
            _ => !v.is_null() && *v != Value::Bool(false),
        });
        if let Some(Value::Object(admin)) = f.get_mut("admin") {
            admin.retain(|key, v| {
                !v.is_null() && *v != Value::Bool(false) && !(key == "position" && v == "main")
            });
            if admin.is_empty() {
                f.remove("admin");
            }
        }
        if let Some(inner) = f.get_mut("fields") {
            compact_fields(inner);
        }
        for key in ["tabs", "blocks"] {
            if let Some(Value::Array(parts)) = f.get_mut(key) {
                for part in parts {
                    if let Value::Object(t) = part {
                        t.retain(|_, v| !v.is_null());
                        if let Some(inner) = t.get_mut("fields") {
                            compact_fields(inner);
                        }
                    }
                }
            }
        }
    }
}

/// Admin options any field can carry, data or layout. `root` is the data shape; `inside`
/// holds the data paths the field itself covers, which its condition can't depend on;
/// `within` the block types it sits in (see `field_steps`).
fn check_admin(
    admin: &Admin,
    root: &[Field],
    inside: &[String],
    layout: bool,
    within: Within,
) -> Result<(), String> {
    if let Some(condition) = &admin.condition {
        let target = &condition.field;
        let Some(steps) = field_steps(root, target, "", within) else {
            return Err(format!(
                "admin.condition refers to `{target}`, which isn't a field"
            ));
        };
        let f = *steps.last().expect("a path has at least one step");
        // A field inside a list exists once per row, so only fields in that same row can
        // depend on it.
        let mut reach = String::new();
        for (step, segment) in steps.iter().zip(target.split('.')) {
            reach = join(&reach, segment);
            if matches!(
                step.kind,
                FieldKind::Array { .. } | FieldKind::Blocks { .. }
            ) && reach != *target
                && !inside.iter().any(|p| p.starts_with(&format!("{reach}.")))
            {
                return Err(format!(
                    "admin.condition can't reach into the list `{reach}` from outside it"
                ));
            }
        }
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
            (FieldKind::Array { .. } | FieldKind::Blocks { .. }, ConditionTest::Exists(_)) => {}
            (FieldKind::Array { .. } | FieldKind::Blocks { .. }, _) => {
                return Err(format!(
                    "admin.condition on the {} `{target}` can only test exists",
                    f.kind.name()
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
    // Digits with an optional fraction, like CSS writes them: no signs, exponents or bare dots.
    let (whole, fraction) = number.split_once('.').unwrap_or((number, "0"));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(whole) && digits(fraction) && number.parse::<f64>().is_ok_and(|v| v > 0.0 && v <= max)
}

/// A virtual field's own rules. Whether its targets have the field is `check_virtual_targets`.
fn check_virtual(f: &Field, path: &str, root: &[Field], in_list: bool) -> Result<(), String> {
    if in_list {
        return Err("virtual fields can't be inside a list yet".into());
    }
    if matches!(f.kind, FieldKind::Slug { .. }) {
        return Err("slug fields can't be virtual; a slug is stored so it can stay unique".into());
    }
    if matches!(
        f.kind,
        FieldKind::Group { .. }
            | FieldKind::Array { .. }
            | FieldKind::Blocks { .. }
            | FieldKind::Relationship { .. }
            | FieldKind::Join { .. }
    ) || f.kind.is_layout()
    {
        return Err(format!(
            "{} fields can't be virtual; a virtual field holds one value",
            f.kind.name()
        ));
    }
    if f.required || f.unique || f.filterable || f.default.is_some() {
        return Err(
            "virtual fields can't be required, unique, filterable or have a default".into(),
        );
    }
    if split_virtual(root, path).is_none() {
        return Err("virtual must name a relationship field, then a field in the linked document, like author.name".into());
    }
    Ok(())
}

/// Splits `meta.editor.name` into the relationship field (`meta.editor`, through groups) and
/// the path inside the linked document (`name`).
fn split_virtual<'a>(root: &'a [Field], path: &str) -> Option<(&'a Field, String, String)> {
    let segments: Vec<&str> = path.split('.').collect();
    let mut current = root;
    for (i, segment) in segments.iter().enumerate() {
        let f = current.iter().find(|f| f.name == *segment)?;
        match &f.kind {
            FieldKind::Group { fields } => current = fields,
            FieldKind::Relationship { .. } if i + 1 < segments.len() => {
                return Some((f, segments[..=i].join("."), segments[i + 1..].join(".")));
            }
            _ => return None,
        }
    }
    None
}

enum ReadError {
    Missing,
    InList,
}

/// The field a virtual reads, through groups only: a value inside a list has no one place.
fn read_field<'a>(fields: &'a [Field], path: &str) -> Result<&'a Field, ReadError> {
    let mut current = fields;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        let f = current
            .iter()
            .find(|f| f.name == segment)
            .ok_or(ReadError::Missing)?;
        if segments.peek().is_none() {
            return Ok(f);
        }
        current = match &f.kind {
            FieldKind::Group { fields } => fields,
            FieldKind::Array { .. } | FieldKind::Blocks { .. } => return Err(ReadError::InList),
            _ => return Err(ReadError::Missing),
        };
    }
    Err(ReadError::Missing)
}

/// Joins page by their own sort, so a filter whose results only make sense nearest-first
/// doesn't fit them yet.
pub const NEAR_IN_JOIN: &str =
    "joins can't be ordered by distance yet, so `near` isn't allowed in their where";

/// A join field: the documents of `collection` whose `on` relationship links here.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinField {
    /// Where the value goes, such as `posts` or `meta.posts`.
    pub path: String,
    pub name: String,
    pub collection: String,
    pub on: String,
    pub r#where: Option<Map<String, Value>>,
    pub limit: u8,
    pub sort: Option<String>,
}

/// A field whose value is read through a relationship.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualField {
    /// Where the value goes, such as `authorName` or `meta.editorNames`.
    pub path: String,
    /// The relationship field it reads through, such as `author`.
    pub link: String,
    /// The field in the linked document, such as `name` or `profile.city`.
    pub read: String,
    /// The link holds a list, so the value is a list too.
    pub many: bool,
}

/// Rules for layout fields themselves, on the raw tree. Data rules run on `data_fields`.
/// `loc` is where these fields sit in the JSON (`fields.0.fields`), which names a layout
/// field in errors since it has no name; `prefix` is their data path.
fn check_layout(
    fields: &[Field],
    root: &[Field],
    loc: &str,
    prefix: &str,
    within: Within,
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
            check_admin(&f.admin, root, &inside, true, within).map_err(invalid)?;
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
                check_layout(fields, root, &format!("{here}.fields"), prefix, within)?
            }
            FieldKind::Group { fields } | FieldKind::Array { fields, .. } => check_layout(
                fields,
                root,
                &format!("{here}.fields"),
                &join(prefix, &f.name),
                within,
            )?,
            FieldKind::Blocks { blocks, .. } => {
                let path = join(prefix, &f.name);
                for (b, block) in blocks.iter().enumerate() {
                    let Some(block) = block.def() else { continue };
                    check_layout(
                        &block.fields,
                        root,
                        &format!("{here}.blocks.{b}.fields"),
                        &path,
                        &enter(within, &path, &block.slug),
                    )?;
                }
            }
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
                        within,
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
            if let Some(Value::Array(blocks)) = item.get("blocks") {
                for (b, block) in blocks.iter().enumerate() {
                    if let Some(found) = locate(
                        block.get("fields"),
                        &format!("{here}.blocks.{b}.fields"),
                        inner_names,
                    ) {
                        return Some(found);
                    }
                }
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
    /// The collection's own `access`, by the rule at fault.
    fn locate_access(raw: &Value) -> Option<String> {
        let access = raw.get("access")?;
        for (action, rule) in access.as_object().into_iter().flatten() {
            if let Err(e) = crate::access::parse_rule(rule) {
                return Some(format!("access.{action}: {e}"));
            }
        }
        crate::access::CollectionAccess::deserialize(access)
            .err()
            .map(|e| format!("access: {e}"))
    }
    locate(raw.get("fields"), "fields", "")
        .or_else(|| locate_access(raw))
        .unwrap_or_else(|| e.to_string())
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
    #[error("{0}")]
    InvalidOption(String),
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
            Self::Join { .. } => "join",
            Self::Group { .. } => "group",
            Self::Array { .. } => "array",
            Self::Blocks { .. } => "blocks",
            Self::Row { .. } => "row",
            Self::Collapsible { .. } => "collapsible",
            Self::Tabs { .. } => "tabs",
        }
    }

    /// The block type a `blocks` field's row names in its `blockType`.
    pub fn block(&self, slug: &str) -> Option<&BlockDef> {
        match self {
            Self::Blocks { blocks, .. } => blocks
                .iter()
                .filter_map(BlockSource::def)
                .find(|b| b.slug == slug),
            _ => None,
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
                ..
            } => !has_many && !relation_to.is_polymorphic(),
            Self::Text { has_many, .. } | Self::Number { has_many, .. } => !has_many,
            Self::Json
            | Self::Join { .. }
            | Self::Group { .. }
            | Self::Array { .. }
            | Self::Blocks { .. }
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
    /// The data shape (see `data_fields`), worked out once per schema.
    pub fn data(&self) -> &[Field] {
        self.data
            .get_or_init(|| data_fields(&self.fields).into_owned())
    }

    /// This schema with other fields, such as only those a caller may read.
    pub fn with_fields(&self, fields: Vec<Field>) -> Self {
        Self {
            slug: self.slug.clone(),
            kind: self.kind,
            fields,
            default_populate: self.default_populate.clone(),
            access: self.access.clone(),
            labels: self.labels.clone(),
            data: OnceLock::new(),
        }
    }

    /// Every join field, with how to find its documents.
    pub fn join_fields(&self) -> Vec<JoinField> {
        fn walk(fields: &[Field], prefix: &str, out: &mut Vec<JoinField>) {
            for f in fields {
                let path = join(prefix, &f.name);
                match &f.kind {
                    FieldKind::Join {
                        collection,
                        on,
                        r#where,
                        default_limit,
                        default_sort,
                    } => out.push(JoinField {
                        path,
                        name: f.name.clone(),
                        collection: collection.clone(),
                        on: on.clone(),
                        r#where: r#where.clone(),
                        limit: default_limit.unwrap_or(10),
                        sort: default_sort.clone(),
                    }),
                    FieldKind::Group { fields } => walk(fields, &path, out),
                    _ => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(self.data(), "", &mut out);
        out
    }

    /// Checks each join's collection exists, its `on` field links here, and its default
    /// filter and sort fit that collection. `lookup` finds other collections.
    pub fn check_join_targets<S: std::borrow::Borrow<CollectionSchema>>(
        &self,
        lookup: impl Fn(&str) -> Option<S>,
    ) -> Result<(), SchemaError> {
        for j in self.join_fields() {
            let invalid = |message: String| SchemaError::InvalidField {
                field: j.name.clone(),
                message,
            };
            let found = if j.collection == self.slug {
                None
            } else {
                lookup(&j.collection)
            };
            let target: &CollectionSchema = match &found {
                Some(s) => s.borrow(),
                None if j.collection == self.slug => self,
                None => {
                    return Err(invalid(format!(
                        "collection `{}` doesn't exist",
                        j.collection
                    )));
                }
            };
            if target.kind != CollectionKind::Collection {
                return Err(invalid(format!(
                    "collection `{}` doesn't exist",
                    j.collection
                )));
            }
            let slug = &j.collection;
            // Inside blocks, several block types may have the path: any one linking here will do.
            let found = fields_at(target.data(), &j.on);
            let links_here = |f: &&Field| matches!(&f.kind, FieldKind::Relationship { relation_to, .. } if relation_to.allows(&self.slug));
            let Some(on) = found
                .iter()
                .copied()
                .find(links_here)
                .or(found.first().copied())
            else {
                return Err(invalid(format!("{slug} has no field `{}`", j.on)));
            };
            let FieldKind::Relationship { relation_to, .. } = &on.kind else {
                return Err(invalid(format!(
                    "{slug}.{} isn't a relationship field",
                    j.on
                )));
            };
            if !relation_to.allows(&self.slug) {
                return Err(invalid(format!(
                    "{slug}.{} links to {relation_to}, not {}",
                    j.on, self.slug
                )));
            }
            if let Some(w) = &j.r#where {
                // Globals can't be linked to, so a filter never reaches one.
                let reach = |slug: &str| {
                    if slug == self.slug {
                        Some(self.clone())
                    } else {
                        lookup(slug)
                            .map(|s| s.borrow().clone())
                            .filter(|s| s.kind == CollectionKind::Collection)
                    }
                };
                let filter = crate::query::parse_filter_with(
                    target.data(),
                    &Value::Object(w.clone()),
                    &reach,
                )
                .map_err(|e| invalid(e.to_string()))?;
                if filter.near().is_some() {
                    return Err(invalid(NEAR_IN_JOIN.into()));
                }
            }
            if let Some(sort) = &j.sort {
                crate::query::parse_sort(target.data(), sort)
                    .map_err(|e| invalid(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Every virtual field, with where it reads from.
    pub fn virtual_fields(&self) -> Vec<VirtualField> {
        fn walk(fields: &[Field], root: &[Field], prefix: &str, out: &mut Vec<VirtualField>) {
            for f in fields {
                let path = join(prefix, &f.name);
                if let Some(v) = &f.virtual_path
                    && let Some((link, link_path, read)) = split_virtual(root, v)
                {
                    out.push(VirtualField {
                        path,
                        link: link_path,
                        read,
                        many: matches!(link.kind, FieldKind::Relationship { has_many: true, .. }),
                    });
                } else if let FieldKind::Group { fields } = &f.kind {
                    walk(fields, root, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(self.data(), self.data(), "", &mut out);
        out
    }

    /// Checks every collection a virtual field can read from has the field it names, of the
    /// same type. `lookup` finds other collections; this one is found by itself.
    pub fn check_virtual_targets<S: std::borrow::Borrow<CollectionSchema>>(
        &self,
        lookup: impl Fn(&str) -> Option<S>,
    ) -> Result<(), SchemaError> {
        for v in self.virtual_fields() {
            let field = find_field(self.data(), &v.path).expect("virtual_fields found it");
            let invalid = |message: String| SchemaError::InvalidField {
                field: field.name.clone(),
                message,
            };
            let Some(FieldKind::Relationship { relation_to, .. }) =
                find_field(self.data(), &v.link).map(|f| &f.kind)
            else {
                continue;
            };
            for slug in relation_to.targets() {
                let found = if slug == self.slug {
                    None
                } else {
                    lookup(slug)
                };
                let target: &CollectionSchema = match &found {
                    Some(s) => s.borrow(),
                    None if slug == self.slug => self,
                    None => continue,
                };
                let read = read_field(target.data(), &v.read).map_err(|e| {
                    invalid(match e {
                        ReadError::Missing => format!("{slug} has no field `{}` to read", v.read),
                        ReadError::InList => {
                            format!("{slug}.{} is inside a list, so it can't be read", v.read)
                        }
                    })
                })?;
                if read.virtual_path.is_some() {
                    return Err(invalid(format!(
                        "{slug}.{} is itself virtual; read the field it reads",
                        v.read
                    )));
                }
                if !crate::diff::same_type(&read.kind, &field.kind) {
                    let list_of = match &read.kind {
                        FieldKind::Text { has_many: true, .. } => Some("text"),
                        FieldKind::Number { has_many: true, .. } => Some("numbers"),
                        FieldKind::Select { multiple: true, .. } => Some("select values"),
                        _ => None,
                    };
                    return Err(invalid(match list_of {
                        Some(what) => format!(
                            "{slug}.{} is a list of {what}, which a {} field can't hold",
                            v.read,
                            field.kind.name()
                        ),
                        None => format!(
                            "{slug}.{} is a {} field, but {} is a {} field",
                            v.read,
                            read.kind.name(),
                            field.name,
                            field.kind.name()
                        ),
                    }));
                }
            }
        }
        Ok(())
    }

    /// The schema as JSON without options left at their defaults: what people read and
    /// keep in files. Parses back to an equal schema.
    pub fn to_compact_json(&self) -> Value {
        let mut v = serde_json::to_value(self).expect("schemas always serialize");
        if let Some(fields) = v.get_mut("fields") {
            compact_fields(fields);
        }
        if let Value::Object(top) = &mut v {
            top.retain(|_, value| !value.is_null());
        }
        v
    }

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
                    FieldKind::Blocks { blocks, .. } => {
                        for block in blocks.iter().filter_map(BlockSource::def) {
                            walk(&block.fields, out);
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
        let data = self.data();
        if self.kind == CollectionKind::Block {
            if self.access.is_some() {
                return Err(SchemaError::InvalidOption(
                    "access: a shared block has no documents, so it takes no access rules; put rules on its fields".into(),
                ));
            }
            if self.default_populate.is_some() {
                return Err(SchemaError::InvalidOption(
                    "defaultPopulate: a shared block is never populated, so it can't have one"
                        .into(),
                ));
            }
            check_layout(&self.fields, data, "fields", "", &[])?;
            return check_fields(data, Level::BlockRow, data, "", true, &[]);
        }
        if self.labels.is_some() {
            return Err(SchemaError::InvalidOption(
                "labels: only shared blocks take labels".into(),
            ));
        }
        check_layout(&self.fields, data, "fields", "", &[])?;
        check_fields(data, Level::Top, data, "", false, &[])?;
        if self.kind == CollectionKind::Global
            && let Some(j) = self.join_fields().first()
        {
            return Err(SchemaError::InvalidField {
                field: j.name.clone(),
                message: "globals can't have join fields, since nothing links to a global".into(),
            });
        }
        if self.default_populate.is_some() && self.kind == CollectionKind::Global {
            return Err(SchemaError::InvalidOption(
                "defaultPopulate: globals are never populated, so they can't have one".into(),
            ));
        }
        if let Some(dp) = &self.default_populate {
            crate::select::parse_select(data, &Value::Object(dp.clone()), "defaultPopulate")
                .map_err(|e| SchemaError::InvalidOption(e.to_string()))?;
        }
        Ok(())
    }

    /// The shared blocks this schema's `blocks` fields name directly, each once, in schema
    /// order. Those name others in turn; `expand_placed` follows them.
    pub fn block_refs(&self) -> Vec<String> {
        // ponytail: walks a copy so one walker serves reading and rewriting; schemas are
        // small and this runs per save and per cache reload.
        let mut fields = self.fields.clone();
        let mut out: Vec<String> = Vec::new();
        visit_fields(&mut fields, "", "", &mut |f, _, _| {
            if let FieldKind::Blocks { blocks, .. } = &f.kind {
                for b in blocks {
                    if let BlockSource::Ref(slug) = b
                        && !out.contains(slug)
                    {
                        out.push(slug.clone());
                    }
                }
            }
            Ok(())
        })
        .expect("this visit never fails");
        out
    }

    /// This schema with every shared block reference replaced by the block's definition,
    /// which `lookup` finds, at any depth. See `expand_placed`.
    pub fn expand<S: Borrow<CollectionSchema>>(
        &self,
        lookup: impl Fn(&str) -> Option<S>,
    ) -> Result<CollectionSchema, SchemaError> {
        self.expand_placed(lookup).map(|(schema, _)| schema)
    }

    /// `expand`, with where each shared block went: the schema-change path of the blocks
    /// field naming it (`layout`, or `layout.section.items` through another shared block)
    /// and its slug. Refused: a reference to anything but a shared block, shared blocks
    /// naming each other in a loop, and nesting deeper than `MAX_BLOCK_NESTING`.
    ///
    /// A shared block's conditions name fields from the block's own top (`style`); placed,
    /// they're prefixed with the data path of the blocks field (`layout.style`), the form
    /// inline blocks use, so the expanded schema reads like an inline one.
    pub fn expand_placed<S: Borrow<CollectionSchema>>(
        &self,
        lookup: impl Fn(&str) -> Option<S>,
    ) -> Result<(CollectionSchema, Vec<(String, String)>), SchemaError> {
        let mut fields = self.fields.clone();
        let mut placed = Vec::new();
        // A shared block starts its own chain, so naming itself is a loop.
        let mut chain = if self.kind == CollectionKind::Block {
            vec![self.slug.clone()]
        } else {
            Vec::new()
        };
        expand_fields(&mut fields, "", &lookup, &mut chain, &mut placed)?;
        Ok((self.with_fields(fields), placed))
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
            FieldKind::Blocks { blocks, .. } => {
                if let (Some(Value::Array(raw_blocks)), Some(Value::Array(known_blocks))) =
                    (raw.get("blocks"), known.get("blocks"))
                {
                    for (b, ((block, raw_block), known_block)) in
                        blocks.iter().zip(raw_blocks).zip(known_blocks).enumerate()
                    {
                        for (part, r, k) in [
                            ("", Some(raw_block), Some(known_block)),
                            (
                                "labels.",
                                raw_block.get("labels"),
                                known_block.get("labels"),
                            ),
                        ] {
                            if let (Some(Value::Object(r)), Some(Value::Object(k))) = (r, k)
                                && let Some(option) = r.keys().find(|key| !k.contains_key(*key))
                            {
                                return Err(SchemaError::UnknownOption {
                                    field: label.clone(),
                                    option: format!("blocks.{b}.{part}{option}"),
                                });
                            }
                        }
                        if let Some(block) = block.def() {
                            unknown_in_fields(
                                &block.fields,
                                raw_block.get("fields"),
                                &format!("{here}.blocks.{b}.fields"),
                            )?;
                        }
                    }
                }
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

/// How deep shared blocks may name each other: a page's `section` naming a `cta` is 2.
pub const MAX_BLOCK_NESTING: usize = 8;

/// The one walk over a raw field tree. Calls `visit` on every field at any depth (inside
/// groups, lists, layout fields and inline block types, not references) with the data path
/// of the list it sits in (`layout`, block types not named) and its schema-change path
/// (`layout.hero`, see `diff::Renames`). `visit` runs before a field's insides are walked,
/// so it may replace them.
type Visit<'v> = dyn FnMut(&mut Field, &str, &str) -> Result<(), SchemaError> + 'v;

fn visit_fields(
    fields: &mut [Field],
    data: &str,
    change: &str,
    visit: &mut Visit,
) -> Result<(), SchemaError> {
    for f in fields {
        visit(f, data, change)?;
        let (inner_data, inner_change) = (join(data, &f.name), join(change, &f.name));
        match &mut f.kind {
            FieldKind::Group { fields } | FieldKind::Array { fields, .. } => {
                visit_fields(fields, &inner_data, &inner_change, visit)?
            }
            FieldKind::Row { fields } | FieldKind::Collapsible { fields, .. } => {
                visit_fields(fields, data, change, visit)?
            }
            FieldKind::Tabs { tabs } => {
                for tab in tabs {
                    let (d, c) = match &tab.name {
                        Some(name) => (join(data, name), join(change, name)),
                        None => (data.to_owned(), change.to_owned()),
                    };
                    visit_fields(&mut tab.fields, &d, &c, visit)?;
                }
            }
            FieldKind::Blocks { blocks, .. } => {
                for b in blocks.iter_mut().filter_map(BlockSource::def_mut) {
                    let c = join(&inner_change, &b.slug);
                    visit_fields(&mut b.fields, &inner_data, &c, visit)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Replaces each reference in `fields` (under schema-change path `change`) with its
/// definition, expanded in turn. `chain` holds the shared blocks being expanded, outermost
/// first.
fn expand_fields<S: Borrow<CollectionSchema>>(
    fields: &mut [Field],
    change: &str,
    lookup: &impl Fn(&str) -> Option<S>,
    chain: &mut Vec<String>,
    placed: &mut Vec<(String, String)>,
) -> Result<(), SchemaError> {
    visit_fields(fields, "", change, &mut |f, data, change| {
        let (data, change) = (join(data, &f.name), join(change, &f.name));
        let name = f.name.clone();
        let FieldKind::Blocks { blocks, .. } = &mut f.kind else {
            return Ok(());
        };
        for b in blocks.iter_mut() {
            let BlockSource::Ref(slug) = b else { continue };
            let slug = slug.clone();
            let invalid = |message: String| SchemaError::InvalidField {
                field: name.clone(),
                message,
            };
            let path = |chain: &[String]| {
                let mut all = chain.to_vec();
                all.push(slug.clone());
                all.join(" → ")
            };
            if let Some(start) = chain.iter().position(|c| *c == slug) {
                return Err(invalid(format!(
                    "shared blocks can't name each other in a loop: {}",
                    path(&chain[start..])
                )));
            }
            if chain.len() >= MAX_BLOCK_NESTING {
                return Err(invalid(format!(
                    "shared blocks can nest at most {MAX_BLOCK_NESTING} deep: {}",
                    path(chain)
                )));
            }
            let found = lookup(&slug)
                .filter(|s| s.borrow().kind == CollectionKind::Block)
                .ok_or_else(|| {
                    invalid(format!("blockRef `{slug}` doesn't match a shared block"))
                })?;
            let found = found.borrow();
            let mut inner = found.fields.clone();
            chain.push(slug.clone());
            expand_fields(&mut inner, &join(&change, &slug), lookup, chain, placed)?;
            chain.pop();
            visit_fields(&mut inner, "", "", &mut |f, _, _| {
                if let Some(c) = &mut f.admin.condition {
                    c.field = join(&data, &c.field);
                }
                Ok(())
            })?;
            placed.push((change.clone(), slug.clone()));
            *b = BlockSource::Inline(BlockDef {
                slug,
                labels: found.labels.clone(),
                fields: inner,
            });
        }
        Ok(())
    })
}

/// The block types a field sits in: each enclosing `blocks` field's data path, with the
/// slug of the block type around it.
type Within<'a> = &'a [(String, String)];

/// `within` one level deeper: inside block type `slug` of the blocks field at `path`.
fn enter(within: Within, path: &str, slug: &str) -> Vec<(String, String)> {
    let mut out = within.to_vec();
    out.push((path.to_owned(), slug.to_owned()));
    out
}

/// The field at a dotted path, looking through groups, arrays and blocks. A path into
/// blocks doesn't name the block type (`layout.heading`), so it finds the first block
/// type, in schema order, that has the whole rest of the path.
pub(crate) fn find_field<'a>(fields: &'a [Field], path: &str) -> Option<&'a Field> {
    field_steps(fields, path, "", &[]).and_then(|steps| steps.last().copied())
}

/// Every field at a dotted path: one, or one per block type that has the path, in schema
/// order.
fn fields_at<'a>(fields: &'a [Field], path: &str) -> Vec<&'a Field> {
    let (head, rest) = path
        .split_once('.')
        .map_or((path, None), |(h, r)| (h, Some(r)));
    let Some(f) = fields.iter().find(|f| f.name == head) else {
        return Vec::new();
    };
    match (&f.kind, rest) {
        (_, None) => vec![f],
        (FieldKind::Group { fields } | FieldKind::Array { fields, .. }, Some(rest)) => {
            fields_at(fields, rest)
        }
        (FieldKind::Blocks { blocks, .. }, Some(rest)) => blocks
            .iter()
            .filter_map(BlockSource::def)
            .flat_map(|b| fields_at(&b.fields, rest))
            .collect(),
        _ => Vec::new(),
    }
}

/// Every field along `path` (under `prefix`), outermost first. Inside a blocks field listed
/// in `within` only that block type is searched, so a field inside a block sees its own
/// block type and what's above it; elsewhere every block type is.
fn field_steps<'a>(
    fields: &'a [Field],
    path: &str,
    prefix: &str,
    within: Within,
) -> Option<Vec<&'a Field>> {
    let (head, rest) = path
        .split_once('.')
        .map_or((path, None), |(h, r)| (h, Some(r)));
    let f = fields.iter().find(|f| f.name == head)?;
    let Some(rest) = rest else {
        return Some(vec![f]);
    };
    let here = join(prefix, head);
    let mut steps = match &f.kind {
        FieldKind::Group { fields } | FieldKind::Array { fields, .. } => {
            field_steps(fields, rest, &here, within)?
        }
        FieldKind::Blocks { blocks, .. } => {
            let only = within.iter().find(|(p, _)| *p == here).map(|(_, s)| s);
            blocks
                .iter()
                .filter_map(BlockSource::def)
                .filter(|b| only.is_none_or(|slug| *slug == b.slug))
                .find_map(|b| field_steps(&b.fields, rest, &here, within))?
        }
        _ => return None,
    };
    steps.insert(0, f);
    Some(steps)
}

fn is_identifier(s: &str, allow_upper: bool) -> bool {
    let ok = |c: char| c.is_ascii_lowercase() || (allow_upper && c.is_ascii_uppercase());
    let mut chars = s.chars();
    s.len() <= 64
        && chars.next().is_some_and(ok)
        && chars.all(|c| ok(c) || c.is_ascii_digit() || c == '_')
}

/// Where a list of fields sits, for the names it can't use.
#[derive(Clone, Copy, PartialEq)]
enum Level {
    Top,
    Nested,
    BlockRow,
}

fn check_fields(
    fields: &[Field],
    level: Level,
    root: &[Field],
    prefix: &str,
    in_list: bool,
    within: Within,
) -> Result<(), SchemaError> {
    let top_level = level == Level::Top;
    let mut seen = HashSet::new();
    for f in fields {
        if !is_identifier(&f.name, true) {
            return Err(SchemaError::InvalidFieldName(f.name.clone()));
        }
        let reserved = match level {
            Level::Top => RESERVED_FIELD_NAMES,
            Level::Nested => &[],
            Level::BlockRow => BLOCK_ROW_KEYS,
        };
        if reserved.contains(&f.name.as_str()) {
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
            FieldKind::Blocks {
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
            FieldKind::Relationship {
                max_depth: Some(d), ..
            } if *d > 5 => {
                return Err(invalid("maxDepth can be at most 5".into()));
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
            FieldKind::Group { fields } => check_fields(
                fields,
                Level::Nested,
                root,
                &join(prefix, &f.name),
                in_list,
                within,
            )?,
            FieldKind::Array { fields, .. } => check_fields(
                fields,
                Level::Nested,
                root,
                &join(prefix, &f.name),
                true,
                within,
            )?,
            FieldKind::Blocks { blocks, .. } => {
                if blocks.is_empty() {
                    return Err(invalid("blocks needs at least one block type".into()));
                }
                let mut slugs = HashSet::new();
                for block in blocks {
                    let slug = block.slug();
                    if !is_identifier(slug, false) {
                        return Err(invalid(format!(
                            "block slug `{slug}` must start with a lowercase letter and use only lowercase letters, digits and underscores (64 at most)"
                        )));
                    }
                    if !slugs.insert(slug) {
                        return Err(invalid(format!("block `{slug}` is defined more than once")));
                    }
                    // A shared block was checked when it was saved; `expand` checks it fits.
                    let Some(block) = block.def() else { continue };
                    let path = join(prefix, &f.name);
                    check_fields(
                        &block.fields,
                        Level::BlockRow,
                        root,
                        &path,
                        true,
                        &enter(within, &path, &block.slug),
                    )?;
                }
            }
            _ => {}
        }
        if let FieldKind::Join {
            collection,
            on,
            default_limit,
            ..
        } = &f.kind
        {
            if in_list {
                return Err(invalid("join fields can't be inside a list".into()));
            }
            if f.required
                || f.unique
                || f.filterable
                || f.default.is_some()
                || f.virtual_path.is_some()
            {
                return Err(invalid(
                    "join fields can't be required, unique, filterable, virtual or have a default"
                        .into(),
                ));
            }
            if default_limit.is_some_and(|l| !(1..=100).contains(&l)) {
                return Err(invalid("defaultLimit must be between 1 and 100".into()));
            }
            if !is_identifier(collection, false) {
                return Err(invalid(format!(
                    "collection `{collection}` isn't a valid collection slug"
                )));
            }
            if !on.split('.').all(|s| is_identifier(s, true)) {
                return Err(invalid(format!(
                    "on `{on}` must be a field path like category or links.target"
                )));
            }
        }
        if let Some(path) = &f.virtual_path {
            check_virtual(f, path, root, in_list).map_err(invalid)?;
        }
        check_admin(&f.admin, root, &[join(prefix, &f.name)], false, within).map_err(invalid)?;
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
    fn shared_blocks_nest_with_labels_and_conditions() {
        let block = |slug: &str, fields: Value| {
            schema(json!({"slug": slug, "kind": "block", "fields": fields}))
        };
        // Labels survive a round trip; conditions name the block's own fields.
        let cta = schema(json!({"slug": "cta", "kind": "block",
        "labels": {"singular": "Call", "plural": "Calls"},
        "fields": [
            {"name": "style", "type": "select", "options": ["link", "button"]},
            {"name": "url", "type": "text", "admin": {"condition": {"field": "style", "equals": "link"}}}
        ]}));
        assert_eq!(cta.check(), Ok(()));
        assert_eq!(
            cta.to_compact_json()["labels"],
            json!({"singular": "Call", "plural": "Calls"})
        );
        let section = block(
            "section",
            json!([{"name": "items", "type": "blocks", "blocks": [{"blockRef": "cta"}]}]),
        );
        let page = schema(json!({"slug": "pages", "fields": [
            {"name": "layout", "type": "blocks", "blocks": [{"blockRef": "section"}]}
        ]}));
        let all = [cta.clone(), section.clone(), page.clone()];
        let find = |slug: &str| all.iter().find(|s| s.slug == slug);
        let (expanded, placed) = page.expand_placed(find).unwrap();
        assert_eq!(
            placed,
            [
                ("layout.section.items".to_owned(), "cta".to_owned()),
                ("layout".to_owned(), "section".to_owned())
            ]
        );
        let inner = expanded.data()[0].kind.block("section").unwrap().fields[0]
            .kind
            .block("cta")
            .unwrap();
        assert_eq!(inner.labels.as_ref().unwrap().singular, "Call");
        assert_eq!(
            inner.fields[1].admin.condition.as_ref().unwrap().field,
            "layout.items.style"
        );
        assert_eq!(expanded.check(), Ok(()));
        // The expanded block checks too, conditions and all.
        assert_eq!(section.expand(find).unwrap().check(), Ok(()));

        // A loop is refused, naming it.
        let looped = block(
            "cta",
            json!([{"name": "more", "type": "blocks", "blocks": [{"blockRef": "section"}]}]),
        );
        let with_loop = [looped.clone(), section.clone()];
        let find = |slug: &str| with_loop.iter().find(|s| s.slug == slug);
        assert_eq!(
            looped.expand(find).unwrap_err().to_string(),
            "field `items`: shared blocks can't name each other in a loop: cta → section → cta"
        );
        assert_eq!(
            page.expand(find).unwrap_err().to_string(),
            "field `more`: shared blocks can't name each other in a loop: section → cta → section"
        );
        // So is nesting deeper than eight.
        let chain: Vec<CollectionSchema> = (0..10)
            .map(|i| {
                block(
                    &format!("b{i}"),
                    json!([{"name": "l", "type": "blocks", "blocks": [{"blockRef": format!("b{}", i + 1)}]}]),
                )
            })
            .chain([block("b10", json!([]))])
            .collect();
        let find = |slug: &str| chain.iter().find(|s| s.slug == slug);
        assert!(chain[3].expand(find).is_ok());
        assert_eq!(
            chain[0].expand(find).unwrap_err().to_string(),
            "field `l`: shared blocks can nest at most 8 deep: b0 → b1 → b2 → b3 → b4 → b5 → b6 → b7 → b8"
        );
    }

    #[test]
    fn shared_blocks_check_and_expand() {
        let cta = schema(json!({"slug": "cta", "kind": "block", "fields": [
            {"name": "label", "type": "text", "required": true}
        ]}));
        assert_eq!(cta.check(), Ok(()));
        let pages = schema(json!({"slug": "pages", "fields": [
            {"name": "layout", "type": "blocks", "blocks": [
                {"blockRef": "cta"}, {"slug": "quote", "fields": [{"name": "text", "type": "text"}]}
            ]},
            {"type": "tabs", "tabs": [{"label": "More", "fields": [
                {"name": "more", "type": "blocks", "blocks": [{"blockRef": "cta"}, {"blockRef": "hero"}]}
            ]}]}
        ]}));
        assert_eq!(pages.block_refs(), ["cta", "hero"]);
        // The reference survives a round trip.
        assert_eq!(
            pages.to_compact_json()["fields"][0]["blocks"][0],
            json!({"blockRef": "cta"})
        );
        let lookup = |slug: &str| (slug == "cta").then(|| cta.clone());
        assert_eq!(
            pages.expand(lookup).unwrap_err().to_string(),
            "field `more`: blockRef `hero` doesn't match a shared block"
        );
        let hero = schema(json!({"slug": "hero", "kind": "block", "fields": []}));
        let posts = schema(json!({"slug": "posts", "fields": []}));
        let found = |slug: &str| match slug {
            "cta" => Some(cta.clone()),
            "hero" => Some(hero.clone()),
            _ => Some(posts.clone()),
        };
        let (expanded, placed) = pages.expand_placed(found).unwrap();
        let placed: Vec<(&str, &str)> = placed
            .iter()
            .map(|(p, s)| (p.as_str(), s.as_str()))
            .collect();
        assert_eq!(
            placed,
            [("layout", "cta"), ("more", "cta"), ("more", "hero")]
        );
        assert!(expanded.block_refs().is_empty());
        assert_eq!(expanded.check(), Ok(()));
        let layout = &expanded.data()[0].kind;
        assert!(layout.block("cta").unwrap().fields[0].required);
        assert!(layout.block("quote").is_some());
        // Only shared blocks can be named.
        let mut wrong = pages.clone();
        wrong.fields.truncate(1);
        if let FieldKind::Blocks { blocks, .. } = &mut wrong.fields[0].kind {
            blocks[0] = BlockSource::Ref("posts".into());
        }
        assert!(wrong.expand(found).is_err());
        // A reference and an inline block can't share a slug.
        let twice = schema(
            json!({"slug": "p", "fields": [{"name": "l", "type": "blocks", "blocks": [
                {"blockRef": "cta"}, {"slug": "cta", "fields": []}
            ]}]}),
        );
        assert!(twice.check().is_err());

        for (bad, message) in [
            (
                json!({"slug": "b", "kind": "block", "access": {"read": true}, "fields": []}),
                "access: a shared block has no documents, so it takes no access rules; put rules on its fields",
            ),
            (
                json!({"slug": "b", "kind": "block", "fields": [{"name": "id", "type": "text"}]}),
                "field name `id` is reserved",
            ),
            (
                json!({"slug": "b", "kind": "block", "fields": [{"name": "t", "type": "text", "unique": true}]}),
                "field `t`: unique, filterable and slug fields must be at the top level",
            ),
            (
                json!({"slug": "posts", "labels": {"singular": "Post", "plural": "Posts"}, "fields": []}),
                "labels: only shared blocks take labels",
            ),
            (
                json!({"slug": "blocks", "kind": "block", "fields": []}),
                "collection slug `blocks` is reserved",
            ),
        ] {
            assert_eq!(schema(bad).check().unwrap_err().to_string(), message);
        }
        let extra = json!({"slug": "p", "fields": [{"name": "l", "type": "blocks", "blocks": [
            {"blockRef": "cta", "slug": "x"}
        ]}]});
        assert_eq!(
            schema(extra.clone())
                .reject_unknown_options(&extra)
                .unwrap_err()
                .to_string(),
            "field `l`: unknown option `blocks.0.slug`"
        );
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
        // `/api/access` answers what the caller may do, for collections and globals alike.
        for kind in ["collection", "global"] {
            let s = schema(json!({"slug": "access", "kind": kind, "fields": []}));
            assert_eq!(s.check(), Err(SchemaError::ReservedSlug("access".into())));
        }
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
        let top = json!({"slug": "posts", "lables": {}, "fields": []});
        assert_eq!(
            schema(top.clone()).reject_unknown_options(&top),
            Err(SchemaError::UnknownOption {
                field: String::new(),
                option: "lables".into()
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
                has_many: false,
                max_depth: None,
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
            ("50.px", false),
            ("1e2px", false),
            ("+50%", false),
            (".5%", false),
            ("12.5px", true),
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
            (
                json!([{"name": "links", "type": "array", "fields": [{"name": "url", "type": "url"}]},
                       {"name": "t", "type": "text", "admin": {"condition": {"field": "links.url", "exists": true}}}]),
                "field `t`: admin.condition can't reach into the list `links` from outside it",
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
                ]},
                {"name": "rows", "type": "array", "fields": [
                    {"name": "kind", "type": "text"},
                    {"name": "extra", "type": "text", "admin": {"condition": {"field": "rows.kind", "equals": "x"}}}
                ]}
            ])),
            "ok"
        );
    }

    #[test]
    fn data_fields_borrow_when_nothing_dissolves() {
        let plain = schema(json!({"slug": "posts", "fields": [
            {"name": "seo", "type": "group", "fields": [{"name": "title", "type": "text"}]}
        ]}));
        assert!(matches!(
            data_fields(&plain.fields),
            std::borrow::Cow::Borrowed(_)
        ));
        let nested_row = schema(json!({"slug": "posts", "fields": [
            {"name": "seo", "type": "group", "fields": [
                {"type": "row", "fields": [{"name": "title", "type": "text"}]}
            ]}
        ]}));
        let flat = data_fields(&nested_row.fields);
        assert!(matches!(flat, std::borrow::Cow::Owned(_)));
        assert!(matches!(data_fields(&flat), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn a_schemas_data_shape_is_worked_out_once() {
        let s = schema(json!({"slug": "posts", "fields": [
            {"type": "row", "fields": [{"name": "title", "type": "text"}]}
        ]}));
        assert!(std::ptr::eq(s.data(), s.data()));
        assert_eq!(s.data()[0].name, "title");
        assert_eq!(s, s.clone(), "the cache doesn't affect equality");
        let fresh = schema(json!({"slug": "posts", "fields": [
            {"type": "row", "fields": [{"name": "title", "type": "text"}]}
        ]}));
        assert_eq!(s, fresh);
    }

    #[test]
    fn compact_json_drops_defaults_and_round_trips() {
        let input = json!({"slug": "posts", "fields": [
            {"type": "row", "fields": [
                {"name": "title", "type": "text", "required": true, "maxLength": 80},
                {"name": "kind", "type": "select", "options": ["a", {"label": "B", "value": "b"}]}
            ]},
            {"name": "body", "type": "textarea", "admin": {"description": "Main text"}},
            {"name": "meta", "type": "json", "default": {"x": null, "y": false}}
        ]});
        let s = schema(input);
        let compact = s.to_compact_json();
        assert_eq!(
            compact,
            json!({"slug": "posts", "kind": "collection", "fields": [
                {"type": "row", "fields": [
                    {"name": "title", "type": "text", "required": true, "maxLength": 80},
                    {"name": "kind", "type": "select", "options": ["a", {"label": "B", "value": "b"}]}
                ]},
                {"name": "body", "type": "textarea", "admin": {"description": "Main text"}},
                {"name": "meta", "type": "json", "default": {"x": null, "y": false}}
            ]})
        );
        assert_eq!(schema(compact.clone()), s);
        assert_eq!(s.reject_unknown_options(&compact), Ok(()));
    }

    #[test]
    fn default_populate_is_checked_and_round_trips() {
        let input = json!({"slug": "authors", "defaultPopulate": {"name": true, "id": true},
                           "fields": [{"name": "name", "type": "text"}, {"name": "bio", "type": "textarea"}]});
        let s = schema(input.clone());
        assert_eq!(s.check(), Ok(()));
        assert_eq!(s.reject_unknown_options(&input), Ok(()));
        assert_eq!(
            s.to_compact_json()["defaultPopulate"],
            json!({"name": true, "id": true})
        );
        let bad = schema(json!({"slug": "authors", "defaultPopulate": {"nope": true},
                                "fields": [{"name": "name", "type": "text"}]}));
        assert_eq!(
            bad.check().unwrap_err().to_string(),
            "defaultPopulate[nope]: unknown field"
        );
        let plain = schema(json!({"slug": "authors", "fields": []}));
        assert!(plain.to_compact_json().get("defaultPopulate").is_none());
    }

    fn blocks_schema() -> Value {
        json!({"slug": "pages", "fields": [
            {"name": "title", "type": "text"},
            {"name": "layout", "type": "blocks", "minRows": 1, "maxRows": 3, "blocks": [
                {"slug": "hero", "labels": {"singular": "Hero", "plural": "Heroes"}, "fields": [
                    {"name": "heading", "type": "text", "required": true},
                    {"type": "row", "fields": [
                        {"name": "tagline", "type": "text",
                         "admin": {"condition": {"field": "layout.heading", "exists": true}}},
                        {"name": "dark", "type": "boolean", "default": false}
                    ]}
                ]},
                {"slug": "quote", "fields": [
                    {"name": "text", "type": "textarea"},
                    {"name": "author", "type": "relationship", "relationTo": "authors"}
                ]}
            ]}
        ]})
    }

    #[test]
    fn blocks_parse_and_check() {
        let input = blocks_schema();
        let s = schema(input.clone());
        assert_eq!(s.check(), Ok(()));
        assert_eq!(s.reject_unknown_options(&input), Ok(()));
        assert_eq!(schema(s.to_compact_json()), s, "compact JSON round trips");
        assert_eq!(s.relation_targets(), ["authors"]);

        // Layout inside a block dissolves, block by block.
        let layout = &s.data()[1];
        let hero = layout.kind.block("hero").expect("hero is a block");
        let names: Vec<&str> = hero.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["heading", "tagline", "dark"]);
        assert_eq!(
            hero.labels,
            Some(Labels {
                singular: "Hero".into(),
                plural: "Heroes".into()
            })
        );
        assert!(layout.kind.block("nope").is_none());
        assert_eq!(
            find_field(s.data(), "layout.author").map(|f| f.kind.name()),
            Some("relationship")
        );

        let with_layout = |blocks: Value| {
            let mut v = blocks_schema();
            v["fields"][1]["blocks"] = blocks;
            check_err(v["fields"].clone())
        };
        let cases = [
            (
                json!([{"slug": "hero", "fields": []}, {"slug": "hero", "fields": []}]),
                "field `layout`: block `hero` is defined more than once",
            ),
            (
                json!([{"slug": "hero", "fields": [{"name": "blockType", "type": "text"}]}]),
                "field name `blockType` is reserved",
            ),
            (
                json!([{"slug": "hero", "fields": [{"name": "id", "type": "text"}]}]),
                "field name `id` is reserved",
            ),
            (
                json!([{"slug": "Hero!", "fields": []}]),
                "field `layout`: block slug `Hero!` must start with a lowercase letter and use only lowercase letters, digits and underscores (64 at most)",
            ),
            (
                json!([]),
                "field `layout`: blocks needs at least one block type",
            ),
            (
                json!([{"slug": "hero", "fields": [
                    {"name": "t", "type": "text"}, {"name": "t", "type": "text"}
                ]}]),
                "field `t` is defined more than once",
            ),
            (
                json!([{"slug": "hero", "fields": [{"name": "t", "type": "text", "unique": true}]}]),
                "field `t`: unique, filterable and slug fields must be at the top level",
            ),
            (
                json!([{"slug": "hero", "fields": [
                    {"name": "a", "type": "text"},
                    {"type": "row", "name": "oops", "fields": []}
                ]}]),
                "field `fields.1.blocks.0.fields.1`: row fields don't take a name",
            ),
        ];
        for (blocks, message) in cases {
            assert_eq!(with_layout(blocks), message);
        }
        // The same name in two block types is fine: they never share a row.
        assert_eq!(
            with_layout(json!([
                {"slug": "a", "fields": [{"name": "t", "type": "text"}]},
                {"slug": "b", "fields": [{"name": "t", "type": "number"}]}
            ])),
            "ok"
        );

        let mut rows = blocks_schema();
        rows["fields"][1]["minRows"] = json!(4);
        assert_eq!(
            check_err(rows["fields"].clone()),
            "field `layout`: minRows is greater than maxRows"
        );
        let mut outside = blocks_schema();
        outside["fields"][0]["admin"] =
            json!({"condition": {"field": "layout.heading", "exists": true}});
        assert_eq!(
            check_err(outside["fields"].clone()),
            "field `title`: admin.condition can't reach into the list `layout` from outside it"
        );
        // A condition inside a block sees its own block type and what's above it, never
        // another block type's fields.
        let mut other_type = blocks_schema();
        other_type["fields"][1]["blocks"][0]["fields"][0]["admin"] =
            json!({"condition": {"field": "layout.text", "exists": true}});
        assert_eq!(
            check_err(other_type["fields"].clone()),
            "field `heading`: admin.condition refers to `layout.text`, which isn't a field"
        );
        let mut above = blocks_schema();
        above["fields"][1]["blocks"][1]["fields"][0]["admin"] =
            json!({"condition": {"field": "title", "equals": "x"}});
        assert_eq!(check_err(above["fields"].clone()), "ok");
        // Across block types, a path finds the first type that has all of it.
        let shared = schema(
            json!({"slug": "pages", "fields": [{"name": "layout", "type": "blocks", "blocks": [
                {"slug": "a", "fields": [{"name": "cta", "type": "text"}]},
                {"slug": "b", "fields": [{"name": "cta", "type": "group", "fields": [{"name": "label", "type": "text"}]}]}
            ]}]}),
        );
        assert_eq!(
            find_field(shared.data(), "layout.cta.label").map(|f| f.kind.name()),
            Some("text")
        );

        for (raw, field, option) in [
            (
                json!({"slug": "hero", "icon": "x", "fields": []}),
                "layout",
                "blocks.0.icon",
            ),
            (
                json!({"slug": "hero", "labels": {"singular": "H", "plural": "Hs", "x": 1}, "fields": []}),
                "layout",
                "blocks.0.labels.x",
            ),
            (
                json!({"slug": "hero", "fields": [{"name": "t", "type": "text", "maxLenght": 3}]}),
                "t",
                "maxLenght",
            ),
        ] {
            let mut input = blocks_schema();
            input["fields"][1]["blocks"] = json!([raw]);
            assert_eq!(
                schema(input.clone()).reject_unknown_options(&input),
                Err(SchemaError::UnknownOption {
                    field: field.into(),
                    option: option.into()
                })
            );
        }
        let raw = json!({"slug": "pages", "fields": [{"name": "layout", "type": "blocks", "blocks": [
            {"slug": "hero", "fields": [{"name": "kind", "type": "select", "options": [1]}]}
        ]}]});
        let e = CollectionSchema::deserialize(&raw).unwrap_err();
        assert!(
            explain_parse_error(&raw, &e).starts_with("field `layout.kind`: "),
            "{}",
            explain_parse_error(&raw, &e)
        );
    }

    #[test]
    fn max_depth_is_checked_and_parsed() {
        let s = schema(json!({"slug": "posts", "fields": [
            {"name": "author", "type": "relationship", "relationTo": "authors", "maxDepth": 1}
        ]}));
        assert_eq!(s.check(), Ok(()));
        let FieldKind::Relationship { max_depth, .. } = &s.fields[0].kind else {
            panic!()
        };
        assert_eq!(*max_depth, Some(1));
        let deep = schema(json!({"slug": "posts", "fields": [
            {"name": "author", "type": "relationship", "relationTo": "authors", "maxDepth": 6}
        ]}));
        assert_eq!(
            deep.check().unwrap_err().to_string(),
            "field `author`: maxDepth can be at most 5"
        );
    }
}
