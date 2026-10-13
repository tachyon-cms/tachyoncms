//! Accounts, sessions, API keys and roles. Users are documents in the `users` collection;
//! everything here is the credential side, which no document code path ever reads.

use std::time::Duration;

use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use tachyon_core::CollectionSchema;
use tachyon_core::auth::Abilities;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::collections::{self, Collection};
use crate::{DEFAULT_TENANT, DbError};

/// The slug of the system collection admin users live in.
pub const USERS: &str = "users";

/// Session lifetimes and lockout rules, from configuration.
#[derive(Debug, Clone)]
pub struct Limits {
    pub idle: Duration,
    pub absolute: Duration,
    pub attempts: i32,
    pub window: Duration,
    pub lockout: Duration,
}

pub struct NewAccount<'a> {
    pub email: &'a str,
    pub name: &'a str,
    pub password_hash: &'a str,
    pub role: &'a str,
}

#[derive(Debug, Clone)]
pub struct Account {
    pub user_id: Uuid,
    pub email: String,
    pub role: String,
    pub password_hash: String,
    /// Too many recent failed logins.
    pub locked: bool,
}

/// Who a session or key belongs to, and what their role lets them do.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub user_id: Uuid,
    pub role: String,
    pub abilities: Abilities,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyInfo {
    pub id: Uuid,
    pub name: String,
    pub prefix: String,
    pub scopes: Vec<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A key found by its prefix: the hash to check the secret against, and its scopes.
#[derive(Debug, Clone)]
pub struct KeyRow {
    pub id: Uuid,
    pub secret_hash: Vec<u8>,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Role {
    pub slug: String,
    pub name: String,
    pub builtin: bool,
    pub abilities: Abilities,
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn abilities(v: Value) -> Result<Abilities, DbError> {
    serde_json::from_value(v).map_err(|e| DbError::Sqlx(sqlx::Error::Decode(Box::new(e))))
}

/// The `users` collection, created the first time it's needed.
pub async fn ensure_users_collection(pool: &PgPool) -> Result<Collection, DbError> {
    match collections::get_by_slug(pool, USERS).await {
        Ok(c) => return Ok(c),
        Err(DbError::NotFound) => {}
        Err(e) => return Err(e),
    }
    let schema: CollectionSchema = serde_json::from_value(json!({
        "slug": USERS,
        "fields": [{"name": "name", "type": "text", "required": true}]
    }))
    .expect("the users schema is valid");
    match collections::create(pool, &schema).await {
        Ok(c) => Ok(c),
        // Another server got there first.
        Err(DbError::Conflict(_)) => collections::get_by_slug(pool, USERS).await,
        Err(e) => Err(e),
    }
}

/// Whether anyone has an account yet (so setup has run).
pub async fn has_accounts(pool: &PgPool) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM accounts a JOIN documents d ON d.id = a.user_id
             WHERE a.tenant_id = $1 AND d.deleted_at IS NULL
           ) AS "any!""#,
        DEFAULT_TENANT,
    )
    .fetch_one(pool)
    .await?)
}

/// Creates a user document and its account together. With `setup`, only while no account
/// exists: concurrent setups are serialised by a lock, so exactly one wins.
pub async fn create_account(
    pool: &PgPool,
    users: &Collection,
    new: &NewAccount<'_>,
    setup: bool,
) -> Result<Uuid, DbError> {
    // The profile holds a name when the users collection has that field.
    let profile = if users.schema.data().iter().any(|f| f.name == "name") {
        json!({ "name": new.name })
    } else {
        json!({})
    };
    let profile =
        crate::documents::Prepared::plain(profile.as_object().cloned().unwrap_or_default());
    create_account_with(pool, users, new, &profile, setup, None).await
}

/// `create_account` with a validated profile document (and the links it holds), recording
/// `actor` as its creator (or the new user themselves).
pub async fn create_account_with(
    pool: &PgPool,
    users: &Collection,
    new: &NewAccount<'_>,
    profile: &crate::documents::Prepared,
    setup: bool,
    actor: Option<Uuid>,
) -> Result<Uuid, DbError> {
    let mut tx = pool.begin().await?;
    if setup {
        // An arbitrary constant: the one lock every setup attempt shares.
        sqlx::query!("SELECT pg_advisory_xact_lock(7311001)")
            .fetch_one(&mut *tx)
            .await?;
        let any = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM accounts WHERE tenant_id = $1) AS "any!""#,
            DEFAULT_TENANT,
        )
        .fetch_one(&mut *tx)
        .await?;
        if any {
            return Err(DbError::Conflict("an account".into()));
        }
    }
    let id = Uuid::now_v7();
    let creator = actor.unwrap_or(id);
    sqlx::query!(
        r#"INSERT INTO documents (id, tenant_id, collection_id, status, published, published_at, created_by, updated_by)
           VALUES ($1, $2, $3, 'published', $4, now(), $5, $5)"#,
        id,
        DEFAULT_TENANT,
        users.id,
        sqlx::types::Json(&profile.data) as _,
        creator,
    )
    .execute(&mut *tx)
    .await?;
    crate::documents::sync_relations(&mut tx, id, &profile.relations).await?;
    sqlx::query!(
        "INSERT INTO accounts (user_id, tenant_id, email, password_hash, role) VALUES ($1, $2, $3, $4, $5)",
        id,
        DEFAULT_TENANT,
        new.email.to_lowercase(),
        new.password_hash,
        new.role,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => {
            DbError::Conflict("an account with this email".into())
        }
        sqlx::Error::Database(ref db) if db.is_foreign_key_violation() => DbError::NotFound,
        e => e.into(),
    })?;
    tx.commit().await?;
    Ok(id)
}

/// A live user's account by email, whatever its case.
pub async fn account_by_email(pool: &PgPool, email: &str) -> Result<Option<Account>, DbError> {
    Ok(sqlx::query_as!(
        Account,
        r#"SELECT a.user_id, a.email, a.role, a.password_hash,
                  (a.locked_until IS NOT NULL AND a.locked_until > now()) AS "locked!"
           FROM accounts a JOIN documents d ON d.id = a.user_id
           WHERE a.tenant_id = $1 AND lower(a.email) = lower($2) AND d.deleted_at IS NULL"#,
        DEFAULT_TENANT,
        email,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn account(pool: &PgPool, user_id: Uuid) -> Result<Option<Account>, DbError> {
    Ok(sqlx::query_as!(
        Account,
        r#"SELECT a.user_id, a.email, a.role, a.password_hash,
                  (a.locked_until IS NOT NULL AND a.locked_until > now()) AS "locked!"
           FROM accounts a JOIN documents d ON d.id = a.user_id
           WHERE a.tenant_id = $1 AND a.user_id = $2 AND d.deleted_at IS NULL"#,
        DEFAULT_TENANT,
        user_id,
    )
    .fetch_optional(pool)
    .await?)
}

/// The email and role of each of `ids` that has an account, for showing users in a list.
pub async fn emails_and_roles(
    pool: &PgPool,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, String, String)>, DbError> {
    Ok(sqlx::query!(
        "SELECT user_id, email, role FROM accounts WHERE tenant_id = $1 AND user_id = ANY($2)",
        DEFAULT_TENANT,
        ids,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| (r.user_id, r.email, r.role))
    .collect())
}

/// Counts a failed login; enough of them within the window lock the account.
pub async fn record_failure(pool: &PgPool, user_id: Uuid, limits: &Limits) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE accounts SET
             failed_logins = CASE WHEN first_failure_at > now() - make_interval(secs => $2)
                                  THEN failed_logins + 1 ELSE 1 END,
             first_failure_at = CASE WHEN first_failure_at > now() - make_interval(secs => $2)
                                     THEN first_failure_at ELSE now() END,
             locked_until = CASE WHEN (CASE WHEN first_failure_at > now() - make_interval(secs => $2)
                                            THEN failed_logins + 1 ELSE 1 END) >= $3
                                 THEN now() + make_interval(secs => $4) ELSE locked_until END
           WHERE user_id = $1"#,
        user_id,
        secs(limits.window),
        limits.attempts,
        secs(limits.lockout),
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn reset_failures(pool: &PgPool, user_id: Uuid) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET failed_logins = 0, first_failure_at = NULL, locked_until = NULL WHERE user_id = $1",
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_password(pool: &PgPool, user_id: Uuid, hash: &str) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET password_hash = $2, password_changed_at = now() WHERE user_id = $1",
        user_id,
        hash,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_email(pool: &PgPool, user_id: Uuid, email: &str) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET email = $2 WHERE user_id = $1",
        user_id,
        email.to_lowercase(),
    )
    .execute(pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => {
            DbError::Conflict("an account with this email".into())
        }
        e => e.into(),
    })?;
    Ok(())
}

/// Starts a session that ends after `limits.absolute` at the latest. Returns its ID.
pub async fn create_session(
    pool: &PgPool,
    user_id: Uuid,
    token_hash: &[u8],
    limits: &Limits,
    user_agent: Option<&str>,
    ip: Option<&str>,
) -> Result<Uuid, DbError> {
    let id = Uuid::now_v7();
    let user_agent = user_agent.map(|ua| ua.chars().take(256).collect::<String>());
    sqlx::query!(
        r#"INSERT INTO sessions (id, tenant_id, user_id, token_hash, expires_at, user_agent, ip)
           VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5), $6, $7)"#,
        id,
        DEFAULT_TENANT,
        user_id,
        token_hash,
        secs(limits.absolute),
        user_agent,
        ip,
    )
    .execute(pool)
    .await?;
    Ok(id)
}

/// A live, unexpired session by its token's hash, and who it belongs to. Notes the use at
/// most once a minute, which is what the idle timeout counts from.
pub async fn session_by_hash(
    pool: &PgPool,
    token_hash: &[u8],
    limits: &Limits,
) -> Result<Option<(Uuid, Identity)>, DbError> {
    let Some(row) = sqlx::query!(
        r#"SELECT s.id, s.user_id, a.role, r.abilities,
                  (s.last_seen_at < now() - interval '1 minute') AS "stale!"
           FROM sessions s
           JOIN accounts a ON a.user_id = s.user_id
           JOIN roles r ON r.tenant_id = a.tenant_id AND r.slug = a.role
           JOIN documents d ON d.id = s.user_id AND d.deleted_at IS NULL
           WHERE s.token_hash = $1 AND s.tenant_id = $2 AND s.expires_at > now()
             AND s.last_seen_at > now() - make_interval(secs => $3)"#,
        token_hash,
        DEFAULT_TENANT,
        secs(limits.idle),
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    if row.stale {
        sqlx::query!(
            "UPDATE sessions SET last_seen_at = now() WHERE id = $1",
            row.id
        )
        .execute(pool)
        .await?;
    }
    let identity = Identity {
        user_id: row.user_id,
        role: row.role,
        abilities: abilities(row.abilities)?,
    };
    Ok(Some((row.id, identity)))
}

/// Ends one of a user's sessions. False when it isn't theirs or doesn't exist.
pub async fn revoke_session(pool: &PgPool, user_id: Uuid, id: Uuid) -> Result<bool, DbError> {
    let done = sqlx::query!(
        "DELETE FROM sessions WHERE id = $1 AND user_id = $2",
        id,
        user_id
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Ends every session of a user except `keep`.
pub async fn revoke_other_sessions(
    pool: &PgPool,
    user_id: Uuid,
    keep: Option<Uuid>,
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM sessions WHERE user_id = $1 AND ($2::uuid IS NULL OR id <> $2)",
        user_id,
        keep,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_sessions(pool: &PgPool, user_id: Uuid) -> Result<Vec<SessionInfo>, DbError> {
    Ok(sqlx::query_as!(
        SessionInfo,
        r#"SELECT id, created_at, last_seen_at, expires_at, user_agent, ip
           FROM sessions WHERE user_id = $1 AND expires_at > now()
           ORDER BY last_seen_at DESC"#,
        user_id,
    )
    .fetch_all(pool)
    .await?)
}

/// Fails when `user_id` is the only remaining Admin, so the install never loses its last one.
pub async fn guard_removal(pool: &PgPool, user_id: Uuid) -> Result<(), DbError> {
    let mut conn = pool.acquire().await?;
    guard_last_admin(&mut conn, user_id, None).await
}

/// Locks the Admin accounts and refuses a change that would leave none: `new_role` is the
/// role `user_id` is moving to, or `None` when the account is going away.
pub(crate) async fn guard_last_admin(
    conn: &mut PgConnection,
    user_id: Uuid,
    new_role: Option<&str>,
) -> Result<(), DbError> {
    let admins = sqlx::query_scalar!(
        r#"SELECT a.user_id FROM accounts a JOIN documents d ON d.id = a.user_id
           WHERE a.tenant_id = $1 AND a.role = 'admin' AND d.deleted_at IS NULL
           FOR UPDATE OF a"#,
        DEFAULT_TENANT,
    )
    .fetch_all(&mut *conn)
    .await?;
    let leaving = admins.contains(&user_id) && new_role != Some("admin");
    if leaving && admins.len() <= 1 {
        return Err(DbError::Refused(
            "This is the last Admin. Make someone else an Admin first.".into(),
        ));
    }
    Ok(())
}

/// When the document being deleted is a user: keeps the last Admin, and ends their sessions
/// and API keys. Runs inside the delete's transaction. Other documents pass straight through.
pub(crate) async fn before_removing(conn: &mut PgConnection, id: Uuid) -> Result<(), DbError> {
    let is_account = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM accounts WHERE user_id = $1) AS "is!""#,
        id,
    )
    .fetch_one(&mut *conn)
    .await?;
    if !is_account {
        return Ok(());
    }
    guard_last_admin(conn, id, None).await?;
    sqlx::query!("DELETE FROM sessions WHERE user_id = $1", id)
        .execute(&mut *conn)
        .await?;
    sqlx::query!(
        "UPDATE api_keys SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL",
        id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Changes a user's role, never leaving the install without an Admin.
pub async fn set_role(pool: &PgPool, user_id: Uuid, role: &str) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    guard_last_admin(&mut tx, user_id, Some(role)).await?;
    let done = sqlx::query!(
        "UPDATE accounts SET role = $2 WHERE user_id = $1",
        user_id,
        role
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db) if db.is_foreign_key_violation() => DbError::NotFound,
        e => e.into(),
    })?;
    if done.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    tx.commit().await?;
    Ok(())
}

pub async fn count_admins(pool: &PgPool) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM accounts a JOIN documents d ON d.id = a.user_id
           WHERE a.tenant_id = $1 AND a.role = 'admin' AND d.deleted_at IS NULL"#,
        DEFAULT_TENANT,
    )
    .fetch_one(pool)
    .await?)
}

pub async fn roles_list(pool: &PgPool) -> Result<Vec<Role>, DbError> {
    sqlx::query!(
        "SELECT slug, name, builtin, abilities FROM roles WHERE tenant_id = $1 ORDER BY builtin DESC, slug",
        DEFAULT_TENANT,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| {
        Ok(Role {
            slug: r.slug,
            name: r.name,
            builtin: r.builtin,
            abilities: abilities(r.abilities)?,
        })
    })
    .collect()
}

/// Adds or changes a custom role. Built-in roles can't be changed.
pub async fn role_upsert(
    pool: &PgPool,
    slug: &str,
    name: &str,
    abilities: &Abilities,
) -> Result<(), DbError> {
    let value = serde_json::to_value(abilities).expect("abilities serialize");
    let done = sqlx::query!(
        r#"INSERT INTO roles (tenant_id, slug, name, abilities) VALUES ($1, $2, $3, $4)
           ON CONFLICT (tenant_id, slug) DO UPDATE SET name = $3, abilities = $4
           WHERE NOT roles.builtin"#,
        DEFAULT_TENANT,
        slug,
        name,
        value,
    )
    .execute(pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(DbError::Refused(format!(
            "`{slug}` is a built-in role and can't be changed or removed."
        )));
    }
    Ok(())
}

/// Removes a custom role nobody has.
pub async fn role_delete(pool: &PgPool, slug: &str) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let builtin = sqlx::query_scalar!(
        "SELECT builtin FROM roles WHERE tenant_id = $1 AND slug = $2 FOR UPDATE",
        DEFAULT_TENANT,
        slug,
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DbError::NotFound)?;
    if builtin {
        return Err(DbError::Refused(format!(
            "`{slug}` is a built-in role and can't be changed or removed."
        )));
    }
    let used = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM accounts WHERE tenant_id = $1 AND role = $2) AS "used!""#,
        DEFAULT_TENANT,
        slug,
    )
    .fetch_one(&mut *tx)
    .await?;
    if used {
        return Err(DbError::Refused(format!(
            "Some users have the role `{slug}`. Give them another role first."
        )));
    }
    sqlx::query!(
        "DELETE FROM roles WHERE tenant_id = $1 AND slug = $2",
        DEFAULT_TENANT,
        slug
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn role_abilities(pool: &PgPool, slug: &str) -> Result<Option<Abilities>, DbError> {
    sqlx::query_scalar!(
        "SELECT abilities FROM roles WHERE tenant_id = $1 AND slug = $2",
        DEFAULT_TENANT,
        slug,
    )
    .fetch_optional(pool)
    .await?
    .map(abilities)
    .transpose()
}

pub async fn create_api_key(
    pool: &PgPool,
    user_id: Uuid,
    name: &str,
    prefix: &str,
    secret_hash: &[u8],
    scopes: &[String],
    expires_at: Option<OffsetDateTime>,
) -> Result<ApiKeyInfo, DbError> {
    Ok(sqlx::query_as!(
        ApiKeyInfo,
        r#"INSERT INTO api_keys (id, tenant_id, user_id, name, prefix, secret_hash, scopes, expires_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           RETURNING id, name, prefix, scopes, expires_at, last_used_at, created_at"#,
        Uuid::now_v7(),
        DEFAULT_TENANT,
        user_id,
        name,
        prefix,
        secret_hash,
        scopes,
        expires_at,
    )
    .fetch_one(pool)
    .await?)
}

/// A live key by its public prefix: not revoked, not expired, its owner still a user.
pub async fn api_key_by_prefix(
    pool: &PgPool,
    prefix: &str,
) -> Result<Option<(KeyRow, Identity)>, DbError> {
    let Some(row) = sqlx::query!(
        r#"SELECT k.id, k.secret_hash, k.scopes, k.user_id, a.role, r.abilities
           FROM api_keys k
           JOIN accounts a ON a.user_id = k.user_id
           JOIN roles r ON r.tenant_id = a.tenant_id AND r.slug = a.role
           JOIN documents d ON d.id = k.user_id AND d.deleted_at IS NULL
           WHERE k.prefix = $1 AND k.tenant_id = $2 AND k.revoked_at IS NULL
             AND (k.expires_at IS NULL OR k.expires_at > now())"#,
        prefix,
        DEFAULT_TENANT,
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let key = KeyRow {
        id: row.id,
        secret_hash: row.secret_hash,
        scopes: row.scopes,
    };
    let identity = Identity {
        user_id: row.user_id,
        role: row.role,
        abilities: abilities(row.abilities)?,
    };
    Ok(Some((key, identity)))
}

/// Notes a key's use, at most once a minute.
pub async fn touch_api_key(pool: &PgPool, id: Uuid) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE api_keys SET last_used_at = now()
           WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')"#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn revoke_api_key(pool: &PgPool, user_id: Uuid, id: Uuid) -> Result<bool, DbError> {
    let done = sqlx::query!(
        "UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
        id,
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

pub async fn list_api_keys(pool: &PgPool, user_id: Uuid) -> Result<Vec<ApiKeyInfo>, DbError> {
    Ok(sqlx::query_as!(
        ApiKeyInfo,
        r#"SELECT id, name, prefix, scopes, expires_at, last_used_at, created_at
           FROM api_keys WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC"#,
        user_id,
    )
    .fetch_all(pool)
    .await?)
}

/// Replaces a key's secret: a new key with the same name, scopes and expiry, and the old one
/// revoked, together. `None` when the key isn't the user's or is already revoked.
pub async fn rotate_api_key(
    pool: &PgPool,
    user_id: Uuid,
    id: Uuid,
    prefix: &str,
    secret_hash: &[u8],
) -> Result<Option<ApiKeyInfo>, DbError> {
    let mut tx = pool.begin().await?;
    let Some(old) = sqlx::query!(
        r#"UPDATE api_keys SET revoked_at = now()
           WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL
           RETURNING name, scopes, expires_at"#,
        id,
        user_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    let new = sqlx::query_as!(
        ApiKeyInfo,
        r#"INSERT INTO api_keys (id, tenant_id, user_id, name, prefix, secret_hash, scopes, expires_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           RETURNING id, name, prefix, scopes, expires_at, last_used_at, created_at"#,
        Uuid::now_v7(),
        DEFAULT_TENANT,
        user_id,
        old.name,
        prefix,
        secret_hash,
        &old.scopes,
        old.expires_at,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(new))
}
