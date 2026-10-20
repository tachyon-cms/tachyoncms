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
    let doc = documents::insert(&pool, c, 1, None, &Prepared::plain(data("Hello")))
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
    let doc = documents::insert(&pool, posts, 1, None, &Prepared::plain(data("Hello")))
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
        documents::insert(&pool, c, 1, None, &Prepared::plain(data(title)))
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
    let doc = documents::insert(&pool, c, 1, None, &Prepared::plain(data("Old")))
        .await
        .unwrap();
    let updated = documents::update(&pool, c, 1, doc.id, |current| {
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
    let res = documents::update(&pool, c, 1, Uuid::now_v7(), |_| {
        Ok::<_, DbError>(Prepared::plain(data("x")))
    })
    .await;
    assert!(matches!(res, Err(DbError::NotFound)));
}

#[sqlx::test]
async fn delete_hides_the_document(pool: PgPool) {
    let c = collection(&pool, "posts").await;
    let doc = documents::insert(&pool, c, 1, None, &Prepared::plain(data("Bye")))
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
        1,
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
    let ann = documents::insert(&pool, authors, 1, None, &Prepared::plain(data("Ann")))
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

    let gone = documents::insert(&pool, authors, 1, None, &Prepared::plain(data("Gone")))
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
    let ann = documents::insert(&pool, authors, 1, None, &Prepared::plain(data("Ann")))
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
    documents::update(&pool, posts, 1, post.id, |_| {
        Ok::<_, DbError>(Prepared::plain(data("p")))
    })
    .await
    .unwrap();
    documents::delete(&pool, authors, ann.id).await.unwrap();
}

#[sqlx::test]
async fn deleting_the_linker_frees_the_target_and_self_links_never_block(pool: PgPool) {
    let posts = collection(&pool, "posts").await;
    let a = documents::insert(&pool, posts, 1, None, &Prepared::plain(data("a")))
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
        1,
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
    let a = documents::insert(&pool, posts, 1, None, &Prepared::plain(data("a")))
        .await
        .unwrap();
    let b = documents::insert(&pool, posts, 1, None, &Prepared::plain(data("b")))
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
    let first = documents::insert(&pool, posts, 1, Some(id), &Prepared::plain(data("a")))
        .await
        .unwrap();
    assert_eq!(first.id, id);
    let err = documents::insert(&pool, posts, 1, Some(id), &Prepared::plain(data("b")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation(ref c) if c == "documents_pkey"),
        "{err:?}"
    );
}

#[sqlx::test]
async fn saving_documents_that_link_to_each_other_at_once_never_deadlocks(pool: PgPool) {
    let posts = collection(&pool, "posts").await;
    let a = documents::insert(&pool, posts, 1, None, &Prepared::plain(data("a")))
        .await
        .unwrap();
    let b = link(&pool, posts, "b", &[(posts, a.id)]).await.unwrap();
    let linking = |title: &str, to: Uuid| Prepared {
        data: data(title),
        relations: vec![Relation {
            path: "refs.0".into(),
            collection_id: posts,
            id: to,
        }],
    };
    for _ in 0..10 {
        let (ra, rb) = tokio::join!(
            documents::update(&pool, posts, 1, a.id, |_| Ok::<_, DbError>(linking(
                "a", b.id
            ))),
            documents::update(&pool, posts, 1, b.id, |_| Ok::<_, DbError>(linking(
                "b", a.id
            ))),
        );
        ra.unwrap();
        rb.unwrap();
    }
}

#[sqlx::test]
async fn writes_against_an_old_schema_version_are_refused(pool: PgPool) {
    let c = collection(&pool, "posts").await;
    let doc = documents::insert(&pool, c, 1, None, &Prepared::plain(data("a")))
        .await
        .unwrap();
    sqlx::query("UPDATE collections SET schema_version = 2 WHERE id = $1")
        .bind(c)
        .execute(&pool)
        .await
        .unwrap();

    let stale_insert = documents::insert(&pool, c, 1, None, &Prepared::plain(data("b"))).await;
    assert!(
        matches!(stale_insert, Err(DbError::SchemaChanged)),
        "{stale_insert:?}"
    );
    let stale_update = documents::update(&pool, c, 1, doc.id, |_| {
        Ok::<_, DbError>(Prepared::plain(data("c")))
    })
    .await;
    assert!(
        matches!(stale_update, Err(DbError::SchemaChanged)),
        "{stale_update:?}"
    );
    documents::insert(&pool, c, 2, None, &Prepared::plain(data("d")))
        .await
        .unwrap();
}

#[sqlx::test]
async fn rows_cant_point_across_tenants(pool: PgPool) {
    let c = collection(&pool, "posts").await;
    let doc = documents::insert(&pool, c, 1, None, &Prepared::plain(data("Hello")))
        .await
        .unwrap();
    let other = Uuid::now_v7();
    sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, 'other')")
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    let is_fk_violation = |r: Result<sqlx::postgres::PgQueryResult, sqlx::Error>| matches!(r, Err(sqlx::Error::Database(ref e)) if e.is_foreign_key_violation());

    let doc_in_other_tenant = sqlx::query(
        "INSERT INTO documents (id, tenant_id, collection_id, published) VALUES ($1, $2, $3, '{}')",
    )
    .bind(Uuid::now_v7())
    .bind(other)
    .bind(c)
    .execute(&pool)
    .await;
    assert!(
        is_fk_violation(doc_in_other_tenant),
        "a document can't sit in another tenant's collection"
    );

    let link_in_other_tenant = sqlx::query(
        "INSERT INTO relations (tenant_id, from_doc, field_path, to_doc) VALUES ($1, $2, 'x', $2)",
    )
    .bind(other)
    .bind(doc.id)
    .execute(&pool)
    .await;
    assert!(
        is_fk_violation(link_in_other_tenant),
        "a link can't join documents from another tenant"
    );
}

fn filter(qs: &str) -> tachyon_core::query::Filter {
    let schema: CollectionSchema = serde_json::from_value(json!({"slug": "any", "fields": [
        {"name": "title", "type": "text"},
        {"name": "author", "type": "text"}
    ]}))
    .unwrap();
    let params = tachyon_core::query::parse_query(qs).unwrap();
    tachyon_core::query::parse_filter(&schema.fields, &params["where"]).unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn read_filters_apply_to_get_many_by_collection(pool: PgPool) {
    use std::collections::HashMap;
    use tachyon_core::query::Filter;
    let posts = collection(&pool, "posts").await;
    let pages = collection(&pool, "pages").await;
    let notes = collection(&pool, "notes").await;
    let mut ids = Vec::new();
    for (c, title) in [
        (posts, "open"),
        (posts, "hidden"),
        (pages, "page"),
        (notes, "note"),
    ] {
        let doc = documents::insert(&pool, c, 1, None, &Prepared::plain(data(title)))
            .await
            .unwrap();
        ids.push(doc.id);
    }
    let titles = |found: Vec<(Uuid, documents::Document)>| {
        let mut t: Vec<String> = found
            .iter()
            .map(|(_, d)| d.data.0["title"].as_str().unwrap().to_owned())
            .collect();
        t.sort();
        t
    };

    // posts by filter, pages whole, notes not at all (missing from the map).
    let read = HashMap::from([
        (posts, Some(filter("where[title][equals]=open"))),
        (pages, None),
    ]);
    let found = query::get_many_readable(&pool, &ids, &read).await.unwrap();
    assert!(found.iter().all(|(c, _)| *c == posts || *c == pages));
    assert_eq!(titles(found), ["open", "page"]);

    // A filter that matches nothing hides the whole collection; no map hides everything.
    let read = HashMap::from([(posts, Some(Filter::Any(Vec::new()))), (pages, None)]);
    let found = query::get_many_readable(&pool, &ids, &read).await.unwrap();
    assert_eq!(titles(found), ["page"]);
    let found = query::get_many_readable(&pool, &ids, &HashMap::new())
        .await
        .unwrap();
    assert!(found.is_empty());
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn write_guards_are_checked_in_the_transaction(pool: PgPool) {
    let posts = collection(&pool, "posts").await;
    let insert = |title: &str| {
        let doc = json!({"title": title, "author": "me"});
        let pool = pool.clone();
        async move {
            documents::insert(
                &pool,
                posts,
                1,
                None,
                &Prepared::plain(doc.as_object().unwrap().clone()),
            )
            .await
            .unwrap()
            .id
        }
    };
    let mine = filter("where[author][equals]=me");
    let rename = |current: &Map<String, Value>| -> Result<Prepared, DbError> {
        let mut next = current.clone();
        next.insert("title".into(), json!("renamed"));
        Ok(Prepared::plain(next))
    };

    // A matching guard lets the write through.
    let a = insert("a").await;
    documents::update_as(&pool, posts, 1, a, None, Some(&mine), rename)
        .await
        .unwrap();
    // A guard that doesn't match refuses it and leaves the document alone.
    let other = filter("where[author][equals]=someone-else");
    let refused = documents::update_as(&pool, posts, 1, a, None, Some(&other), rename).await;
    assert!(matches!(refused, Err(DbError::Forbidden)), "{refused:?}");
    let refused = documents::delete_as(&pool, posts, a, None, Some(&other)).await;
    assert!(matches!(refused, Err(DbError::Forbidden)), "{refused:?}");
    assert_eq!(
        documents::get(&pool, posts, a).await.unwrap().data.0["title"],
        "renamed"
    );

    // Another transaction hands the document to someone else while the guarded write waits
    // on its lock: the write must judge the row it locked, not the one it saw first.
    for delete in [false, true] {
        let id = insert("b").await;
        let mut other_tx = pool.begin().await.unwrap();
        sqlx::query(
            "UPDATE documents SET published = jsonb_set(published, '{author}', '\"thief\"') WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *other_tx)
        .await
        .unwrap();
        let write = {
            let (pool, mine) = (pool.clone(), mine.clone());
            tokio::spawn(async move {
                if delete {
                    documents::delete_as(&pool, posts, id, None, Some(&mine)).await
                } else {
                    documents::update_as(&pool, posts, 1, id, None, Some(&mine), rename)
                        .await
                        .map(|_| ())
                }
            })
        };
        // Wait until the guarded write is blocked on the row lock.
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting > 0 {
                break;
            }
            assert!(
                !write.is_finished(),
                "the guarded write never waited for the lock"
            );
            tokio::task::yield_now().await;
        }
        other_tx.commit().await.unwrap();
        let result = write.await.unwrap();
        assert!(matches!(result, Err(DbError::Forbidden)), "{result:?}");
        let stored = documents::get(&pool, posts, id).await.unwrap();
        assert_eq!(stored.data.0["title"], "b");
        assert_eq!(stored.data.0["author"], "thief");
    }
}
