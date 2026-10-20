//! What the caller may do, per request: each collection's access rules compiled for them,
//! or the role defaults where a collection has no rule for an action, always narrowed by an
//! API key's scopes. A join's schema `where` and the relationships inside rule filters are
//! deliberately left unguarded: Admins write them, so they can't be used to probe.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use axum::extract::{FromRequestParts, MatchedPath, RawPathParams};
use axum::http::request::Parts;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use std::borrow::Cow;

use tachyon_core::access::{
    Caller, Compiled, Rule, compile, path_readable, readable_fields, readable_schema,
    strip_unreadable, unwritable,
};
use tachyon_core::auth::{Action, Decision, Scope, decide, decide_anonymous, scopes_allow};
use tachyon_core::query::{Filter, Target, parse_filter};
use tachyon_core::{CollectionKind, CollectionSchema, Field};
use tachyon_db::DbError;
use tachyon_db::auth::USERS;
use tachyon_db::collections::Collection;
use tachyon_db::documents::{self, Actor, Document};
use tachyon_db::query::get_many_readable;
use uuid::Uuid;

use crate::auth::principal::Principal;
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
}

/// Which collections' rules a request can reach, to decide whether it needs the profile.
pub(crate) enum Reach<'a> {
    /// Nothing: the route names no collection that exists.
    Nothing,
    /// This collection or global (every action), and the read rules of every collection it
    /// links or joins to, however indirectly.
    From(&'a Collection),
    /// Every collection's read rule, as for references, which can come from anywhere.
    Everything,
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

/// Who `principal` is, for static rules: field rules never read `$user`.
pub(crate) fn caller_of(principal: &Principal) -> Caller<'_> {
    let identity = principal.identity();
    Caller {
        signed_in: identity.is_some(),
        admin: identity.is_some_and(|i| i.role == "admin"),
        role: identity.map(|i| i.role.as_str()),
        user: None,
    }
}

/// A 403 naming each field in `input` that `caller` may not write with `action`.
pub(crate) fn refuse_fields(
    c: &Collection,
    input: &Value,
    action: Action,
    caller: &Caller,
) -> Result<(), ApiError> {
    let Value::Object(input) = input else {
        return Ok(());
    };
    let refused = unwritable(c.schema.data(), input, action, caller);
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
    let start = match reach {
        Reach::Nothing => return false,
        Reach::Everything => return schemas.all().iter().any(|c| read(c)),
        Reach::From(c) => c,
    };
    let any_action = start.schema.access.as_ref().is_some_and(|a| {
        [&a.read, &a.create, &a.update, &a.delete]
            .into_iter()
            .flatten()
            .any(uses_profile)
    });
    if any_action {
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

    /// `doc` of `c` without the fields the caller can't read.
    pub(crate) fn strip(&self, c: &Collection, doc: &mut Value) {
        if let Value::Object(doc) = doc {
            strip_unreadable(c.schema.data(), doc, &self.caller());
        }
    }

    /// Whether the caller can read every field along `path` in `c`.
    pub(crate) fn reads(&self, c: &Collection, path: &str) -> bool {
        path_readable(c.schema.data(), path, &self.caller())
    }

    /// The fields of `c` the caller can read: all a filter or sort may name.
    pub(crate) fn readable_fields<'c>(&self, c: &'c Collection) -> Cow<'c, [Field]> {
        readable_fields(c.schema.data(), &self.caller())
    }

    /// Another collection's schema as the caller can read it, for filters through links.
    pub(crate) fn lookup(&self, slug: &str) -> Option<CollectionSchema> {
        self.schemas
            .lookup(slug)
            .map(|s| readable_schema(&s, &self.caller()))
    }

    /// A 403 naming each field in `input` the caller may not write with `action`.
    pub(crate) fn writes(
        &self,
        c: &Collection,
        input: &Value,
        action: Action,
    ) -> Result<(), ApiError> {
        refuse_fields(c, input, action, &self.caller())
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
        // Accounts keep their own rules: Admins see every profile, others only their own.
        if slug == USERS {
            return match self.principal.identity() {
                _ if action != Action::Read => Compiled::Deny,
                _ if self.by_role(Action::Users, USERS) == Decision::Allow => Compiled::Allow,
                None => Compiled::Deny,
                Some(i) => parse_filter(c.schema.data(), &json!({"id": {"equals": i.user_id}}))
                    .map_or(Compiled::Deny, Compiled::Filter),
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
        let read = match self.allows(Action::Read, c) {
            Compiled::Deny => return Err(ApiError::NotFound),
            Compiled::Allow => None,
            Compiled::Filter(f) => Some(f),
        };
        let write = match self.allows(action, c) {
            // Refused for the whole collection, so a 403 before the readability check is
            // intended: it says nothing about this document.
            Compiled::Deny => return Err(self.refuse()),
            Compiled::Allow => None,
            Compiled::Filter(f) => Some(f),
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
            Target::Row { list, inner } => Target::Row {
                list,
                inner: Box::new(self.guard_target(*inner)),
            },
            other => other,
        }
    }
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
}
