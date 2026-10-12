use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_db::documents::{Prepared, Relation};
use tachyon_db::query::{self, ListQuery};
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
    let doc = documents::insert(&pool, c, None, &Prepared::plain(data("Hello")))
        .await
        .unwrap();
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
    let doc = documents::insert(&pool, posts, None, &Prepared::plain(data("Hello")))
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
        documents::insert(&pool, c, None, &Prepared::plain(data(title)))
            .await
            .unwrap();
    }
    let page = query::list(
        &pool,
        c,
        &ListQuery {
            filter: None,
            sort: None,
            limit: 2,
            offset: 0,
            after: None,
            count: true,
        },
    )
    .await
    .unwrap();
    let titles: Vec<_> = page
        .docs
        .iter()
        .map(|d| d.data.0["title"].clone())
        .collect();
    assert_eq!(titles, [json!("three"), json!("two")]);
    assert_eq!(page.total, Some(3));

    let rest = query::list(
        &pool,
        c,
        &ListQuery {
            filter: None,
            sort: None,
            limit: 2,
            offset: 2,
            after: None,
            count: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(rest.docs.len(), 1);
    assert_eq!(rest.total, None);
}

#[sqlx::test]
async fn update_applies_the_closure(pool: PgPool) {
    let c = collection(&pool, "posts").await;
    let doc = documents::insert(&pool, c, None, &Prepared::plain(data("Old")))
        .await
        .unwrap();
    let updated = documents::update(&pool, c, doc.id, |current| {
        assert_eq!(current["title"], "Old");
        Ok::<_, DbError>(Prepared::plain(data("New")))
    })
    .await
    .unwrap();
    assert_eq!(updated.data.0, data("New"));
    assert!(updated.updated_at >= doc.updated_at);
}

#[sqlx::test]
async fn update_missing_is_not_found(pool: PgPool) {
    let c = collection(&pool, "posts").await;
    let res = documents::update(&pool, c, Uuid::now_v7(), |_| {
        Ok::<_, DbError>(Prepared::plain(data("x")))
    })
    .await;
    assert!(matches!(res, Err(DbError::NotFound)));
}

#[sqlx::test]
async fn delete_hides_the_document(pool: PgPool) {
    let c = collection(&pool, "posts").await;
    let doc = documents::insert(&pool, c, None, &Prepared::plain(data("Bye")))
        .await
        .unwrap();
    documents::delete(&pool, c, doc.id).await.unwrap();
    assert!(matches!(
        documents::get(&pool, c, doc.id).await,
        Err(DbError::NotFound)
    ));
    assert!(
        query::list(
            &pool,
            c,
            &ListQuery {
                filter: None,
                sort: None,
                limit: 10,
                offset: 0,
                after: None,
                count: false
            }
        )
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

async fn link(
    pool: &PgPool,
    c: Uuid,
    title: &str,
    to: &[(Uuid, Uuid)],
) -> Result<documents::Document, DbError> {
    let relations = to
        .iter()
        .enumerate()
        .map(|(i, (collection_id, id))| Relation {
            path: format!("refs.{i}"),
            collection_id: *collection_id,
            id: *id,
        })
        .collect();
    documents::insert(
        pool,
        c,
        None,
        &Prepared {
            data: data(title),
            relations,
        },
    )
    .await
}

#[sqlx::test]
async fn relations_must_point_at_live_documents_in_the_right_collection(pool: PgPool) {
    let posts = collection(&pool, "posts").await;
    let authors = collection(&pool, "authors").await;
    let ann = documents::insert(&pool, authors, None, &Prepared::plain(data("Ann")))
        .await
        .unwrap();

    link(&pool, posts, "ok", &[(authors, ann.id)])
        .await
        .unwrap();

    let wrong_collection = link(&pool, posts, "bad", &[(posts, ann.id)])
        .await
        .unwrap_err();
    let random = link(&pool, posts, "bad", &[(authors, Uuid::now_v7())])
        .await
        .unwrap_err();
    for err in [wrong_collection, random] {
        let DbError::Validation(errors) = err else {
            panic!("expected validation error, got {err:?}")
        };
        assert_eq!(errors[0].path, "refs.0");
    }

    let gone = documents::insert(&pool, authors, None, &Prepared::plain(data("Gone")))
        .await
        .unwrap();
    documents::delete(&pool, authors, gone.id).await.unwrap();
    assert!(matches!(
        link(&pool, posts, "bad", &[(authors, gone.id)]).await,
        Err(DbError::Validation(_))
    ));
}

#[sqlx::test]
async fn linked_documents_cannot_be_deleted_until_unlinked(pool: PgPool) {
    let posts = collection(&pool, "posts").await;
    let authors = collection(&pool, "authors").await;
    let ann = documents::insert(&pool, authors, None, &Prepared::plain(data("Ann")))
        .await
        .unwrap();
    let post = link(&pool, posts, "p", &[(authors, ann.id)]).await.unwrap();

    assert!(matches!(
        documents::delete(&pool, authors, ann.id).await,
        Err(DbError::Referenced(1))
    ));

    let refs = documents::references(&pool, ann.id).await.unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(
        (
            refs[0].collection.as_str(),
            refs[0].id,
            refs[0].path.as_str()
        ),
        ("posts", post.id, "refs.0")
    );

    // Dropping the link through an update frees the target.
    documents::update(&pool, posts, post.id, |_| {
        Ok::<_, DbError>(Prepared::plain(data("p")))
    })
    .await
    .unwrap();
    documents::delete(&pool, authors, ann.id).await.unwrap();
}

#[sqlx::test]
async fn deleting_the_linker_frees_the_target_and_self_links_never_block(pool: PgPool) {
    let posts = collection(&pool, "posts").await;
    let a = documents::insert(&pool, posts, None, &Prepared::plain(data("a")))
        .await
        .unwrap();
    let b = link(&pool, posts, "b", &[(posts, a.id)]).await.unwrap();
    documents::delete(&pool, posts, b.id).await.unwrap();
    documents::delete(&pool, posts, a.id).await.unwrap();

    let me = Uuid::now_v7();
    let rel = Relation {
        path: "self".into(),
        collection_id: posts,
        id: me,
    };
    documents::insert(
        &pool,
        posts,
        Some(me),
        &Prepared {
            data: data("me"),
            relations: vec![rel],
        },
    )
    .await
    .unwrap();
    documents::delete(&pool, posts, me).await.unwrap();
}

#[sqlx::test]
async fn get_many_returns_live_documents_with_their_collection(pool: PgPool) {
    let posts = collection(&pool, "posts").await;
    let a = documents::insert(&pool, posts, None, &Prepared::plain(data("a")))
        .await
        .unwrap();
    let b = documents::insert(&pool, posts, None, &Prepared::plain(data("b")))
        .await
        .unwrap();
    documents::delete(&pool, posts, b.id).await.unwrap();
    let found = documents::get_many(&pool, &[a.id, b.id, Uuid::now_v7()])
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].0, found[0].1.id), (posts, a.id));
}

#[sqlx::test]
async fn insert_with_a_chosen_id_conflicts_on_reuse(pool: PgPool) {
    let posts = collection(&pool, "posts").await;
    let id = Uuid::now_v7();
    let first = documents::insert(&pool, posts, Some(id), &Prepared::plain(data("a")))
        .await
        .unwrap();
    assert_eq!(first.id, id);
    let err = documents::insert(&pool, posts, Some(id), &Prepared::plain(data("b")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation(ref c) if c == "documents_pkey"),
        "{err:?}"
    );
}
