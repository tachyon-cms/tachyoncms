use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_core::query::{Cursor, Filter, Sort, parse_filter, parse_query, parse_sort};
use tachyon_db::documents::{self, Prepared};
use tachyon_db::query::{self, ListQuery};
use tachyon_db::{DbError, collections};
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

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn pages_report_whether_more_follow_and_near_refuses_cursors(pool: PgPool) {
    let s: CollectionSchema = serde_json::from_value(json!({"slug": "places", "fields": [
        {"name": "title", "type": "text"}, {"name": "at", "type": "point"}
    ]}))
    .unwrap();
    let c = collections::create(&pool, &s).await.unwrap().id;
    for (title, at) in [("A", json!([0.0, 0.0])), ("B", json!([0.001, 0.0]))] {
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
        &parse_query("where[at][near]=0,0").unwrap()["where"],
    )
    .unwrap();
    let page = |offset| ListQuery {
        filter: Some(&f),
        sort: None,
        limit: 1,
        offset,
        after: None,
        count: false,
    };
    let first = query::list(&pool, c, &page(0)).await.unwrap();
    assert!(first.has_more);
    let last = query::list(&pool, c, &page(1)).await.unwrap();
    assert!(!last.has_more);

    let exact = query::list(
        &pool,
        c,
        &ListQuery {
            filter: None,
            sort: None,
            limit: 2,
            offset: 0,
            after: None,
            count: false,
        },
    )
    .await
    .unwrap();
    assert!(
        !exact.has_more && exact.next.is_none(),
        "a page that ends exactly at the last document says so"
    );

    let cursor = Cursor {
        value: None,
        id: Uuid::nil(),
    };
    let refused = query::list(
        &pool,
        c,
        &ListQuery {
            filter: Some(&f),
            sort: None,
            limit: 1,
            offset: 0,
            after: Some(&cursor),
            count: false,
        },
    )
    .await;
    assert!(
        matches!(refused, Err(DbError::InvalidQuery(_))),
        "{refused:?}"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn big_integers_filter_and_page_exactly(pool: PgPool) {
    let c = collections::create(&pool, &schema()).await.unwrap().id;
    for price in [9007199254740993_u64, 9007199254740992] {
        let data = json!({"price": price}).as_object().unwrap().clone();
        documents::insert(&pool, c, 1, None, &Prepared::plain(data))
            .await
            .unwrap();
    }
    let prices = |page: &query::Page| -> Vec<u64> {
        page.docs
            .iter()
            .map(|d| d.data.0["price"].as_u64().unwrap())
            .collect()
    };
    let f = filter("where[price][equals]=9007199254740993");
    let page = query::list(&pool, c, &q(Some(&f), None)).await.unwrap();
    assert_eq!(prices(&page), [9007199254740993]);

    let s = sort("-price");
    let first = query::list(
        &pool,
        c,
        &ListQuery {
            limit: 1,
            ..q(None, Some(&s))
        },
    )
    .await
    .unwrap();
    assert_eq!(prices(&first), [9007199254740993]);
    let next = Cursor::decode(&first.next.unwrap().encode()).unwrap();
    let second = query::list(
        &pool,
        c,
        &ListQuery {
            limit: 1,
            after: Some(&next),
            ..q(None, Some(&s))
        },
    )
    .await
    .unwrap();
    assert_eq!(prices(&second), [9007199254740992]);
}

mod joins {
    use super::*;
    use std::collections::HashMap;
    use tachyon_core::joins::on_pattern;
    use tachyon_db::documents::Relation;
    use tachyon_db::query::{JoinQuery, joined};

    struct Fixture {
        posts: Uuid,
        categories: Uuid,
        cats: [Uuid; 2],
        schema: CollectionSchema,
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        let categories: CollectionSchema = serde_json::from_value(
            json!({"slug": "categories", "fields": [{"name": "name", "type": "text"}]}),
        )
        .unwrap();
        let categories = collections::create(pool, &categories).await.unwrap().id;
        let schema: CollectionSchema = serde_json::from_value(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "category", "type": "relationship", "relationTo": "categories"},
            {"name": "links", "type": "array", "fields": [
                {"name": "target", "type": "relationship", "relationTo": "categories"}
            ]}
        ]}))
        .unwrap();
        let posts = collections::create(pool, &schema).await.unwrap().id;
        let mut cats = [Uuid::nil(); 2];
        for (i, name) in ["news", "guides"].into_iter().enumerate() {
            let data = json!({"name": name}).as_object().unwrap().clone();
            cats[i] = documents::insert(pool, categories, 1, None, &Prepared::plain(data))
                .await
                .unwrap()
                .id;
        }
        Fixture {
            posts,
            categories,
            cats,
            schema,
        }
    }

    /// A post titled `title`, linking at each `(path, category)`.
    async fn post(pool: &PgPool, f: &Fixture, title: &str, links: &[(&str, Uuid)]) -> Uuid {
        let relations = links
            .iter()
            .map(|(path, id)| Relation {
                path: (*path).to_owned(),
                collection_id: f.categories,
                id: *id,
            })
            .collect();
        let data = json!({"title": title}).as_object().unwrap().clone();
        documents::insert(pool, f.posts, 1, None, &Prepared { data, relations })
            .await
            .unwrap()
            .id
    }

    fn titles(pages: &HashMap<Uuid, query::Page>, parent: Uuid) -> Vec<String> {
        pages
            .get(&parent)
            .map(|p| {
                p.docs
                    .iter()
                    .map(|d| d.data.0["title"].as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn q<'a>(f: &Fixture, pattern: &'a str) -> JoinQuery<'a> {
        JoinQuery {
            collection_id: f.posts,
            pattern,
            filter: None,
            sort: None,
            limit: 10,
            offset: 0,
            count: false,
        }
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn joined_documents_page_sort_filter_and_count_per_parent(pool: PgPool) {
        let f = fixture(&pool).await;
        let [news, guides] = f.cats;
        for t in ["a", "b", "c"] {
            post(&pool, &f, t, &[("category", news)]).await;
        }
        post(&pool, &f, "g", &[("category", guides)]).await;
        let pattern = on_pattern(f.schema.data(), "category").unwrap();

        let all = joined(&pool, &[news, guides], &q(&f, &pattern))
            .await
            .unwrap();
        assert_eq!(titles(&all, news), ["c", "b", "a"], "newest first");
        assert_eq!(titles(&all, guides), ["g"]);

        let by_title = parse_sort(&f.schema.fields, "title").unwrap();
        let page = JoinQuery {
            sort: Some(&by_title),
            limit: 2,
            offset: 0,
            count: true,
            ..q(&f, &pattern)
        };
        let pages = joined(&pool, &[news, guides], &page).await.unwrap();
        assert_eq!(titles(&pages, news), ["a", "b"]);
        assert!(pages[&news].has_more);
        assert_eq!(pages[&news].total, Some(3));
        assert!(!pages[&guides].has_more);
        assert_eq!(pages[&guides].total, Some(1));
        let second = JoinQuery { offset: 2, ..page };
        assert_eq!(
            titles(&joined(&pool, &[news], &second).await.unwrap(), news),
            ["c"]
        );

        let not_b = parse_filter(
            &f.schema.fields,
            &parse_query("where[title][not_equals]=b").unwrap()["where"],
        )
        .unwrap();
        let filtered = JoinQuery {
            filter: Some(&not_b),
            count: true,
            ..q(&f, &pattern)
        };
        let pages = joined(&pool, &[news], &filtered).await.unwrap();
        assert_eq!(titles(&pages, news), ["c", "a"]);
        assert_eq!(pages[&news].total, Some(2));
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn links_in_arrays_match_their_pattern_and_count_once(pool: PgPool) {
        let f = fixture(&pool).await;
        let [news, _] = f.cats;
        post(
            &pool,
            &f,
            "twice",
            &[("links.0.target", news), ("links.1.target", news)],
        )
        .await;
        post(&pool, &f, "direct", &[("category", news)]).await;
        let pattern = on_pattern(f.schema.data(), "links.target").unwrap();
        let pages = joined(
            &pool,
            &[news],
            &JoinQuery {
                count: true,
                ..q(&f, &pattern)
            },
        )
        .await
        .unwrap();
        assert_eq!(
            titles(&pages, news),
            ["twice"],
            "only links through links.target, once"
        );
        assert_eq!(pages[&news].total, Some(1));
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn deleted_linking_documents_are_left_out(pool: PgPool) {
        let f = fixture(&pool).await;
        let [news, _] = f.cats;
        let gone = post(&pool, &f, "gone", &[("category", news)]).await;
        post(&pool, &f, "kept", &[("category", news)]).await;
        sqlx::query("UPDATE documents SET deleted_at = now() WHERE id = $1")
            .bind(gone)
            .execute(&pool)
            .await
            .unwrap();
        let pattern = on_pattern(f.schema.data(), "category").unwrap();
        let pages = joined(
            &pool,
            &[news],
            &JoinQuery {
                count: true,
                ..q(&f, &pattern)
            },
        )
        .await
        .unwrap();
        assert_eq!(titles(&pages, news), ["kept"]);
        assert_eq!(pages[&news].total, Some(1));
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn repeated_parents_are_answered_once(pool: PgPool) {
        let f = fixture(&pool).await;
        let [news, _] = f.cats;
        post(&pool, &f, "a", &[("category", news)]).await;
        let pattern = on_pattern(f.schema.data(), "category").unwrap();
        let pages = joined(
            &pool,
            &[news, news],
            &JoinQuery {
                limit: 1,
                ..q(&f, &pattern)
            },
        )
        .await
        .unwrap();
        assert_eq!(titles(&pages, news), ["a"]);
        assert!(!pages[&news].has_more);
    }
}

mod cross {
    use super::*;
    use tachyon_core::query::parse_filter_with;

    struct World {
        posts: Uuid,
        schemas: Vec<CollectionSchema>,
        ann: Uuid,
    }

    fn s(v: Value) -> CollectionSchema {
        serde_json::from_value(v).unwrap()
    }

    async fn insert(pool: &PgPool, c: Uuid, v: Value) -> Uuid {
        documents::insert(
            pool,
            c,
            1,
            None,
            &Prepared::plain(v.as_object().unwrap().clone()),
        )
        .await
        .unwrap()
        .id
    }

    async fn world(pool: &PgPool) -> World {
        let companies =
            s(json!({"slug": "companies", "fields": [{"name": "name", "type": "text"}]}));
        let authors = s(json!({"slug": "authors", "fields": [
            {"name": "name", "type": "text"},
            {"name": "company", "type": "relationship", "relationTo": "companies"}
        ]}));
        let posts = s(json!({"slug": "posts", "fields": [
            {"name": "title", "type": "text"},
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "tags", "type": "relationship", "relationTo": "authors", "hasMany": true},
            {"name": "related", "type": "relationship", "relationTo": ["authors", "companies"], "hasMany": true},
            {"name": "one", "type": "relationship", "relationTo": ["authors", "companies"]},
            {"name": "sections", "type": "array", "fields": [
                {"name": "parts", "type": "array", "fields": [{"name": "n", "type": "number"}]}
            ]},
            {"name": "links", "type": "array", "fields": [
                {"name": "url", "type": "url"},
                {"name": "label", "type": "text"},
                {"name": "by", "type": "relationship", "relationTo": "authors"}
            ]}
        ]}));
        let company_id = collections::create(pool, &companies).await.unwrap().id;
        let author_id = collections::create(pool, &authors).await.unwrap().id;
        let post_id = collections::create(pool, &posts).await.unwrap().id;
        let acme = insert(pool, company_id, json!({"name": "Acme"})).await;
        let ann = insert(pool, author_id, json!({"name": "Ann", "company": acme})).await;
        let bo = insert(pool, author_id, json!({"name": "Bo"})).await;
        let gone = insert(pool, author_id, json!({"name": "Gone"})).await;
        // A company named Ann, to prove polymorphic links compare only the right collection.
        let ann_co = insert(pool, company_id, json!({"name": "Ann"})).await;
        for post in [
            json!({"title": "by-ann", "author": ann, "tags": [bo, ann],
                   "links": [{"url": "https://a.test", "label": "x", "by": ann}, {"url": "https://b.test", "label": "y"}]}),
            json!({"title": "by-bo", "author": bo, "tags": [bo],
                   "one": {"relationTo": "companies", "value": acme},
                   "sections": [{"parts": [{"n": 1}, {"n": 7}]}],
                   "related": [{"relationTo": "companies", "value": ann_co}],
                   "links": [{"url": "https://a.test", "label": "y"}]}),
            json!({"title": "alone", "links": []}),
            json!({"title": "broken", "author": "not-a-uuid", "tags": "nope", "links": [5, {"url": 3}]}),
            json!({"title": "by-gone", "author": gone}),
        ] {
            insert(pool, post_id, post).await;
        }
        sqlx::query("UPDATE documents SET deleted_at = now() WHERE id = $1")
            .bind(gone)
            .execute(pool)
            .await
            .unwrap();
        World {
            posts: post_id,
            schemas: vec![companies, authors, posts],
            ann,
        }
    }

    async fn titles_where(pool: &PgPool, w: &World, qs: &str) -> Vec<String> {
        let lookup = |slug: &str| w.schemas.iter().find(|s| s.slug == slug).cloned();
        let posts = w.schemas.iter().find(|s| s.slug == "posts").unwrap();
        let f =
            parse_filter_with(&posts.fields, &parse_query(qs).unwrap()["where"], &lookup).unwrap();
        let by_title = sort("title");
        let mut found = titles(pool, w.posts, q(Some(&f), Some(&by_title))).await;
        found.sort();
        found
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn filters_through_a_relationship(pool: PgPool) {
        let w = world(&pool).await;
        assert_eq!(
            titles_where(&pool, &w, "where[author.name][equals]=Ann").await,
            ["by-ann"]
        );
        assert_eq!(
            titles_where(&pool, &w, "where[tags.name][equals]=Bo").await,
            ["by-ann", "by-bo"]
        );
        assert_eq!(
            titles_where(&pool, &w, "where[author.company.name][equals]=Acme").await,
            ["by-ann"]
        );
        assert_eq!(
            titles_where(&pool, &w, "where[author.name][like]=o").await,
            ["by-bo"]
        );
        assert_eq!(
            titles_where(&pool, &w, "where[author.name][in]=Ann,Bo").await,
            ["by-ann", "by-bo"]
        );
        let id = w.ann.to_string();
        assert_eq!(
            titles_where(&pool, &w, &format!("where[author.id][equals]={id}")).await,
            ["by-ann"]
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn filters_into_list_rows_and_links_inside_them(pool: PgPool) {
        let w = world(&pool).await;
        assert_eq!(
            titles_where(&pool, &w, "where[links.url][equals]=https://a.test").await,
            ["by-ann", "by-bo"]
        );
        assert_eq!(
            titles_where(&pool, &w, "where[links.by.name][equals]=Ann").await,
            ["by-ann"]
        );
        assert_eq!(
            titles_where(&pool, &w, "where[links.label][exists]=true").await,
            ["by-ann", "by-bo"]
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn negatives_match_documents_with_no_match(pool: PgPool) {
        let w = world(&pool).await;
        assert_eq!(
            titles_where(&pool, &w, "where[author.name][not_equals]=Ann").await,
            ["alone", "broken", "by-bo", "by-gone"]
        );
        assert_eq!(
            titles_where(&pool, &w, "where[links.url][not_in]=https://a.test").await,
            ["alone", "broken", "by-gone"]
        );
        assert_eq!(
            titles_where(&pool, &w, "where[author.name][exists]=false").await,
            ["alone", "broken", "by-gone"],
            "no live author with a name"
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn each_condition_checks_its_own_row(pool: PgPool) {
        let w = world(&pool).await;
        // by-ann has a.test (label x) and b.test (label y): different rows satisfy each.
        assert_eq!(
            titles_where(
                &pool,
                &w,
                "where[and][0][links.url][equals]=https://b.test&where[and][1][links.label][equals]=x"
            )
            .await,
            ["by-ann"]
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn deleted_and_malformed_links_never_match(pool: PgPool) {
        let w = world(&pool).await;
        assert_eq!(
            titles_where(&pool, &w, "where[author.name][equals]=Gone").await,
            Vec::<String>::new()
        );
        assert_eq!(
            titles_where(&pool, &w, "where[author.name][like]=").await,
            ["by-ann", "by-bo"]
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn polymorphic_links_only_compare_the_right_collection(pool: PgPool) {
        let w = world(&pool).await;
        // by-bo links a *company* named Ann; the author named Ann isn't linked there.
        assert_eq!(
            titles_where(&pool, &w, "where[related.name][equals]=Ann").await,
            ["by-bo"]
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn single_polymorphic_links_nested_lists_and_exists_through_links(pool: PgPool) {
        let w = world(&pool).await;
        assert_eq!(
            titles_where(&pool, &w, "where[one.name][equals]=Acme").await,
            ["by-bo"]
        );
        assert_eq!(
            titles_where(&pool, &w, "where[sections.parts.n][gt]=5").await,
            ["by-bo"]
        );
        // Bo has no company: a live author exists, but the field doesn't.
        assert_eq!(
            titles_where(&pool, &w, "where[author.company][exists]=true").await,
            ["by-ann"]
        );
    }
}
