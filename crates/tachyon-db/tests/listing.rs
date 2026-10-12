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

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn mistyped_stored_values_never_break_filters(pool: PgPool) {
    // Values left behind by a confirmed retype: a string in a number field, a string in a boolean.
    let c = collections::create(&pool, &schema()).await.unwrap().id;
    for item in [
        json!({"title": "ok", "price": 5, "live": false}),
        json!({"title": "bad", "price": "abc", "live": "yes"}),
    ] {
        let data = item.as_object().unwrap().clone();
        documents::insert(&pool, c, 1, None, &Prepared::plain(data))
            .await
            .unwrap();
    }
    let f = filter("where[price][gt]=1");
    assert_eq!(titles(&pool, c, q(Some(&f), None)).await, ["ok"]);
    let f = filter("where[live][equals]=false");
    assert_eq!(titles(&pool, c, q(Some(&f), None)).await, ["ok"]);
    let by_price = sort("price");
    assert_eq!(
        titles(&pool, c, q(None, Some(&by_price))).await,
        ["ok", "bad"]
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn contains_works_on_text_and_number_lists(pool: PgPool) {
    let s: CollectionSchema = serde_json::from_value(json!({"slug": "things", "fields": [
        {"name": "title", "type": "text"},
        {"name": "labels", "type": "text", "hasMany": true},
        {"name": "sizes", "type": "number", "hasMany": true}
    ]}))
    .unwrap();
    let c = collections::create(&pool, &s).await.unwrap().id;
    for item in [
        json!({"title": "a", "labels": ["red", "big"], "sizes": [1, 2.5]}),
        json!({"title": "b", "labels": ["blue"], "sizes": [3]}),
    ] {
        documents::insert(
            &pool,
            c,
            1,
            None,
            &Prepared::plain(item.as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    }
    let titles_for = |qs: &str| {
        let f = parse_filter(&s.fields, &parse_query(qs).unwrap()["where"]).unwrap();
        let pool = pool.clone();
        async move { titles(&pool, c, q(Some(&f), None)).await }
    };
    assert_eq!(titles_for("where[labels][contains]=big").await, ["a"]);
    assert_eq!(titles_for("where[sizes][contains]=2.5").await, ["a"]);
    assert_eq!(titles_for("where[sizes][contains]=3").await, ["b"]);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn near_finds_places_within_a_distance(pool: PgPool) {
    let s: CollectionSchema = serde_json::from_value(json!({"slug": "places", "fields": [
        {"name": "title", "type": "text"}, {"name": "at", "type": "point"}
    ]}))
    .unwrap();
    let c = collections::create(&pool, &s).await.unwrap().id;
    for (title, at) in [
        ("Trafalgar", json!([-0.1281, 51.5080])), // ~90 m from the centre
        ("Greenwich", json!([-0.0015, 51.4779])), // ~9 km
        ("Paris", json!([2.3522, 48.8566])),      // ~340 km
        ("Broken", json!("not a point")),         // left over from a retype
    ] {
        let data = json!({"title": title, "at": at})
            .as_object()
            .unwrap()
            .clone();
        documents::insert(&pool, c, 1, None, &Prepared::plain(data))
            .await
            .unwrap();
    }
    let near = |qs: &str| parse_filter(&s.fields, &parse_query(qs).unwrap()["where"]).unwrap();
    let by_title = parse_sort(&s.fields, "title").unwrap();
    let centre = "-0.1276,51.5072";
    let f = near(&format!("where[at][near]={centre},1000"));
    assert_eq!(
        titles(&pool, c, q(Some(&f), Some(&by_title))).await,
        ["Trafalgar"]
    );
    let f = near(&format!("where[at][near]={centre},20000,1000"));
    assert_eq!(
        titles(&pool, c, q(Some(&f), Some(&by_title))).await,
        ["Greenwich"]
    );
    let f = near(&format!("where[at][near]={centre}"));
    assert_eq!(
        titles(&pool, c, q(Some(&f), Some(&by_title))).await,
        ["Greenwich", "Paris", "Trafalgar"]
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn near_ignores_values_that_are_not_points(pool: PgPool) {
    let s: CollectionSchema = serde_json::from_value(json!({"slug": "places", "fields": [
        {"name": "title", "type": "text"}, {"name": "at", "type": "point"}
    ]}))
    .unwrap();
    let c = collections::create(&pool, &s).await.unwrap().id;
    for (title, at) in [
        ("Real", json!([-0.1276, 51.5072])),
        ("Three", json!([-0.1276, 51.5072, 7])),
        ("Object", json!({"0": -0.1276, "1": 51.5072})),
        ("OffMap", json!([0, 1000])),
    ] {
        let data = json!({"title": title, "at": at})
            .as_object()
            .unwrap()
            .clone();
        documents::insert(&pool, c, 1, None, &Prepared::plain(data))
            .await
            .unwrap();
    }
    // Too big for a float, written outside the API; must not make the query fail.
    sqlx::query(
        "INSERT INTO documents (id, tenant_id, collection_id, status, published)
         VALUES ($1, $2, $3, 'published', '{\"title\": \"Huge\", \"at\": [0, 1e400]}'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .bind(tachyon_db::DEFAULT_TENANT)
    .bind(c)
    .execute(&pool)
    .await
    .unwrap();
    let by_title = parse_sort(&s.fields, "title").unwrap();
    for qs in [
        "where[at][near]=-0.1276,51.5072",
        "where[at][near]=-0.1276,51.5072,1000",
    ] {
        let f = parse_filter(&s.fields, &parse_query(qs).unwrap()["where"]).unwrap();
        assert_eq!(
            titles(&pool, c, q(Some(&f), Some(&by_title))).await,
            ["Real"],
            "{qs}"
        );
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn near_without_a_sort_returns_nearest_first(pool: PgPool) {
    let s: CollectionSchema = serde_json::from_value(json!({"slug": "places", "fields": [
        {"name": "title", "type": "text"}, {"name": "at", "type": "point"}
    ]}))
    .unwrap();
    let c = collections::create(&pool, &s).await.unwrap().id;
    for (title, at) in [
        ("Paris", json!([2.3522, 48.8566])),
        ("Trafalgar", json!([-0.1281, 51.5080])),
        ("Greenwich", json!([-0.0015, 51.4779])),
    ] {
        let data = json!({"title": title, "at": at})
            .as_object()
            .unwrap()
            .clone();
        documents::insert(&pool, c, 1, None, &Prepared::plain(data))
            .await
            .unwrap();
    }
    let f = parse_filter(
        &s.fields,
        &parse_query("where[at][near]=-0.1276,51.5072").unwrap()["where"],
    )
    .unwrap();
    assert_eq!(
        titles(&pool, c, q(Some(&f), None)).await,
        ["Trafalgar", "Greenwich", "Paris"]
    );
    let by_title = parse_sort(&s.fields, "title").unwrap();
    assert_eq!(
        titles(&pool, c, q(Some(&f), Some(&by_title))).await,
        ["Greenwich", "Paris", "Trafalgar"],
        "an explicit sort wins"
    );
}
