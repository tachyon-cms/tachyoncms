use std::time::Duration;

use sqlx::PgPool;
use tachyon_core::auth::{Abilities, Content};
use tachyon_db::DbError;
use tachyon_db::auth::{self, Limits, NewAccount};
use uuid::Uuid;

fn limits() -> Limits {
    Limits {
        idle: Duration::from_secs(7 * 86400),
        absolute: Duration::from_secs(30 * 86400),
        attempts: 5,
        window: Duration::from_secs(15 * 60),
        lockout: Duration::from_secs(15 * 60),
    }
}

async fn account(pool: &PgPool, email: &str, role: &str) -> Uuid {
    let users = auth::ensure_users_collection(pool).await.unwrap();
    auth::create_account(
        pool,
        &users,
        &NewAccount {
            email,
            name: "Someone",
            password_hash: "hash",
            role,
        },
        false,
    )
    .await
    .unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn users_collection_is_created_once(pool: PgPool) {
    let a = auth::ensure_users_collection(&pool).await.unwrap();
    let b = auth::ensure_users_collection(&pool).await.unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(a.schema.slug, "users");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn setup_creates_an_account_only_when_there_are_none(pool: PgPool) {
    assert!(!auth::has_accounts(&pool).await.unwrap());
    let users = auth::ensure_users_collection(&pool).await.unwrap();
    let new = NewAccount {
        email: "a@b.co",
        name: "A",
        password_hash: "h",
        role: "admin",
    };
    auth::create_account(&pool, &users, &new, true)
        .await
        .unwrap();
    assert!(auth::has_accounts(&pool).await.unwrap());
    let again = NewAccount {
        email: "c@d.co",
        ..new
    };
    assert!(matches!(
        auth::create_account(&pool, &users, &again, true).await,
        Err(DbError::Conflict(_))
    ));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn accounts_are_unique_by_lowercased_email(pool: PgPool) {
    account(&pool, "Ann@Example.com", "editor").await;
    let users = auth::ensure_users_collection(&pool).await.unwrap();
    let dup = NewAccount {
        email: "ann@example.COM",
        name: "B",
        password_hash: "h",
        role: "viewer",
    };
    assert!(matches!(
        auth::create_account(&pool, &users, &dup, false).await,
        Err(DbError::Conflict(_))
    ));
    let found = auth::account_by_email(&pool, "ANN@example.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.email, "ann@example.com");
    assert_eq!(found.role, "editor");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn failures_lock_after_five_in_the_window(pool: PgPool) {
    let id = account(&pool, "a@b.co", "editor").await;
    for _ in 0..4 {
        auth::record_failure(&pool, id, &limits()).await.unwrap();
    }
    assert!(!auth::account(&pool, id).await.unwrap().unwrap().locked);
    auth::record_failure(&pool, id, &limits()).await.unwrap();
    assert!(auth::account(&pool, id).await.unwrap().unwrap().locked);

    // Failures spread beyond the window start counting again.
    let other = account(&pool, "c@d.co", "editor").await;
    for _ in 0..4 {
        auth::record_failure(&pool, other, &limits()).await.unwrap();
    }
    sqlx::query(
        "UPDATE accounts SET first_failure_at = now() - interval '16 minutes' WHERE user_id = $1",
    )
    .bind(other)
    .execute(&pool)
    .await
    .unwrap();
    auth::record_failure(&pool, other, &limits()).await.unwrap();
    assert!(!auth::account(&pool, other).await.unwrap().unwrap().locked);

    auth::reset_failures(&pool, id).await.unwrap();
    sqlx::query(
        "UPDATE accounts SET locked_until = now() - interval '1 second' WHERE user_id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(!auth::account(&pool, id).await.unwrap().unwrap().locked);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn sessions_expire_idle_and_absolute(pool: PgPool) {
    let id = account(&pool, "a@b.co", "author").await;
    let s = auth::create_session(
        &pool,
        id,
        b"token-1",
        &limits(),
        Some("ua"),
        Some("1.2.3.4"),
    )
    .await
    .unwrap();
    let (session, who) = auth::session_by_hash(&pool, b"token-1", &limits())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session, s);
    assert_eq!(who.user_id, id);
    assert_eq!(who.role, "author");
    assert_eq!(
        who.abilities,
        Abilities {
            content: Content::Own,
            schemas: false,
            users: false
        }
    );

    sqlx::query("UPDATE sessions SET last_seen_at = now() - interval '8 days' WHERE id = $1")
        .bind(s)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        auth::session_by_hash(&pool, b"token-1", &limits())
            .await
            .unwrap()
            .is_none(),
        "idle"
    );

    let s2 = auth::create_session(&pool, id, b"token-2", &limits(), None, None)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(s2)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        auth::session_by_hash(&pool, b"token-2", &limits())
            .await
            .unwrap()
            .is_none(),
        "absolute"
    );
    assert!(
        auth::session_by_hash(&pool, b"nope", &limits())
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn revoking_other_sessions_keeps_the_current_one(pool: PgPool) {
    let id = account(&pool, "a@b.co", "author").await;
    let keep = auth::create_session(&pool, id, b"keep", &limits(), None, None)
        .await
        .unwrap();
    auth::create_session(&pool, id, b"other", &limits(), None, None)
        .await
        .unwrap();
    auth::revoke_other_sessions(&pool, id, Some(keep))
        .await
        .unwrap();
    assert!(
        auth::session_by_hash(&pool, b"keep", &limits())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        auth::session_by_hash(&pool, b"other", &limits())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        auth::list_sessions(&pool, id, &limits())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_last_admin_cant_be_demoted_or_removed(pool: PgPool) {
    let first = account(&pool, "a@b.co", "admin").await;
    assert!(matches!(
        auth::set_role(&mut pool.acquire().await.unwrap(), first, "editor").await,
        Err(DbError::Refused(_))
    ));
    let users = auth::ensure_users_collection(&pool).await.unwrap();
    assert!(matches!(
        tachyon_db::documents::delete(&pool, users.id, first).await,
        Err(DbError::Refused(_))
    ));
    let second = account(&pool, "c@d.co", "admin").await;
    auth::set_role(&mut pool.acquire().await.unwrap(), first, "editor")
        .await
        .unwrap();
    assert_eq!(auth::count_admins(&pool).await.unwrap(), 1);
    assert!(matches!(
        auth::set_role(&mut pool.acquire().await.unwrap(), second, "viewer").await,
        Err(DbError::Refused(_))
    ));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn roles_in_use_and_builtin_roles_cant_be_deleted(pool: PgPool) {
    let abilities = Abilities {
        content: Content::Read,
        schemas: false,
        users: false,
    };
    auth::role_upsert(&pool, "reviewer", "Reviewer", &abilities)
        .await
        .unwrap();
    assert!(
        auth::roles_list(&pool)
            .await
            .unwrap()
            .iter()
            .any(|r| r.slug == "reviewer")
    );
    assert!(matches!(
        auth::role_upsert(&pool, "admin", "Boss", &abilities).await,
        Err(DbError::Refused(_))
    ));
    assert!(matches!(
        auth::role_delete(&pool, "editor").await,
        Err(DbError::Refused(_))
    ));
    let id = account(&pool, "a@b.co", "reviewer").await;
    assert!(matches!(
        auth::role_delete(&pool, "reviewer").await,
        Err(DbError::Refused(_))
    ));
    auth::set_role(&mut pool.acquire().await.unwrap(), id, "viewer")
        .await
        .unwrap();
    auth::role_delete(&pool, "reviewer").await.unwrap();
    assert!(matches!(
        auth::role_delete(&pool, "reviewer").await,
        Err(DbError::NotFound)
    ));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn api_keys_resolve_until_revoked_or_expired(pool: PgPool) {
    let id = account(&pool, "a@b.co", "editor").await;
    let scopes = vec!["posts:read".to_owned()];
    let key = auth::create_api_key(&pool, id, "site", "abcd1234", b"secret", &scopes, None)
        .await
        .unwrap();
    let (found, who) = auth::api_key_by_prefix(&pool, "abcd1234")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, key.id);
    assert_eq!(found.secret_hash, b"secret");
    assert_eq!(found.scopes, scopes);
    assert_eq!(who.user_id, id);
    auth::touch_api_key(&pool, key.id).await.unwrap();
    assert!(
        auth::list_api_keys(&pool, id).await.unwrap()[0]
            .last_used_at
            .is_some()
    );
    assert!(auth::revoke_api_key(&pool, id, key.id).await.unwrap());
    assert!(
        auth::api_key_by_prefix(&pool, "abcd1234")
            .await
            .unwrap()
            .is_none()
    );

    let past = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    auth::create_api_key(&pool, id, "old", "efgh5678", b"s", &scopes, Some(past))
        .await
        .unwrap();
    assert!(
        auth::api_key_by_prefix(&pool, "efgh5678")
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_reused_key_prefix_is_a_unique_violation(pool: PgPool) {
    let id = account(&pool, "a@b.co", "editor").await;
    let scopes = vec!["posts:read".to_owned()];
    auth::create_api_key(&pool, id, "one", "samepfx1", b"a", &scopes, None)
        .await
        .unwrap();
    assert!(matches!(
        auth::create_api_key(&pool, id, "two", "samepfx1", b"b", &scopes, None).await,
        Err(DbError::UniqueViolation(_))
    ));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn rules_and_the_roles_they_name_stay_in_step(pool: PgPool) {
    let abilities = Abilities {
        content: Content::Read,
        schemas: false,
        users: false,
    };
    auth::role_upsert(&pool, "reviewer", "Reviewer", &abilities)
        .await
        .unwrap();
    let schema = |slug: &str, role: &str| {
        serde_json::from_value::<tachyon_core::CollectionSchema>(serde_json::json!({
            "slug": slug, "fields": [{"name": "title", "type": "text", "access": {"update": {"role": role}}}]
        }))
        .unwrap()
    };
    let refused = |r: Result<(), DbError>| match r {
        Err(DbError::Refused(why)) => why,
        other => panic!("{other:?}"),
    };

    // Saves recheck the roles inside their own transaction.
    let why = refused(
        tachyon_db::collections::create(&pool, &schema("posts", "ghost"))
            .await
            .map(drop),
    );
    assert_eq!(
        why,
        "access rules name the role `ghost`, which doesn't exist"
    );
    tachyon_db::collections::create(&pool, &schema("posts", "reviewer"))
        .await
        .unwrap();
    let change = tachyon_db::schema_change::apply(
        &pool,
        "posts",
        &schema("posts", "ghost"),
        &Default::default(),
        false,
    )
    .await;
    // The change rechecks every rule under the lock, roles included.
    assert!(
        matches!(&change, Err(tachyon_db::schema_change::ChangeError::Invalid(e))
            if e.to_string() == "field `title`: access.update: there's no role `ghost`"),
        "{change:?}"
    );

    // And a role a rule names can't go.
    assert_eq!(
        refused(auth::role_delete(&pool, "reviewer").await),
        "access rules in posts name the role `reviewer`; change them first"
    );
    tachyon_db::schema_change::apply(
        &pool,
        "posts",
        &schema("posts", "editor"),
        &Default::default(),
        false,
    )
    .await
    .unwrap();
    auth::role_delete(&pool, "reviewer").await.unwrap();
}
