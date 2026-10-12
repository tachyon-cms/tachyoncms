use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_db::{DbError, collections, documents};
use uuid::Uuid;

async fn collection(pool: &PgPool, slug: &str) -> Uuid {
    let schema: CollectionSchema = serde_json::from_value(
        json!({"slug": slug, "fields": [{"name": "title", "type": "text"}]}),
    )
    .unwrap();
    collections::create(pool, &schema).await.unwrap().id
}

fn data(title: &str) -> Map<String, Value> {
    json!({"title": title}).as_object().unwrap().clone()
}

#[sqlx::test]
async fn insert_then_get(pool: PgPool) {
    let c = collection(&pool, "posts").await;
    let doc = documents::insert(&pool, c, &data("Hello")).await.unwrap();
    assert_eq!(doc.status, "published");
    let found = documents::get(&pool, c, doc.id).await.unwrap();
    assert_eq!(found.data.0, data("Hello"));

    let json = serde_json::to_value(&found).unwrap();
    assert_eq!(json["title"], "Hello");
    assert_eq!(json["id"], doc.id.to_string());
    assert!(json["createdAt"].as_str().unwrap().ends_with('Z'), "{json}");
}

#[sqlx::test]
async fn documents_are_scoped_to_their_collection(pool: PgPool) {
    let posts = collection(&pool, "posts").await;
    let pages = collection(&pool, "pages").await;
    let doc = documents::insert(&pool, posts, &data("Hello"))
        .await
        .unwrap();
    assert!(matches!(
        documents::get(&pool, pages, doc.id).await,
        Err(DbError::NotFound)
    ));
}

#[sqlx::test]
async fn list_pages_newest_first(pool: PgPool) {
    let c = collection(&pool, "posts").await;
    for title in ["one", "two", "three"] {
        documents::insert(&pool, c, &data(title)).await.unwrap();
    }
    let page = documents::list(&pool, c, 2, 0, true).await.unwrap();
    let titles: Vec<_> = page
        .docs
        .iter()
        .map(|d| d.data.0["title"].clone())
        .collect();
    assert_eq!(titles, [json!("three"), json!("two")]);
    assert_eq!(page.total, Some(3));

    let rest = documents::list(&pool, c, 2, 2, false).await.unwrap();
    assert_eq!(rest.docs.len(), 1);
    assert_eq!(rest.total, None);
}

#[sqlx::test]
async fn update_applies_the_closure(pool: PgPool) {
    let c = collection(&pool, "posts").await;
    let doc = documents::insert(&pool, c, &data("Old")).await.unwrap();
    let updated = documents::update(&pool, c, doc.id, |current| {
        assert_eq!(current["title"], "Old");
        Ok::<_, DbError>(data("New"))
    })
    .await
    .unwrap();
    assert_eq!(updated.data.0, data("New"));
    assert!(updated.updated_at >= doc.updated_at);
}

#[sqlx::test]
async fn update_missing_is_not_found(pool: PgPool) {
    let c = collection(&pool, "posts").await;
    let res = documents::update(&pool, c, Uuid::now_v7(), |_| Ok::<_, DbError>(data("x"))).await;
    assert!(matches!(res, Err(DbError::NotFound)));
}

#[sqlx::test]
async fn delete_hides_the_document(pool: PgPool) {
    let c = collection(&pool, "posts").await;
    let doc = documents::insert(&pool, c, &data("Bye")).await.unwrap();
    documents::delete(&pool, c, doc.id).await.unwrap();
    assert!(matches!(
        documents::get(&pool, c, doc.id).await,
        Err(DbError::NotFound)
    ));
    assert!(
        documents::list(&pool, c, 10, 0, false)
            .await
            .unwrap()
            .docs
            .is_empty()
    );
    assert!(matches!(
        documents::delete(&pool, c, doc.id).await,
        Err(DbError::NotFound)
    ));
}
