//! The rules of signing in and of who may do what, with no I/O.

/// Long enough to resist guessing, short enough that hashing can't be used to burn CPU.
pub fn check_password(p: &str) -> Result<(), &'static str> {
    match p.chars().count() {
        0..=11 => Err("a password needs at least 12 characters"),
        12..=256 => Ok(()),
        _ => Err("a password can be at most 256 characters"),
    }
}

/// What a role may do with content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Content {
    /// Create, change and delete any document, and change globals.
    All,
    /// Create documents, and change or delete only their own.
    Own,
    /// Read only.
    Read,
}

/// A role's default access, used until a collection has its own rules.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Abilities {
    pub content: Content,
    /// Change collection schemas.
    pub schemas: bool,
    /// Manage users, roles and other people's sessions.
    pub users: bool,
}

/// The roles every install has: `(slug, name, abilities)`.
pub fn builtin_roles() -> [(&'static str, &'static str, Abilities); 4] {
    let role = |content, admin| Abilities {
        content,
        schemas: admin,
        users: admin,
    };
    [
        ("admin", "Admin", role(Content::All, true)),
        ("editor", "Editor", role(Content::All, false)),
        ("author", "Author", role(Content::Own, false)),
        ("viewer", "Viewer", role(Content::Read, false)),
    ]
}

/// What an API key may do, on top of what its owner may.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// `posts:read`, or `*:read` for every collection.
    Read(String),
    /// `posts:write`: create, change and delete, which includes reading.
    Write(String),
    /// Schemas, users and roles.
    Admin,
}

/// Reads scopes like `posts:read`, `*:write` and `admin`. A key needs at least one.
pub fn parse_scopes(scopes: &[String]) -> Result<Vec<Scope>, String> {
    if scopes.is_empty() {
        return Err("a key needs at least one scope, like posts:read".into());
    }
    scopes
        .iter()
        .map(|s| {
            let bad = || format!("`{s}` isn't a scope; use collection:read, collection:write, *:read, *:write or admin");
            if s == "admin" {
                return Ok(Scope::Admin);
            }
            let (collection, verb) = s.split_once(':').ok_or_else(bad)?;
            // Reserved paths like `admin` aren't collections a key could be scoped to.
            let valid = collection == "*"
                || (!collection.is_empty()
                    && !crate::schema::RESERVED_SLUGS.contains(&collection)
                    && collection.starts_with(|c: char| c.is_ascii_lowercase())
                    && collection.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'));
            match (valid, verb) {
                (true, "read") => Ok(Scope::Read(collection.to_owned())),
                (true, "write") => Ok(Scope::Write(collection.to_owned())),
                _ => Err(bad()),
            }
        })
        .collect()
}

/// Something a request wants to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Read,
    Create,
    Update,
    Delete,
    UpdateGlobal,
    Schemas,
    Users,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Allowed on documents the caller created.
    OwnOnly,
    Deny,
}

/// Anyone may read; nobody signed out may do anything else.
pub fn decide_anonymous(action: Action) -> Decision {
    if action == Action::Read {
        Decision::Allow
    } else {
        Decision::Deny
    }
}

/// What a signed-in user, or an API key acting for one (`scopes`), may do on `collection`.
/// A key's scopes only ever narrow its owner's role.
pub fn decide(
    abilities: &Abilities,
    scopes: Option<&[Scope]>,
    action: Action,
    collection: &str,
) -> Decision {
    use Action::*;
    let by_role = match (action, abilities.content) {
        (Read, _) => Decision::Allow,
        (Schemas, _) => yes(abilities.schemas),
        (Users, _) => yes(abilities.users),
        (_, Content::Read) => Decision::Deny,
        (Create, _) => Decision::Allow,
        (UpdateGlobal, Content::All) => Decision::Allow,
        (UpdateGlobal, _) => Decision::Deny,
        (Update | Delete, Content::All) => Decision::Allow,
        (Update | Delete, Content::Own) => Decision::OwnOnly,
    };
    match scopes {
        Some(scopes) if !scopes.iter().any(|s| scope_allows(s, action, collection)) => {
            Decision::Deny
        }
        _ => by_role,
    }
}

fn yes(allowed: bool) -> Decision {
    if allowed {
        Decision::Allow
    } else {
        Decision::Deny
    }
}

fn scope_allows(scope: &Scope, action: Action, collection: &str) -> bool {
    let covers = |c: &str| c == "*" || c == collection;
    match (scope, action) {
        (Scope::Admin, _) => true,
        (_, Action::Schemas | Action::Users) => false,
        (Scope::Read(c), Action::Read) => covers(c),
        (Scope::Read(_), _) => false,
        (Scope::Write(c), _) => covers(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn passwords_need_12_to_256_characters() {
        assert!(check_password("short").is_err());
        assert!(
            check_password(&"é".repeat(11)).is_err(),
            "characters, not bytes"
        );
        assert!(check_password(&"é".repeat(12)).is_ok());
        assert!(check_password(&"a".repeat(256)).is_ok());
        assert_eq!(
            check_password(&"a".repeat(257)),
            Err("a password can be at most 256 characters")
        );
        assert_eq!(
            check_password("eleven-char"),
            Err("a password needs at least 12 characters")
        );
    }

    #[test]
    fn builtin_roles_match_the_spec_table() {
        let roles = builtin_roles();
        let find = |slug: &str| roles.iter().find(|(s, _, _)| *s == slug).unwrap().2.clone();
        assert_eq!(
            find("admin"),
            Abilities {
                content: Content::All,
                schemas: true,
                users: true
            }
        );
        assert_eq!(
            find("editor"),
            Abilities {
                content: Content::All,
                schemas: false,
                users: false
            }
        );
        assert_eq!(
            find("author"),
            Abilities {
                content: Content::Own,
                schemas: false,
                users: false
            }
        );
        assert_eq!(
            find("viewer"),
            Abilities {
                content: Content::Read,
                schemas: false,
                users: false
            }
        );
    }

    #[test]
    fn scopes_parse_and_match() {
        let scopes = parse_scopes(&owned(&["posts:read", "*:write", "admin"])).unwrap();
        assert_eq!(
            scopes,
            vec![
                Scope::Read("posts".into()),
                Scope::Write("*".into()),
                Scope::Admin
            ]
        );
        for bad in ["posts", "posts:delete", "Posts:read", ":read", "admin:read"] {
            assert!(parse_scopes(&owned(&[bad])).is_err(), "{bad}");
        }
        assert!(parse_scopes(&[]).is_err(), "a key needs at least one scope");
    }

    #[test]
    fn decide_follows_the_role_table() {
        use Action::*;
        let roles = builtin_roles();
        let role = |slug: &str| roles.iter().find(|(s, _, _)| *s == slug).unwrap().2.clone();
        let table = [
            (
                "viewer",
                [
                    Decision::Allow,
                    Decision::Deny,
                    Decision::Deny,
                    Decision::Deny,
                    Decision::Deny,
                    Decision::Deny,
                    Decision::Deny,
                ],
            ),
            (
                "author",
                [
                    Decision::Allow,
                    Decision::Allow,
                    Decision::OwnOnly,
                    Decision::OwnOnly,
                    Decision::Deny,
                    Decision::Deny,
                    Decision::Deny,
                ],
            ),
            (
                "editor",
                [
                    Decision::Allow,
                    Decision::Allow,
                    Decision::Allow,
                    Decision::Allow,
                    Decision::Allow,
                    Decision::Deny,
                    Decision::Deny,
                ],
            ),
            ("admin", [Decision::Allow; 7]),
        ];
        let actions = [Read, Create, Update, Delete, UpdateGlobal, Schemas, Users];
        for (slug, expected) in table {
            for (action, want) in actions.iter().zip(expected) {
                assert_eq!(
                    decide(&role(slug), None, *action, "posts"),
                    want,
                    "{slug} {action:?}"
                );
            }
        }
        assert_eq!(decide_anonymous(Read), Decision::Allow);
        for action in [Create, Update, Delete, UpdateGlobal, Schemas, Users] {
            assert_eq!(decide_anonymous(action), Decision::Deny, "{action:?}");
        }
    }

    #[test]
    fn scopes_only_narrow_a_role() {
        use Action::*;
        let roles = builtin_roles();
        let admin = roles
            .iter()
            .find(|(s, _, _)| *s == "admin")
            .unwrap()
            .2
            .clone();
        let viewer = roles
            .iter()
            .find(|(s, _, _)| *s == "viewer")
            .unwrap()
            .2
            .clone();
        let read_posts = parse_scopes(&owned(&["posts:read"])).unwrap();
        assert_eq!(
            decide(&admin, Some(&read_posts), Read, "posts"),
            Decision::Allow
        );
        assert_eq!(
            decide(&admin, Some(&read_posts), Read, "pages"),
            Decision::Deny
        );
        assert_eq!(
            decide(&admin, Some(&read_posts), Update, "posts"),
            Decision::Deny
        );
        assert_eq!(
            decide(&admin, Some(&read_posts), Schemas, "posts"),
            Decision::Deny
        );
        let write_all = parse_scopes(&owned(&["*:write"])).unwrap();
        assert_eq!(
            decide(&admin, Some(&write_all), Update, "pages"),
            Decision::Allow
        );
        assert_eq!(
            decide(&admin, Some(&write_all), Read, "pages"),
            Decision::Allow,
            "write implies read"
        );
        assert_eq!(
            decide(&viewer, Some(&write_all), Update, "pages"),
            Decision::Deny,
            "a key never exceeds its owner"
        );
        let admin_scope = parse_scopes(&owned(&["admin"])).unwrap();
        assert_eq!(
            decide(&admin, Some(&admin_scope), Schemas, "posts"),
            Decision::Allow
        );
        assert_eq!(
            decide(&admin, Some(&admin_scope), Users, "posts"),
            Decision::Allow
        );
    }
}
