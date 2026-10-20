//! Access rules: who may read and write documents and fields. Rules live in the schema,
//! are checked when it's saved, and compile per caller into allow, deny or a filter.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::FieldError;
use crate::auth::Action;
use crate::query::{Filter, Lookup, check_target, parse_filter_with};
use crate::schema::{CollectionKind, CollectionSchema, Field, FieldKind, SchemaError, data_fields};

/// One access rule, as stored in the schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Value", into = "Value")]
pub enum Rule {
    Bool(bool),
    /// The caller has one of these roles.
    Role(Vec<String>),
    /// The caller is (or isn't) signed in.
    SignedIn(bool),
    And(Vec<Rule>),
    Or(Vec<Rule>),
    /// A `where` filter the document must match, as written (with shorthand and `$user`).
    Where(Map<String, Value>),
}

impl TryFrom<Value> for Rule {
    type Error = String;

    fn try_from(v: Value) -> Result<Self, String> {
        parse_rule(&v)
    }
}

impl From<Rule> for Value {
    fn from(rule: Rule) -> Value {
        let list = |rules: Vec<Rule>| rules.into_iter().map(Value::from).collect::<Vec<_>>();
        match rule {
            Rule::Bool(b) => b.into(),
            Rule::Role(mut roles) if roles.len() == 1 => json!({"role": roles.remove(0)}),
            Rule::Role(roles) => json!({"role": roles}),
            Rule::SignedIn(b) => json!({"signedIn": b}),
            Rule::And(rules) => json!({"and": list(rules)}),
            Rule::Or(rules) => json!({"or": list(rules)}),
            Rule::Where(w) => Value::Object(w),
        }
    }
}

/// `access` on a collection. A global takes only `read` and `update`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionAccess {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete: Option<Rule>,
}

/// `access` on a field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldAccess {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<Rule>,
}

/// Reads a rule. An object with the single key `role` (a name or list of names),
/// `signedIn` (a boolean), `and` or `or` (a list of rules) is that rule; any other object
/// is a `where` filter, checked against the schema by `check_rules`. So a filter on a field
/// named `role` spells out its operator: `{"role": {"equals": "editor"}}`.
pub fn parse_rule(v: &Value) -> Result<Rule, String> {
    const SHAPES: &str = "a rule is true, false, {\"role\": …}, {\"signedIn\": …}, {\"and\": […]}, {\"or\": […]} or a where filter";
    let Value::Object(obj) = v else {
        return v.as_bool().map(Rule::Bool).ok_or_else(|| SHAPES.into());
    };
    let mut entries = obj.iter();
    let single = match (entries.next(), entries.next()) {
        (Some(entry), None) => entry,
        (None, _) => return Err(SHAPES.into()),
        _ => return Ok(Rule::Where(obj.clone())),
    };
    match single {
        (k, Value::String(role)) if k == "role" => Ok(Rule::Role(vec![role.clone()])),
        (k, Value::Array(roles)) if k == "role" => roles
            .iter()
            .map(|r| r.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .filter(|roles| !roles.is_empty())
            .map(Rule::Role)
            .ok_or_else(|| "role takes a role name or a list of them".into()),
        (k, Value::Bool(b)) if k == "signedIn" => Ok(Rule::SignedIn(*b)),
        (k, Value::Array(rules)) if k == "and" || k == "or" => {
            if rules.is_empty() {
                return Err(format!("{k} needs at least one rule"));
            }
            let rules = rules
                .iter()
                .enumerate()
                .map(|(i, r)| parse_rule(r).map_err(|e| format!("{k}.{i}: {e}")))
                .collect::<Result<_, _>>()?;
            Ok(if k == "and" {
                Rule::And(rules)
            } else {
                Rule::Or(rules)
            })
        }
        _ => Ok(Rule::Where(obj.clone())),
    }
}

/// Who's asking.
#[derive(Debug, Clone, Copy, Default)]
pub struct Caller<'a> {
    pub signed_in: bool,
    /// Admins pass every rule, so nobody can lock themselves out.
    pub admin: bool,
    pub role: Option<&'a str>,
    /// What `$user.<name>` reads: `id`, `role` and the profile fields.
    pub user: Option<&'a Map<String, Value>>,
}

/// A rule's answer for one caller.
#[derive(Debug, Clone, PartialEq)]
pub enum Compiled {
    Allow,
    Deny,
    /// Allowed for the documents that match.
    Filter(Filter),
}

impl Compiled {
    fn from_bool(allowed: bool) -> Self {
        if allowed { Self::Allow } else { Self::Deny }
    }
}

/// Refused if any part is; otherwise the parts' filters, all of which must match.
fn all(parts: impl IntoIterator<Item = Compiled>) -> Compiled {
    let mut filters = Vec::new();
    for part in parts {
        match part {
            Compiled::Deny => return Compiled::Deny,
            Compiled::Allow => {}
            Compiled::Filter(f) => filters.push(f),
        }
    }
    match filters.len() {
        0 => Compiled::Allow,
        1 => Compiled::Filter(filters.remove(0)),
        _ => Compiled::Filter(Filter::All(filters)),
    }
}

/// Allowed if any part is; otherwise the parts' filters, any of which may match.
fn any(parts: impl IntoIterator<Item = Compiled>) -> Compiled {
    let mut filters = Vec::new();
    for part in parts {
        match part {
            Compiled::Allow => return Compiled::Allow,
            Compiled::Deny => {}
            Compiled::Filter(f) => filters.push(f),
        }
    }
    match filters.len() {
        0 => Compiled::Deny,
        1 => Compiled::Filter(filters.remove(0)),
        _ => Compiled::Filter(Filter::Any(filters)),
    }
}

/// What `rule` means for `caller` in a collection with `fields`. Admins pass every rule.
/// A filter condition whose `$user` value is missing (always so when signed out), or
/// doesn't fit its field, matches nothing; the second is logged, since it's a mistake in
/// the rule rather than in the request.
pub fn compile(rule: &Rule, caller: &Caller, fields: &[Field], lookup: Lookup) -> Compiled {
    if caller.admin {
        return Compiled::Allow;
    }
    match rule {
        Rule::Bool(b) => Compiled::from_bool(*b),
        Rule::Role(roles) => {
            Compiled::from_bool(caller.role.is_some_and(|r| roles.iter().any(|x| x == r)))
        }
        Rule::SignedIn(b) => Compiled::from_bool(caller.signed_in == *b),
        Rule::And(rules) => all(rules.iter().map(|r| compile(r, caller, fields, lookup))),
        Rule::Or(rules) => any(rules.iter().map(|r| compile(r, caller, fields, lookup))),
        Rule::Where(group) => compile_group(group, caller, fields, lookup),
    }
}

/// A `where` condition: field, operator and value.
type Condition<'a> = (&'a str, &'a str, &'a Value);

/// The conditions in a `where` group, as `(field, operator, value)` with shorthand values
/// read as `equals`, and its nested `and`/`or` groups.
fn split(group: &Map<String, Value>) -> (Vec<Condition<'_>>, Vec<(&str, &Value)>) {
    let mut conds = Vec::new();
    let mut groups = Vec::new();
    for (key, value) in group {
        match (key.as_str(), value) {
            ("and" | "or", _) => groups.push((key.as_str(), value)),
            // An empty operator object falls through to `equals {}`, which matches nothing.
            (_, Value::Object(ops)) if !ops.is_empty() => {
                conds.extend(ops.iter().map(|(op, v)| (key.as_str(), op.as_str(), v)))
            }
            _ => conds.push((key.as_str(), "equals", value)),
        }
    }
    (conds, groups)
}

/// The members of a nested `and`/`or`, which `where` takes as a list or an object.
fn members(v: &Value) -> Option<Vec<&Map<String, Value>>> {
    let items: Vec<&Value> = match v {
        Value::Array(items) => items.iter().collect(),
        Value::Object(m) => m.values().collect(),
        _ => return None,
    };
    items
        .into_iter()
        .map(|m| m.as_object().filter(|m| !m.is_empty()))
        .collect::<Option<Vec<_>>>()
        .filter(|ms| !ms.is_empty())
}

/// Compiled one condition at a time, so a `$user` value that can't be used rules out only
/// its own condition.
// ponytail: the parser's MAX_CONDITIONS and MAX_DEPTH apply per condition here, not to the
// whole rule. Rules are written by admins and serde bounds JSON depth; count conditions
// across the rule in `check_rules` if rules ever come from less trusted hands.
fn compile_group(
    group: &Map<String, Value>,
    caller: &Caller,
    fields: &[Field],
    lookup: Lookup,
) -> Compiled {
    let (conds, groups) = split(group);
    let conds = conds.into_iter().map(|(name, op, value)| {
        let Some(value) = substitute(op, value, caller.user) else {
            return Compiled::Deny;
        };
        let one = json!({ name: { op: value } });
        match parse_filter_with(fields, &one, lookup) {
            Ok(f) => Compiled::Filter(f),
            Err(e) => {
                tracing::warn!("an access rule's condition matches nothing for this caller: {e}");
                Compiled::Deny
            }
        }
    });
    let groups = groups.into_iter().map(|(key, v)| {
        let Some(members) = members(v) else {
            return Compiled::Deny;
        };
        let parts = members
            .into_iter()
            .map(|m| compile_group(m, caller, fields, lookup));
        if key == "and" { all(parts) } else { any(parts) }
    });
    all(conds.chain(groups))
}

/// Operators that would read a `$user` value as something other than one value to
/// compare: `near` as a point and distances, `exists` as a flag.
const NO_USER_VALUES: [&str; 2] = ["exists", "near"];

/// The caller's value for `s` when it's a `$user.<name>` reference: `Some(None)` when the
/// caller has none. Empty text and empty lists count as none, so `like` can't match all.
fn user_value<'a>(s: &str, user: Option<&'a Map<String, Value>>) -> Option<Option<&'a Value>> {
    let name = s.strip_prefix("$user.")?;
    Some(user.and_then(|u| u.get(name)).filter(|v| match v {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }))
}

/// `value` with each `$user.<name>` replaced by the caller's, or `None` when the caller
/// has no such value. A `$user` value is always one value, or a list's members: `in`
/// gets a list, so text with commas in it is never split into several values.
fn substitute(op: &str, value: &Value, user: Option<&Map<String, Value>>) -> Option<Value> {
    let refs = user_refs(value);
    if refs.is_empty() {
        return Some(value.clone());
    }
    if NO_USER_VALUES.contains(&op) {
        return None;
    }
    let items = match value {
        Value::Array(items) => items.as_slice(),
        _ if matches!(op, "in" | "not_in") => std::slice::from_ref(value),
        // One `$user` value, used as it is.
        Value::String(s) => return user_value(s, user).flatten().cloned(),
        _ => return Some(value.clone()),
    };
    let mut out = Vec::new();
    for item in items {
        match item.as_str().and_then(|s| user_value(s, user)) {
            None => out.push(item.clone()),
            Some(found) => match found? {
                Value::Array(members) => out.extend(members.iter().cloned()),
                v => out.push(v.clone()),
            },
        }
    }
    Some(Value::Array(out))
}

/// The `$user.<name>` references in a condition's value.
fn user_refs(value: &Value) -> Vec<&str> {
    match value {
        Value::String(s) => s.strip_prefix("$user.").into_iter().collect(),
        Value::Array(items) => items.iter().flat_map(user_refs).collect(),
        _ => Vec::new(),
    }
}

/// Checks a schema's rules before it's stored: their syntax (already parsed), that roles
/// exist, that `$user.<name>` names `id`, `role` or one of `user_fields`, that filter fields
/// and operators exist, and that create and field rules are static.
pub fn check_rules(
    schema: &CollectionSchema,
    user_fields: &[Field],
    roles: &[&str],
    lookup: Lookup,
) -> Result<(), SchemaError> {
    let user_fields = data_fields(user_fields);
    let checker = Checker {
        fields: &schema.fields,
        user_fields: &user_fields,
        roles,
        lookup,
    };
    if let Some(a) = &schema.access {
        for (action, rule) in [
            ("read", &a.read),
            ("create", &a.create),
            ("update", &a.update),
            ("delete", &a.delete),
        ] {
            let Some(rule) = rule else { continue };
            let fail = |m: String| SchemaError::InvalidOption(format!("access.{action}: {m}"));
            if schema.kind == CollectionKind::Global && matches!(action, "create" | "delete") {
                return Err(fail("globals have only read and update rules".into()));
            }
            let fixed = (action == "create").then_some(
                "can use only roles, signedIn, true and false, since there's no document to compare yet",
            );
            checker.rule(rule, fixed).map_err(fail)?;
        }
    }
    checker.fields(&schema.fields, "", "fields")
}

/// Whether any of the schema's rules, on it or on a field at any depth, names `role`.
pub fn names_role(schema: &CollectionSchema, role: &str) -> bool {
    fn in_rule(rule: &Rule, role: &str) -> bool {
        match rule {
            Rule::Role(roles) => roles.iter().any(|r| r == role),
            Rule::And(rules) | Rule::Or(rules) => rules.iter().any(|r| in_rule(r, role)),
            _ => false,
        }
    }
    // Saving refuses rules on layout fields, so the data fields hold every field rule.
    fn in_fields(fields: &[Field], role: &str) -> bool {
        fields.iter().any(|f| {
            f.access.as_ref().is_some_and(|a| {
                [&a.read, &a.create, &a.update]
                    .into_iter()
                    .flatten()
                    .any(|r| in_rule(r, role))
            }) || in_fields(children(f), role)
        })
    }
    schema.access.as_ref().is_some_and(|a| {
        [&a.read, &a.create, &a.update, &a.delete]
            .into_iter()
            .flatten()
            .any(|r| in_rule(r, role))
    }) || in_fields(&data_fields(&schema.fields), role)
}

struct Checker<'a> {
    fields: &'a [Field],
    user_fields: &'a [Field],
    roles: &'a [&'a str],
    lookup: Lookup<'a>,
}

impl Checker<'_> {
    /// `fixed` is the complaint when the rule must be static but holds a filter.
    fn rule(&self, rule: &Rule, fixed: Option<&str>) -> Result<(), String> {
        match rule {
            Rule::Bool(_) | Rule::SignedIn(_) => Ok(()),
            Rule::Role(roles) => match roles.iter().find(|r| !self.roles.contains(&r.as_str())) {
                Some(r) => Err(format!("there's no role `{r}`")),
                None => Ok(()),
            },
            Rule::And(rules) | Rule::Or(rules) => {
                rules.iter().try_for_each(|r| self.rule(r, fixed))
            }
            Rule::Where(group) => match fixed {
                Some(complaint) => Err(complaint.into()),
                None => self.group(group),
            },
        }
    }

    fn group(&self, group: &Map<String, Value>) -> Result<(), String> {
        let (conds, groups) = split(group);
        for (key, v) in groups {
            members(v)
                .ok_or_else(|| format!("{key} takes a list of conditions"))?
                .into_iter()
                .try_for_each(|m| self.group(m))?;
        }
        for (name, value) in group {
            if value.as_object().is_some_and(Map::is_empty) {
                return Err(format!("`{name}` needs an operator like equals"));
            }
        }
        for (name, op, value) in conds {
            let refs = user_refs(value);
            if let Some(bad) = refs.iter().find(|r| {
                !matches!(**r, "id" | "role") && !self.user_fields.iter().any(|f| f.name == **r)
            }) {
                return Err(format!("`$user.{bad}` isn't id, role or a users field"));
            }
            // `substitute` hands a list to `in` and `not_in` only; anything else gets one value.
            for f in refs
                .iter()
                .filter_map(|r| self.user_fields.iter().find(|f| f.name == *r))
            {
                let list = matches!(
                    &f.kind,
                    FieldKind::Text { has_many: true, .. }
                        | FieldKind::Number { has_many: true, .. }
                        | FieldKind::Select { multiple: true, .. }
                ) || matches!(&f.kind, FieldKind::Relationship { has_many: true, relation_to, .. }
                    if !relation_to.is_polymorphic());
                if list && !matches!(op, "in" | "not_in") {
                    return Err(format!(
                        "`$user.{}` holds a list, so it works only with in or not_in",
                        f.name
                    ));
                }
                if !list && !f.kind.is_indexable() {
                    return Err(format!(
                        "`$user.{}` isn't one value or a list of them, so a rule can't compare with it",
                        f.name
                    ));
                }
            }
            let parse = |v: &Value| {
                parse_filter_with(self.fields, &json!({ name: { op: v } }), self.lookup)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            };
            if refs.is_empty() {
                parse(value)?;
                continue;
            }
            if NO_USER_VALUES.contains(&op) {
                return Err("`$user` values can't be used with exists or near".into());
            }
            check_target(self.fields, name, op, self.lookup).map_err(|e| e.to_string())?;
            // The literal members of a list beside `$user` values are known now.
            if let Value::Array(items) = value {
                let literals: Vec<&Value> = items
                    .iter()
                    .filter(|v| !v.as_str().is_some_and(|s| s.starts_with("$user.")))
                    .collect();
                if !literals.is_empty() {
                    parse(&json!(literals))?;
                }
            }
        }
        Ok(())
    }

    /// Field rules, at any depth: static, never on layout fields, and only `read` on fields
    /// that are never written. `loc` names layout fields, which have no name.
    fn fields(&self, fields: &[Field], prefix: &str, loc: &str) -> Result<(), SchemaError> {
        for (i, f) in fields.iter().enumerate() {
            let path = if prefix.is_empty() {
                f.name.clone()
            } else {
                format!("{prefix}.{}", f.name)
            };
            let here = format!("{loc}.{i}");
            if let Some(a) = &f.access {
                let label = if f.name.is_empty() {
                    here.clone()
                } else {
                    path.clone()
                };
                let fail = |message: String| SchemaError::InvalidField {
                    field: label.clone(),
                    message,
                };
                if f.kind.is_layout() {
                    return Err(fail(
                        "layout fields hold no data, so access rules go on the fields inside"
                            .into(),
                    ));
                }
                let written = f.virtual_path.is_none() && !matches!(f.kind, FieldKind::Join { .. });
                for (action, rule) in [
                    ("read", &a.read),
                    ("create", &a.create),
                    ("update", &a.update),
                ] {
                    let Some(rule) = rule else { continue };
                    if !written && action != "read" {
                        return Err(fail(format!(
                            "access.{action}: virtual and join fields are never written, so they take only a read rule"
                        )));
                    }
                    self.rule(
                        rule,
                        Some("field rules can use only roles, signedIn, true and false"),
                    )
                    .map_err(|m| fail(format!("access.{action}: {m}")))?;
                }
            }
            match &f.kind {
                FieldKind::Group { fields } | FieldKind::Array { fields, .. } => {
                    self.fields(fields, &path, &format!("{here}.fields"))?
                }
                FieldKind::Row { fields } | FieldKind::Collapsible { fields, .. } => {
                    self.fields(fields, prefix, &format!("{here}.fields"))?
                }
                FieldKind::Tabs { tabs } => {
                    for (t, tab) in tabs.iter().enumerate() {
                        let loc = format!("{here}.tabs.{t}.fields");
                        match &tab.name {
                            Some(name) => {
                                let inner = if prefix.is_empty() {
                                    name.clone()
                                } else {
                                    format!("{prefix}.{name}")
                                };
                                self.fields(&tab.fields, &inner, &loc)?
                            }
                            None => self.fields(&tab.fields, prefix, &loc)?,
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Whether `f`'s rule for `action` lets the caller do it. A field with no rule for the
/// action adds nothing to the collection's rule. Field rules are static, so they come to
/// allow or deny; a filter (which saving refuses) counts as deny.
pub fn field_allows(f: &Field, action: Action, caller: &Caller) -> bool {
    let Some(a) = &f.access else {
        return true;
    };
    let rule = match action {
        Action::Read => &a.read,
        Action::Create => &a.create,
        Action::Update | Action::UpdateGlobal => &a.update,
        Action::Delete | Action::Schemas | Action::Users => return true,
    };
    rule.as_ref()
        .is_none_or(|r| compile(r, caller, &[], &|_| None) == Compiled::Allow)
}

fn children(f: &Field) -> &[Field] {
    match &f.kind {
        FieldKind::Group { fields } | FieldKind::Array { fields, .. } => fields,
        _ => &[],
    }
}

/// What the caller may do with each of `fields` (data fields), at any depth, by dotted path
/// (a list's fields without a row index): `[read, create, update]`. A field inside one the
/// caller can't read, create or update can't be either. An update replaces a list whole
/// (as `unwritable` counts it), so a list and everything in it can be updated only when all
/// of it can. Join and virtual fields are filled on read, so they're never written.
pub fn field_answers(fields: &[Field], caller: &Caller) -> Vec<(String, [bool; 3])> {
    fn updatable(fields: &[Field], caller: &Caller) -> bool {
        fields
            .iter()
            .all(|f| field_allows(f, Action::Update, caller) && updatable(children(f), caller))
    }
    fn walk(
        fields: &[Field],
        prefix: &str,
        within: [bool; 3],
        caller: &Caller,
        out: &mut Vec<(String, [bool; 3])>,
    ) {
        for f in fields {
            let path = if prefix.is_empty() {
                f.name.clone()
            } else {
                format!("{prefix}.{}", f.name)
            };
            let [read, create, update] = within;
            let mut answer = [
                read && field_allows(f, Action::Read, caller),
                create && field_allows(f, Action::Create, caller),
                update && field_allows(f, Action::Update, caller),
            ];
            if matches!(f.kind, FieldKind::Array { .. }) {
                answer[2] = answer[2] && updatable(children(f), caller);
            }
            if matches!(f.kind, FieldKind::Join { .. }) || f.virtual_path.is_some() {
                (answer[1], answer[2]) = (false, false);
            }
            out.push((path.clone(), answer));
            walk(children(f), &path, answer, caller, out);
        }
    }
    let mut out = Vec::new();
    walk(fields, "", [true; 3], caller, &mut out);
    out
}

/// `fields` (data fields) without those the caller can't read, at any depth: all that
/// filters and sorts may name. Borrows when nothing is hidden.
pub fn readable_fields<'f>(fields: &'f [Field], caller: &Caller) -> Cow<'f, [Field]> {
    fn hides(fields: &[Field], caller: &Caller) -> bool {
        fields
            .iter()
            .any(|f| !field_allows(f, Action::Read, caller) || hides(children(f), caller))
    }
    if !hides(fields, caller) {
        return Cow::Borrowed(fields);
    }
    Cow::Owned(
        fields
            .iter()
            .filter(|f| field_allows(f, Action::Read, caller))
            .map(|f| {
                let mut f = f.clone();
                if let FieldKind::Group { fields } | FieldKind::Array { fields, .. } = &mut f.kind {
                    *fields = readable_fields(fields, caller).into_owned();
                }
                f
            })
            .collect(),
    )
}

/// `schema` with only the fields the caller can read.
pub fn readable_schema(schema: &CollectionSchema, caller: &Caller) -> CollectionSchema {
    schema.with_fields(readable_fields(schema.data(), caller).into_owned())
}

/// Removes every field the caller can't read from `doc` of `fields` (data fields), at any
/// depth: inside groups and in each list row.
pub fn strip_unreadable(fields: &[Field], doc: &mut Map<String, Value>, caller: &Caller) {
    if let Cow::Owned(visible) = readable_fields(fields, caller) {
        strip_to(fields, &visible, doc);
    }
}

/// Removes from `doc` of `fields` what `visible` (`fields` as `readable_fields` pruned them)
/// leaves out, at any depth, without compiling a rule: worked out once, used per document.
pub fn strip_to(fields: &[Field], visible: &[Field], doc: &mut Map<String, Value>) {
    for f in fields {
        let Some(v) = visible.iter().find(|v| v.name == f.name) else {
            doc.remove(&f.name);
            continue;
        };
        // Something may be hidden at any depth below, so every container is walked.
        let (all, kept) = (children(f), children(v));
        if all.is_empty() {
            continue;
        }
        match doc.get_mut(&f.name) {
            Some(Value::Object(inner)) => strip_to(all, kept, inner),
            Some(Value::Array(rows)) => {
                for row in rows.iter_mut().filter_map(Value::as_object_mut) {
                    strip_to(all, kept, row);
                }
            }
            _ => {}
        }
    }
}

/// Walks `path` (dotted, as validation and references name fields) in `fields` (data
/// fields): `Some(false)` at the first field the caller can't read, `Some(true)` when every
/// field along it is readable, `None` when it names no field. A list's row index is
/// skipped, and what follows a field that holds no fields (a link's `value`, a list
/// item's index, a key inside `json`) is inside its value.
fn walk_path(fields: &[Field], path: &str, caller: &Caller) -> Option<bool> {
    let mut current = fields;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        let f = current.iter().find(|f| f.name == segment)?;
        if !field_allows(f, Action::Read, caller) {
            return Some(false);
        }
        match &f.kind {
            FieldKind::Group { fields } => current = fields,
            FieldKind::Array { fields, .. } => {
                segments.next_if(|s| s.parse::<usize>().is_ok());
                current = fields;
            }
            _ => return Some(true),
        }
    }
    Some(true)
}

/// Whether `path` names a field the caller can read through every field along it.
pub fn path_readable(fields: &[Field], path: &str, caller: &Caller) -> bool {
    walk_path(fields, path, caller) == Some(true)
}

/// Whether `path` names a field in `fields`, readable or not.
pub fn path_exists(fields: &[Field], path: &str) -> bool {
    let anyone = Caller {
        admin: true,
        ..Caller::default()
    };
    walk_path(fields, path, &anyone) == Some(true)
}

/// What validation tells the caller, `errors` about `fields`, without naming or describing
/// a field they can't read: those become one entry with no path. Errors about the document
/// as a whole, or a field that doesn't exist, are theirs to see.
pub fn visible_errors(
    fields: &[Field],
    errors: Vec<FieldError>,
    caller: &Caller,
) -> Vec<FieldError> {
    let (hidden, mut out): (Vec<_>, Vec<_>) = errors
        .into_iter()
        .partition(|e| walk_path(fields, &e.path, caller) == Some(false));
    if !hidden.is_empty() {
        out.push(FieldError {
            path: String::new(),
            message: "a field you can't see needs fixing; ask someone who can see it".into(),
        });
    }
    out
}

/// The fields in `input` the caller may not write with `action` (create or update), by
/// path. Sending a field counts as writing it, whatever the value. An update replaces a
/// list whole, and a group sent as anything but an object, so every field inside counts.
pub fn unwritable(
    fields: &[Field],
    input: &Map<String, Value>,
    action: Action,
    caller: &Caller,
) -> Vec<String> {
    fn push(out: &mut Vec<String>, path: String) {
        if !out.contains(&path) {
            out.push(path);
        }
    }
    fn within(
        fields: &[Field],
        prefix: &str,
        action: Action,
        caller: &Caller,
        out: &mut Vec<String>,
    ) {
        for f in fields {
            let path = format!("{prefix}.{}", f.name);
            if field_allows(f, action, caller) {
                within(children(f), &path, action, caller, out);
            } else {
                push(out, path);
            }
        }
    }
    fn walk(
        fields: &[Field],
        input: &Map<String, Value>,
        prefix: &str,
        action: Action,
        caller: &Caller,
        out: &mut Vec<String>,
    ) {
        for f in fields {
            let Some(value) = input.get(&f.name) else {
                continue;
            };
            let path = if prefix.is_empty() {
                f.name.clone()
            } else {
                format!("{prefix}.{}", f.name)
            };
            if !field_allows(f, action, caller) {
                push(out, path);
                continue;
            }
            match (&f.kind, value) {
                (FieldKind::Group { fields }, Value::Object(inner)) => {
                    walk(fields, inner, &path, action, caller, out)
                }
                // `field_answers` reports a list as updatable only as a whole, to match.
                (FieldKind::Group { fields } | FieldKind::Array { fields, .. }, _)
                    if action != Action::Create =>
                {
                    within(fields, &path, action, caller, out)
                }
                (FieldKind::Array { fields, .. }, Value::Array(rows)) => {
                    for row in rows.iter().filter_map(Value::as_object) {
                        walk(fields, row, &path, action, caller, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    if !caller.admin {
        walk(&data_fields(fields), input, "", action, caller, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Change, Renames, diff};
    use crate::query::parse_filter_with;
    use serde_json::json;

    fn posts() -> CollectionSchema {
        schema(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "status", "type": "select", "options": ["draft", "published"]},
            {"name": "price", "type": "number"},
            {"name": "team", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "users"},
            {"name": "seo", "type": "group", "fields": [{"name": "title", "type": "text"}]},
            {"name": "links", "type": "array", "fields": [{"name": "url", "type": "url"}]},
            {"type": "tabs", "tabs": [{"name": "meta", "label": "Meta", "fields": [{"name": "note", "type": "text"}]}]},
            {"type": "row", "fields": [{"name": "summary", "type": "text"}]},
            {"name": "byline", "type": "text", "virtual": "author.name"}
        ]}))
    }

    fn users() -> CollectionSchema {
        schema(json!({"slug": "users", "fields": [
            {"name": "name", "type": "text"},
            {"name": "team", "type": "text"},
            {"name": "level", "type": "number"}
        ]}))
    }

    fn schema(v: Value) -> CollectionSchema {
        serde_json::from_value(v).unwrap()
    }

    fn lookup(slug: &str) -> Option<CollectionSchema> {
        (slug == "users").then(users)
    }

    fn rule(v: Value) -> Rule {
        parse_rule(&v).unwrap()
    }

    /// The filter `where` would build from `v`, to compare compiled rules against.
    fn filter(v: Value) -> Compiled {
        Compiled::Filter(parse_filter_with(&posts().fields, &v, &lookup).unwrap())
    }

    fn run(r: Value, caller: &Caller) -> Compiled {
        compile(&rule(r), caller, &posts().fields, &lookup)
    }

    fn signed_in(role: &str) -> Caller<'_> {
        Caller {
            signed_in: true,
            role: Some(role),
            ..Caller::default()
        }
    }

    const ROLES: &[&str] = &["admin", "editor", "author", "viewer"];

    #[test]
    fn hidden_fields_are_stripped_however_deep() {
        let ed = json!({"read": {"role": "editor"}});
        let pair =
            json!([{"name": "x", "type": "text", "access": ed}, {"name": "y", "type": "text"}]);
        let s = schema(json!({"slug": "deep", "fields": [
            {"name": "gg", "type": "group", "fields": [
                {"name": "g", "type": "group", "fields": pair}
            ]},
            {"name": "ag", "type": "array", "fields": [
                {"name": "g", "type": "group", "fields": pair}
            ]},
            {"name": "ga", "type": "group", "fields": [
                {"name": "a", "type": "array", "fields": pair}
            ]},
            {"type": "tabs", "tabs": [{"name": "tab", "label": "Tab", "fields": [
                {"name": "g", "type": "group", "fields": pair}
            ]}]},
            {"type": "collapsible", "label": "C", "fields": [
                {"name": "cg", "type": "group", "fields": [
                    {"type": "row", "fields": [{"name": "g", "type": "group", "fields": pair}]}
                ]}
            ]}
        ]}));
        let xy = json!({"x": "x", "y": "y"});
        let doc = json!({"gg": {"g": xy}, "ag": [{"g": xy}, {"g": xy}], "ga": {"a": [xy, xy]},
                         "tab": {"g": xy}, "cg": {"g": xy}});
        let y = json!({"y": "y"});
        let hidden = json!({"gg": {"g": y}, "ag": [{"g": y}, {"g": y}], "ga": {"a": [y, y]},
                            "tab": {"g": y}, "cg": {"g": y}});
        for (caller, want) in [(signed_in("viewer"), &hidden), (signed_in("editor"), &doc)] {
            let mut by_rule = doc.as_object().unwrap().clone();
            strip_unreadable(s.data(), &mut by_rule, &caller);
            assert_eq!(&Value::Object(by_rule), want);
            let visible = readable_fields(s.data(), &caller);
            let mut by_shape = doc.as_object().unwrap().clone();
            strip_to(s.data(), &visible, &mut by_shape);
            assert_eq!(&Value::Object(by_shape), want);
        }
    }

    #[test]
    fn field_rules_strip_and_refuse_at_any_depth() {
        let ed = json!({"read": {"role": "editor"}, "update": {"role": "editor"}});
        let s = schema(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "secret", "type": "text", "access": {"read": false, "create": false}},
            {"name": "seo", "type": "group", "fields": [
                {"name": "title", "type": "text"},
                {"name": "note", "type": "text", "access": ed}
            ]},
            {"name": "links", "type": "array", "fields": [
                {"name": "label", "type": "text"},
                {"name": "url", "type": "text", "access": ed}
            ]},
            {"type": "tabs", "tabs": [{"name": "meta", "label": "Meta", "fields": [
                {"name": "code", "type": "text", "access": ed}
            ]}]},
            {"type": "row", "fields": [{"name": "inRow", "type": "text", "access": ed}]}
        ]}));
        let viewer = signed_in("viewer");
        let editor = signed_in("editor");
        let admin = Caller {
            admin: true,
            ..signed_in("admin")
        };
        let doc = json!({"title": "T", "secret": "s", "seo": {"title": "S", "note": "n"},
            "links": [{"label": "a", "url": "u"}, {"label": "b"}], "meta": {"code": "c"},
            "inRow": "r", "id": "x"});
        let stripped = |caller: &Caller| {
            let mut d = doc.as_object().unwrap().clone();
            strip_unreadable(s.data(), &mut d, caller);
            Value::Object(d)
        };
        assert_eq!(
            stripped(&viewer),
            json!({"title": "T", "seo": {"title": "S"}, "links": [{"label": "a"}, {"label": "b"}],
                   "meta": {}, "id": "x"})
        );
        let mut all_but_secret = doc.clone();
        all_but_secret.as_object_mut().unwrap().remove("secret");
        assert_eq!(stripped(&editor), all_but_secret);
        assert_eq!(stripped(&admin), doc, "Admins pass every rule");

        // What filters may name.
        let names = |fields: &[Field]| -> Vec<String> {
            fn walk(fields: &[Field], prefix: &str, out: &mut Vec<String>) {
                for f in fields {
                    let path = format!("{prefix}{}", f.name);
                    out.push(path.clone());
                    walk(children(f), &format!("{path}."), out);
                }
            }
            let mut out = Vec::new();
            walk(fields, "", &mut out);
            out
        };
        assert_eq!(
            names(&readable_fields(s.data(), &viewer)),
            ["title", "seo", "seo.title", "links", "links.label", "meta"]
        );
        assert!(matches!(
            readable_fields(s.data(), &admin),
            Cow::Borrowed(_)
        ));
        assert_eq!(
            names(&readable_schema(&s, &editor).fields).len(),
            names(s.data()).len() - 1
        );
        for (path, readable) in [
            ("title", true),
            ("secret", false),
            ("seo.title", true),
            ("seo.note", false),
            ("links.0.label", true),
            ("links.1.url", false),
            ("meta.code", false),
            ("inRow", false),
            // Unknown names aren't readable; what's inside a value or past a row index is.
            ("nope", false),
            ("seo.nope", false),
            ("links.x.url", false),
            ("links.0", true),
            ("title.0", true),
            ("", false),
        ] {
            assert_eq!(path_readable(s.data(), path, &viewer), readable, "{path}");
            assert_eq!(
                path_readable(s.data(), path, &admin),
                path_exists(s.data(), path)
            );
        }
        assert!(!path_exists(s.data(), "nope"));
        assert!(path_exists(s.data(), "links.1.url"));

        // Stripping by a shape worked out once matches compiling the rules each time.
        let visible = readable_fields(s.data(), &viewer);
        let mut d = doc.as_object().unwrap().clone();
        strip_to(s.data(), &visible, &mut d);
        assert_eq!(Value::Object(d), stripped(&viewer));

        // Validation errors: what the caller can't read folds into one unnamed entry.
        let e = |path: &str, message: &str| FieldError {
            path: path.into(),
            message: message.into(),
        };
        let errors = vec![
            e("", "expected a JSON object"),
            e("title", "expected text"),
            e("seo.note", "must be at most 3 characters"),
            e("links.1.url", "is required"),
            e("nope", "unknown field"),
        ];
        let generic = e(
            "",
            "a field you can't see needs fixing; ask someone who can see it",
        );
        assert_eq!(
            visible_errors(s.data(), errors.clone(), &viewer),
            [
                errors[0].clone(),
                errors[1].clone(),
                errors[4].clone(),
                generic
            ]
        );
        assert_eq!(visible_errors(s.data(), errors.clone(), &editor), errors);

        // Writes: by name, at any depth.
        let refused = |input: Value, action: Action, caller: &Caller| {
            unwritable(&s.fields, input.as_object().unwrap(), action, caller)
        };
        let input = json!({"title": "T", "secret": null, "seo": {"note": "n"},
            "links": [{"label": "a"}, {"url": "u"}, {"url": "v"}], "inRow": "r"});
        assert_eq!(
            refused(input.clone(), Action::Create, &viewer),
            ["secret"],
            "only secret has a create rule"
        );
        assert_eq!(
            refused(input.clone(), Action::Update, &viewer),
            ["seo.note", "links.url", "inRow"]
        );
        assert!(refused(input.clone(), Action::Update, &editor).is_empty());
        assert!(refused(input, Action::Create, &admin).is_empty());
        // An update that would replace what the caller can't change.
        assert_eq!(
            refused(
                json!({"seo": null, "links": [{"label": "b"}]}),
                Action::Update,
                &viewer
            ),
            ["seo.note", "links.url"]
        );
        assert!(refused(json!({"seo": {"title": "x"}}), Action::Update, &viewer).is_empty());
        assert_eq!(
            refused(
                json!({"meta": {"code": "c"}}),
                Action::UpdateGlobal,
                &viewer
            ),
            ["meta.code"]
        );
    }

    fn check(s: &CollectionSchema) -> Result<(), String> {
        check_rules(s, &users().fields, ROLES, &lookup).map_err(|e| e.to_string())
    }

    /// `posts` with `access` set on the collection.
    fn with_access(access: Value) -> CollectionSchema {
        let mut v = serde_json::to_value(posts()).unwrap();
        v["access"] = access;
        schema(v)
    }

    /// `posts` with `access` set on the field at `fields[index]`.
    fn with_field_access(pointer: &str, access: Value) -> CollectionSchema {
        let mut v = serde_json::to_value(posts()).unwrap();
        *v.pointer_mut(pointer).unwrap() = json!(access);
        schema(v)
    }

    #[test]
    fn rules_parse_shorthand_and_combinators() {
        assert_eq!(rule(json!(true)), Rule::Bool(true));
        assert_eq!(rule(json!(false)), Rule::Bool(false));
        assert_eq!(
            rule(json!({"role": "editor"})),
            Rule::Role(vec!["editor".into()])
        );
        assert_eq!(
            rule(json!({"role": ["editor", "author"]})),
            Rule::Role(vec!["editor".into(), "author".into()])
        );
        assert_eq!(rule(json!({"signedIn": true})), Rule::SignedIn(true));
        assert_eq!(
            rule(json!({"and": [true, {"or": [{"signedIn": false}, {"role": "viewer"}]}]})),
            Rule::And(vec![
                Rule::Bool(true),
                Rule::Or(vec![
                    Rule::SignedIn(false),
                    Rule::Role(vec!["viewer".into()])
                ]),
            ])
        );
        // Any other object is a filter, kept as written.
        let Rule::Where(w) = rule(json!({"status": "published"})) else {
            panic!("expected a filter")
        };
        assert_eq!(Value::Object(w), json!({"status": "published"}));
        // A `role` that isn't a role name or list of them is a filter on a field named role.
        assert!(matches!(
            rule(json!({"role": {"equals": "editor"}})),
            Rule::Where(_)
        ));

        for (bad, message) in [
            (
                json!(1),
                "a rule is true, false, {\"role\": …}, {\"signedIn\": …}, {\"and\": […]}, {\"or\": […]} or a where filter",
            ),
            (
                json!("editor"),
                "a rule is true, false, {\"role\": …}, {\"signedIn\": …}, {\"and\": […]}, {\"or\": […]} or a where filter",
            ),
            (
                json!({}),
                "a rule is true, false, {\"role\": …}, {\"signedIn\": …}, {\"and\": […]}, {\"or\": […]} or a where filter",
            ),
            (json!({"and": []}), "and needs at least one rule"),
            (
                json!({"role": []}),
                "role takes a role name or a list of them",
            ),
            (
                json!({"role": ["editor", 1]}),
                "role takes a role name or a list of them",
            ),
            (
                json!({"or": [true, 2]}),
                "or.1: a rule is true, false, {\"role\": …}, {\"signedIn\": …}, {\"and\": […]}, {\"or\": […]} or a where filter",
            ),
        ] {
            assert_eq!(parse_rule(&bad).unwrap_err(), message, "{bad}");
        }

        // Rules round-trip through the schema, and unknown actions are refused.
        let s = with_access(json!({
            "read": {"or": [{"role": "editor"}, {"status": "published"}]},
            "create": {"role": ["editor", "author"]},
        }));
        let compact = s.to_compact_json();
        assert_eq!(
            compact["access"],
            json!({
                "read": {"or": [{"role": "editor"}, {"status": "published"}]},
                "create": {"role": ["editor", "author"]},
            })
        );
        assert_eq!(schema(compact), s);
        let mut raw = serde_json::to_value(posts()).unwrap();
        raw["access"] = json!({"remove": true});
        assert!(serde_json::from_value::<CollectionSchema>(raw).is_err());
        raw = serde_json::to_value(posts()).unwrap();
        raw["fields"][0]["access"] = json!({"delete": true});
        assert!(serde_json::from_value::<CollectionSchema>(raw).is_err());
    }

    #[test]
    fn compile_simplifies_roles_and_sign_in() {
        let anon = Caller::default();
        let editor = signed_in("editor");
        let author = signed_in("author");
        let admin = Caller {
            admin: true,
            ..signed_in("admin")
        };
        use Compiled::{Allow, Deny};

        assert_eq!(run(json!({"role": "editor"}), &editor), Allow);
        assert_eq!(run(json!({"role": "editor"}), &author), Deny);
        assert_eq!(run(json!({"role": "editor"}), &anon), Deny);
        assert_eq!(run(json!({"signedIn": true}), &anon), Deny);
        assert_eq!(run(json!({"signedIn": true}), &author), Allow);
        assert_eq!(run(json!({"signedIn": false}), &anon), Allow);
        // Admins pass every rule.
        assert_eq!(run(json!(false), &admin), Allow);
        assert_eq!(run(json!({"status": "draft"}), &admin), Allow);

        // Shorthand is `equals`.
        let published = filter(json!({"status": {"equals": "published"}}));
        assert_eq!(run(json!({"status": "published"}), &anon), published);

        // And drops what's allowed and is refused by any refusal; Or the other way round.
        let both = json!({"and": [{"role": "editor"}, {"status": "published"}]});
        assert_eq!(run(both.clone(), &editor), published);
        assert_eq!(run(both, &anon), Deny);
        let either = json!({"or": [{"role": "editor"}, {"status": "published"}]});
        assert_eq!(run(either.clone(), &editor), Allow);
        assert_eq!(run(either, &anon), published);
        assert_eq!(
            run(json!({"and": [true, {"signedIn": false}]}), &anon),
            Allow
        );
        assert_eq!(run(json!({"or": [false, {"role": "viewer"}]}), &anon), Deny);

        // Several filters stay together.
        let Compiled::Filter(Filter::Any(parts)) = run(
            json!({"or": [{"status": "published"}, {"price": {"gt": 10}}]}),
            &anon,
        ) else {
            panic!("expected an or of two filters")
        };
        assert_eq!(parts.len(), 2);
        // A where group's own `and`/`or`, and relationship paths, work as in `where`.
        assert_eq!(
            run(
                json!({"author.name": "Ann", "or": [{"status": "draft"}, {"price": {"lt": 5}}]}),
                &anon
            ),
            filter(json!({
                "author.name": {"equals": "Ann"},
                "or": [{"status": {"equals": "draft"}}, {"price": {"lt": 5}}]
            }))
        );
    }

    #[test]
    fn user_values_substitute_and_signed_out_matches_nothing() {
        let id = "0192f0c1-7a3e-7c3e-8a1b-2c3d4e5f6a7b".to_string();
        let profile = json!({"id": id, "role": "author", "team": "red", "level": 3});
        let profile = profile.as_object().unwrap();
        let me = Caller {
            user: Some(profile),
            ..signed_in("author")
        };
        let anon = Caller::default();

        assert_eq!(
            run(json!({"author": "$user.id"}), &me),
            filter(json!({"author": {"equals": id}}))
        );
        assert_eq!(
            run(
                json!({"title": {"not_equals": "$user.role"}, "price": {"gte": "$user.level"}}),
                &me
            ),
            filter(json!({"title": {"not_equals": "author"}, "price": {"gte": 3}}))
        );
        assert_eq!(
            run(json!({"team": {"in": ["$user.team", "blue"]}}), &me),
            filter(json!({"team": {"in": ["red", "blue"]}}))
        );

        // Signed out, a `$user` condition matches nothing: its group is refused, and an
        // `or` keeps only its other branches.
        assert_eq!(run(json!({"author": "$user.id"}), &anon), Compiled::Deny);
        assert_eq!(
            run(json!({"author": "$user.id", "status": "published"}), &anon),
            Compiled::Deny
        );
        assert_eq!(
            run(
                json!({"or": [{"author": "$user.id"}, {"status": "published"}]}),
                &anon
            ),
            filter(json!({"status": {"equals": "published"}}))
        );
        assert_eq!(
            run(
                json!({"status": "draft", "or": [{"team": "$user.team"}, {"price": {"lt": 5}}]}),
                &anon
            ),
            filter(json!({"and": [{"status": {"equals": "draft"}}, {"price": {"lt": 5}}]}))
        );

        // A `$user` value is always one value: a comma in it never splits it into several.
        let commas = json!({"id": id, "role": "author", "team": "red,blue", "tags": ["a", "b"]});
        let commas = Caller {
            user: commas.as_object(),
            ..signed_in("author")
        };
        assert_eq!(
            run(json!({"team": {"in": "$user.team"}}), &commas),
            filter(json!({"team": {"in": ["red,blue"]}}))
        );
        assert_eq!(
            run(
                json!({"team": {"not_in": ["$user.team", "green"]}}),
                &commas
            ),
            filter(json!({"team": {"not_in": ["red,blue", "green"]}}))
        );
        // A list value is its members.
        assert_eq!(
            run(json!({"team": {"in": "$user.tags"}}), &commas),
            filter(json!({"team": {"in": ["a", "b"]}}))
        );
        // Signed out, a list holding a `$user` value matches nothing, literals included.
        assert_eq!(
            run(json!({"team": {"in": ["$user.team", "blue"]}}), &anon),
            Compiled::Deny
        );
        // `near` and `exists` would read a `$user` value as distances or a flag.
        assert_eq!(
            run(json!({"team": {"exists": "$user.team"}}), &me),
            Compiled::Deny
        );

        // An empty value counts as missing, so `like` can't match everything.
        let empty = json!({"id": id, "role": "author", "team": ""});
        let empty = Caller {
            user: empty.as_object(),
            ..signed_in("author")
        };
        for op in ["equals", "like", "in", "not_equals"] {
            assert_eq!(
                run(json!({"team": {op: "$user.team"}}), &empty),
                Compiled::Deny,
                "{op}"
            );
        }

        // A value the user doesn't have, or that doesn't fit the field, matches nothing too.
        let bare = json!({"id": id, "role": "author"});
        let bare = Caller {
            user: bare.as_object(),
            ..signed_in("author")
        };
        assert_eq!(run(json!({"team": "$user.team"}), &bare), Compiled::Deny);
        assert_eq!(
            run(json!({"price": {"gt": "$user.team"}}), &me),
            Compiled::Deny
        );
        assert_eq!(run(json!({"status": "$user.team"}), &me), Compiled::Deny);
    }

    #[test]
    fn create_and_field_rules_must_be_static() {
        assert_eq!(
            check(&with_access(json!({"create": {"role": "author"}}))),
            Ok(())
        );
        assert_eq!(
            check(&with_access(json!({"create": {"or": [{"role": "author"}, {"status": "draft"}]}}))),
            Err("access.create: can use only roles, signedIn, true and false, since there's no document to compare yet".into())
        );
        assert_eq!(
            check(&with_field_access(
                "/fields/0/access",
                json!({"read": {"role": "editor"}, "update": {"signedIn": true}})
            )),
            Ok(())
        );
        assert_eq!(
            check(&with_field_access("/fields/0/access", json!({"read": {"status": "published"}}))),
            Err("field `title`: access.read: field rules can use only roles, signedIn, true and false".into())
        );
        // A field inside a row is still top-level data.
        assert_eq!(
            check(&with_field_access(
                "/fields/8/fields/0/access",
                json!({"read": false})
            )),
            Ok(())
        );
        assert_eq!(
            check(&with_field_access("/fields/8/access", json!({"read": false}))),
            Err("field `fields.8`: layout fields hold no data, so access rules go on the fields inside".into())
        );
        assert_eq!(
            check(&with_field_access(
                "/fields/5/fields/0/access",
                json!({"read": false})
            )),
            Ok(())
        );
        // Nested fields take rules at any depth: list rows and named tabs too.
        assert_eq!(
            check(&with_field_access(
                "/fields/6/fields/0/access",
                json!({"update": {"role": "editor"}})
            )),
            Ok(())
        );
        assert_eq!(
            check(&with_field_access(
                "/fields/7/tabs/0/fields/0/access",
                json!({"read": {"signedIn": true}})
            )),
            Ok(())
        );
        assert_eq!(
            check(&with_field_access("/fields/6/fields/0/access", json!({"read": {"status": "draft"}}))),
            Err("field `links.url`: access.read: field rules can use only roles, signedIn, true and false".into())
        );
        assert_eq!(
            check(&with_field_access(
                "/fields/9/access",
                json!({"read": {"role": "editor"}})
            )),
            Ok(())
        );
        assert_eq!(
            check(&with_field_access("/fields/9/access", json!({"update": false}))),
            Err("field `byline`: access.update: virtual and join fields are never written, so they take only a read rule".into())
        );

        // Globals have only read and update rules.
        let global = |access: Value| {
            schema(json!({"slug": "site", "kind": "global", "access": access,
                          "fields": [{"name": "title", "type": "text"}]}))
        };
        assert_eq!(
            check(&global(
                json!({"read": true, "update": {"title": {"exists": true}}})
            )),
            Ok(())
        );
        for action in ["create", "delete"] {
            assert_eq!(
                check(&global(json!({action: {"role": "editor"}}))),
                Err(format!(
                    "access.{action}: globals have only read and update rules"
                ))
            );
        }
    }

    #[test]
    fn rules_are_checked_against_fields_roles_and_user_fields() {
        let read = |r: Value| check(&with_access(json!({"read": r})));
        assert_eq!(read(json!({"role": ["editor", "author"]})), Ok(()));
        assert_eq!(
            read(json!({"or": [{"role": "ghost"}, true]})),
            Err("access.read: there's no role `ghost`".into())
        );
        assert_eq!(
            check(&with_field_access(
                "/fields/0/access",
                json!({"create": {"role": "ghost"}})
            )),
            Err("field `title`: access.create: there's no role `ghost`".into())
        );

        // Filters are checked as `where` would check them, relationship paths included.
        assert_eq!(
            read(json!({"author.name": "Ann", "price": {"gt": 3}})),
            Ok(())
        );
        assert!(
            read(json!({"nope": "x"}))
                .unwrap_err()
                .starts_with("access.read: where[nope][equals]: ")
        );
        assert!(
            read(json!({"author.nope": "x"}))
                .unwrap_err()
                .starts_with("access.read: where[author.nope][equals]: ")
        );
        assert_eq!(
            read(json!({"price": {"like": "1"}})),
            Err("access.read: where[price][like]: like can't be used on this field".into())
        );
        assert_eq!(
            read(json!({"status": "nope"})),
            Err("access.read: where[status][equals]: expected one of: draft, published".into())
        );
        assert_eq!(
            read(json!({"status": {}})),
            Err("access.read: `status` needs an operator like equals".into())
        );
        assert_eq!(
            read(json!({"status": "draft", "or": [{}]})),
            Err("access.read: or takes a list of conditions".into())
        );

        // `$user` values: the name must exist, and the field and operator must fit; the
        // value itself is only known per request, so any field type is fine.
        for ok in [
            json!({"author": "$user.id"}),
            json!({"title": "$user.role"}),
            json!({"status": "$user.team"}),
            json!({"price": {"in": ["$user.level", 4]}}),
            json!({"author.name": {"not_equals": "$user.name"}}),
        ] {
            assert_eq!(read(ok.clone()), Ok(()), "{ok}");
        }
        assert_eq!(
            read(json!({"status": {"in": ["$user.team", "bogus"]}})),
            Err("access.read: where[status][in]: expected one of: draft, published".into())
        );
        assert_eq!(
            read(json!({"team": {"exists": "$user.team"}})),
            Err("access.read: `$user` values can't be used with exists or near".into())
        );
        assert_eq!(
            read(json!({"team": "$user.nope"})),
            Err("access.read: `$user.nope` isn't id, role or a users field".into())
        );
        assert_eq!(
            read(json!({"price": {"like": "$user.team"}})),
            Err("access.read: where[price][like]: like can't be used on this field".into())
        );
        assert!(
            read(json!({"nope": "$user.id"}))
                .unwrap_err()
                .starts_with("access.read: where[nope][equals]: ")
        );
    }

    #[test]
    fn rule_changes_are_option_changes() {
        let old = posts();
        let new = with_access(json!({"read": {"status": "published"}}));
        let d = diff(&old, &new, &Renames::new()).unwrap();
        assert_eq!(d.changes, vec![Change::CollectionOptionsChanged]);
        assert!(!d.is_destructive());

        let new = with_field_access("/fields/0/access", json!({"read": {"role": "editor"}}));
        let d = diff(&old, &new, &Renames::new()).unwrap();
        assert_eq!(
            d.changes,
            vec![Change::OptionsChanged {
                path: "title".into()
            }]
        );
        assert!(!d.is_destructive());

        // A field inside a row is compared as data too.
        let new = with_field_access("/fields/8/fields/0/access", json!({"read": false}));
        let d = diff(&old, &new, &Renames::new()).unwrap();
        assert_eq!(
            d.changes,
            vec![Change::OptionsChanged {
                path: "summary".into()
            }]
        );
    }
}
