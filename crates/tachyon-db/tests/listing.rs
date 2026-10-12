use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_core::query::{Cursor, Filter, Sort, parse_filter, parse_query, parse_sort};
use tachyon_db::collections;
use tachyon_db::documents::{self, Prepared};
use tachyon_db::query::{self, ListQuery};
use uuid::Uuid;

fn schema() -> CollectionSchema {
    serde_json::from_value(json!({"slug": "items", "fields": [
        {"name": "title", "type": "text"},
        {"name": "price", "type": "number"},
        {"name": "live", "type": "boolean"},
        {"name": "at", "type": "date"},
        {"name": "tags", "type": "select", "options": ["a", "b"], "multiple": true},
        {"name": "seo", "type": "group", "fields": [{"name": "title", "type": "text"}]}
    ]}))
    .unwrap()
}

async fn seed(pool: &PgPool) -> Uuid {
    let c = collections::create(pool, &schema()).await.unwrap().id;
    for item in [
        json!({"title": "Apple", "price": 3, "live": true, "at": "2026-01-01T00:00:00.000Z", "tags": ["a"], "seo": {"title": "Red fruit"}}),
        json!({"title": "banana", "price": 1.5, "live": false, "at": "2026-02-01T00:00:00.000Z", "tags": ["a", "b"]}),
        json!({"title": "Cherry 100%", "price": 10, "live": true, "tags": []}),
        json!({"title": "date_fruit", "live": true}),
    ] {
        let data: Map<String, Value> = item.as_object().unwrap().clone();
        documents::insert(pool, c, 1, None, &Prepared::plain(data))
            .await
            .unwrap();
    }
    c
}

fn filter(qs: &str) -> Filter {
    parse_filter(&schema().fields, &parse_query(qs).unwrap()["where"]).unwrap()
}

fn sort(s: &str) -> Sort {
    parse_sort(&schema().fields, s).unwrap()
}

async fn titles(pool: &PgPool, c: Uuid, q: ListQuery<'_>) -> Vec<String> {
    query::list(pool, c, &q)
        .await
        .unwrap()
        .docs
        .iter()
        .map(|d| d.data.0["title"].as_str().unwrap().to_owned())
        .collect()
}

fn q<'a>(filter: Option<&'a Filter>, sort: Option<&'a Sort>) -> ListQuery<'a> {
    ListQuery {
        filter,
        sort,
        limit: 100,
        offset: 0,
        after: None,
        count: false,
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn filters_by_every_operator(pool: PgPool) {
    let c = seed(&pool).await;
    let by_title = sort("title");
    for (qs, expected) in [
        ("where[price][gt]=2", vec!["Apple", "Cherry 100%"]),
        ("where[price][lte]=3", vec!["Apple", "banana"]),
        ("where[price][in]=1.5,10", vec!["banana", "Cherry 100%"]),
        (
            "where[price][not_in]=3",
            vec!["banana", "Cherry 100%", "date_fruit"],
        ),
        (
            "where[price][not_equals]=3",
            vec!["banana", "Cherry 100%", "date_fruit"],
        ),
        ("where[price][exists]=false", vec!["date_fruit"]),
        ("where[live][equals]=false", vec!["banana"]),
        ("where[title][like]=AN", vec!["banana"]),
        ("where[title][like]=100%25", vec!["Cherry 100%"]),
        ("where[title][like]=_", vec!["date_fruit"]),
        ("where[at][gte]=2026-01-15T00:00:00Z", vec!["banana"]),
        ("where[tags][contains]=b", vec!["banana"]),
        ("where[seo.title][equals]=Red fruit", vec!["Apple"]),
        (
            "where[or][0][price][lt]=2&where[or][1][title][equals]=Apple",
            vec!["Apple", "banana"],
        ),
    ] {
        let f = filter(qs);
        assert_eq!(
            titles(&pool, c, q(Some(&f), Some(&by_title))).await,
            expected,
            "{qs}"
        );
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn sorts_with_nulls_last_both_ways(pool: PgPool) {
    let c = seed(&pool).await;
    let asc = sort("price");
    let desc = sort("-price");
    assert_eq!(
        titles(&pool, c, q(None, Some(&asc))).await,
        ["banana", "Apple", "Cherry 100%", "date_fruit"]
    );
    assert_eq!(
        titles(&pool, c, q(None, Some(&desc))).await,
        ["Cherry 100%", "Apple", "banana", "date_fruit"]
    );
    assert_eq!(
        titles(&pool, c, q(None, None)).await,
        ["date_fruit", "Cherry 100%", "banana", "Apple"],
        "default: newest first"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn cursors_walk_every_document_once(pool: PgPool) {
    let c = seed(&pool).await;
    for s in [
        None,
        Some(sort("price")),
        Some(sort("-price")),
        Some(sort("-title")),
        Some(sort("createdAt")),
    ] {
        let mut seen = Vec::new();
        let mut after: Option<Cursor> = None;
        loop {
            let page = query::list(
                &pool,
                c,
                &ListQuery {
                    filter: None,
                    sort: s.as_ref(),
                    limit: 1,
                    offset: 0,
                    after: after.as_ref(),
                    count: false,
                },
            )
            .await
            .unwrap();
            seen.extend(page.docs.iter().map(|d| d.id));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
            assert!(seen.len() <= 4, "cursor loop for {s:?}");
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 4, "{s:?}");
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn counts_match_the_filter(pool: PgPool) {
    let c = seed(&pool).await;
    let f = filter("where[live][equals]=true");
    let page = query::list(
        &pool,
        c,
        &ListQuery {
            filter: Some(&f),
            sort: None,
            limit: 1,
            offset: 0,
            after: None,
            count: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(page.docs.len(), 1);
    assert_eq!(page.total, Some(3));
}

#[test]
#[should_panic(expected = "not a safe identifier")]
fn field_expr_refuses_unsafe_names() {
    query::field_expr(
        &["x') OR 1=1 --".to_owned()],
        tachyon_core::query::ValueKind::Text,
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn numbers_compare_exactly(pool: PgPool) {
    let c = collections::create(&pool, &schema()).await.unwrap().id;
    for (title, price) in [
        ("small", json!(0.3)),
        ("tricky", json!(0.30000000000000004)),
        ("big", json!(1234567890123456_i64)),
    ] {
        let data = json!({"title": title, "price": price})
            .as_object()
            .unwrap()
            .clone();
        documents::insert(&pool, c, 1, None, &Prepared::plain(data))
            .await
            .unwrap();
    }
    let f = filter("where[price][equals]=1234567890123456");
    assert_eq!(titles(&pool, c, q(Some(&f), None)).await, ["big"]);
    let f = filter("where[price][in]=0.30000000000000004");
    assert_eq!(titles(&pool, c, q(Some(&f), None)).await, ["tricky"]);

    let by_price = sort("price");
    let mut seen = Vec::new();
    let mut after: Option<Cursor> = None;
    while seen.len() < 10 {
        let page = query::list(
            &pool,
            c,
            &ListQuery {
                filter: None,
                sort: Some(&by_price),
                limit: 1,
                offset: 0,
                after: after.as_ref(),
                count: false,
            },
        )
        .await
        .unwrap();
        seen.extend(
            page.docs
                .iter()
                .map(|d| d.data.0["title"].as_str().unwrap().to_owned()),
        );
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, ["small", "tricky", "big"]);
}
