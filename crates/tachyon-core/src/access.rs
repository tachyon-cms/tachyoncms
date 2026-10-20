//! Access rules: who may read and write documents and fields. Rules live in the schema,
//! are checked when it's saved, and compile per caller into allow, deny or a filter.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

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
fn compile_group(
    group: &Map<String, Value>,
    caller: &Caller,
    fields: &[Field],
    lookup: Lookup,
) -> Compiled {
    let (conds, groups) = split(group);
    let conds = conds.into_iter().map(|(name, op, value)| {
        let Some(value) = substitute(value, caller.user) else {
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

/// `value` with each `$user.<name>` replaced by the caller's, or `None` when the caller
/// has no such value.
fn substitute(value: &Value, user: Option<&Map<String, Value>>) -> Option<Value> {
    match value {
        Value::String(s) => match s.strip_prefix("$user.") {
            Some(name) => user?.get(name).filter(|v| !v.is_null()).cloned(),
            None => Some(value.clone()),
        },
        Value::Array(items) => items.iter().map(|v| substitute(v, user)).collect(),
        _ => Some(value.clone()),
    }
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
    checker.fields(&schema.fields, "", "fields", false)
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
            let checked = if refs.is_empty() {
                parse_filter_with(self.fields, &json!({ name: { op: value } }), self.lookup)
                    .map(|_| ())
            } else {
                check_target(self.fields, name, op, self.lookup)
            };
            checked.map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Field rules: static, on top-level data fields only, and only `read` on fields that
    /// are never written. `loc` names layout fields, which have no name.
    fn fields(
        &self,
        fields: &[Field],
        prefix: &str,
        loc: &str,
        nested: bool,
    ) -> Result<(), SchemaError> {
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
                if nested {
                    return Err(fail("only top-level fields can have access rules".into()));
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
                    self.fields(fields, &path, &format!("{here}.fields"), true)?
                }
                FieldKind::Row { fields } | FieldKind::Collapsible { fields, .. } => {
                    self.fields(fields, prefix, &format!("{here}.fields"), nested)?
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
                                self.fields(&tab.fields, &inner, &loc, true)?
                            }
                            None => self.fields(&tab.fields, prefix, &loc, nested)?,
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
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
                "/fields/6/fields/0/access",
                json!({"read": false})
            )),
            Ok(())
        );
        assert_eq!(
            check(&with_field_access("/fields/6/access", json!({"read": false}))),
            Err("field `fields.6`: layout fields hold no data, so access rules go on the fields inside".into())
        );
        assert_eq!(
            check(&with_field_access(
                "/fields/5/fields/0/access",
                json!({"read": false})
            )),
            Err("field `seo.title`: only top-level fields can have access rules".into())
        );
        assert_eq!(
            check(&with_field_access(
                "/fields/7/access",
                json!({"read": {"role": "editor"}})
            )),
            Ok(())
        );
        assert_eq!(
            check(&with_field_access("/fields/7/access", json!({"update": false}))),
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
        let new = with_field_access("/fields/6/fields/0/access", json!({"read": false}));
        let d = diff(&old, &new, &Renames::new()).unwrap();
        assert_eq!(
            d.changes,
            vec![Change::OptionsChanged {
                path: "summary".into()
            }]
        );
    }
}
