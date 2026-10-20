//! What the caller may do, per request: each collection's access rules compiled for them,
//! or the role defaults where a collection has no rule for an action, always narrowed by an
//! API key's scopes.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tachyon_core::CollectionKind;
use tachyon_core::access::{Caller, Compiled, Rule, compile};
use tachyon_core::auth::{Action, Decision, decide, decide_anonymous, scopes_allow};
use tachyon_core::query::{Filter, Target, parse_filter};
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
}

impl FromRequestParts<AppState> for Access {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let principal = parts
            .extensions
            .get::<Principal>()
            .cloned()
            .unwrap_or(Principal::Anonymous);
        Access::new(state, principal).await
    }
}

/// Both filters, either, or neither.
pub(crate) fn both(a: Option<Filter>, b: Option<Filter>) -> Option<Filter> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Filter::All(vec![a, b])),
        (a, b) => a.or(b),
    }
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

impl Access {
    /// Loads the caller's profile only when a collection's rule reads one of its fields.
    // ponytail: scans every schema's rules per signed-in request; keep a flag on the schema
    // snapshot if there are ever thousands of collections.
    pub(crate) async fn new(state: &AppState, principal: Principal) -> Result<Self, ApiError> {
        let user = match principal.identity() {
            None => None,
            Some(identity) => {
                let needs_profile = state.schemas.all().iter().any(|c| {
                    c.schema.access.as_ref().is_some_and(|a| {
                        [&a.read, &a.create, &a.update, &a.delete]
                            .into_iter()
                            .flatten()
                            .any(uses_profile)
                    })
                });
                let mut user = Map::new();
                if needs_profile && let Some(users) = state.schemas.get(USERS) {
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
        })
    }

    pub(crate) fn schemas(&self) -> &Schemas {
        &self.schemas
    }

    fn caller(&self) -> Caller<'_> {
        let identity = self.principal.identity();
        Caller {
            signed_in: identity.is_some(),
            admin: identity.is_some_and(|i| i.role == "admin"),
            role: identity.map(|i| i.role.as_str()),
            user: self.user.as_ref(),
        }
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
    pub(crate) fn readable(&self) -> HashMap<Uuid, Option<Filter>> {
        self.schemas
            .all()
            .iter()
            .filter(|c| c.schema.kind == CollectionKind::Collection)
            .filter_map(|c| match self.allows(Action::Read, c) {
                Compiled::Allow => Some((c.id, None)),
                Compiled::Filter(f) => Some((c.id, Some(f))),
                Compiled::Deny => None,
            })
            .collect()
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
