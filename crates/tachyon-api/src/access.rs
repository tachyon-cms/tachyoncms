//! What the caller may do, per request: each collection's access rules compiled for them,
//! or the role defaults where a collection has no rule for an action, always narrowed by an
//! API key's scopes. A join's schema `where` and the relationships inside rule filters are
//! deliberately left unguarded: Admins write them, so they can't be used to probe.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex, OnceLock};

use axum::Json;
use axum::extract::{FromRequestParts, MatchedPath, RawPathParams, State};
use axum::http::request::Parts;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tachyon_core::access::{
    Caller, Compiled, Rule, compile, field_answers, has_blocks, path_exists, readable_fields,
    readable_rows, strip_to, unwritable, visible_errors,
};
use tachyon_core::auth::{Action, Decision, Scope, decide, decide_anonymous, scopes_allow};
use tachyon_core::query::{Filter, Target};
use tachyon_core::{CollectionKind, CollectionSchema, Field};
use tachyon_db::DbError;
use tachyon_db::auth::USERS;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Actor, Document};
use tachyon_db::query::get_many_readable;
use uuid::Uuid;

use crate::auth::authorize;
use crate::auth::principal::Principal;
use crate::auth::users::{account_answers, manages_users};
use crate::schemas::Schemas;
use crate::{ApiError, AppState};

pub(crate) struct Access {
    principal: Principal,
    /// What `$user.<name>` reads: `id`, `role` and, when some rule uses them, the profile
    /// fields. `None` when signed out.
    user: Option<Map<String, Value>>,
    schemas: Arc<Schemas>,
    /// `readable()`, worked out once per request.
    readable: OnceLock<HashMap<Uuid, Option<Filter>>>,
    /// Each collection (by ID and schema version) as the caller can read it, worked out
    /// once per request.
    visible: Mutex<HashMap<(Uuid, i32), Shape>>,
}

/// A collection with the fields the caller can't read removed, or `None` for all of it.
type Pruned = Option<Arc<CollectionSchema>>;

/// A collection as the caller can read it.
#[derive(Clone)]
struct Shape {
    /// What filters, sorts and paths may name: a name inside blocks only when every block
    /// type with it lets the caller read it.
    named: Pruned,
    /// What documents show, each block row by its own type; `None` when nothing is hidden.
    rows: Option<Arc<Vec<Field>>>,
    /// Holds blocks, whose rows of a removed block type every read cuts down.
    blocks: bool,
}

/// A collection's fields as the caller can read them: all of them, or a pruned copy
/// shared for the request.
pub(crate) enum Visible<'c> {
    All(&'c [Field]),
    Some(Arc<CollectionSchema>),
}

impl Deref for Visible<'_> {
    type Target = [Field];

    fn deref(&self) -> &[Field] {
        match self {
            Visible::All(fields) => fields,
            Visible::Some(schema) => schema.data(),
        }
    }
}

/// Why an update or delete is refused before any document is looked at.
pub(crate) enum WriteRefusal {
    /// The caller can't read the collection at all.
    Hidden,
    /// The caller may not do this to any of it.
    Refused,
}

/// The read and write filters a write must match on the stored document (`None`: anywhere).
pub(crate) type WriteRules = Result<(Option<Filter>, Option<Filter>), WriteRefusal>;

/// Which collections' rules a request can reach, to decide whether it needs the profile.
pub(crate) enum Reach<'a> {
    /// Nothing: the route names no collection that exists.
    Nothing,
    /// This collection or global (every action), and the read rules of every collection it
    /// links or joins to, however indirectly.
    From(&'a Collection),
    /// Every collection's read rule, as for references, which can come from anywhere.
    Everything,
    /// Every rule of every collection, as `/api/access` answers for all of them.
    EveryRule,
}

impl FromRequestParts<AppState> for Access {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let principal = parts
            .extensions
            .get::<Principal>()
            .cloned()
            .unwrap_or(Principal::Anonymous);
        // The collection or global the route names: `{collection}` or `{slug}`.
        let named = RawPathParams::from_request_parts(parts, state)
            .await
            .ok()
            .and_then(|params| {
                params
                    .iter()
                    .find(|(k, _)| *k == "collection" || *k == "slug")
                    .and_then(|(_, slug)| state.schemas.get(slug))
            });
        let references = parts
            .extensions
            .get::<MatchedPath>()
            .is_some_and(|m| m.as_str().ends_with("/references"));
        let reach = match (references, &named) {
            (true, _) => Reach::Everything,
            (false, Some(c)) => Reach::From(c),
            (false, None) => Reach::Nothing,
        };
        Access::new(state, principal, reach).await
    }
}

/// Both filters, either, or neither.
pub(crate) fn both(a: Option<Filter>, b: Option<Filter>) -> Option<Filter> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Filter::All(vec![a, b])),
        (a, b) => a.or(b),
    }
}

/// Someone signed in with `role`, for static rules: field rules never read `$user`.
pub(crate) fn signed_in_as(role: &str) -> Caller<'_> {
    Caller {
        signed_in: true,
        admin: role == "admin",
        role: Some(role),
        user: None,
    }
}

/// Who `principal` is, for static rules.
pub(crate) fn caller_of(principal: &Principal) -> Caller<'_> {
    principal
        .identity()
        .map_or_else(Caller::default, |i| signed_in_as(&i.role))
}

/// `e`, with validation errors about fields `caller` can't read folded into one that
/// names none of them. `doc` is the document validated (`merged`), when there is one, so a
/// block row's own type decides what in it they can read.
pub(crate) fn hide_errors(
    c: &Collection,
    e: ApiError,
    caller: &Caller,
    doc: Option<&Map<String, Value>>,
) -> ApiError {
    match e {
        ApiError::Validation(errors) => {
            ApiError::Validation(visible_errors(c.schema.data(), errors, caller, doc))
        }
        e => e,
    }
}

/// A 403 naming each field in `input` that `caller` may not write with `action`, over
/// `existing` (the stored document) when it's known.
pub(crate) fn refuse_fields(
    c: &Collection,
    input: &Value,
    action: Action,
    caller: &Caller,
    existing: Option<&Map<String, Value>>,
) -> Result<(), ApiError> {
    let Value::Object(input) = input else {
        return Ok(());
    };
    let refused = unwritable(c.schema.data(), input, action, caller, existing);
    if refused.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = refused.iter().map(|p| format!("`{p}`")).collect();
    Err(ApiError::Forbidden(format!(
        "You don't have permission to write {}.",
        names.join(", ")
    )))
}

/// A filter that matches nothing.
fn nothing() -> Filter {
    Filter::Any(Vec::new())
}

/// Whether `rule` reads a `$user` value other than `id` and `role`, which need the profile.
fn uses_profile(rule: &Rule) -> bool {
    fn value(v: &Value) -> bool {
        match v {
            Value::String(s) => s
                .strip_prefix("$user.")
                .is_some_and(|name| name != "id" && name != "role"),
            Value::Array(items) => items.iter().any(value),
            Value::Object(m) => m.values().any(value),
            _ => false,
        }
    }
    match rule {
        Rule::And(rules) | Rule::Or(rules) => rules.iter().any(uses_profile),
        Rule::Where(w) => w.values().any(value),
        Rule::Bool(_) | Rule::Role(_) | Rule::SignedIn(_) => false,
    }
}

/// Whether a rule the request can reach reads a profile field. Rules that don't match the
/// request's collections never compile for it, and a rule missing a profile value it needed
/// fails closed anyway, so this only decides whether the lookup is worth making.
fn needs_profile(schemas: &Schemas, reach: &Reach) -> bool {
    let read = |c: &Collection| {
        c.schema
            .access
            .as_ref()
            .and_then(|a| a.read.as_ref())
            .is_some_and(uses_profile)
    };
    let any_action = |c: &Collection| {
        c.schema.access.as_ref().is_some_and(|a| {
            [&a.read, &a.create, &a.update, &a.delete]
                .into_iter()
                .flatten()
                .any(uses_profile)
        })
    };
    let start = match reach {
        Reach::Nothing => return false,
        Reach::Everything => return schemas.all().iter().any(|c| read(c)),
        Reach::EveryRule => return schemas.all().iter().any(|c| any_action(c)),
        Reach::From(c) => c,
    };
    if any_action(start) {
        return true;
    }
    // Every collection reachable through links and joins: populate, virtual fields, joins
    // and filters only ever read those.
    let links = |c: &Collection| -> Vec<String> {
        c.schema
            .relation_targets()
            .into_iter()
            .map(str::to_owned)
            .chain(c.schema.join_fields().into_iter().map(|j| j.collection))
            .collect()
    };
    let mut seen = vec![start.schema.slug.clone()];
    let mut queue = links(start);
    while let Some(slug) = queue.pop() {
        if seen.contains(&slug) {
            continue;
        }
        let Some(c) = schemas.get(&slug) else {
            continue;
        };
        if read(&c) {
            return true;
        }
        queue.extend(links(&c));
        seen.push(slug);
    }
    false
}

impl Access {
    /// Loads the caller's profile once, and only when a rule the request can reach reads one
    /// of its fields. Admins pass every rule, so theirs is never needed.
    pub(crate) async fn new(
        state: &AppState,
        principal: Principal,
        reach: Reach<'_>,
    ) -> Result<Self, ApiError> {
        let user = match principal.identity() {
            None => None,
            Some(identity) => {
                let mut user = Map::new();
                if identity.role != "admin"
                    && needs_profile(&state.schemas, &reach)
                    && let Some(users) = state.schemas.get(USERS)
                {
                    match documents::get(&state.pool, users.id, identity.user_id).await {
                        Ok(doc) => user = doc.data.0,
                        Err(DbError::NotFound) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                user.insert("id".into(), identity.user_id.to_string().into());
                user.insert("role".into(), identity.role.clone().into());
                Some(user)
            }
        };
        Ok(Self {
            principal,
            user,
            schemas: state.schemas.clone(),
            readable: OnceLock::new(),
            visible: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn schemas(&self) -> &Schemas {
        &self.schemas
    }

    pub(crate) fn caller(&self) -> Caller<'_> {
        Caller {
            user: self.user.as_ref(),
            ..caller_of(&self.principal)
        }
    }

    /// `c` as the caller can read it.
    fn shape(&self, c: &Collection) -> Shape {
        let mut visible = self.visible.lock().expect("never held across a panic");
        visible
            .entry((c.id, c.schema_version))
            .or_insert_with(|| {
                let caller = self.caller();
                let fields = c.schema.data();
                Shape {
                    named: match readable_fields(fields, &caller) {
                        Cow::Borrowed(_) => None,
                        Cow::Owned(fields) => Some(Arc::new(c.schema.with_fields(fields))),
                    },
                    rows: match readable_rows(fields, &caller) {
                        Cow::Borrowed(_) => None,
                        Cow::Owned(fields) => Some(Arc::new(fields)),
                    },
                    blocks: has_blocks(fields),
                }
            })
            .clone()
    }

    /// `c` as filters and paths may name it, when some of it is hidden from the caller.
    fn pruned(&self, c: &Collection) -> Pruned {
        self.shape(c).named
    }

    /// `doc` of `c` without the fields the caller can't read, and with rows of removed block
    /// types cut down to their `id` and `blockType`, whoever reads them.
    pub(crate) fn strip(&self, c: &Collection, doc: &mut Value) {
        let Value::Object(doc) = doc else { return };
        let shape = self.shape(c);
        match (&shape.rows, shape.blocks) {
            (Some(visible), _) => strip_to(c.schema.data(), visible, doc),
            (None, true) => strip_to(c.schema.data(), c.schema.data(), doc),
            (None, false) => {}
        }
    }

    /// Whether the caller can read every field along `path` in `c`: whether it's still
    /// there once what they can't read is pruned.
    pub(crate) fn reads(&self, c: &Collection, path: &str) -> bool {
        path_exists(&self.readable_fields(c), path)
    }

    /// The fields of `c` the caller can read: all a filter or sort may name.
    pub(crate) fn readable_fields<'c>(&self, c: &'c Collection) -> Visible<'c> {
        match self.pruned(c) {
            Some(schema) => Visible::Some(schema),
            None => Visible::All(c.schema.data()),
        }
    }

    /// Another collection's schema as the caller can read it, for filters through links.
    pub(crate) fn lookup(&self, slug: &str) -> Option<CollectionSchema> {
        let c = self
            .schemas
            .get(slug)
            .filter(|c| c.schema.kind == CollectionKind::Collection)?;
        Some(match self.pruned(&c) {
            Some(schema) => (*schema).clone(),
            None => c.schema.0.clone(),
        })
    }

    /// `e`, with validation errors about fields of `c` the caller can't read folded into
    /// one that names none of them; `doc` is the document validated, when there is one.
    pub(crate) fn hide(
        &self,
        c: &Collection,
        e: ApiError,
        doc: Option<&Map<String, Value>>,
    ) -> ApiError {
        hide_errors(c, e, &self.caller(), doc)
    }

    /// A 403 naming each field in `input` the caller may not write with `action`, over
    /// `existing` (the stored document) when it's known.
    pub(crate) fn writes(
        &self,
        c: &Collection,
        input: &Value,
        action: Action,
        existing: Option<&Map<String, Value>>,
    ) -> Result<(), ApiError> {
        refuse_fields(c, input, action, &self.caller(), existing)
    }

    /// The role default, narrowed by a key's scopes.
    fn by_role(&self, action: Action, slug: &str) -> Decision {
        match &self.principal {
            Principal::Anonymous => decide_anonymous(action),
            Principal::User { identity, .. } => decide(&identity.abilities, None, action, slug),
            Principal::Key { identity, scopes } => {
                decide(&identity.abilities, Some(scopes), action, slug)
            }
        }
    }

    fn rule<'c>(&self, action: Action, c: &'c Collection) -> Option<&'c Rule> {
        let a = c.schema.access.as_ref()?;
        match action {
            Action::Read => a.read.as_ref(),
            Action::Create => a.create.as_ref(),
            Action::Update | Action::UpdateGlobal => a.update.as_ref(),
            Action::Delete => a.delete.as_ref(),
            Action::Schemas | Action::Users => None,
        }
    }

    /// A 403 when an API key's scopes don't cover `action` on `slug`, whatever the rules say.
    pub(crate) fn scope(&self, action: Action, slug: &str) -> Result<(), ApiError> {
        match &self.principal {
            Principal::Key { scopes, .. } if !scopes_allow(scopes, action, slug) => Err(
                ApiError::Forbidden("You don't have permission to do this.".into()),
            ),
            _ => Ok(()),
        }
    }

    /// What `action` on `c` comes to for this caller: its rule, compiled, or the role default
    /// where there's none. An Author's own-only default is `Allow` here; `actor` carries it.
    pub(crate) fn allows(&self, action: Action, c: &Collection) -> Compiled {
        let slug = c.schema.slug.as_str();
        if self.scope(action, slug).is_err() {
            return Compiled::Deny;
        }
        // A profile reads like content wherever a document links to it (populate, filters,
        // virtuals, joins, references); field rules hide what's private. Accounts are
        // listed and changed only through their own routes, which check self-or-admin.
        if slug == USERS {
            return match action {
                Action::Read if self.by_role(action, USERS) != Decision::Deny => Compiled::Allow,
                _ => Compiled::Deny,
            };
        }
        match self.rule(action, c) {
            Some(rule) => {
                let lookup = |s: &str| self.schemas.lookup(s);
                compile(rule, &self.caller(), c.schema.data(), &lookup)
            }
            None if self.by_role(action, slug) == Decision::Deny => Compiled::Deny,
            None => Compiled::Allow,
        }
    }

    /// The refusal for this caller: sign in first, or not allowed.
    pub(crate) fn refuse(&self) -> ApiError {
        match self.principal {
            Principal::Anonymous => ApiError::Unauthorized("Sign in first.".into()),
            _ => ApiError::Forbidden("You don't have permission to do this.".into()),
        }
    }

    /// `action` on `c` as a whole: refused, or allowed where the filter matches (`None`:
    /// everywhere).
    pub(crate) fn permit(
        &self,
        action: Action,
        c: &Collection,
    ) -> Result<Option<Filter>, ApiError> {
        self.scope(action, &c.schema.slug)?;
        match self.allows(action, c) {
            Compiled::Allow => Ok(None),
            Compiled::Filter(f) => Ok(Some(f)),
            Compiled::Deny => Err(self.refuse()),
        }
    }

    /// Who the database records as writing, and whether the role default limits them to
    /// documents they created (only when `c` has no rule for `action`).
    pub(crate) fn actor(&self, action: Action, c: &Collection) -> Option<Actor> {
        let own_only = self.rule(action, c).is_none()
            && self.by_role(action, &c.schema.slug) == Decision::OwnOnly;
        self.principal.identity().map(|i| Actor {
            id: i.user_id,
            own_only,
        })
    }

    /// The rules an update or delete (`action`) on `c` runs under, before any document is
    /// looked at. `write_guard`, the global update and `/api/access` all decide by this.
    pub(crate) fn write_rules(&self, action: Action, c: &Collection) -> WriteRules {
        let read = match self.allows(Action::Read, c) {
            Compiled::Deny => return Err(WriteRefusal::Hidden),
            Compiled::Allow => None,
            Compiled::Filter(f) => Some(f),
        };
        let write = match self.allows(action, c) {
            Compiled::Deny => return Err(WriteRefusal::Refused),
            Compiled::Allow => None,
            Compiled::Filter(f) => Some(f),
        };
        Ok((read, write))
    }

    /// Whether the caller may create in `c`. A create rule can't depend on a document that
    /// doesn't exist yet: schemas refuse such rules when saved, and one that got through
    /// anyway refuses here.
    pub(crate) fn may_create(&self, c: &Collection) -> bool {
        self.allows(Action::Create, c) == Compiled::Allow
    }

    /// `/api/access`'s answer for `action` (update or delete) on `c`: conditional when either
    /// rule is a filter or the role default is own-only.
    fn write_answer(&self, action: Action, c: &Collection) -> Value {
        let own_only = self.actor(action, c).is_some_and(|a| a.own_only);
        match self.write_rules(action, c) {
            Err(_) => false.into(),
            Ok((None, None)) if !own_only => true.into(),
            Ok(_) => "conditional".into(),
        }
    }

    /// Each collection the caller may read, with its read filter (`None`: all of it).
    /// Globals are left out: nothing links to them.
    pub(crate) fn readable(&self) -> &HashMap<Uuid, Option<Filter>> {
        self.readable
            .get_or_init(|| self.read_filters(|c| c.schema.kind == CollectionKind::Collection))
    }

    /// `readable`, with globals too: a global can link to a document, so it can be one of
    /// that document's references.
    pub(crate) fn readable_with_globals(&self) -> HashMap<Uuid, Option<Filter>> {
        self.read_filters(|_| true)
    }

    fn read_filters(&self, which: impl Fn(&Collection) -> bool) -> HashMap<Uuid, Option<Filter>> {
        self.schemas
            .all()
            .iter()
            .filter(|c| which(c))
            .filter_map(|c| match self.allows(Action::Read, c) {
                Compiled::Allow => Some((c.id, None)),
                Compiled::Filter(f) => Some((c.id, Some(f))),
                Compiled::Deny => None,
            })
            .collect()
    }

    /// Whether the caller is an Admin whose credential no key scope narrows: a session, or
    /// a key with the `admin` scope. Only they may see everything, such as how many
    /// documents link to one.
    pub(crate) fn unscoped_admin(&self) -> bool {
        self.caller().admin
            && match &self.principal {
                Principal::Key { scopes, .. } => scopes.contains(&Scope::Admin),
                _ => true,
            }
    }

    /// Whether the caller may read `doc` of `c`, which they just wrote.
    pub(crate) async fn can_read(
        &self,
        pool: &PgPool,
        c: &Collection,
        doc: Uuid,
    ) -> Result<bool, ApiError> {
        Ok(match self.allows(Action::Read, c) {
            Compiled::Allow => true,
            Compiled::Deny => false,
            Compiled::Filter(f) => {
                let one = HashMap::from([(c.id, Some(f))]);
                !get_many_readable(pool, &[doc], &one).await?.is_empty()
            }
        })
    }

    /// Document `id` of `c`, when the caller may read it; a 404 when they may not.
    pub(crate) async fn read_one(
        &self,
        pool: &PgPool,
        c: &Collection,
        id: Uuid,
    ) -> Result<Document, ApiError> {
        self.scope(Action::Read, &c.schema.slug)?;
        let filter = match self.allows(Action::Read, c) {
            Compiled::Deny => return Err(ApiError::NotFound),
            Compiled::Allow => return Ok(documents::get(pool, c.id, id).await?),
            Compiled::Filter(f) => f,
        };
        let read = HashMap::from([(c.id, Some(filter))]);
        get_many_readable(pool, &[id], &read)
            .await?
            .pop()
            .map(|(_, doc)| doc)
            .ok_or(ApiError::NotFound)
    }

    /// The guard an update or delete of document `id` runs under: the read filter and the
    /// action's filter, both checked on the locked row. A document the caller can't read is
    /// a 404; one they can read but not change is a 403 (from the write, when it's a filter).
    pub(crate) async fn write_guard(
        &self,
        pool: &PgPool,
        action: Action,
        c: &Collection,
        id: Uuid,
    ) -> Result<Option<Filter>, ApiError> {
        self.scope(action, &c.schema.slug)?;
        let (read, write) = match self.write_rules(action, c) {
            Err(WriteRefusal::Hidden) => return Err(ApiError::NotFound),
            // Refused for the whole collection, so a 403 before the readability check is
            // intended: it says nothing about this document.
            Err(WriteRefusal::Refused) => return Err(self.refuse()),
            Ok(filters) => filters,
        };
        // Checked first so a hidden document is a 404; the guard checks it again under the
        // lock, so a change in between can only turn this into a 403.
        if let Some(f) = &read {
            let one = HashMap::from([(c.id, Some(f.clone()))]);
            if get_many_readable(pool, &[id], &one).await?.is_empty() {
                return Err(ApiError::NotFound);
            }
        }
        Ok(both(read, write))
    }

    /// `f` with every relationship it reaches through, at any depth, guarded by the linked
    /// collections' read rules, so it can't find documents by what hidden ones hold.
    pub(crate) fn guard(&self, f: Filter) -> Filter {
        match f {
            Filter::All(parts) => Filter::All(parts.into_iter().map(|p| self.guard(p)).collect()),
            Filter::Any(parts) => Filter::Any(parts.into_iter().map(|p| self.guard(p)).collect()),
            Filter::Cond(mut c) => {
                c.target = self.guard_target(c.target);
                Filter::Cond(c)
            }
        }
    }

    fn guard_target(&self, t: Target) -> Target {
        match t {
            Target::Via {
                link,
                many,
                polymorphic,
                collections,
                inner,
                ..
            } => {
                let mut guards: Vec<(String, Option<Box<Filter>>)> = collections
                    .iter()
                    .map(|slug| {
                        let read = self
                            .schemas
                            .get(slug)
                            .filter(|c| c.schema.kind == CollectionKind::Collection)
                            .map_or(Compiled::Deny, |c| self.allows(Action::Read, &c));
                        let guard = match read {
                            Compiled::Allow => None,
                            Compiled::Filter(f) => Some(Box::new(f)),
                            Compiled::Deny => Some(Box::new(nothing())),
                        };
                        (slug.clone(), guard)
                    })
                    .collect();
                // No guards would mean unguarded; with no collections nothing should match.
                if guards.is_empty() {
                    guards.push((String::new(), Some(Box::new(nothing()))));
                }
                Target::Via {
                    link,
                    many,
                    polymorphic,
                    collections,
                    inner: Box::new(self.guard_target(*inner)),
                    guards,
                }
            }
            Target::Row {
                list,
                inner,
                blocks,
            } => Target::Row {
                list,
                inner: Box::new(self.guard_target(*inner)),
                blocks,
            },
            other => other,
        }
    }
}

/// An answer for `/api/access`.
fn answer(c: &Compiled) -> Value {
    match c {
        Compiled::Allow => true.into(),
        Compiled::Deny => false.into(),
        Compiled::Filter(_) => "conditional".into(),
    }
}

/// Each field of `c` the caller may do something with, by dotted path: `{read, create,
/// update}`, or `{read, update}` for a global, which is only ever updated. `gate` is the
/// collection's own `[read, create, update]`: no field allows more than that.
fn field_map(c: &Collection, caller: &Caller, gate: [bool; 3]) -> Value {
    let global = c.schema.kind == CollectionKind::Global;
    let fields = field_answers(c.schema.data(), caller)
        .into_iter()
        .map(|(path, a)| (path, [0, 1, 2].map(|i| a[i] && gate[i])))
        .filter(|(_, [read, create, update])| *read || *update || (*create && !global))
        .map(|(path, [read, create, update])| {
            let a = if global {
                json!({"read": read, "update": update})
            } else {
                json!({"read": read, "create": create, "update": update})
            };
            (path, a)
        });
    Value::Object(fields.collect())
}

/// `GET /api/access`: what the caller may do with each collection, global and field, and
/// whether they may change schemas and manage users. Signed out is an answer too.
///
/// - Only what the caller can do something with is listed: a collection, global or field
///   whose answers would all be false is left out, so a missing key means no access.
/// - `"conditional"` means the answer depends on the document, even when every stored one
///   happens to pass, and for a global with a filter rule that hasn't been saved yet.
/// - Answers agree with what the routes enforce: update and delete are false where the
///   caller can't read at all; a field allows no more than its collection or global does;
///   a list (and everything in it) is updatable only when all of it is; join and virtual
///   fields are never written; a create rule shaped like a filter is false; a global's
///   fields have no create.
/// - For `users`, `read` is about profiles as links show them, like content; create,
///   update and delete are about the account routes, which keep to self-or-admin.
pub(crate) async fn report(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Value>, ApiError> {
    let schemas = authorize(&principal, Action::Schemas, "").is_ok();
    let users = manages_users(&principal);
    let accounts = account_answers(&principal);
    let access = Access::new(&state, principal, Reach::EveryRule).await?;
    let caller = access.caller();
    let (mut collections, mut globals) = (Map::new(), Map::new());
    for c in access.schemas().all().iter() {
        let mut a = if c.schema.slug == USERS {
            let mut a = accounts.clone();
            a["read"] = answer(&access.allows(Action::Read, c));
            a
        } else if c.schema.kind == CollectionKind::Global {
            json!({
                "read": answer(&access.allows(Action::Read, c)),
                "update": access.write_answer(Action::UpdateGlobal, c),
            })
        } else {
            json!({
                "read": answer(&access.allows(Action::Read, c)),
                "create": access.may_create(c),
                "update": access.write_answer(Action::Update, c),
                "delete": access.write_answer(Action::Delete, c),
            })
        };
        if a.as_object()
            .is_some_and(|m| m.values().all(|v| v == false))
        {
            continue;
        }
        let gate = ["read", "create", "update"].map(|k| a[k] != false && !a[k].is_null());
        a["fields"] = field_map(c, &caller, gate);
        match c.schema.kind {
            CollectionKind::Global => globals.insert(c.schema.slug.clone(), a),
            _ => collections.insert(c.schema.slug.clone(), a),
        };
    }
    Ok(Json(json!({
        "collections": collections,
        "globals": globals,
        "schemas": schemas,
        "users": users,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tachyon_core::CollectionSchema;
    use tachyon_core::auth::builtin_roles;
    use tachyon_db::auth::{Identity, NewAccount, create_account, ensure_users_collection};
    use tachyon_db::collections;

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn the_profile_loads_only_when_a_reachable_rule_reads_it(pool: PgPool) {
        let users = ensure_users_collection(&pool).await.unwrap();
        let new = NewAccount {
            email: "ann@example.com",
            name: "Ann",
            password_hash: "x",
            role: "viewer",
        };
        let me = create_account(&pool, &users, &new, false).await.unwrap();
        let schema = |v: Value| -> CollectionSchema { serde_json::from_value(v).unwrap() };
        for s in [
            json!({"slug": "boards", "access": {"read": {"title": "$user.name"}},
                   "fields": [{"name": "title", "type": "text"}]}),
            json!({"slug": "pins", "fields": [
                {"name": "board", "type": "relationship", "relationTo": "boards"}
            ]}),
            json!({"slug": "posts", "access": {"update": {"author": "$user.id"}},
                   "fields": [{"name": "author", "type": "text"}]}),
        ] {
            collections::create(&pool, &schema(s)).await.unwrap();
        }
        let state = AppState {
            pool: pool.clone(),
            schemas: Arc::new(Schemas::load(&pool).await.unwrap()),
            auth: Arc::new(crate::auth::AuthState::new(Default::default()).unwrap()),
        };
        let viewer = builtin_roles()[3].2.clone();
        let principal = Principal::User {
            identity: Identity {
                user_id: me,
                role: "viewer".into(),
                abilities: viewer,
            },
            session: Uuid::now_v7(),
            cookie: false,
        };
        let loaded = |slug: Option<&str>, reach_all: bool| {
            let (state, principal) = (state.clone(), principal.clone());
            let slug = slug.map(str::to_owned);
            async move {
                let c = slug.and_then(|s| state.schemas.get(&s));
                let reach = match (&c, reach_all) {
                    (_, true) => Reach::Everything,
                    (Some(c), _) => Reach::From(c),
                    (None, _) => Reach::Nothing,
                };
                let access = Access::new(&state, principal, reach).await.unwrap();
                let user = access.user.unwrap();
                assert_eq!(user["id"], me.to_string(), "always there");
                user.contains_key("name")
            }
        };
        assert!(loaded(Some("boards"), false).await, "its own rule reads it");
        assert!(loaded(Some("pins"), false).await, "pins link to boards");
        assert!(
            !loaded(Some("posts"), false).await,
            "$user.id needs no profile"
        );
        assert!(!loaded(None, false).await);
        assert!(
            loaded(None, true).await,
            "references can come from anywhere"
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn every_rule_loads_the_profile_for_any_action(pool: PgPool) {
        let users = ensure_users_collection(&pool).await.unwrap();
        let new = NewAccount {
            email: "ann@example.com",
            name: "Ann",
            password_hash: "x",
            role: "viewer",
        };
        let me = create_account(&pool, &users, &new, false).await.unwrap();
        // Only an update rule reads the profile, which references never need.
        let tasks = json!({"slug": "tasks", "access": {"update": {"title": "$user.name"}},
                           "fields": [{"name": "title", "type": "text"}]});
        collections::create(&pool, &serde_json::from_value(tasks).unwrap())
            .await
            .unwrap();
        let state = AppState {
            pool: pool.clone(),
            schemas: Arc::new(Schemas::load(&pool).await.unwrap()),
            auth: Arc::new(crate::auth::AuthState::new(Default::default()).unwrap()),
        };
        let principal = Principal::User {
            identity: Identity {
                user_id: me,
                role: "viewer".into(),
                abilities: builtin_roles()[3].2.clone(),
            },
            session: Uuid::now_v7(),
            cookie: false,
        };
        let loaded = |reach| {
            let (state, principal) = (state.clone(), principal.clone());
            async move {
                let access = Access::new(&state, principal, reach).await.unwrap();
                access.user.unwrap().contains_key("name")
            }
        };
        assert!(!loaded(Reach::Everything).await);
        assert!(loaded(Reach::EveryRule).await);
    }
}
