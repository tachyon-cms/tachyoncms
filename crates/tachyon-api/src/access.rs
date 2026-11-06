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
use tachyon_core::query::{DocCopy, Filter, Readable, Target};
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
    /// Whether the request works on drafts: asked for with `?draft=true`, or a write to a
    /// collection with drafts. Each linked document is then read as its draft where the
    /// caller may read that draft, else as what's published. Set once, before anything is
    /// read: the first read fixes it.
    drafts: OnceLock<bool>,
    /// `linked()`, worked out once per request.
    linked: OnceLock<HashMap<Uuid, Readable>>,
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
    // A read is of either copy, so `readDrafts` counts as reading.
    let read = |c: &Collection| {
        c.schema.access.as_ref().is_some_and(|a| {
            [&a.read, &a.read_drafts]
                .into_iter()
                .flatten()
                .any(uses_profile)
        })
    };
    let any_action = |c: &Collection| {
        c.schema.access.as_ref().is_some_and(|a| {
            [
                &a.read,
                &a.create,
                &a.update,
                &a.delete,
                &a.read_drafts,
                &a.read_versions,
                &a.publish,
            ]
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
                    match documents::get(
                        &state.pool,
                        users.id,
                        identity.user_id,
                        DocCopy::Published,
                    )
                    .await
                    {
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
            drafts: OnceLock::new(),
            linked: OnceLock::new(),
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

    /// The role default, narrowed by a key's scopes when `scoped`; otherwise what a key's
    /// owner may do, whatever its scopes.
    fn by_role_in(&self, action: Action, slug: &str, scoped: bool) -> Decision {
        match &self.principal {
            Principal::Anonymous => decide_anonymous(action),
            Principal::User { identity, .. } => decide(&identity.abilities, None, action, slug),
            Principal::Key {
                identity, scopes, ..
            } => decide(&identity.abilities, scoped.then_some(scopes), action, slug),
        }
    }

    fn rule<'c>(&self, action: Action, c: &'c Collection) -> Option<&'c Rule> {
        let a = c.schema.access.as_ref()?;
        match action {
            Action::Read => a.read.as_ref(),
            Action::Create => a.create.as_ref(),
            Action::Update | Action::UpdateGlobal => a.update.as_ref(),
            Action::Delete => a.delete.as_ref(),
            Action::ReadDrafts => a.read_drafts.as_ref(),
            Action::ReadVersions => a.read_versions.as_ref(),
            Action::Publish => a.publish.as_ref(),
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
        self.allows_in(action, c, true)
    }

    /// `allows`, or with `scoped` false what a key's owner may do, whatever its scopes:
    /// only for what publishing reads on a `:publish` key's behalf (Ruling T1-2).
    fn allows_in(&self, action: Action, c: &Collection, scoped: bool) -> Compiled {
        let slug = c.schema.slug.as_str();
        if scoped && self.scope(action, slug).is_err() {
            return Compiled::Deny;
        }
        // A profile reads like content wherever a document links to it (populate, filters,
        // virtuals, joins, references); field rules hide what's private. Accounts are
        // listed and changed only through their own routes, which check self-or-admin.
        if slug == USERS {
            return match action {
                Action::Read if self.by_role_in(action, USERS, scoped) != Decision::Deny => {
                    Compiled::Allow
                }
                _ => Compiled::Deny,
            };
        }
        match self.rule(action, c) {
            Some(rule) => {
                let lookup = |s: &str| self.schemas.lookup(s);
                compile(rule, &self.caller(), c.schema.data(), &lookup)
            }
            None if self.by_role_in(action, slug, scoped) == Decision::Deny => Compiled::Deny,
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

    /// Whether the role default limits `action` on `c` to documents the caller created
    /// (only when `c` has no rule for it). `allows` reads that as `Allow`.
    fn own_only(&self, action: Action, c: &Collection) -> bool {
        self.own_only_in(action, c, true)
    }

    /// `own_only`, scoped as `allows_in` is.
    fn own_only_in(&self, action: Action, c: &Collection, scoped: bool) -> bool {
        self.rule(action, c).is_none()
            && self.by_role_in(action, &c.schema.slug, scoped) == Decision::OwnOnly
    }

    /// Who the database records as writing, and whether the role default limits them to
    /// documents they created.
    pub(crate) fn actor(&self, action: Action, c: &Collection) -> Option<Actor> {
        let own_only = self.own_only(action, c);
        self.principal.identity().map(|i| Actor {
            id: i.user_id,
            own_only,
        })
    }

    /// Whether the request works on drafts (see `drafts`). Asking fixes the answer.
    pub(crate) fn drafts(&self) -> bool {
        *self.drafts.get_or_init(|| false)
    }

    /// Makes the request one that works on drafts. Refused once anything has been read
    /// without them, since what was read then would no longer match.
    fn use_drafts(&self) -> Result<(), ApiError> {
        match self.drafts.set(true) {
            Ok(()) => Ok(()),
            Err(_) if self.drafts() => Ok(()),
            Err(_) => {
                debug_assert!(false, "drafts turned on after a published read");
                Err(ApiError::Internal(
                    "drafts were asked for after reading began".into(),
                ))
            }
        }
    }

    /// The copy a write to `c` changes: its draft when it has drafts, which makes the
    /// request one that works on drafts.
    pub(crate) fn write_copy(&self, c: &Collection) -> Result<DocCopy, ApiError> {
        if c.schema.drafts() {
            self.use_drafts()?;
            Ok(DocCopy::Draft)
        } else {
            Ok(DocCopy::Published)
        }
    }

    /// How the caller reads `c`'s drafts, given its `read` filter: the read rule and the
    /// `readDrafts` rule both judge the draft, and an own-only right narrows it to what they
    /// created. `None` when `c` has no drafts or they may not read them. Nobody created a
    /// global, so an own-only right reaches no global's draft.
    fn draft_readable(&self, c: &Collection, read: Option<Filter>) -> Option<Readable> {
        self.draft_readable_in(c, read, true)
    }

    /// `draft_readable`, scoped as `allows_in` is.
    fn draft_readable_in(
        &self,
        c: &Collection,
        read: Option<Filter>,
        scoped: bool,
    ) -> Option<Readable> {
        if !c.schema.drafts() {
            return None;
        }
        let drafts = match self.allows_in(Action::ReadDrafts, c, scoped) {
            Compiled::Deny => return None,
            Compiled::Allow => None,
            Compiled::Filter(f) => Some(f),
        };
        let owner = match self.own_only_in(Action::ReadDrafts, c, scoped) {
            false => None,
            true if c.schema.kind == CollectionKind::Global => return None,
            true => Some(self.principal.identity()?.user_id),
        };
        Some(Readable {
            filter: both(read, drafts),
            copy: DocCopy::Draft,
            owner,
            otherwise: None,
        })
    }

    /// How the caller reads a document of `c` that something links to: with `drafts`, as
    /// its draft where they may read that draft, else as what's published where they may
    /// read that. Filters through links, joins, populate, virtuals, rendering, link checks
    /// and slug lookups all read linked documents this way.
    fn linked_in(&self, c: &Collection, drafts: bool) -> Option<Readable> {
        let read = self.readable_in(c, false)?;
        match self.readable_in(c, drafts) {
            Some(draft) if draft.copy == DocCopy::Draft => Some(Readable {
                otherwise: Some(Box::new(read)),
                ..draft
            }),
            _ => Some(read),
        }
    }

    /// How the caller reads documents of `c` that this request links to (see `linked_in`).
    pub(crate) fn linked_of(&self, c: &Collection) -> Option<Readable> {
        self.linked_in(c, self.drafts())
    }

    /// How the caller reads `c`, its draft when `drafts` and they may read it there, else
    /// its published copy; `None` when they can't read `c`. Never both: a document asked
    /// for itself is read in one copy.
    fn readable_in(&self, c: &Collection, drafts: bool) -> Option<Readable> {
        let read = match self.allows(Action::Read, c) {
            Compiled::Deny => return None,
            Compiled::Allow => None,
            Compiled::Filter(f) => Some(f),
        };
        match drafts {
            true => self.draft_readable(c, read.clone()),
            false => None,
        }
        .or(Some(Readable::published(read)))
    }

    /// How the caller reads `c` in this request (see `drafts`), for documents asked for
    /// themselves (a list, a get): one copy, no fallback. Links use `linked_of`.
    pub(crate) fn readable_of(&self, c: &Collection) -> Option<Readable> {
        self.readable_in(c, self.drafts())
    }

    /// The copy of `c` a read asking for its draft (`draft`) gets: a 400 when `c` has no
    /// drafts, and a 403 when the caller may not read them (a 401 when signed out). Asking
    /// for it makes the request one that works on drafts.
    pub(crate) fn copy_for(&self, c: &Collection, draft: bool) -> Result<DocCopy, ApiError> {
        if !draft {
            return Ok(DocCopy::Published);
        }
        let slug = &c.schema.slug;
        if !c.schema.drafts() {
            return Err(ApiError::BadRequest(format!(
                "`draft`: `{slug}` has no drafts"
            )));
        }
        if self
            .readable_in(c, true)
            .is_none_or(|r| r.copy != DocCopy::Draft)
        {
            // Signed out is a 401, as every other refusal gives it (Ruling F-6).
            if matches!(self.principal, Principal::Anonymous) {
                return Err(self.refuse());
            }
            return Err(ApiError::Forbidden(format!(
                "`draft`: you don't have permission to read the drafts of `{slug}`"
            )));
        }
        self.use_drafts()?;
        Ok(DocCopy::Draft)
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

    /// How the caller reads each collection's documents that this request links to (see
    /// `linked_in`). Globals are left out: nothing links to them.
    pub(crate) fn linked(&self) -> &HashMap<Uuid, Readable> {
        let drafts = self.drafts();
        self.linked.get_or_init(|| {
            self.schemas
                .all()
                .iter()
                .filter(|c| c.schema.kind == CollectionKind::Collection)
                .filter_map(|c| Some((c.id, self.linked_in(c, drafts)?)))
                .collect()
        })
    }

    /// Each collection, globals too, as the caller reads it in one copy: drafts where they
    /// may read them there (`drafts`), else published. A global can link to a document, so
    /// it can be one of that document's references.
    pub(crate) fn readable_with_globals(&self, drafts: bool) -> HashMap<Uuid, Readable> {
        self.schemas
            .all()
            .iter()
            .filter_map(|c| Some((c.id, self.readable_in(c, drafts)?)))
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

    /// Whether the caller may read `copy` of document `doc` of `c`, which they just wrote.
    pub(crate) async fn can_read(
        &self,
        pool: &PgPool,
        c: &Collection,
        doc: Uuid,
        copy: DocCopy,
    ) -> Result<bool, ApiError> {
        Ok(self.read_copy(pool, c, doc, copy).await?.is_some())
    }

    /// `copy` of document `doc` of `c`, when the caller may read that copy of it.
    pub(crate) async fn read_copy(
        &self,
        pool: &PgPool,
        c: &Collection,
        doc: Uuid,
        copy: DocCopy,
    ) -> Result<Option<Document>, ApiError> {
        let read = self
            .readable_in(c, copy == DocCopy::Draft)
            .filter(|r| r.copy == copy);
        Ok(match read {
            None => None,
            Some(r) => {
                let one = HashMap::from([(c.id, r)]);
                get_many_readable(pool, &[doc], &one)
                    .await?
                    .pop()
                    .map(|(_, d)| d)
            }
        })
    }

    /// Document `id` of `c` as this request reads it, when the caller may; a 404 when they
    /// may not. Always through the read filter, copy and owner together: an own-only draft
    /// read must never fall back to a plain get.
    pub(crate) async fn read_one(
        &self,
        pool: &PgPool,
        c: &Collection,
        id: Uuid,
    ) -> Result<Document, ApiError> {
        self.scope(Action::Read, &c.schema.slug)?;
        let read = self.readable_of(c).ok_or(ApiError::NotFound)?;
        let read = HashMap::from([(c.id, read)]);
        get_many_readable(pool, &[id], &read)
            .await?
            .pop()
            .map(|(_, doc)| doc)
            .ok_or(ApiError::NotFound)
    }

    /// Whether the caller may read the published copy of document `id` of `c`.
    async fn sees_published(
        &self,
        pool: &PgPool,
        c: &Collection,
        id: Uuid,
    ) -> Result<bool, ApiError> {
        let Some(read) = self.readable_in(c, false) else {
            return Ok(false);
        };
        let one = HashMap::from([(c.id, read)]);
        Ok(!get_many_readable(pool, &[id], &one).await?.is_empty())
    }

    /// Whether the caller may read either copy of document `id` of `c`.
    pub(crate) async fn sees(
        &self,
        pool: &PgPool,
        c: &Collection,
        id: Uuid,
    ) -> Result<bool, ApiError> {
        for drafts in [false, true] {
            if let Some(r) = self.readable_in(c, drafts)
                && (r.copy == DocCopy::Draft) == drafts
            {
                let one = HashMap::from([(c.id, r)]);
                if !get_many_readable(pool, &[id], &one).await?.is_empty() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// The guard an update or delete of document `id` runs under: the read filter and the
    /// action's filter, both checked on the locked row. A document the caller can't read is
    /// a 404; one they can read but not change is a 403 (from the write, when it's a filter).
    /// With drafts, a write changes the draft, so it's the draft they must be able to read
    /// (`readDrafts` too, and their own for an own-only right), and the draft the guard
    /// judges.
    pub(crate) async fn write_guard(
        &self,
        pool: &PgPool,
        action: Action,
        c: &Collection,
        id: Uuid,
    ) -> Result<Option<Filter>, ApiError> {
        self.may_write(action, c)?;
        let (read, write) = match self.write_rules(action, c) {
            Err(WriteRefusal::Hidden) => return Err(ApiError::NotFound),
            // Refused for the whole collection, so a 403 before the readability check is
            // intended: it says nothing about this document.
            Err(WriteRefusal::Refused) => return Err(self.refuse()),
            Ok(filters) => filters,
        };
        let read = match self.draft_readable(c, read.clone()) {
            Some(r) => r,
            // Refused for the whole collection, as above.
            None if c.schema.drafts() => return Err(self.refuse()),
            None => Readable::published(read),
        };
        // Checked first so a hidden document is a 404; the guard checks it again under the
        // lock, so a change in between can only turn this into a 403. The owner never
        // changes, so it needn't be. A draft the caller can't read, of a document whose
        // published copy they can, is theirs to know about: a 403.
        if read.filter.is_some() || read.owner.is_some() {
            let one = HashMap::from([(c.id, read.clone())]);
            if get_many_readable(pool, &[id], &one).await?.is_empty() {
                return Err(match read.copy {
                    DocCopy::Draft if self.sees_published(pool, c, id).await? => self.refuse(),
                    _ => ApiError::NotFound,
                });
            }
        }
        Ok(both(read.filter, write))
    }

    /// The refusals an update or delete (`action`) of `c` meets before any document is
    /// looked at, as `write_guard` gives them: so a refused caller hears that first, before
    /// anything about the request's parameters.
    pub(crate) fn may_write(&self, action: Action, c: &Collection) -> Result<(), ApiError> {
        self.scope(action, &c.schema.slug)?;
        match self.write_rules(action, c) {
            Err(WriteRefusal::Hidden) => Err(ApiError::NotFound),
            Err(WriteRefusal::Refused) => Err(self.refuse()),
            Ok((read, _))
                if c.schema.drafts() && self.draft_readable(c, read.clone()).is_none() =>
            {
                Err(self.refuse())
            }
            Ok(_) => Ok(()),
        }
    }

    /// How the caller reaches `c`'s trash, where everything takes `delete` (plan call 9):
    /// the copy a delete judges (the draft, with drafts, which makes the request one that
    /// works on drafts), the read filter (and `readDrafts`) with the delete filter, and
    /// only what they created when either right is own-only. The trash shows a document
    /// only this way, so one it doesn't reach is a 404.
    pub(crate) fn trash_readable(&self, c: &Collection) -> Result<Readable, ApiError> {
        self.may_write(Action::Delete, c)?;
        let (read, delete) = self
            .write_rules(Action::Delete, c)
            .map_err(|_| self.refuse())?;
        let read = match self.draft_readable(c, read.clone()) {
            Some(r) => r,
            // `may_write` refused a drafts collection whose drafts they can't read.
            None => Readable::published(read),
        };
        self.write_copy(c)?;
        let owner = match self.own_only(Action::Delete, c) {
            true => Some(self.user_id().ok_or_else(|| self.refuse())?),
            false => read.owner,
        };
        Ok(Readable {
            filter: both(read.filter, delete),
            owner,
            ..read
        })
    }

    /// What publishing or unpublishing document `id` of `c` runs under, judged on its draft
    /// once locked: the read, `readDrafts` and `publish` rules, and the creator for an
    /// own-only `readDrafts`. A key needs only the `:publish` scope (Ruling T1-2): its
    /// owner's rights decide what it may read to publish, and it reads nothing back. A
    /// document whose draft the caller can't read is a 404, or a 403 when they can read
    /// what's published; a global, whose existence isn't secret, is a 403.
    pub(crate) async fn publish_guard(
        &self,
        pool: &PgPool,
        c: &Collection,
        id: Uuid,
    ) -> Result<Readable, ApiError> {
        let global = c.schema.kind == CollectionKind::Global;
        let draft = self.publish_rules(c)?;
        // Checked first so a hidden document is a 404; the lock checks it again.
        let one = HashMap::from([(c.id, draft.clone())]);
        if get_many_readable(pool, &[id], &one).await?.is_empty() {
            let known = match global {
                true => documents::get(pool, c.id, id, DocCopy::Draft).await.is_ok(),
                false => self.sees_published(pool, c, id).await?,
            };
            return Err(if known {
                self.refuse()
            } else {
                ApiError::NotFound
            });
        }
        Ok(draft)
    }

    /// What publishing a document of `c` runs under, before any document is looked at (see
    /// `publish_guard`, which also judges the document): the draft under the read,
    /// `readDrafts` and `publish` rules. A 400 when `c` has no drafts; a 404 when the
    /// caller can't read the collection, or a 403 for a global; a 403 when they may not
    /// publish or read its drafts.
    fn publish_rules(&self, c: &Collection) -> Result<Readable, ApiError> {
        let slug = &c.schema.slug;
        // Callers check the `:publish` scope first, before anything about the request.
        if !c.schema.drafts() {
            return Err(ApiError::BadRequest(format!("`{slug}` has no drafts")));
        }
        let global = c.schema.kind == CollectionKind::Global;
        let read = match self.allows_in(Action::Read, c, false) {
            Compiled::Deny if !global => return Err(ApiError::NotFound),
            Compiled::Deny => return Err(self.refuse()),
            Compiled::Allow => None,
            Compiled::Filter(f) => Some(f),
        };
        let publish = match self.allows(Action::Publish, c) {
            Compiled::Deny => return Err(self.refuse()),
            Compiled::Allow => None,
            Compiled::Filter(f) => Some(f),
        };
        let draft = self
            .draft_readable_in(c, read, false)
            .ok_or_else(|| self.refuse())?;
        Ok(Readable {
            filter: both(draft.filter, publish),
            ..draft
        })
    }

    /// Refuses unless the caller may read the past versions of document `id` of `c`: its
    /// `readVersions` and read rules, judged on the copy they may read (the draft where they
    /// may read drafts, else what's published), and the creator for an own-only right, which
    /// reaches no global. Refused and missing are both a 404 (W1-R14), as is a document in
    /// the trash; a global, whose existence isn't secret, is refused with a 403 (Ruling
    /// T5-5), even before it's saved when a rule limits the caller. Answers whether they may
    /// also read the versions that were drafts, which takes `readDrafts` on the document too
    /// (Ruling T5-3).
    pub(crate) async fn versions_of(
        &self,
        pool: &PgPool,
        c: &Collection,
        id: Uuid,
    ) -> Result<bool, ApiError> {
        let global = c.schema.kind == CollectionKind::Global;
        let (versions, read) = self.versions_rules(c)?;
        let copy = versions.copy;
        // A rule could refuse even what isn't saved yet, so a global it limits is refused,
        // saved or not; only a global nothing limits can be missing, a 404.
        let limited = versions.filter.is_some();
        let one = HashMap::from([(c.id, versions)]);
        if get_many_readable(pool, &[id], &one).await?.is_empty() {
            let known = global && (limited || documents::get(pool, c.id, id, copy).await.is_ok());
            return Err(if known {
                self.refuse()
            } else {
                ApiError::NotFound
            });
        }
        // Without drafts, every version is of what was published; judged on the published
        // copy, the caller may not read drafts.
        if copy == DocCopy::Published {
            return Ok(!c.schema.drafts());
        }
        let Some(drafts) = self.draft_readable(c, read) else {
            return Ok(false);
        };
        // The draft was just found, so only a filter or an owner can still hide it. Not
        // folded into the query above: each takes one readable per collection, and these
        // two judge the same copy by different rules.
        if drafts.filter.is_none() && drafts.owner.is_none() {
            return Ok(true);
        }
        let one = HashMap::from([(c.id, drafts)]);
        Ok(!get_many_readable(pool, &[id], &one).await?.is_empty())
    }

    /// What reading the past versions of a document of `c` runs under, before any document
    /// is looked at (see `versions_of`, which also judges the document): the copy they may
    /// read under the read and `readVersions` rules, and the creator for an own-only right.
    /// Also the read filter alone. Refusals are as `versions_of` gives them.
    fn versions_rules(&self, c: &Collection) -> Result<(Readable, Option<Filter>), ApiError> {
        let slug = &c.schema.slug;
        self.scope(Action::ReadVersions, slug)?;
        if !c.schema.versioned() {
            return Err(ApiError::BadRequest(format!("`{slug}` keeps no versions")));
        }
        let global = c.schema.kind == CollectionKind::Global;
        let refused = || match global {
            true => self.refuse(),
            false => ApiError::NotFound,
        };
        let rule = |action| match self.allows(action, c) {
            Compiled::Deny => Err(refused()),
            Compiled::Allow => Ok(None),
            Compiled::Filter(f) => Ok(Some(f)),
        };
        let read = rule(Action::Read)?;
        let filter = both(read.clone(), rule(Action::ReadVersions)?);
        let owner = match self.own_only(Action::ReadVersions, c) {
            false => None,
            true if global => return Err(refused()),
            true => Some(self.principal.identity().ok_or_else(refused)?.user_id),
        };
        // Judged on the copy the caller may read: the draft only where they may read drafts
        // at all, else what's published, so a never-published document stays hidden.
        let copy = match self.draft_readable(c, read.clone()) {
            Some(_) => DocCopy::Draft,
            None => DocCopy::Published,
        };
        let versions = Readable {
            filter,
            copy,
            owner,
            otherwise: None,
        };
        Ok((versions, read))
    }

    /// `/api/access`'s answers for drafts and history on `c`, added to `a`: `readDrafts`
    /// and `publish` when it has drafts, `readVersions` when it keeps versions. Each is
    /// judged as its routes judge it before looking at a document, and is conditional when
    /// a rule is a filter or the right is own-only, as `write_answer` is.
    fn history_answers(&self, c: &Collection, a: &mut Value) {
        let answer = |r: Option<Readable>| -> Value {
            match r {
                None => false.into(),
                Some(r) if r.filter.is_none() && r.owner.is_none() => true.into(),
                Some(_) => "conditional".into(),
            }
        };
        if c.schema.drafts() {
            // What `?draft=true` reads, which every draft route goes by.
            let drafts = self
                .readable_in(c, true)
                .filter(|r| r.copy == DocCopy::Draft);
            a["readDrafts"] = answer(drafts);
            let publish = match self.scope(Action::Publish, &c.schema.slug) {
                Ok(()) => self.publish_rules(c).ok(),
                Err(_) => None,
            };
            a["publish"] = answer(publish);
        }
        if c.schema.versioned() {
            a["readVersions"] = answer(self.versions_rules(c).ok().map(|(r, _)| r));
        }
    }

    /// What a published copy may link to: each collection's published documents the caller
    /// may read, as their key's owner would (see `publish_guard`). Globals are left out:
    /// nothing links to them.
    pub(crate) fn publishable(&self) -> HashMap<Uuid, Readable> {
        self.schemas
            .all()
            .iter()
            .filter(|c| c.schema.kind == CollectionKind::Collection)
            .filter_map(|c| {
                let read = match self.allows_in(Action::Read, c, false) {
                    Compiled::Deny => return None,
                    Compiled::Allow => None,
                    Compiled::Filter(f) => Some(f),
                };
                Some((c.id, Readable::published(read)))
            })
            .collect()
    }

    /// Who the caller acts for, as versions and publishing record them.
    pub(crate) fn user_id(&self) -> Option<Uuid> {
        self.principal.identity().map(|i| i.user_id)
    }

    /// The API key the caller came with, if any, which a job they schedule acts as.
    pub(crate) fn key_id(&self) -> Option<Uuid> {
        match &self.principal {
            Principal::Key { key, .. } => Some(*key),
            _ => None,
        }
    }

    /// The guard a global's update runs under (see `write_guard`); a global's existence
    /// isn't secret, so one the caller can't read or change is refused, not hidden.
    pub(crate) fn global_guard(&self, c: &Collection) -> Result<Option<Filter>, ApiError> {
        let (read, write) = self
            .write_rules(Action::UpdateGlobal, c)
            .map_err(|_| self.refuse())?;
        let read = match self.draft_readable(c, read.clone()) {
            Some(r) => r.filter,
            None if c.schema.drafts() => return Err(self.refuse()),
            None => read,
        };
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
                let mut guards: Vec<(String, Readable)> = collections
                    .iter()
                    .map(|slug| {
                        let read = self
                            .schemas
                            .get(slug)
                            .filter(|c| c.schema.kind == CollectionKind::Collection)
                            .and_then(|c| self.linked_of(&c));
                        let guard = read.unwrap_or_else(|| Readable::published(Some(nothing())));
                        (slug.clone(), guard)
                    })
                    .collect();
                // No guards would mean unguarded; with no collections nothing should match.
                if guards.is_empty() {
                    guards.push((String::new(), Readable::published(Some(nothing()))));
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
/// - A collection or global with drafts also answers `readDrafts` and `publish`, and one
///   that keeps versions `readVersions`; a `:publish` key's `publish` goes by its owner's
///   read rights, as publishing does.
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
        if c.schema.slug != USERS {
            access.history_answers(c, &mut a);
        }
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
