//! Query strings, filters, sorts and cursors. Everything here is validated against the
//! collection schema, so the database layer only ever sees well-typed conditions.

use std::fmt;

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::schema::{BlockSource, Field, FieldKind, SelectOption, data_fields};
use crate::validate::{is_date, normalize_date_time};

const MAX_PARAMS: usize = 100;
const MAX_KEY_DEPTH: usize = 16;
const MAX_CONDITIONS: usize = 50;
const MAX_DEPTH: usize = 5;
const MAX_LIST: usize = 100;

/// Parses `a[b][c]=1&d=2` into nested objects. Every leaf stays a string.
pub fn parse_query(raw: &str) -> Result<Map<String, Value>, String> {
    let mut root = Map::new();
    for (i, (key, value)) in url::form_urlencoded::parse(raw.as_bytes()).enumerate() {
        if i >= MAX_PARAMS {
            return Err(format!("too many query parameters (max {MAX_PARAMS})"));
        }
        // Postgres can't store or compare U+0000, so stop it here rather than as a 500.
        if key.contains('\0') || value.contains('\0') {
            return Err("query parameters can't contain NUL characters".into());
        }
        let path = split_key(&key).ok_or_else(|| format!("malformed parameter `{key}`"))?;
        if path.len() > MAX_KEY_DEPTH {
            return Err(format!("parameter `{key}` is nested too deeply"));
        }
        insert(&mut root, &path, value.into_owned())
            .map_err(|()| format!("conflicting parameter `{key}`"))?;
    }
    Ok(root)
}

fn split_key(key: &str) -> Option<Vec<String>> {
    let (head, mut rest) = match key.find('[') {
        Some(i) => (&key[..i], &key[i..]),
        None => (key, ""),
    };
    if head.is_empty() {
        return None;
    }
    let mut out = vec![head.to_owned()];
    while !rest.is_empty() {
        let inner = rest.strip_prefix('[')?;
        let end = inner.find(']')?;
        out.push(inner[..end].to_owned());
        rest = &inner[end + 1..];
    }
    Some(out)
}

fn insert(map: &mut Map<String, Value>, path: &[String], value: String) -> Result<(), ()> {
    let (last, parents) = path.split_last().ok_or(())?;
    let mut current = map;
    for p in parents {
        current = match current
            .entry(p.clone())
            .or_insert_with(|| Value::Object(Map::new()))
        {
            Value::Object(inner) => inner,
            _ => return Err(()),
        };
    }
    if current.contains_key(last) {
        return Err(());
    }
    current.insert(last.clone(), Value::String(value));
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    All(Vec<Filter>),
    Any(Vec<Filter>),
    Cond(Cond),
}

impl Filter {
    /// The point of a `near` condition that every result must meet (top level or inside
    /// `and`), which orders results by distance when no sort is given. A `near` inside `or`
    /// doesn't qualify, since other branches can match rows that aren't near anything. With
    /// several qualifying conditions the first one (in field-name order) sets the order.
    pub fn near(&self) -> Option<(&[String], f64, f64)> {
        match self {
            Filter::Cond(Cond {
                target: Target::Field { path, .. },
                op: Op::Near,
                value: Operand::Near { lng, lat, .. },
            }) => Some((path, *lng, *lat)),
            Filter::All(parts) => parts.iter().find_map(Filter::near),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cond {
    pub target: Target,
    pub op: Op,
    pub value: Operand,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Id,
    CreatedAt,
    UpdatedAt,
    /// A path of schema-validated field names, outermost first.
    Field {
        path: Vec<String>,
        kind: ValueKind,
    },
    /// Some document linked at `link` (of one of `collections`) matches `inner`, which is
    /// read in that document.
    Via {
        link: Vec<String>,
        many: bool,
        polymorphic: bool,
        collections: Vec<String>,
        inner: Box<Target>,
        /// The read filter of each linked collection the caller may read (`None`: all of
        /// it), so hidden documents never match. Empty means unguarded. Otherwise a
        /// collection missing from the list is unreadable, and nothing in it matches.
        guards: Vec<(String, Option<Box<Filter>>)>,
    },
    /// Some row of the list at `list` matches `inner`, whose path is relative to the row.
    Row {
        list: Vec<String>,
        inner: Box<Target>,
        /// For a `blocks` list, the block types whose rows can match: those that have the
        /// field. `None` for an array, whose rows all have it.
        blocks: Option<Vec<String>>,
    },
}

impl Target {
    pub fn kind(&self) -> ValueKind {
        match self.innermost() {
            Self::Id => ValueKind::Text,
            Self::CreatedAt | Self::UpdatedAt => ValueKind::Date,
            Self::Field { kind, .. } => *kind,
            Self::Via { .. } | Self::Row { .. } => unreachable!("innermost is never Via or Row"),
        }
    }

    /// The target a value is finally compared with, inside any links and lists.
    pub fn innermost(&self) -> &Target {
        match self {
            Self::Via { inner, .. } | Self::Row { inner, .. } => inner.innermost(),
            other => other,
        }
    }

    /// Reaches through a relationship or into a list.
    pub fn is_nested(&self) -> bool {
        matches!(self, Self::Via { .. } | Self::Row { .. })
    }

    /// A built-in column (`id`, timestamps), here or at the end of a relationship.
    fn is_builtin(&self) -> bool {
        matches!(
            self.innermost(),
            Self::Id | Self::CreatedAt | Self::UpdatedAt
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Text,
    Number,
    Boolean,
    Date,
    List,
    /// A list of polymorphic links (`{"relationTo", "value"}` objects).
    LinkList,
    /// A `[longitude, latitude]` location.
    Point,
}

/// How a field's values compare, or `None` when they can't be filtered or sorted.
pub fn value_kind(kind: &FieldKind) -> Option<ValueKind> {
    Some(match kind {
        FieldKind::Relationship {
            relation_to,
            has_many: true,
            ..
        } if relation_to.is_polymorphic() => ValueKind::LinkList,
        FieldKind::Select { multiple: true, .. }
        | FieldKind::Text { has_many: true, .. }
        | FieldKind::Number { has_many: true, .. }
        | FieldKind::Relationship { has_many: true, .. } => ValueKind::List,
        FieldKind::Number { .. } => ValueKind::Number,
        FieldKind::Boolean => ValueKind::Boolean,
        FieldKind::Date { .. } => ValueKind::Date,
        FieldKind::Point => ValueKind::Point,
        FieldKind::Text { .. }
        | FieldKind::Textarea { .. }
        | FieldKind::Email
        | FieldKind::Url
        | FieldKind::Select { .. }
        | FieldKind::Radio { .. }
        | FieldKind::Code { .. }
        | FieldKind::Slug { .. }
        | FieldKind::Relationship { .. } => ValueKind::Text,
        FieldKind::Json
        | FieldKind::Join { .. }
        | FieldKind::Group { .. }
        | FieldKind::Array { .. }
        | FieldKind::Blocks { .. }
        | FieldKind::Row { .. }
        | FieldKind::Collapsible { .. }
        | FieldKind::Tabs { .. } => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Equals,
    NotEquals,
    In,
    NotIn,
    Gt,
    Gte,
    Lt,
    Lte,
    Like,
    Contains,
    Exists,
    Near,
}

impl Op {
    const ALL: [Op; 12] = [
        Op::Equals,
        Op::NotEquals,
        Op::In,
        Op::NotIn,
        Op::Gt,
        Op::Gte,
        Op::Lt,
        Op::Lte,
        Op::Like,
        Op::Contains,
        Op::Exists,
        Op::Near,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Equals => "equals",
            Self::NotEquals => "not_equals",
            Self::In => "in",
            Self::NotIn => "not_in",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
            Self::Like => "like",
            Self::Contains => "contains",
            Self::Exists => "exists",
            Self::Near => "near",
        }
    }

    fn parse(s: &str) -> Option<Op> {
        Self::ALL.into_iter().find(|op| op.as_str() == s)
    }

    fn allowed(self, target: &Target) -> bool {
        use Op::*;
        if target.is_builtin() && matches!(self, Exists | Like) {
            return false;
        }
        match target.kind() {
            ValueKind::Text => matches!(self, Equals | NotEquals | In | NotIn | Like | Exists),
            ValueKind::Number | ValueKind::Date => !matches!(self, Like | Contains | Near),
            ValueKind::Point => matches!(self, Near | Exists),
            ValueKind::Boolean => matches!(self, Equals | NotEquals | Exists),
            ValueKind::List | ValueKind::LinkList => matches!(self, Contains | Exists),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Text(String),
    /// Exact as sent: whole numbers beyond 2^53 keep every digit.
    Number(serde_json::Number),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    One(Scalar),
    Many(Vec<Scalar>),
    /// Within `max` metres (and at least `min`) of a point.
    Near {
        lng: f64,
        lat: f64,
        max: Option<f64>,
        min: Option<f64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FilterError {
    /// The query parameter at fault, such as `where[price][gt]`.
    pub param: String,
    pub message: String,
}

impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.param, self.message)
    }
}

fn err(param: &str, message: impl Into<String>) -> FilterError {
    FilterError {
        param: param.to_owned(),
        message: message.into(),
    }
}

/// Finds another collection's schema by slug, so a filter can reach through a relationship.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<crate::CollectionSchema>;

/// The most relationships, and the most nested lists, one filter path may reach through.
const MAX_HOPS: u8 = 3;

/// Parses the `where` subtree of a query into a filter checked against `fields`. Paths
/// can go into lists but not through relationships; see `parse_filter_with`.
pub fn parse_filter(fields: &[Field], input: &Value) -> Result<Filter, FilterError> {
    let fields = data_fields(fields);
    Parser {
        fields: &fields,
        count: 0,
        lookup: None,
    }
    .group(input, "where", 0)
}

/// `parse_filter`, with paths through relationships (`author.name`) resolved in the
/// collections `lookup` finds.
pub fn parse_filter_with(
    fields: &[Field],
    input: &Value,
    lookup: Lookup<'_>,
) -> Result<Filter, FilterError> {
    let fields = data_fields(fields);
    Parser {
        fields: &fields,
        count: 0,
        lookup: Some(lookup),
    }
    .group(input, "where", 0)
}

/// Checks `name` can be filtered with `op`, whatever the value. Access rules check
/// `$user` conditions this way, since their values only exist per request.
pub(crate) fn check_target(
    fields: &[Field],
    name: &str,
    op: &str,
    lookup: Lookup<'_>,
) -> Result<(), FilterError> {
    let fields = data_fields(fields);
    Parser {
        fields: &fields,
        count: 0,
        lookup: Some(lookup),
    }
    .target(name, op, &format!("where[{name}][{op}]"))
    .map(|_| ())
}

struct Parser<'a> {
    fields: &'a [Field],
    count: usize,
    lookup: Option<Lookup<'a>>,
}

impl Parser<'_> {
    fn group(&mut self, input: &Value, param: &str, depth: usize) -> Result<Filter, FilterError> {
        if depth > MAX_DEPTH {
            return Err(err(param, "is nested too deeply"));
        }
        let Value::Object(obj) = input else {
            return Err(err(
                param,
                "expected conditions like where[title][equals]=Hello",
            ));
        };
        let mut parts = Vec::new();
        for (key, value) in obj {
            let p = format!("{param}[{key}]");
            if key == "and" || key == "or" {
                let members: Vec<(String, &Value)> = match value {
                    Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), v)).collect(),
                    Value::Array(items) => items
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (i.to_string(), v))
                        .collect(),
                    _ => {
                        return Err(err(
                            &p,
                            "expected a list of condition groups like [0][title][equals]",
                        ));
                    }
                };
                let mut branches = Vec::new();
                for (i, member) in members {
                    branches.push(self.group(member, &format!("{p}[{i}]"), depth + 1)?);
                }
                parts.push(if key == "and" {
                    Filter::All(branches)
                } else {
                    Filter::Any(branches)
                });
            } else {
                let Value::Object(ops) = value else {
                    return Err(err(&p, "expected an operator like [equals]"));
                };
                for (op, operand) in ops {
                    parts.push(self.cond(key, op, operand, &format!("{p}[{op}]"))?);
                }
            }
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Filter::All(parts)
        })
    }

    fn cond(
        &mut self,
        name: &str,
        op: &str,
        operand: &Value,
        param: &str,
    ) -> Result<Filter, FilterError> {
        self.count += 1;
        if self.count > MAX_CONDITIONS {
            return Err(err(
                param,
                format!("too many conditions (max {MAX_CONDITIONS})"),
            ));
        }
        let (op, target, field) = self.target(name, op, param)?;
        let field = field.as_ref();
        let value = match op {
            Op::Near => parse_near(operand).map_err(|()| {
                err(
                    param,
                    "expected longitude,latitude[,maxDistance[,minDistance]] in metres",
                )
            })?,
            Op::Exists => Operand::One(Scalar::Bool(
                parse_bool(operand).ok_or_else(|| err(param, "expected true or false"))?,
            )),
            Op::In | Op::NotIn => {
                let items = list(operand).ok_or_else(|| {
                    err(
                        param,
                        format!("expected up to {MAX_LIST} comma-separated values"),
                    )
                })?;
                let values = items
                    .iter()
                    .map(|v| scalar(target.innermost(), field, v))
                    .collect::<Result<_, _>>()
                    .map_err(|m| err(param, m))?;
                Operand::Many(values)
            }
            Op::Like => Operand::One(Scalar::Text(
                operand
                    .as_str()
                    .ok_or_else(|| err(param, "expected text"))?
                    .to_owned(),
            )),
            _ => {
                Operand::One(scalar(target.innermost(), field, operand).map_err(|m| err(param, m))?)
            }
        };
        Ok(Filter::Cond(Cond { target, op, value }))
    }

    /// The operator and what `name` points at, checked to fit each other.
    fn target(
        &self,
        name: &str,
        op: &str,
        param: &str,
    ) -> Result<(Op, Target, Option<Field>), FilterError> {
        let op = Op::parse(op).ok_or_else(|| {
            err(
                param,
                "unknown operator; use equals, not_equals, in, not_in, gt, gte, lt, lte, like, contains, exists or near",
            )
        })?;
        let (target, field) = resolve(
            &Scope {
                root: self.fields,
                fields: self.fields,
                collection: None,
                in_row: false,
            },
            name,
            name,
            "filtered",
            self.lookup,
            Hops::default(),
        )
        .map_err(|m| err(param, m))?;
        if op == Op::Near && target.is_nested() {
            return Err(err(
                param,
                "near can't be used through a relationship or into a list yet",
            ));
        }
        if !op.allowed(&target) {
            return Err(err(
                param,
                format!("{} can't be used on this field", op.as_str()),
            ));
        }
        Ok((op, target, field))
    }
}

/// Where a path is being resolved: `fields` at this level, `root` the top of the document
/// (virtual fields name paths from there), `collection` its slug when it's a linked one.
struct Scope<'a> {
    root: &'a [Field],
    fields: &'a [Field],
    collection: Option<&'a str>,
    /// List rows have no `id` or timestamps of their own.
    in_row: bool,
}

#[derive(Clone, Copy, Default)]
struct Hops {
    links: u8,
    lists: u8,
}

/// The target `name` points at, and its innermost field (owned: it may live in another
/// collection's schema). `full` is the whole path, for errors; `verb` is what's being done,
/// `filtered` or `sorted`. Without a `lookup`, paths stop at relationships.
fn resolve(
    scope: &Scope<'_>,
    name: &str,
    full: &str,
    verb: &str,
    lookup: Option<Lookup<'_>>,
    hops: Hops,
) -> Result<(Target, Option<Field>), String> {
    if !scope.in_row {
        match name {
            "id" => return Ok((Target::Id, None)),
            "createdAt" => return Ok((Target::CreatedAt, None)),
            "updatedAt" => return Ok((Target::UpdatedAt, None)),
            _ => {}
        }
    }
    if name.split('.').any(str::is_empty) {
        return Err(format!("unknown field `{full}`"));
    }
    let unknown = |segment: &str| match scope.collection {
        Some(slug) => format!("`{full}`: {slug} has no field `{segment}`"),
        None => format!("unknown field `{full}`"),
    };
    let mut current = scope.fields;
    let mut path = Vec::new();
    let segments: Vec<&str> = name.split('.').collect();
    let mut i = 0;
    while i < segments.len() {
        let segment = segments[i];
        let f = current
            .iter()
            .find(|f| f.name == segment)
            .ok_or_else(|| unknown(segment))?;
        path.push(f.name.clone());
        let rest = segments[i + 1..].join(".");
        let last = rest.is_empty();
        match &f.kind {
            _ if last && f.virtual_path.is_some() => {
                let source = f.virtual_path.as_deref().expect("checked above");
                let root = Scope {
                    root: scope.root,
                    fields: scope.root,
                    collection: scope.collection,
                    in_row: false,
                };
                return resolve(&root, source, full, verb, lookup, hops);
            }
            FieldKind::Array { fields, .. } if !last => {
                if hops.lists >= MAX_HOPS {
                    return Err(format!(
                        "`{full}` reaches into too many nested lists ({MAX_HOPS} at most)"
                    ));
                }
                let inner_scope = Scope {
                    root: scope.root,
                    fields,
                    collection: scope.collection,
                    in_row: true,
                };
                let hops = Hops {
                    lists: hops.lists + 1,
                    ..hops
                };
                let (inner, field) = resolve(&inner_scope, &rest, full, verb, lookup, hops)?;
                return Ok((
                    Target::Row {
                        list: path,
                        inner: Box::new(inner),
                        blocks: None,
                    },
                    field,
                ));
            }
            FieldKind::Blocks { blocks, .. } if !last => {
                if hops.lists >= MAX_HOPS {
                    return Err(format!(
                        "`{full}` reaches into too many nested lists ({MAX_HOPS} at most)"
                    ));
                }
                let hops = Hops {
                    lists: hops.lists + 1,
                    ..hops
                };
                let slugs = || {
                    blocks
                        .iter()
                        .filter_map(BlockSource::def)
                        .map(|b| b.slug.clone())
                        .collect::<Vec<_>>()
                };
                if rest == "blockType" {
                    let options = slugs().into_iter().map(SelectOption::Value).collect();
                    let block_type = Field {
                        kind: FieldKind::Select {
                            options,
                            multiple: false,
                        },
                        ..f.clone()
                    };
                    return Ok((
                        Target::Row {
                            list: path,
                            inner: Box::new(Target::Field {
                                path: vec!["blockType".into()],
                                kind: ValueKind::Text,
                            }),
                            blocks: Some(slugs()),
                        },
                        Some(block_type),
                    ));
                }
                let (inner, field, types) =
                    in_blocks(scope, blocks, &rest, full, verb, lookup, hops)?;
                return Ok((
                    Target::Row {
                        list: path,
                        inner: Box::new(inner),
                        blocks: Some(types),
                    },
                    field,
                ));
            }
            FieldKind::Group { fields } if !last => current = fields,
            // A polymorphic link is an object: its `relationTo`, or its `value` (the default).
            FieldKind::Relationship {
                relation_to,
                has_many: false,
                ..
            } if relation_to.is_polymorphic()
                && (last || rest == "relationTo" || rest == "value") =>
            {
                path.push(
                    if rest == "relationTo" {
                        "relationTo"
                    } else {
                        "value"
                    }
                    .to_owned(),
                );
                return Ok((
                    Target::Field {
                        path,
                        kind: ValueKind::Text,
                    },
                    Some(f.clone()),
                ));
            }
            FieldKind::Relationship {
                relation_to,
                has_many,
                ..
            } if !last => {
                if *has_many && (rest == "relationTo" || rest == "value") {
                    let at = path.join(".");
                    return Err(format!(
                        "`{at}` holds several links; use where[{at}][contains] to match one"
                    ));
                }
                let Some(lookup) = lookup else {
                    return Err(if verb == "sorted" {
                        format!("`{full}` goes through a relationship and can't be sorted yet")
                    } else {
                        format!(
                            "`{full}` goes through a relationship; this filter can't look at other collections"
                        )
                    });
                };
                if hops.links >= MAX_HOPS {
                    return Err(format!(
                        "`{full}` reaches through too many relationships ({MAX_HOPS} at most)"
                    ));
                }
                // `who.value.name` reads the same as `who.name` for a polymorphic link.
                let rest = match rest.strip_prefix("value.") {
                    Some(r) if relation_to.is_polymorphic() => r.to_owned(),
                    _ => rest,
                };
                let hops = Hops {
                    links: hops.links + 1,
                    ..hops
                };
                let mut found: Option<(String, Target, Option<Field>)> = None;
                for slug in relation_to.targets() {
                    let target = lookup(slug)
                        .ok_or_else(|| format!("`{full}`: collection `{slug}` doesn't exist"))?;
                    let data = target.data();
                    let linked = Scope {
                        root: data,
                        fields: data,
                        collection: Some(slug),
                        in_row: false,
                    };
                    let (inner, field) = resolve(&linked, &rest, full, verb, Some(lookup), hops)?;
                    let same_field = |a: &Option<Field>, b: &Option<Field>| match (a, b) {
                        (Some(a), Some(b)) => crate::diff::same_type(&a.kind, &b.kind),
                        (None, None) => true,
                        _ => false,
                    };
                    if let Some((first, seen, seen_field)) = &found
                        && (*seen != inner || !same_field(seen_field, &field))
                    {
                        return Err(format!(
                            "`{full}` means different things in {first} and {slug}"
                        ));
                    }
                    if found.is_none() {
                        found = Some((slug.to_owned(), inner, field));
                    }
                }
                let (_, inner, field) = found.expect("a relationship has at least one target");
                return Ok((
                    Target::Via {
                        link: path,
                        many: *has_many,
                        polymorphic: relation_to.is_polymorphic(),
                        collections: relation_to
                            .targets()
                            .iter()
                            .map(|s| (*s).to_owned())
                            .collect(),
                        inner: Box::new(inner),
                        guards: Vec::new(),
                    },
                    field,
                ));
            }
            _ if !last => return Err(unknown(segment)),
            FieldKind::Join { .. } => {
                return Err(format!(
                    "`{full}` is filled from other documents and can't be {verb}"
                ));
            }
            kind => {
                let kind = value_kind(kind).ok_or_else(|| format!("`{full}` can't be {verb}"))?;
                return Ok((Target::Field { path, kind }, Some(f.clone())));
            }
        }
        i += 1;
    }
    Err(unknown(name))
}

/// `rest` inside the rows of a blocks field: resolved in each block type that has it, all of
/// which must agree on what it is, and those types. A select or radio takes every type's
/// options.
fn in_blocks(
    scope: &Scope<'_>,
    blocks: &[BlockSource],
    rest: &str,
    full: &str,
    verb: &str,
    lookup: Option<Lookup<'_>>,
    hops: Hops,
) -> Result<(Target, Option<Field>, Vec<String>), String> {
    let mut found: Option<(Target, Option<Field>)> = None;
    let mut types = Vec::new();
    let mut first_error = None;
    for b in blocks.iter().filter_map(BlockSource::def) {
        let row = Scope {
            root: scope.root,
            fields: &b.fields,
            collection: scope.collection,
            in_row: true,
        };
        let (inner, field) = match resolve(&row, rest, full, verb, lookup, hops) {
            Ok(resolved) => resolved,
            // A type without the field has no rows that could match.
            Err(e) => {
                first_error.get_or_insert(e);
                continue;
            }
        };
        match &mut found {
            None => found = Some((inner, field)),
            Some((seen, seen_field)) => {
                let same = match (&*seen_field, &field) {
                    (Some(a), Some(b)) => crate::diff::same_type(&a.kind, &b.kind),
                    (a, b) => a.is_none() && b.is_none(),
                };
                if *seen != inner || !same {
                    return Err(format!(
                        "`{full}` means different things in block types {} and {}",
                        types[0], b.slug
                    ));
                }
                if let (
                    Some(Field {
                        kind: FieldKind::Select { options, .. } | FieldKind::Radio { options },
                        ..
                    }),
                    Some(Field {
                        kind:
                            FieldKind::Select { options: more, .. } | FieldKind::Radio { options: more },
                        ..
                    }),
                ) = (seen_field, &field)
                {
                    for o in more {
                        if !options.iter().any(|x| x.value() == o.value()) {
                            options.push(o.clone());
                        }
                    }
                }
            }
        }
        types.push(b.slug.clone());
    }
    let (inner, field) =
        found.ok_or_else(|| first_error.unwrap_or_else(|| format!("unknown field `{full}`")))?;
    Ok((inner, field, types))
}

/// `lng,lat[,maxDistance[,minDistance]]`, distances in metres.
fn parse_near(v: &Value) -> Result<Operand, ()> {
    let text = v.as_str().ok_or(())?;
    let numbers: Vec<f64> = text
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| ())?;
    if numbers.iter().any(|n| !n.is_finite()) {
        return Err(());
    }
    let (lng, lat, max, min) = match numbers.as_slice() {
        [lng, lat] => (*lng, *lat, None, None),
        [lng, lat, max] => (*lng, *lat, Some(*max), None),
        [lng, lat, max, min] => (*lng, *lat, Some(*max), Some(*min)),
        _ => return Err(()),
    };
    let in_range = (-180.0..=180.0).contains(&lng) && (-90.0..=90.0).contains(&lat);
    let distances_ok = [max, min].into_iter().flatten().all(|d| d >= 0.0)
        && !matches!((min, max), (Some(min), Some(max)) if min > max);
    if in_range && distances_ok {
        Ok(Operand::Near { lng, lat, max, min })
    } else {
        Err(())
    }
}

fn parse_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s == "true" => Some(true),
        Value::String(s) if s == "false" => Some(false),
        _ => None,
    }
}

fn list(v: &Value) -> Option<Vec<Value>> {
    let items: Vec<Value> = match v {
        Value::String(s) => s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Value::String(s.to_owned()))
            .collect(),
        Value::Array(items) => items.clone(),
        _ => return None,
    };
    (!items.is_empty() && items.len() <= MAX_LIST).then_some(items)
}

/// A JSON number, or text that reads as one. Never NaN or infinite: JSON can't hold those.
fn number(v: &Value) -> Option<serde_json::Number> {
    match v {
        Value::Number(n) => Some(n.clone()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn scalar(target: &Target, field: Option<&Field>, v: &Value) -> Result<Scalar, String> {
    let kind = field.map(|f| &f.kind);
    match (target.kind(), kind) {
        (ValueKind::List, Some(FieldKind::Number { .. })) => number(v)
            .map(Scalar::Number)
            .ok_or_else(|| "expected a number".into()),
        (ValueKind::Number, _) => number(v)
            .map(Scalar::Number)
            .ok_or_else(|| "expected a number".into()),
        (ValueKind::Boolean, _) => parse_bool(v)
            .map(Scalar::Bool)
            .ok_or_else(|| "expected true or false".into()),
        (ValueKind::Date, Some(FieldKind::Date { date_only: true })) => v
            .as_str()
            .filter(|s| is_date(s))
            .map(|s| Scalar::Text(s.to_owned()))
            .ok_or_else(|| "expected a date like 2026-10-12".into()),
        (ValueKind::Date, _) => v
            .as_str()
            .and_then(normalize_date_time)
            .map(Scalar::Text)
            .ok_or_else(|| "expected a date-time like 2026-10-12T09:30:00Z".into()),
        // The `relationTo` part of a polymorphic link: the path is the field's own name
        // followed by `relationTo` (a field merely named `relationTo` doesn't count).
        (_, Some(f_kind @ FieldKind::Relationship { relation_to, .. }))
            if relation_to.is_polymorphic()
                && matches!(
                    f_kind,
                    FieldKind::Relationship {
                        has_many: false,
                        ..
                    }
                )
                && matches!((target, field), (Target::Field { path, .. }, Some(f))
                    if path.len() >= 2
                        && path[path.len() - 1] == "relationTo"
                        && path[path.len() - 2] == f.name) =>
        {
            v.as_str()
                .filter(|s| relation_to.allows(s))
                .map(|s| Scalar::Text(s.to_owned()))
                .ok_or_else(|| format!("expected one of: {relation_to}"))
        }
        (_, None) | (_, Some(FieldKind::Relationship { .. })) => {
            let id = v
                .as_str()
                .and_then(|s| Uuid::try_parse(s).ok())
                .ok_or("expected a document ID")?;
            Ok(Scalar::Text(id.to_string()))
        }
        (_, Some(FieldKind::Select { options, .. } | FieldKind::Radio { options })) => v
            .as_str()
            .filter(|s| options.iter().any(|o| o.value() == *s))
            .map(|s| Scalar::Text(s.to_owned()))
            .ok_or_else(|| {
                let values: Vec<&str> = options.iter().map(SelectOption::value).collect();
                format!("expected one of: {}", values.join(", "))
            }),
        _ => v
            .as_str()
            .map(|s| Scalar::Text(s.to_owned()))
            .ok_or_else(|| "expected text".into()),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sort {
    pub target: Target,
    pub descending: bool,
}

pub fn parse_sort(fields: &[Field], input: &str) -> Result<Sort, FilterError> {
    if input.contains(',') {
        return Err(err(
            "sort",
            "sorting by more than one field isn't supported yet",
        ));
    }
    let (name, descending) = match input.strip_prefix('-') {
        Some(rest) => (rest, true),
        None => (input, false),
    };
    let fields = data_fields(fields);
    if crate::schema::find_field(&fields, name).is_some_and(|f| f.virtual_path.is_some()) {
        return Err(err(
            "sort",
            format!("`{name}` is read through a relationship and can't be sorted yet"),
        ));
    }
    let scope = Scope {
        root: &fields,
        fields: &fields,
        collection: None,
        in_row: false,
    };
    let (target, _) =
        resolve(&scope, name, name, "sorted", None, Hops::default()).map_err(|m| err("sort", m))?;
    if let Target::Row { .. } = target {
        return Err(err(
            "sort",
            format!("`{name}` is inside a list and can't be sorted"),
        ));
    }
    if matches!(target.kind(), ValueKind::List | ValueKind::LinkList) {
        return Err(err(
            "sort",
            format!("`{name}` holds a list and can't be sorted"),
        ));
    }
    if target.kind() == ValueKind::Point {
        return Err(err(
            "sort",
            format!("`{name}` holds a location and can't be sorted"),
        ));
    }
    Ok(Sort { target, descending })
}

/// Where the previous page ended: the last document's sort value and ID.
#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    pub value: Option<Scalar>,
    pub id: Uuid,
}

impl Cursor {
    pub fn encode(&self) -> String {
        let value = match &self.value {
            None => Value::Null,
            Some(Scalar::Text(s)) => json!(s),
            Some(Scalar::Number(n)) => json!(n),
            Some(Scalar::Bool(b)) => json!(b),
        };
        json!([value, self.id])
            .to_string()
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    pub fn decode(s: &str) -> Option<Cursor> {
        if !s.len().is_multiple_of(2) || s.len() > 2048 {
            return None;
        }
        let bytes = (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
            .collect::<Option<Vec<u8>>>()?;
        let Value::Array(parts) = serde_json::from_slice(&bytes).ok()? else {
            return None;
        };
        let [value, id] = parts.as_slice() else {
            return None;
        };
        let value = match value {
            Value::Null => None,
            Value::String(s) if s.contains('\0') => return None,
            Value::String(s) => Some(Scalar::Text(s.clone())),
            Value::Number(n) => Some(Scalar::Number(n.clone())),
            Value::Bool(b) => Some(Scalar::Bool(*b)),
            _ => return None,
        };
        Some(Cursor {
            value,
            id: Uuid::try_parse(id.as_str()?).ok()?,
        })
    }

    /// Whether this cursor's value can be compared and bound for a sort on `target`.
    pub fn fits(&self, target: &Target) -> bool {
        match (&self.value, target) {
            (Some(Scalar::Text(s)), Target::Id) => return Uuid::try_parse(s).is_ok(),
            (Some(Scalar::Text(s)), Target::CreatedAt | Target::UpdatedAt) => {
                return time::OffsetDateTime::parse(
                    s,
                    &time::format_description::well_known::Rfc3339,
                )
                .is_ok();
            }
            _ => {}
        }
        matches!(
            (&self.value, target.kind()),
            (None, _)
                | (Some(Scalar::Text(_)), ValueKind::Text | ValueKind::Date)
                | (Some(Scalar::Number(_)), ValueKind::Number)
                | (Some(Scalar::Bool(_)), ValueKind::Boolean)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Field;
    use serde_json::json;

    fn fields() -> Vec<Field> {
        serde_json::from_value(json!([
            {"name": "title", "type": "text"},
            {"name": "price", "type": "number"},
            {"name": "live", "type": "boolean"},
            {"name": "on", "type": "date", "dateOnly": true},
            {"name": "at", "type": "date"},
            {"name": "color", "type": "select", "options": ["red", "blue"]},
            {"name": "tags", "type": "select", "options": ["a", "b"], "multiple": true},
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "seo", "type": "group", "fields": [{"name": "title", "type": "text"}]},
            {"name": "links", "type": "array", "fields": [{"name": "url", "type": "url"}]},
            {"name": "data", "type": "json"},
            {"name": "size", "type": "radio", "options": [{"label": "Small", "value": "s"}]},
            {"name": "snippet", "type": "code"},
            {"name": "scores", "type": "number", "hasMany": true},
            {"name": "link", "type": "relationship", "relationTo": ["posts", "pages"]},
            {"name": "linked", "type": "relationship", "relationTo": ["posts", "pages"], "hasMany": true},
            {"name": "spot", "type": "point"}
        ]))
        .unwrap()
    }

    fn filter(qs: &str) -> Result<Filter, String> {
        let params = parse_query(qs).unwrap();
        parse_filter(&fields(), &params["where"]).map_err(|e| e.to_string())
    }

    fn field(name: &str, kind: ValueKind) -> Target {
        Target::Field {
            path: name.split('.').map(str::to_owned).collect(),
            kind,
        }
    }

    fn cond(target: Target, op: Op, value: Operand) -> Filter {
        Filter::Cond(Cond { target, op, value })
    }

    fn text(s: &str) -> Scalar {
        Scalar::Text(s.into())
    }

    #[test]
    fn parse_query_builds_nested_objects() {
        let q = parse_query(
            "where[title][equals]=Hello%20there&where[or][0][price][gt]=5&limit=10&sort=-at",
        )
        .unwrap();
        assert_eq!(
            Value::Object(q),
            json!({
                "where": {"title": {"equals": "Hello there"}, "or": {"0": {"price": {"gt": "5"}}}},
                "limit": "10",
                "sort": "-at"
            })
        );
        assert!(parse_query("where[title=1").is_err());
        assert!(parse_query("limit=1&limit=2").is_err());
        assert!(parse_query("a=1&a[b]=2").is_err());
        let many = (0..101)
            .map(|i| format!("p{i}=1"))
            .collect::<Vec<_>>()
            .join("&");
        assert!(parse_query(&many).is_err());
    }

    #[test]
    fn coerces_values_to_the_field_type() {
        assert_eq!(
            filter("where[price][gt]=5").unwrap(),
            cond(
                field("price", ValueKind::Number),
                Op::Gt,
                Operand::One(Scalar::Number(5.into()))
            )
        );
        assert_eq!(
            filter("where[live][equals]=true").unwrap(),
            cond(
                field("live", ValueKind::Boolean),
                Op::Equals,
                Operand::One(Scalar::Bool(true))
            )
        );
        assert_eq!(
            filter("where[at][gte]=2026-10-12T09:30:00%2B02:00").unwrap(),
            cond(
                field("at", ValueKind::Date),
                Op::Gte,
                Operand::One(text("2026-10-12T07:30:00.000Z"))
            )
        );
        assert_eq!(
            filter("where[color][in]=red,blue").unwrap(),
            cond(
                field("color", ValueKind::Text),
                Op::In,
                Operand::Many(vec![text("red"), text("blue")])
            )
        );
        assert_eq!(
            filter("where[seo.title][like]=rust").unwrap(),
            cond(
                field("seo.title", ValueKind::Text),
                Op::Like,
                Operand::One(text("rust"))
            )
        );
        assert_eq!(
            filter("where[tags][contains]=a").unwrap(),
            cond(
                field("tags", ValueKind::List),
                Op::Contains,
                Operand::One(text("a"))
            )
        );
        assert_eq!(
            filter("where[title][exists]=false").unwrap(),
            cond(
                field("title", ValueKind::Text),
                Op::Exists,
                Operand::One(Scalar::Bool(false))
            )
        );
    }

    #[test]
    fn groups_nest() {
        let f = filter(
            "where[or][0][price][lt]=1&where[or][1][and][0][live][equals]=true&where[or][1][and][1][title][equals]=x",
        )
        .unwrap();
        let Filter::Any(branches) = f else {
            panic!("expected or, got {f:?}")
        };
        assert_eq!(branches.len(), 2);
        assert!(matches!(&branches[1], Filter::All(inner) if inner.len() == 2));
    }

    #[test]
    fn rejects_values_of_the_wrong_type() {
        for (qs, message) in [
            (
                "where[price][gt]=abc",
                "where[price][gt]: expected a number",
            ),
            (
                "where[price][gt]=1e400",
                "where[price][gt]: expected a number",
            ),
            (
                "where[live][equals]=yes",
                "where[live][equals]: expected true or false",
            ),
            (
                "where[on][equals]=2026-13-01",
                "where[on][equals]: expected a date like 2026-10-12",
            ),
            (
                "where[at][gt]=soon",
                "where[at][gt]: expected a date-time like 2026-10-12T09:30:00Z",
            ),
            (
                "where[color][equals]=green",
                "where[color][equals]: expected one of: red, blue",
            ),
            (
                "where[author][equals]=nope",
                "where[author][equals]: expected a document ID",
            ),
            (
                "where[id][equals]=nope",
                "where[id][equals]: expected a document ID",
            ),
            (
                "where[tags][contains]=z",
                "where[tags][contains]: expected one of: a, b",
            ),
        ] {
            assert_eq!(filter(qs).unwrap_err(), message, "{qs}");
        }
    }

    #[test]
    fn rejects_targets_and_operators_that_dont_fit() {
        for (qs, message) in [
            (
                "where[nope][equals]=1",
                "where[nope][equals]: unknown field `nope`",
            ),
            (
                "where[data][equals]=1",
                "where[data][equals]: `data` can't be filtered",
            ),
            (
                "where[seo][equals]=1",
                "where[seo][equals]: `seo` can't be filtered",
            ),
            (
                "where[links.url][near]=0,0",
                "where[links.url][near]: near can't be used through a relationship or into a list yet",
            ),
            (
                "where[live][gt]=true",
                "where[live][gt]: gt can't be used on this field",
            ),
            (
                "where[tags][equals]=a",
                "where[tags][equals]: equals can't be used on this field",
            ),
            (
                "where[id][exists]=true",
                "where[id][exists]: exists can't be used on this field",
            ),
            (
                "where[title][shout]=x",
                "where[title][shout]: unknown operator; use equals, not_equals, in, not_in, gt, gte, lt, lte, like, contains, exists or near",
            ),
            (
                "where[title]=x",
                "where[title]: expected an operator like [equals]",
            ),
        ] {
            assert_eq!(filter(qs).unwrap_err(), message, "{qs}");
        }
    }

    #[test]
    fn limits_size_and_depth() {
        let many = (0..51)
            .map(|i| format!("where[or][{i}][title][equals]=x"))
            .collect::<Vec<_>>()
            .join("&");
        assert!(
            filter(&many)
                .unwrap_err()
                .ends_with("too many conditions (max 50)")
        );
        let deep = "where[or][0][or][0][or][0][or][0][or][0][or][0][title][equals]=x";
        assert!(filter(deep).unwrap_err().ends_with("is nested too deeply"));
    }

    #[test]
    fn sorts() {
        assert_eq!(
            parse_sort(&fields(), "-price").unwrap(),
            Sort {
                target: field("price", ValueKind::Number),
                descending: true
            }
        );
        assert_eq!(
            parse_sort(&fields(), "createdAt").unwrap(),
            Sort {
                target: Target::CreatedAt,
                descending: false
            }
        );
        assert_eq!(
            parse_sort(&fields(), "title,price")
                .unwrap_err()
                .to_string(),
            "sort: sorting by more than one field isn't supported yet"
        );
        assert_eq!(
            parse_sort(&fields(), "tags").unwrap_err().to_string(),
            "sort: `tags` holds a list and can't be sorted"
        );
    }

    #[test]
    fn cursors_round_trip_and_reject_garbage() {
        let c = Cursor {
            value: Some(Scalar::Number(serde_json::Number::from_f64(9.5).unwrap())),
            id: Uuid::nil(),
        };
        assert_eq!(Cursor::decode(&c.encode()), Some(c.clone()));
        assert!(
            c.fits(&field("price", ValueKind::Number)) && !c.fits(&field("title", ValueKind::Text))
        );
        assert!(
            Cursor {
                value: None,
                id: Uuid::nil()
            }
            .fits(&field("title", ValueKind::Text))
        );
        let garbage = Cursor {
            value: Some(text("garbage")),
            id: Uuid::nil(),
        };
        assert!(garbage.fits(&field("title", ValueKind::Text)));
        assert!(!garbage.fits(&Target::Id), "an id cursor must hold an ID");
        assert!(
            !garbage.fits(&Target::CreatedAt),
            "a createdAt cursor must hold a date-time"
        );
        let nul = Cursor {
            value: Some(text("a\u{0}b")),
            id: Uuid::nil(),
        };
        assert_eq!(Cursor::decode(&nul.encode()), None);
        let encoded = [hex("[1]"), hex("[1, \"nope\"]")];
        for bad in ["zz", "", "00"]
            .into_iter()
            .map(String::from)
            .chain(encoded)
        {
            assert_eq!(Cursor::decode(&bad), None, "{bad}");
        }
    }

    fn hex(s: &str) -> String {
        s.bytes().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn nul_characters_are_rejected() {
        assert!(parse_query("where[title][equals]=a%00b").is_err());
        assert!(parse_query("a%00=1").is_err());
    }
    #[test]
    fn radio_and_code_filter_like_text() {
        assert_eq!(
            filter("where[size][equals]=s").unwrap(),
            cond(
                field("size", ValueKind::Text),
                Op::Equals,
                Operand::One(text("s"))
            )
        );
        assert_eq!(
            filter("where[size][equals]=Small").unwrap_err(),
            "where[size][equals]: expected one of: s"
        );
        assert_eq!(
            filter("where[snippet][like]=fn").unwrap(),
            cond(
                field("snippet", ValueKind::Text),
                Op::Like,
                Operand::One(text("fn"))
            )
        );
    }
    #[test]
    fn number_lists_take_numbers() {
        assert_eq!(
            filter("where[scores][contains]=3").unwrap(),
            cond(
                field("scores", ValueKind::List),
                Op::Contains,
                Operand::One(Scalar::Number(3.into()))
            )
        );
        assert_eq!(
            filter("where[scores][contains]=x").unwrap_err(),
            "where[scores][contains]: expected a number"
        );
    }
    #[test]
    fn filters_and_sorts_use_data_paths_through_layout() {
        let f: Vec<Field> = serde_json::from_value(json!([
            {"type": "row", "fields": [{"name": "price", "type": "number"}]},
            {"type": "tabs", "tabs": [{"name": "meta", "label": "Meta", "fields": [{"name": "rank", "type": "number"}]}]}
        ]))
        .unwrap();
        let w = parse_query("where[price][gt]=1&where[meta.rank][lt]=5").unwrap();
        assert!(parse_filter(&f, &w["where"]).is_ok());
        assert_eq!(
            parse_sort(&f, "-meta.rank").unwrap().target,
            field("meta.rank", ValueKind::Number)
        );
    }
    #[test]
    fn polymorphic_links_filter_by_id_and_collection() {
        let id = "0199d5c4-0000-7000-8000-00000000000a";
        let by_id = cond(
            field("link.value", ValueKind::Text),
            Op::Equals,
            Operand::One(text(id)),
        );
        assert_eq!(filter(&format!("where[link][equals]={id}")).unwrap(), by_id);
        assert_eq!(
            filter(&format!("where[link.value][equals]={id}")).unwrap(),
            by_id
        );
        assert_eq!(
            filter("where[link.relationTo][equals]=pages").unwrap(),
            cond(
                field("link.relationTo", ValueKind::Text),
                Op::Equals,
                Operand::One(text("pages"))
            )
        );
        assert_eq!(
            filter("where[link.relationTo][equals]=authors").unwrap_err(),
            "where[link.relationTo][equals]: expected one of: posts, pages"
        );
        assert_eq!(
            filter(&format!("where[linked][contains]={id}")).unwrap(),
            cond(
                field("linked", ValueKind::LinkList),
                Op::Contains,
                Operand::One(text(id))
            )
        );
        assert!(parse_sort(&fields(), "linked").is_err());
    }
    #[test]
    fn near_takes_a_point_and_optional_distances() {
        assert_eq!(
            filter("where[spot][near]=-0.1276,51.5072,1000").unwrap(),
            cond(
                field("spot", ValueKind::Point),
                Op::Near,
                Operand::Near {
                    lng: -0.1276,
                    lat: 51.5072,
                    max: Some(1000.0),
                    min: None
                }
            )
        );
        for bad in [
            "1",
            "200,0",
            "0,95",
            "0,0,-5",
            "0,0,NaN",
            "a,b",
            "0,0,10,20",
        ] {
            assert_eq!(
                filter(&format!("where[spot][near]={bad}")).unwrap_err(),
                "where[spot][near]: expected longitude,latitude[,maxDistance[,minDistance]] in metres",
                "{bad}"
            );
        }
        assert!(
            filter("where[title][near]=0,0").is_err(),
            "near is only for points"
        );
        assert!(parse_sort(&fields(), "spot").is_err());
    }

    #[test]
    fn a_field_named_relation_to_still_filters_by_id() {
        let f: Vec<Field> = serde_json::from_value(json!([
            {"name": "relationTo", "type": "relationship", "relationTo": "authors"}
        ]))
        .unwrap();
        let id = "0199d5c4-0000-7000-8000-00000000000a";
        let w = parse_query(&format!("where[relationTo][equals]={id}")).unwrap();
        assert!(parse_filter(&f, &w["where"]).is_ok());
    }

    #[test]
    fn a_nested_link_list_named_relation_to_filters_by_id() {
        let f: Vec<Field> = serde_json::from_value(json!([
            {"name": "g", "type": "group", "fields": [
                {"name": "relationTo", "type": "relationship", "relationTo": ["posts", "pages"], "hasMany": true}
            ]}
        ]))
        .unwrap();
        let id = "0199d5c4-0000-7000-8000-00000000000a";
        let w = parse_query(&format!("where[g.relationTo][contains]={id}")).unwrap();
        assert!(parse_filter(&f, &w["where"]).is_ok());
    }

    #[test]
    fn numbers_keep_their_exact_value() {
        let big = |qs: &str| match filter(qs).unwrap() {
            Filter::Cond(c) => c.value,
            other => panic!("{other:?}"),
        };
        let exact: serde_json::Number = "9007199254740993".parse().unwrap();
        assert_eq!(
            big("where[price][equals]=9007199254740993"),
            Operand::One(Scalar::Number(exact.clone()))
        );
        let cursor = Cursor {
            value: Some(Scalar::Number(exact)),
            id: Uuid::nil(),
        };
        assert_eq!(Cursor::decode(&cursor.encode()), Some(cursor));
        assert!(filter("where[price][equals]=1e400").is_err());
        // Numbers are read as JSON writes them, so these spellings aren't numbers.
        // (`%2B` is a plus sign; a bare `+` in a query string means a space.)
        for odd in ["%2B5", ".5", "5.", "0x10", "NaN", "inf"] {
            assert!(
                filter(&format!("where[price][equals]={odd}")).is_err(),
                "{odd}"
            );
        }
        assert!(filter("where[price][equals]=-0.5e3").is_ok());
    }

    #[test]
    fn layout_kinds_have_no_value_kind() {
        let schema: crate::CollectionSchema =
            serde_json::from_value(json!({"slug": "p", "fields": [
                {"type": "row", "fields": []},
                {"type": "collapsible", "label": "C", "fields": []},
                {"type": "tabs", "tabs": [{"label": "A", "fields": []}]}
            ]}))
            .unwrap();
        for f in &schema.fields {
            assert_eq!(value_kind(&f.kind), None, "{}", f.kind.name());
        }
    }

    #[test]
    fn sorting_on_a_value_that_cant_be_sorted_says_sorted() {
        assert_eq!(
            parse_sort(&fields(), "data").unwrap_err().to_string(),
            "sort: `data` can't be sorted"
        );
    }

    mod cross {
        use super::*;
        use crate::CollectionSchema;

        fn schema(v: Value) -> CollectionSchema {
            serde_json::from_value(v).unwrap()
        }

        fn lookup(slug: &str) -> Option<CollectionSchema> {
            match slug {
                "authors" => Some(schema(json!({"slug": "authors", "fields": [
                    {"name": "name", "type": "text"},
                    {"name": "age", "type": "number"},
                    {"name": "company", "type": "relationship", "relationTo": "companies"}
                ]}))),
                "companies" => Some(schema(json!({"slug": "companies", "fields": [
                    {"name": "name", "type": "text"},
                    {"name": "owner", "type": "relationship", "relationTo": "authors"}
                ]}))),
                "guests" => Some(schema(json!({"slug": "guests", "fields": [
                    {"name": "name", "type": "text"}
                ]}))),
                "pets" => Some(schema(json!({"slug": "pets", "fields": [
                    {"name": "age", "type": "text"}
                ]}))),
                _ => None,
            }
        }

        fn posts() -> Vec<Field> {
            serde_json::from_value(json!([
                {"name": "title", "type": "text"},
                {"name": "author", "type": "relationship", "relationTo": "authors"},
                {"name": "tags", "type": "relationship", "relationTo": "authors", "hasMany": true},
                {"name": "who", "type": "relationship", "relationTo": ["authors", "guests"]},
                {"name": "odd", "type": "relationship", "relationTo": ["authors", "pets"]},
                {"name": "authorName", "type": "text", "virtual": "author.name"},
                {"name": "links", "type": "array", "fields": [
                    {"name": "url", "type": "url"},
                    {"name": "by", "type": "relationship", "relationTo": "authors"},
                    {"name": "parts", "type": "array", "fields": [{"name": "n", "type": "number"}]}
                ]},
                {"name": "layout", "type": "blocks", "blocks": [
                    {"slug": "hero", "fields": [
                        {"name": "heading", "type": "text"},
                        {"name": "by", "type": "relationship", "relationTo": "authors"},
                        {"name": "rows", "type": "array", "fields": [{"name": "n", "type": "number"}]},
                        {"name": "x", "type": "text"}
                    ]},
                    {"slug": "quote", "fields": [
                        {"name": "by", "type": "relationship", "relationTo": "authors"},
                        {"name": "x", "type": "number"},
                        {"name": "cta", "type": "text"}
                    ]},
                    {"slug": "card", "fields": [
                        {"name": "cta", "type": "group", "fields": [{"name": "label", "type": "text"}]},
                        {"name": "deep", "type": "array", "fields": [
                            {"name": "deeper", "type": "array", "fields": [{"name": "n", "type": "number"}]}
                        ]}
                    ]}
                ]}
            ]))
            .unwrap()
        }

        fn filter(qs: &str) -> Result<Filter, String> {
            let params = parse_query(qs).unwrap();
            parse_filter_with(&posts(), &params["where"], &lookup).map_err(|e| e.to_string())
        }

        fn target(qs: &str) -> Target {
            match filter(qs).unwrap() {
                Filter::Cond(c) => c.target,
                other => panic!("{other:?}"),
            }
        }

        fn via(
            link: &str,
            many: bool,
            polymorphic: bool,
            collections: &[&str],
            inner: Target,
        ) -> Target {
            Target::Via {
                link: link.split('.').map(str::to_owned).collect(),
                many,
                polymorphic,
                collections: collections.iter().map(|s| (*s).to_owned()).collect(),
                inner: Box::new(inner),
                guards: Vec::new(),
            }
        }

        fn row(list: &str, inner: Target) -> Target {
            Target::Row {
                list: list.split('.').map(str::to_owned).collect(),
                inner: Box::new(inner),
                blocks: None,
            }
        }

        fn blocks(list: &str, types: &[&str], inner: Target) -> Target {
            Target::Row {
                list: list.split('.').map(str::to_owned).collect(),
                inner: Box::new(inner),
                blocks: Some(types.iter().map(|s| (*s).to_owned()).collect()),
            }
        }

        #[test]
        fn paths_into_blocks_resolve_to_rows_of_the_types_that_have_them() {
            let all = ["hero", "quote", "card"];
            assert_eq!(
                target("where[layout.blockType][equals]=quote"),
                blocks("layout", &all, field("blockType", ValueKind::Text))
            );
            assert_eq!(
                target("where[layout.heading][like]=a"),
                blocks("layout", &["hero"], field("heading", ValueKind::Text))
            );
            let by = via(
                "by",
                false,
                false,
                &["authors"],
                field("name", ValueKind::Text),
            );
            assert_eq!(
                target("where[layout.by.name][equals]=Ann"),
                blocks("layout", &["hero", "quote"], by)
            );
            assert_eq!(
                target("where[layout.rows.n][gt]=1"),
                blocks(
                    "layout",
                    &["hero"],
                    row("rows", field("n", ValueKind::Number))
                )
            );
            // quote's `cta` is text, so only card's group has a `label`.
            assert_eq!(
                target("where[layout.cta.label][equals]=Go"),
                blocks("layout", &["card"], field("cta.label", ValueKind::Text))
            );
            for (qs, message) in [
                (
                    "where[layout.blockType][equals]=nope",
                    "where[layout.blockType][equals]: expected one of: hero, quote, card",
                ),
                (
                    "where[layout.x][equals]=1",
                    "where[layout.x][equals]: `layout.x` means different things in block types hero and quote",
                ),
                (
                    "where[layout.nope][equals]=1",
                    "where[layout.nope][equals]: unknown field `layout.nope`",
                ),
                (
                    "where[layout.by.nope][equals]=1",
                    "where[layout.by.nope][equals]: `layout.by.nope`: authors has no field `nope`",
                ),
                (
                    "where[layout.id][equals]=1",
                    "where[layout.id][equals]: unknown field `layout.id`",
                ),
            ] {
                assert_eq!(filter(qs).unwrap_err(), message, "{qs}");
            }
            // Blocks count as a list towards the limit.
            assert_eq!(
                target("where[layout.deep.deeper.n][gt]=1"),
                blocks(
                    "layout",
                    &["card"],
                    row("deep", row("deeper", field("n", ValueKind::Number)))
                )
            );
            assert_eq!(
                parse_sort(&posts(), "layout.heading")
                    .unwrap_err()
                    .to_string(),
                "sort: `layout.heading` is inside a list and can't be sorted"
            );
        }

        #[test]
        fn paths_through_relationships_resolve_to_via() {
            assert_eq!(
                target("where[author.name][equals]=Ann"),
                via(
                    "author",
                    false,
                    false,
                    &["authors"],
                    field("name", ValueKind::Text)
                )
            );
            assert_eq!(
                target("where[tags.age][gt]=3"),
                via(
                    "tags",
                    true,
                    false,
                    &["authors"],
                    field("age", ValueKind::Number)
                )
            );
            assert_eq!(
                target("where[author.company.name][equals]=Acme"),
                via(
                    "author",
                    false,
                    false,
                    &["authors"],
                    via(
                        "company",
                        false,
                        false,
                        &["companies"],
                        field("name", ValueKind::Text)
                    )
                )
            );
            assert_eq!(
                target("where[author.createdAt][gt]=2026-01-01T00:00:00Z"),
                via("author", false, false, &["authors"], Target::CreatedAt)
            );
            assert_eq!(
                target("where[who.name][like]=a"),
                via(
                    "who",
                    false,
                    true,
                    &["authors", "guests"],
                    field("name", ValueKind::Text)
                )
            );
            assert_eq!(
                target("where[who.value.name][like]=a"),
                via(
                    "who",
                    false,
                    true,
                    &["authors", "guests"],
                    field("name", ValueKind::Text)
                )
            );
            assert_eq!(
                target("where[who.relationTo][equals]=guests"),
                field("who.relationTo", ValueKind::Text)
            );
        }

        #[test]
        fn paths_into_lists_resolve_to_row() {
            assert_eq!(
                target("where[links.url][like]=docs"),
                row("links", field("url", ValueKind::Text))
            );
            assert_eq!(
                target("where[links.parts.n][gt]=1"),
                row("links", row("parts", field("n", ValueKind::Number)))
            );
            assert_eq!(
                target("where[links.by.name][equals]=Ann"),
                row(
                    "links",
                    via(
                        "by",
                        false,
                        false,
                        &["authors"],
                        field("name", ValueKind::Text)
                    )
                )
            );
        }

        #[test]
        fn virtual_fields_filter_as_their_source() {
            assert_eq!(
                target("where[authorName][equals]=Ann"),
                target("where[author.name][equals]=Ann")
            );
        }

        #[test]
        fn polymorphic_paths_need_every_target_to_have_the_field() {
            assert_eq!(
                filter("where[who.age][gt]=1").unwrap_err(),
                "where[who.age][gt]: `who.age`: guests has no field `age`"
            );
            assert_eq!(
                filter("where[odd.age][equals]=1").unwrap_err(),
                "where[odd.age][equals]: `odd.age` means different things in authors and pets"
            );
        }

        #[test]
        fn too_many_hops_and_near_through_links_are_refused() {
            assert_eq!(
                filter("where[author.company.owner.company.name][equals]=x").unwrap_err(),
                "where[author.company.owner.company.name][equals]: `author.company.owner.company.name` reaches through too many relationships (3 at most)"
            );
            assert!(
                filter("where[author.company.owner.name][equals]=x").is_ok(),
                "3 hops is fine"
            );
            assert_eq!(
                filter("where[author.nope][equals]=x").unwrap_err(),
                "where[author.nope][equals]: `author.nope`: authors has no field `nope`"
            );
        }

        #[test]
        fn without_a_lookup_relationship_paths_explain_themselves() {
            let params = parse_query("where[author.name][equals]=Ann").unwrap();
            assert_eq!(
                parse_filter(&posts(), &params["where"])
                    .unwrap_err()
                    .to_string(),
                "where[author.name][equals]: `author.name` goes through a relationship; this filter can't look at other collections"
            );
        }

        #[test]
        fn sorting_through_relationships_and_lists_is_refused() {
            assert_eq!(
                parse_sort(&posts(), "author.name").unwrap_err().to_string(),
                "sort: `author.name` goes through a relationship and can't be sorted yet"
            );
            assert_eq!(
                parse_sort(&posts(), "links.url").unwrap_err().to_string(),
                "sort: `links.url` is inside a list and can't be sorted"
            );
        }

        #[test]
        fn trailing_dots_and_odd_sub_paths_are_named() {
            assert_eq!(
                filter("where[author.][equals]=x").unwrap_err(),
                "where[author.][equals]: unknown field `author.`"
            );
            assert_eq!(
                filter("where[tags.relationTo][equals]=authors").unwrap_err(),
                "where[tags.relationTo][equals]: `tags` holds several links; use where[tags][contains] to match one"
            );
        }

        #[test]
        fn polymorphic_paths_need_the_same_field_type_in_every_target() {
            let lookup = |slug: &str| match slug {
                "a" => Some(schema(
                    json!({"slug": "a", "fields": [{"name": "status", "type": "select", "options": ["on"]}]}),
                )),
                "b" => Some(schema(
                    json!({"slug": "b", "fields": [{"name": "status", "type": "text"}]}),
                )),
                _ => None,
            };
            let fields: Vec<Field> = serde_json::from_value(json!([
                {"name": "who", "type": "relationship", "relationTo": ["a", "b"]}
            ]))
            .unwrap();
            let params = parse_query("where[who.status][equals]=x").unwrap();
            assert_eq!(
                parse_filter_with(&fields, &params["where"], &lookup)
                    .unwrap_err()
                    .to_string(),
                "where[who.status][equals]: `who.status` means different things in a and b"
            );
        }
    }
}
