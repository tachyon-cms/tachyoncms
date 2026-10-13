mod common;

use axum::Router;
use common::{admin_app, app_as, assert_problem, send};
use serde_json::{Value, json};
use sqlx::PgPool;

/// An Admin who has made `posts` and the `settings` global, with one post of their own.
async fn site(pool: &PgPool) -> (Router, String) {
    let admin = admin_app(pool.clone()).await;
    for schema in [
        json!({"slug": "posts", "fields": [{"name": "title", "type": "text"}]}),
        json!({"slug": "settings", "kind": "global", "fields": [{"name": "name", "type": "text"}]}),
    ] {
        let res = send(
            &admin,
            "POST",
            "/api/admin/collections",
            Some(&schema.to_string()),
        )
        .await;
        assert_eq!(res.status, 201, "{}", res.json);
    }
    let post = send(
        &admin,
        "POST",
        "/api/posts",
        Some(r#"{"title": "admin's"}"#),
    )
    .await;
    (admin, post.json["id"].as_str().unwrap().to_owned())
}

fn status(res: &common::Res) -> u16 {
    res.status.as_u16()
}

async fn try_everything(app: &Router, post: &str) -> Vec<u16> {
    vec![
        status(&send(app, "GET", "/api/posts", None).await),
        status(&send(app, "GET", &format!("/api/posts/{post}"), None).await),
        status(&send(app, "POST", "/api/posts", Some(r#"{"title": "new"}"#)).await),
        status(
            &send(
                app,
                "PATCH",
                &format!("/api/posts/{post}"),
                Some(r#"{"title": "x"}"#),
            )
            .await,
        ),
        status(
            &send(
                app,
                "PATCH",
                "/api/globals/settings",
                Some(r#"{"name": "x"}"#),
            )
            .await,
        ),
        status(&send(app, "GET", "/api/admin/collections", None).await),
        status(
            &send(
                app,
                "POST",
                "/api/admin/collections",
                Some(&json!({"slug": "more", "fields": []}).to_string()),
            )
            .await,
        ),
    ]
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn anonymous_reads_but_cant_write(pool: PgPool) {
    let (_, post) = site(&pool).await;
    let anon = tachyon_api::router(pool).await.unwrap();
    assert_eq!(
        try_everything(&anon, &post).await,
        [200, 200, 401, 401, 401, 401, 401]
    );
    assert_eq!(
        send(&anon, "GET", "/api/globals/settings", None)
            .await
            .status,
        200
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn viewers_read_only(pool: PgPool) {
    let (_, post) = site(&pool).await;
    let viewer = app_as(&pool, "v@example.com", "viewer").await;
    assert_eq!(
        try_everything(&viewer, &post).await,
        [200, 200, 403, 403, 403, 200, 403]
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn authors_edit_only_their_own(pool: PgPool) {
    let (_, admins_post) = site(&pool).await;
    let author = app_as(&pool, "a@example.com", "author").await;
    let mine = send(&author, "POST", "/api/posts", Some(r#"{"title": "mine"}"#)).await;
    assert_eq!(mine.status, 201, "{}", mine.json);
    let mine = mine.json["id"].as_str().unwrap();
    assert_eq!(
        send(
            &author,
            "PATCH",
            &format!("/api/posts/{mine}"),
            Some(r#"{"title": "edited"}"#)
        )
        .await
        .status,
        200
    );
    let theirs = send(
        &author,
        "PATCH",
        &format!("/api/posts/{admins_post}"),
        Some(r#"{"title": "x"}"#),
    )
    .await;
    assert_problem(&theirs, 403);
    assert_eq!(
        theirs.json["detail"],
        "You can only change documents you created."
    );
    assert_problem(
        &send(
            &author,
            "DELETE",
            &format!("/api/posts/{admins_post}"),
            None,
        )
        .await,
        403,
    );
    assert_problem(
        &send(
            &author,
            "PATCH",
            "/api/globals/settings",
            Some(r#"{"name": "x"}"#),
        )
        .await,
        403,
    );
    assert_eq!(
        send(&author, "DELETE", &format!("/api/posts/{mine}"), None)
            .await
            .status,
        204
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn editors_edit_everything_but_schemas(pool: PgPool) {
    let (_, post) = site(&pool).await;
    let editor = app_as(&pool, "e@example.com", "editor").await;
    assert_eq!(
        try_everything(&editor, &post).await,
        [200, 200, 201, 200, 200, 200, 403]
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn created_by_and_updated_by_are_filled(pool: PgPool) {
    let (admin, post) = site(&pool).await;
    let editor_id = common::add_user(&pool, "e@example.com", "editor").await;
    let editor = common::signed_in(
        tachyon_api::router(pool.clone()).await.unwrap(),
        "e@example.com",
    )
    .await;
    send(
        &editor,
        "PATCH",
        &format!("/api/posts/{post}"),
        Some(r#"{"title": "edited"}"#),
    )
    .await;
    let row: (Option<uuid::Uuid>, Option<uuid::Uuid>) =
        sqlx::query_as("SELECT created_by, updated_by FROM documents WHERE id = $1::uuid")
            .bind(&post)
            .fetch_one(&pool)
            .await
            .unwrap();
    let me = send(&admin, "GET", "/api/auth/me", None).await;
    let admin_id: uuid::Uuid = me.json["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(row, (Some(admin_id), Some(editor_id)));
    let _: Value = me.json;
}
