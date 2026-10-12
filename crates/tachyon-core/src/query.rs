//! Query strings, filters, sorts and cursors. Everything here is validated against the
//! collection schema, so the database layer only ever sees well-typed conditions.

use std::fmt;

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::schema::{Field, FieldKind, SelectOption, data_fields};
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
}

impl Target {
    pub fn kind(&self) -> ValueKind {
        match self {
            Self::Id => ValueKind::Text,
            Self::CreatedAt | Self::UpdatedAt => ValueKind::Date,
            Self::Field { kind, .. } => *kind,
        }
    }

    fn is_builtin(&self) -> bool {
        !matches!(self, Self::Field { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Text,
    Number,
    Boolean,
    Date,
    List,
}

/// How a field's values compare, or `None` when they can't be filtered or sorted.
pub fn value_kind(kind: &FieldKind) -> Option<ValueKind> {
    Some(match kind {
        FieldKind::Select { multiple: true, .. }
        | FieldKind::Text { has_many: true, .. }
        | FieldKind::Number { has_many: true, .. }
        | FieldKind::Relationship { has_many: true, .. } => ValueKind::List,
        FieldKind::Number { .. } => ValueKind::Number,
        FieldKind::Boolean => ValueKind::Boolean,
        FieldKind::Date { .. } => ValueKind::Date,
        FieldKind::Json | FieldKind::Group { .. } | FieldKind::Array { .. } => return None,
        _ => ValueKind::Text,
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
}

impl Op {
    const ALL: [Op; 11] = [
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
            ValueKind::Number | ValueKind::Date => !matches!(self, Like | Contains),
            ValueKind::Boolean => matches!(self, Equals | NotEquals | Exists),
            ValueKind::List => matches!(self, Contains | Exists),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Text(String),
    Number(f64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    One(Scalar),
    Many(Vec<Scalar>),
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

/// Parses the `where` subtree of a query into a filter checked against `fields`.
pub fn parse_filter(fields: &[Field], input: &Value) -> Result<Filter, FilterError> {
    let fields = data_fields(fields);
    Parser {
        fields: &fields,
        count: 0,
    }
    .group(input, "where", 0)
}

struct Parser<'a> {
    fields: &'a [Field],
    count: usize,
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
        let op = Op::parse(op).ok_or_else(|| {
            err(
                param,
                "unknown operator; use equals, not_equals, in, not_in, gt, gte, lt, lte, like, contains or exists",
            )
        })?;
        let (target, field) = resolve(self.fields, name).map_err(|m| err(param, m))?;
        if !op.allowed(&target) {
            return Err(err(
                param,
                format!("{} can't be used on this field", op.as_str()),
            ));
        }
        let value = match op {
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
                    .map(|v| scalar(&target, field, v))
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
            _ => Operand::One(scalar(&target, field, operand).map_err(|m| err(param, m))?),
        };
        Ok(Filter::Cond(Cond { target, op, value }))
    }
}

fn resolve<'a>(fields: &'a [Field], name: &str) -> Result<(Target, Option<&'a Field>), String> {
    match name {
        "id" => return Ok((Target::Id, None)),
        "createdAt" => return Ok((Target::CreatedAt, None)),
        "updatedAt" => return Ok((Target::UpdatedAt, None)),
        _ => {}
    }
    let mut current = fields;
    let mut path = Vec::new();
    let mut segments = name.split('.').peekable();
    while let Some(segment) = segments.next() {
        let f = current
            .iter()
            .find(|f| f.name == segment)
            .ok_or_else(|| format!("unknown field `{name}`"))?;
        path.push(f.name.clone());
        let last = segments.peek().is_none();
        match &f.kind {
            FieldKind::Array { .. } if !last => {
                return Err(format!(
                    "`{name}` is inside a list; filtering inside lists isn't supported yet"
                ));
            }
            FieldKind::Group { fields } if !last => current = fields,
            _ if !last => return Err(format!("unknown field `{name}`")),
            kind => {
                let kind = value_kind(kind).ok_or_else(|| format!("`{name}` can't be filtered"))?;
                return Ok((Target::Field { path, kind }, Some(f)));
            }
        }
    }
    Err(format!("unknown field `{name}`"))
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

fn scalar(target: &Target, field: Option<&Field>, v: &Value) -> Result<Scalar, String> {
    let kind = field.map(|f| &f.kind);
    match (target.kind(), kind) {
        (ValueKind::List, Some(FieldKind::Number { .. })) => {
            let n = match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.parse::<f64>().ok(),
                _ => None,
            };
            n.filter(|n| n.is_finite())
                .map(Scalar::Number)
                .ok_or_else(|| "expected a number".into())
        }
        (ValueKind::Number, _) => {
            let n = match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.parse::<f64>().ok(),
                _ => None,
            };
            n.filter(|n| n.is_finite())
                .map(Scalar::Number)
                .ok_or_else(|| "expected a number".into())
        }
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
    let (target, _) = resolve(&fields, name).map_err(|m| err("sort", m))?;
    if target.kind() == ValueKind::List {
        return Err(err(
            "sort",
            format!("`{name}` holds a list and can't be sorted"),
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
            Value::Number(n) => Some(Scalar::Number(n.as_f64()?)),
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
            {"name": "scores", "type": "number", "hasMany": true}
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
                Operand::One(Scalar::Number(5.0))
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
                "where[links.url][equals]=1",
                "where[links.url][equals]: `links.url` is inside a list; filtering inside lists isn't supported yet",
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
                "where[title][shout]: unknown operator; use equals, not_equals, in, not_in, gt, gte, lt, lte, like, contains or exists",
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
            value: Some(Scalar::Number(9.5)),
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
                Operand::One(Scalar::Number(3.0))
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
}
