mod common;

use common::{ADMIN, PASSWORD, add_user, admin_app, app_as, assert_problem, send, signed_in};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn admins_manage_users_and_others_cant(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let body =
        json!({"email": "Ed@Example.com", "name": "Ed", "password": PASSWORD, "role": "editor"});
    let res = send(&admin, "POST", "/api/users", Some(&body.to_string())).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["email"], "ed@example.com");
    assert_eq!(res.json["role"], "editor");
    assert_eq!(res.json["name"], "Ed");
    assert!(res.json.get("password").is_none());
    let ed = res.json["id"].as_str().unwrap().to_owned();

    let list = send(&admin, "GET", "/api/users?sort=createdAt", None).await;
    assert_eq!(list.status, 200, "{}", list.json);
    let emails: Vec<&str> = list.json["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["email"].as_str().unwrap())
        .collect();
    assert_eq!(emails, [ADMIN, "ed@example.com"]);

    let dup = send(&admin, "POST", "/api/users", Some(&body.to_string())).await;
    assert_problem(&dup, 409);
    let bad_role =
        json!({"email": "x@example.com", "name": "X", "password": PASSWORD, "role": "nope"});
    assert_problem(
        &send(&admin, "POST", "/api/users", Some(&bad_role.to_string())).await,
        422,
    );

    let editor = signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "ed@example.com",
    )
    .await;
    let other =
        json!({"email": "y@example.com", "name": "Y", "password": PASSWORD, "role": "viewer"});
    assert_problem(
        &send(&editor, "POST", "/api/users", Some(&other.to_string())).await,
        403,
    );
    assert_problem(&send(&editor, "GET", "/api/users", None).await, 403);

    let promoted = send(
        &admin,
        "PATCH",
        &format!("/api/users/{ed}"),
        Some(r#"{"role": "admin"}"#),
    )
    .await;
    assert_eq!(promoted.json["role"], "admin", "{}", promoted.json);
    assert_eq!(
        send(&admin, "DELETE", &format!("/api/users/{ed}"), None)
            .await
            .status,
        204
    );
    assert_problem(&send(&editor, "GET", "/api/auth/me", None).await, 401);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn users_see_and_edit_only_themselves(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let me_id = add_user(&pool, "v@example.com", "viewer").await;
    let other_id = add_user(&pool, "w@example.com", "viewer").await;
    let viewer = signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "v@example.com",
    )
    .await;

    let mine = send(&viewer, "GET", &format!("/api/users/{me_id}"), None).await;
    assert_eq!(mine.status, 200, "{}", mine.json);
    assert_eq!(mine.json["email"], "v@example.com");
    assert_problem(
        &send(&viewer, "GET", &format!("/api/users/{other_id}"), None).await,
        403,
    );

    let renamed = send(
        &viewer,
        "PATCH",
        &format!("/api/users/{me_id}"),
        Some(r#"{"name": "Vee"}"#),
    )
    .await;
    assert_eq!(renamed.json["name"], "Vee", "{}", renamed.json);
    assert_problem(
        &send(
            &viewer,
            "PATCH",
            &format!("/api/users/{other_id}"),
            Some(r#"{"name": "x"}"#),
        )
        .await,
        403,
    );
    let promote = send(
        &viewer,
        "PATCH",
        &format!("/api/users/{me_id}"),
        Some(r#"{"role": "admin"}"#),
    )
    .await;
    assert_problem(&promote, 403);

    let no_password = send(
        &viewer,
        "PATCH",
        &format!("/api/users/{me_id}"),
        Some(r#"{"email": "new@example.com"}"#),
    )
    .await;
    assert_problem(&no_password, 403);
    let body = json!({"email": "new@example.com", "currentPassword": PASSWORD});
    let moved = send(
        &viewer,
        "PATCH",
        &format!("/api/users/{me_id}"),
        Some(&body.to_string()),
    )
    .await;
    assert_eq!(moved.json["email"], "new@example.com", "{}", moved.json);

    // Admins can change email and role directly.
    let res = send(
        &admin,
        "PATCH",
        &format!("/api/users/{other_id}"),
        Some(r#"{"email": "w2@example.com", "role": "editor"}"#),
    )
    .await;
    assert_eq!(
        (res.json["email"].as_str(), res.json["role"].as_str()),
        (Some("w2@example.com"), Some("editor"))
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_last_admin_is_protected_everywhere(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let me = send(&admin, "GET", "/api/auth/me", None).await.json["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let demote = send(
        &admin,
        "PATCH",
        &format!("/api/users/{me}"),
        Some(r#"{"role": "editor"}"#),
    )
    .await;
    assert_problem(&demote, 409);
    assert_problem(
        &send(&admin, "DELETE", &format!("/api/users/{me}"), None).await,
        409,
    );
    assert_eq!(
        send(&admin, "GET", "/api/auth/me", None).await.json["role"],
        "admin"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn custom_roles_can_be_added_used_and_removed_when_unused(pool: PgPool) {
    let admin = admin_app(pool.clone()).await;
    let roles = send(&admin, "GET", "/api/admin/roles", None).await;
    let slugs: Vec<&str> = roles.json["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, ["admin", "author", "editor", "viewer"]);

    let reviewer = json!({"slug": "reviewer", "name": "Reviewer",
                          "abilities": {"content": "read", "schemas": false, "users": false}});
    assert_eq!(
        send(
            &admin,
            "POST",
            "/api/admin/roles",
            Some(&reviewer.to_string())
        )
        .await
        .status,
        201
    );
    let user =
        json!({"email": "r@example.com", "name": "R", "password": PASSWORD, "role": "reviewer"});
    let created = send(&admin, "POST", "/api/users", Some(&user.to_string())).await;
    assert_eq!(created.status, 201, "{}", created.json);
    let id = created.json["id"].as_str().unwrap();
    assert_problem(
        &send(&admin, "DELETE", "/api/admin/roles/reviewer", None).await,
        409,
    );
    let change = json!({"name": "Proofreader", "abilities": {"content": "own", "schemas": false, "users": false}});
    assert_eq!(
        send(
            &admin,
            "PUT",
            "/api/admin/roles/reviewer",
            Some(&change.to_string())
        )
        .await
        .status,
        200
    );
    assert_problem(
        &send(
            &admin,
            "PUT",
            "/api/admin/roles/admin",
            Some(&change.to_string()),
        )
        .await,
        409,
    );
    assert_problem(
        &send(&admin, "DELETE", "/api/admin/roles/editor", None).await,
        409,
    );
    send(
        &admin,
        "PATCH",
        &format!("/api/users/{id}"),
        Some(r#"{"role": "viewer"}"#),
    )
    .await;
    assert_eq!(
        send(&admin, "DELETE", "/api/admin/roles/reviewer", None)
            .await
            .status,
        204
    );

    let editor = app_as(&pool, "e@example.com", "editor").await;
    assert_problem(
        &send(
            &editor,
            "POST",
            "/api/admin/roles",
            Some(&reviewer.to_string()),
        )
        .await,
        403,
    );
    let bad = json!({"slug": "Bad Slug", "name": "x", "abilities": {"content": "read", "schemas": false, "users": false}});
    assert_problem(
        &send(&admin, "POST", "/api/admin/roles", Some(&bad.to_string())).await,
        422,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn the_users_collection_keeps_its_credentials_apart(pool: PgPool) {
    let admin = admin_app(pool).await;
    let users = send(&admin, "GET", "/api/admin/collections/users", None).await;
    assert_eq!(users.status, 200, "{}", users.json);
    for reserved in ["email", "role", "password", "currentPassword"] {
        let change = json!({"schema": {"slug": "users", "fields": [
            {"name": "name", "type": "text", "required": true},
            {"name": reserved, "type": "text"}
        ]}});
        let res = send(
            &admin,
            "PUT",
            "/api/admin/collections/users",
            Some(&change.to_string()),
        )
        .await;
        assert_problem(&res, 422);
    }
    let added = json!({"schema": {"slug": "users", "fields": [
        {"name": "name", "type": "text", "required": true},
        {"name": "bio", "type": "textarea"}
    ]}});
    assert_eq!(
        send(
            &admin,
            "PUT",
            "/api/admin/collections/users",
            Some(&added.to_string())
        )
        .await
        .status,
        200
    );
    let again = json!({"slug": "users", "fields": []});
    assert_problem(
        &send(
            &admin,
            "POST",
            "/api/admin/collections",
            Some(&again.to_string()),
        )
        .await,
        409,
    );
}
