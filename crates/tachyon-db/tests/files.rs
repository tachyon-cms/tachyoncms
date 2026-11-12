use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tachyon_core::CollectionSchema;
use tachyon_db::collections::{self, Collection};
use tachyon_db::documents::{self, FileChange, Prepared};
use tachyon_db::files::{self, CLEANUP, CLEANUP_ATTEMPTS, NewFile};
use tachyon_db::{DbError, DocCopy, versions};
use uuid::Uuid;

/// `images`, an upload collection with drafts that keeps up to 10 versions.
async fn seed(pool: &PgPool) -> Collection {
    let schema: CollectionSchema = serde_json::from_value(json!({
        "slug": "images", "upload": true,
        "versions": {"drafts": true, "maxVersions": 10},
        "fields": [{"name": "alt", "type": "text"}]
    }))
    .unwrap();
    collections::create(pool, &schema).await.unwrap()
}

fn new_file<'a>(
    c: &Collection,
    filename: &'a str,
    scope: &'a str,
    doc: Option<Uuid>,
) -> NewFile<'a> {
    NewFile {
        collection_id: c.id,
        storage: "default",
        filename,
        scope,
        mime_type: "image/png",
        filesize: 10,
        width: Some(4),
        height: Some(3),
        focal: (50.0, 50.0),
        blurhash: None,
        issued_to: None,
        document: doc,
    }
}

/// A pending file named `filename` (numbered on a clash), for document `doc` when given.
async fn reserve(
    pool: &PgPool,
    c: &Collection,
    filename: &str,
    scope: &str,
    doc: Option<Uuid>,
) -> files::FileRow {
    files::reserve(pool, &new_file(c, filename, scope, doc), |id, name| {
        Ok(format!("images/{id}/{name}"))
    })
    .await
    .unwrap()
}

fn with_file(file: FileChange) -> Prepared {
    Prepared {
        file,
        ..Prepared::plain(Map::new())
    }
}

/// A new draft holding `file`.
async fn draft_with(pool: &PgPool, c: &Collection, file: Uuid) -> Uuid {
    let mut tx = pool.begin().await.unwrap();
    let doc = documents::insert_in(
        &mut tx,
        c.id,
        c.schema_version,
        None,
        DocCopy::Draft,
        &with_file(FileChange::Set(file)),
        None,
    )
    .await
    .unwrap()
    .id;
    tx.commit().await.unwrap();
    doc
}

async fn set_draft_file(pool: &PgPool, c: &Collection, doc: Uuid, file: Uuid) {
    let mut tx = pool.begin().await.unwrap();
    documents::update_in::<DbError>(
        &mut tx,
        c.id,
        c.schema_version,
        doc,
        DocCopy::Draft,
        None,
        None,
        |_| Ok(with_file(FileChange::Set(file))),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn publish(pool: &PgPool, c: &Collection, doc: Uuid) {
    let mut tx = pool.begin().await.unwrap();
    documents::publish_in(&mut tx, c, doc, None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn record(pool: &PgPool, c: &Collection, doc: Uuid, kind: DocCopy) -> Uuid {
    let mut conn = pool.acquire().await.unwrap();
    versions::record_version(&mut conn, doc, c, kind, &Map::new(), None)
        .await
        .unwrap()
}

async fn state(pool: &PgPool, file: Uuid) -> String {
    files::get(pool, file).await.unwrap().state
}

/// Pending `file_cleanup` jobs for `file`.
async fn cleanups(pool: &PgPool, file: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND payload->>'file' = $2
           AND state IN ('queued', 'running') AND run_at <= now()",
    )
    .bind(CLEANUP)
    .bind(file.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn doc_files(pool: &PgPool, doc: Uuid) -> (Option<Uuid>, Option<Uuid>) {
    sqlx::query_as("SELECT published_file, draft_file FROM documents WHERE id = $1")
        .bind(doc)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_file_lives_while_anything_points_at_it(pool: PgPool) {
    let c = seed(&pool).await;
    let a = reserve(&pool, &c, "a.png", "", None).await.id;
    let doc = draft_with(&pool, &c, a).await;
    publish(&pool, &c, doc).await;
    assert_eq!(doc_files(&pool, doc).await, (Some(a), Some(a)), "shared");

    let b = reserve(&pool, &c, "b.png", "", Some(doc)).await.id;
    set_draft_file(&pool, &c, doc, b).await;
    assert_eq!(state(&pool, a).await, "live", "the published copy holds it");

    publish(&pool, &c, doc).await;
    assert_eq!(doc_files(&pool, doc).await, (Some(b), Some(b)));
    assert_eq!(state(&pool, a).await, "live", "a version holds it");
    assert_eq!(cleanups(&pool, a).await, 0);

    let mut one = c.clone();
    one.schema.versions.as_mut().unwrap().max_versions = 1;
    record(&pool, &one, doc, DocCopy::Draft).await;
    assert_eq!(state(&pool, a).await, "gone");
    assert_eq!(cleanups(&pool, a).await, 1);
    assert_eq!(state(&pool, b).await, "live");
    assert_eq!(cleanups(&pool, b).await, 0);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn restore_brings_back_the_old_file(pool: PgPool) {
    let c = seed(&pool).await;
    let a = reserve(&pool, &c, "a.png", "", None).await.id;
    let doc = draft_with(&pool, &c, a).await;
    publish(&pool, &c, doc).await;
    let old = versions::list(&pool, doc, true, 1, 0).await.unwrap()[0].id;
    let b = reserve(&pool, &c, "b.png", "", Some(doc)).await.id;
    set_draft_file(&pool, &c, doc, b).await;
    publish(&pool, &c, doc).await;

    let v = versions::get(&pool, doc, old).await.unwrap();
    assert_eq!(v.file_id, Some(a));
    set_draft_file(&pool, &c, doc, v.file_id.unwrap()).await;
    assert_eq!(doc_files(&pool, doc).await, (Some(b), Some(a)));
    assert_eq!(state(&pool, b).await, "live", "the published copy holds it");
    assert_eq!(cleanups(&pool, b).await, 0);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn purge_releases_files_and_their_versions_files(pool: PgPool) {
    let c = seed(&pool).await;
    let a = reserve(&pool, &c, "a.png", "", None).await.id;
    let doc = draft_with(&pool, &c, a).await;
    publish(&pool, &c, doc).await;
    let cc = reserve(&pool, &c, "c.png", "", Some(doc)).await.id;
    set_draft_file(&pool, &c, doc, cc).await;
    record(&pool, &c, doc, DocCopy::Draft).await;
    let b = reserve(&pool, &c, "b.png", "", Some(doc)).await.id;
    set_draft_file(&pool, &c, doc, b).await;
    assert_eq!(doc_files(&pool, doc).await, (Some(a), Some(b)));
    let held: Vec<Option<Uuid>> =
        sqlx::query_scalar("SELECT file_id FROM versions WHERE document_id = $1 ORDER BY id")
            .bind(doc)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(held, [Some(a), Some(cc)]);

    documents::delete(&pool, c.id, doc).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(documents::purge_in(&mut tx, c.id, &[doc]).await.unwrap(), 1);
    tx.commit().await.unwrap();
    for f in [a, b, cc] {
        assert_eq!(state(&pool, f).await, "gone");
        assert_eq!(cleanups(&pool, f).await, 1, "exactly one cleanup");
    }
    let left: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM documents WHERE id = $1) + (SELECT count(*) FROM versions WHERE document_id = $1)",
    )
    .bind(doc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn trash_keeps_files(pool: PgPool) {
    let c = seed(&pool).await;
    let a = reserve(&pool, &c, "a.png", "", None).await.id;
    let doc = draft_with(&pool, &c, a).await;
    publish(&pool, &c, doc).await;

    documents::delete(&pool, c.id, doc).await.unwrap();
    assert_eq!(state(&pool, a).await, "live", "in the trash");
    let mut tx = pool.begin().await.unwrap();
    let copies = [
        (DocCopy::Draft, with_file(FileChange::Keep)),
        (DocCopy::Published, with_file(FileChange::Keep)),
    ];
    documents::restore_in(&mut tx, &c, doc, &copies, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(state(&pool, a).await, "live");
    assert_eq!(doc_files(&pool, doc).await, (Some(a), Some(a)));
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND state IN ('queued', 'running')",
    )
    .bind(CLEANUP)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs, 0, "no job");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn reserve_numbers_clashes(pool: PgPool) {
    let c = seed(&pool).await;
    let mut names = Vec::new();
    let mut docs = Vec::new();
    for _ in 0..3 {
        let f = reserve(&pool, &c, "a.png", "", None).await;
        names.push(f.filename.clone());
        docs.push(draft_with(&pool, &c, f.id).await);
    }
    assert_eq!(names, ["a.png", "a-1.png", "a-2.png"]);

    // Another scope numbers on its own, pending files counted.
    assert_eq!(
        reserve(&pool, &c, "a.png", r#"["x"]"#, None).await.filename,
        "a.png"
    );
    assert_eq!(
        reserve(&pool, &c, "a.png", r#"["x"]"#, None).await.filename,
        "a-1.png"
    );

    // Replacing its own file keeps the name.
    let again = reserve(&pool, &c, "a.png", "", Some(docs[0])).await;
    assert_eq!(again.filename, "a.png");
    set_draft_file(&pool, &c, docs[0], again.id).await;

    // A name only a version still holds is free.
    let b = reserve(&pool, &c, "b.png", "", None).await.id;
    let other = draft_with(&pool, &c, b).await;
    record(&pool, &c, other, DocCopy::Draft).await;
    let next = reserve(&pool, &c, "next.png", "", Some(other)).await.id;
    set_draft_file(&pool, &c, other, next).await;
    assert_eq!(state(&pool, b).await, "live", "its version holds it");
    assert_eq!(
        reserve(&pool, &c, "b.png", "", None).await.filename,
        "b.png"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn pending_files_expire(pool: PgPool) {
    let c = seed(&pool).await;
    let f = reserve(&pool, &c, "a.png", "", None).await;
    assert_eq!(f.state, "pending");
    assert_eq!(f.key, format!("images/{}/a.png", f.id));
    assert_eq!(f.names, ["a.png"]);
    let (state, wait, max_attempts, payload): (String, f64, i32, Value) = sqlx::query_as(
        "SELECT state, extract(epoch FROM run_at - now())::float8, max_attempts, payload
         FROM jobs WHERE dedupe_key = $1",
    )
    .bind(format!("{CLEANUP}:{}", f.id))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "queued");
    assert!((3590.0..=3600.0).contains(&wait), "+1 h, was {wait}");
    assert_eq!(max_attempts, CLEANUP_ATTEMPTS);
    assert_eq!(CLEANUP_ATTEMPTS, 20);
    assert_eq!(payload, json!({"file": f.id}));

    let mut tx = pool.begin().await.unwrap();
    files::go_live_in(&mut tx, c.id, f.id).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(files::get(&pool, f.id).await.unwrap().state, "live");
    let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE dedupe_key = $1")
        .bind(format!("{CLEANUP}:{}", f.id))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "cancelled");

    // A key that can't be made keeps nothing.
    let refused = files::reserve(&pool, &new_file(&c, "b.png", "", None), |_, _| {
        Err("no".into())
    })
    .await;
    assert!(
        matches!(refused, Err(DbError::Refused(ref why)) if why == "no"),
        "{refused:?}"
    );
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM files WHERE filename = 'b.png'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 0);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn lookups_find_live_files_and_their_holders(pool: PgPool) {
    let c = seed(&pool).await;
    let (user, key) = (Uuid::now_v7(), Uuid::now_v7());
    let issued = NewFile {
        issued_to: Some((user, Some(key))),
        ..new_file(&c, "a.png", "", None)
    };
    let a = files::reserve(&pool, &issued, |id, name| Ok(format!("images/{id}/{name}")))
        .await
        .unwrap();
    assert!(
        files::by_name(&pool, c.id, "a.png")
            .await
            .unwrap()
            .is_empty(),
        "pending"
    );

    // Only whoever it was issued to claims it, with the same key.
    let mut tx = pool.begin().await.unwrap();
    for (u, k) in [(user, None), (Uuid::now_v7(), Some(key))] {
        let none = files::claim_pending(&mut tx, c.id, "a.png", u, k, None)
            .await
            .unwrap();
        assert!(none.is_none());
    }
    let claimed = files::claim_pending(&mut tx, c.id, "a.png", user, Some(key), None)
        .await
        .unwrap();
    assert_eq!(claimed.map(|f| f.id), Some(a.id));
    tx.commit().await.unwrap();

    let doc = draft_with(&pool, &c, a.id).await;
    let found: Vec<Uuid> = files::by_name(&pool, c.id, "a.png")
        .await
        .unwrap()
        .iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(found, [a.id]);
    assert_eq!(
        files::holders(&pool, a.id).await.unwrap(),
        [(doc, DocCopy::Draft)]
    );
    publish(&pool, &c, doc).await;
    assert_eq!(
        files::holders(&pool, a.id).await.unwrap(),
        [(doc, DocCopy::Published), (doc, DocCopy::Draft)]
    );
    documents::delete(&pool, c.id, doc).await.unwrap();
    assert!(
        files::holders(&pool, a.id).await.unwrap().is_empty(),
        "only live documents"
    );

    // A gone file can't be taken again.
    sqlx::query("UPDATE files SET state = 'gone' WHERE id = $1")
        .bind(a.id)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let refused = files::go_live_in(&mut tx, c.id, a.id).await;
    assert!(matches!(refused, Err(DbError::Refused(_))), "{refused:?}");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn unpublish_releases_the_published_file(pool: PgPool) {
    let mut c = seed(&pool).await;
    c.schema.versions.as_mut().unwrap().max_versions = 1;
    let a = reserve(&pool, &c, "a.png", "", None).await.id;
    let doc = draft_with(&pool, &c, a).await;
    publish(&pool, &c, doc).await;
    let b = reserve(&pool, &c, "b.png", "", Some(doc)).await.id;
    set_draft_file(&pool, &c, doc, b).await;
    record(&pool, &c, doc, DocCopy::Draft).await;
    assert_eq!(state(&pool, a).await, "live", "the published copy holds it");

    let mut tx = pool.begin().await.unwrap();
    documents::unpublish_in(&mut tx, &c, doc, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(doc_files(&pool, doc).await, (None, Some(b)));
    assert_eq!(state(&pool, a).await, "gone");
    assert_eq!(cleanups(&pool, a).await, 1);
    assert_eq!(state(&pool, b).await, "live");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn schema_changes_keep_file_references(pool: PgPool) {
    use tachyon_core::diff::Renames;
    use tachyon_db::schema_change::apply;
    let c = seed(&pool).await;
    let a = reserve(&pool, &c, "a.png", "", None).await.id;
    let doc = draft_with(&pool, &c, a).await;
    publish(&pool, &c, doc).await;
    let b = reserve(&pool, &c, "b.png", "", Some(doc)).await.id;
    set_draft_file(&pool, &c, doc, b).await;
    publish(&pool, &c, doc).await;
    let images = |versions: Value| -> CollectionSchema {
        serde_json::from_value(json!({
            "slug": "images", "upload": true, "versions": versions,
            "fields": [{"name": "alt", "type": "text"}]
        }))
        .unwrap()
    };

    // Drafts off: the draft's reference goes with it.
    apply(
        &pool,
        "images",
        &images(json!({"maxVersions": 10})),
        &Renames::new(),
        true,
    )
    .await
    .unwrap();
    assert_eq!(doc_files(&pool, doc).await, (Some(b), None));
    assert_eq!(state(&pool, b).await, "live");

    // Drafts on: the draft starts with the published copy's file.
    let on = images(json!({"drafts": true, "maxVersions": 10}));
    apply(&pool, "images", &on, &Renames::new(), true)
        .await
        .unwrap();
    assert_eq!(doc_files(&pool, doc).await, (Some(b), Some(b)));

    // Pruning past a lower limit releases what only the dropped versions held.
    assert_eq!(state(&pool, a).await, "live");
    let one = images(json!({"drafts": true, "maxVersions": 1}));
    apply(&pool, "images", &one, &Renames::new(), true)
        .await
        .unwrap();
    assert_eq!(state(&pool, a).await, "gone");
    assert_eq!(cleanups(&pool, a).await, 1);
    assert_eq!(state(&pool, b).await, "live");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn many_clashes_take_the_next_number(pool: PgPool) {
    let c = seed(&pool).await;
    for n in 0..50 {
        let f = reserve(&pool, &c, "image.jpg", "", None).await;
        let want = match n {
            0 => "image.jpg".to_owned(),
            n => format!("image-{n}.jpg"),
        };
        assert_eq!(f.filename, want);
    }
    // Names that only look numbered don't fill a gap.
    reserve(&pool, &c, "image-x.jpg", "", None).await;
    reserve(&pool, &c, "image-7.png", "", None).await;
    assert_eq!(
        reserve(&pool, &c, "image.jpg", "", None).await.filename,
        "image-50.jpg"
    );
    // A gap is filled first.
    sqlx::query("UPDATE files SET state = 'gone' WHERE filename = 'image-3.jpg'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        reserve(&pool, &c, "image.jpg", "", None).await.filename,
        "image-3.jpg"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_retried_replacement_keeps_its_name(pool: PgPool) {
    let c = seed(&pool).await;
    let a = reserve(&pool, &c, "a.png", "", None).await.id;
    let doc = draft_with(&pool, &c, a).await;
    // The first try's pending file is still there when the replacement is sent again.
    assert_eq!(
        reserve(&pool, &c, "a.png", "", Some(doc)).await.filename,
        "a.png"
    );
    assert_eq!(
        reserve(&pool, &c, "a.png", "", Some(doc)).await.filename,
        "a.png"
    );
    // Anyone else's upload still clashes with both.
    assert_eq!(
        reserve(&pool, &c, "a.png", "", None).await.filename,
        "a-1.png"
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_file_stays_in_its_collection(pool: PgPool) {
    let c = seed(&pool).await;
    let other: CollectionSchema =
        serde_json::from_value(json!({"slug": "docs", "upload": true, "fields": []})).unwrap();
    let other = collections::create(&pool, &other).await.unwrap();
    let theirs = reserve(&pool, &other, "a.pdf", "", None).await.id;
    let mut tx = pool.begin().await.unwrap();
    let refused = documents::insert_in(
        &mut tx,
        c.id,
        c.schema_version,
        None,
        DocCopy::Draft,
        &with_file(FileChange::Set(theirs)),
        None,
    )
    .await;
    assert!(matches!(refused, Err(DbError::Refused(_))), "{refused:?}");
    drop(tx);
    assert_eq!(state(&pool, theirs).await, "pending");
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn swapping_files_between_documents_never_deadlocks(pool: PgPool) {
    let c = seed(&pool).await;
    let (a, b) = (
        reserve(&pool, &c, "a.png", "", None).await.id,
        reserve(&pool, &c, "b.png", "", None).await.id,
    );
    let (one, two) = (
        draft_with(&pool, &c, a).await,
        draft_with(&pool, &c, b).await,
    );
    // Versions hold both files, so neither is ever left without a reference mid-swap.
    record(&pool, &c, one, DocCopy::Draft).await;
    record(&pool, &c, two, DocCopy::Draft).await;
    let swap = |doc: Uuid, file: Uuid| {
        let (pool, c) = (pool.clone(), c.clone());
        async move {
            let mut tx = pool.begin().await?;
            documents::update_in::<DbError>(
                &mut tx,
                c.id,
                c.schema_version,
                doc,
                DocCopy::Draft,
                None,
                None,
                |_| Ok(with_file(FileChange::Set(file))),
            )
            .await?;
            // Holding the locks a moment makes the two overlap.
            sqlx::query("SELECT pg_sleep(0.01)")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok::<_, DbError>(())
        }
    };
    for round in 0..20 {
        let (x, y) = if round % 2 == 0 { (a, b) } else { (b, a) };
        let (r1, r2) = tokio::join!(swap(one, y), swap(two, x));
        assert!(r1.is_ok() && r2.is_ok(), "round {round}: {r1:?} {r2:?}");
    }
    assert_eq!(
        (state(&pool, a).await, state(&pool, b).await),
        ("live".into(), "live".into())
    );
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_draft_with_another_file_is_pending(pool: PgPool) {
    use tachyon_core::diff::Renames;
    use tachyon_db::schema_change::{ChangeError, apply};
    let c = seed(&pool).await;
    let a = reserve(&pool, &c, "a.png", "", None).await.id;
    let doc = draft_with(&pool, &c, a).await;
    publish(&pool, &c, doc).await;
    // The same JSON, another file.
    let b = reserve(&pool, &c, "a.png", "", Some(doc)).await.id;
    set_draft_file(&pool, &c, doc, b).await;
    let off: CollectionSchema = serde_json::from_value(json!({
        "slug": "images", "upload": true, "versions": {"maxVersions": 10},
        "fields": [{"name": "alt", "type": "text"}]
    }))
    .unwrap();
    let refused = apply(&pool, "images", &off, &Renames::new(), true).await;
    assert!(
        matches!(refused, Err(ChangeError::PendingDrafts(_))),
        "{refused:?}"
    );
    assert_eq!(doc_files(&pool, doc).await, (Some(a), Some(b)));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_restored_file_is_renumbered_on_a_clash(pool: PgPool) {
    let c = seed(&pool).await;
    let named = |file: FileChange| Prepared {
        file,
        ..Prepared::plain(
            json!({"filename": "a.png", "alt": "x"})
                .as_object()
                .unwrap()
                .clone(),
        )
    };
    let a = reserve(&pool, &c, "a.png", "", None).await.id;
    let mut tx = pool.begin().await.unwrap();
    let doc = documents::insert_in(
        &mut tx,
        c.id,
        c.schema_version,
        None,
        DocCopy::Draft,
        &named(FileChange::Set(a)),
        None,
    )
    .await
    .unwrap()
    .id;
    tx.commit().await.unwrap();
    publish(&pool, &c, doc).await;
    documents::delete(&pool, c.id, doc).await.unwrap();

    // The name is free while it's in the trash.
    let b = reserve(&pool, &c, "a.png", "", None).await;
    assert_eq!(b.filename, "a.png");
    let other = draft_with(&pool, &c, b.id).await;

    let mut tx = pool.begin().await.unwrap();
    let copies = [
        (DocCopy::Draft, named(FileChange::Keep)),
        (DocCopy::Published, named(FileChange::Keep)),
    ];
    let (restored, _) = documents::restore_in(&mut tx, &c, doc, &copies, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(restored.data["filename"], "a-1.png");
    let f = files::get(&pool, a).await.unwrap();
    assert_eq!(
        (f.filename.as_str(), f.names.clone()),
        ("a-1.png", vec!["a-1.png".to_owned()])
    );
    let (published, draft): (Value, Value) =
        sqlx::query_as("SELECT published, draft FROM documents WHERE id = $1")
            .bind(doc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (&published["filename"], &draft["filename"]),
        (&json!("a-1.png"), &json!("a-1.png"))
    );
    assert_eq!(published["alt"], "x");
    assert_eq!(
        files::get(&pool, b.id).await.unwrap().filename,
        "a.png",
        "the other keeps it"
    );
    assert_eq!(doc_files(&pool, other).await.1, Some(b.id));

    // Without a clash, a restore keeps the name.
    documents::delete(&pool, c.id, doc).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    documents::restore_in(&mut tx, &c, doc, &copies, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(files::get(&pool, a).await.unwrap().filename, "a-1.png");
}

/// Document `doc`, just trashed, restored with both copies as stored.
async fn restore(pool: &PgPool, c: &Collection, doc: Uuid) -> Result<(), DbError> {
    let named = |file: FileChange| Prepared {
        file,
        ..Prepared::plain(json!({"filename": "a.png"}).as_object().unwrap().clone())
    };
    let mut tx = pool.begin().await?;
    let copies = [
        (DocCopy::Draft, named(FileChange::Keep)),
        (DocCopy::Published, named(FileChange::Keep)),
    ];
    documents::restore_in(&mut tx, c, doc, &copies, None).await?;
    tx.commit().await?;
    Ok(())
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_restore_never_gives_two_files_one_new_name(pool: PgPool) {
    let c = seed(&pool).await;
    let a = reserve(&pool, &c, "a.png", "", None).await.id;
    let doc = draft_with(&pool, &c, a).await;
    publish(&pool, &c, doc).await;
    // The replacement keeps the name: the document's own file doesn't clash.
    let b = reserve(&pool, &c, "a.png", "", Some(doc)).await;
    assert_eq!(b.filename, "a.png");
    set_draft_file(&pool, &c, doc, b.id).await;
    documents::delete(&pool, c.id, doc).await.unwrap();
    let theirs = reserve(&pool, &c, "a.png", "", None).await;
    assert_eq!(theirs.filename, "a.png");
    draft_with(&pool, &c, theirs.id).await;

    restore(&pool, &c, doc).await.unwrap();
    let names = (
        files::get(&pool, a).await.unwrap().filename,
        files::get(&pool, b.id).await.unwrap().filename,
    );
    assert_eq!(
        names,
        ("a-1.png".into(), "a-2.png".into()),
        "distinct, in ID order"
    );
    let (published, draft): (Value, Value) =
        sqlx::query_as("SELECT published, draft FROM documents WHERE id = $1")
            .bind(doc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (&published["filename"], &draft["filename"]),
        (&json!("a-1.png"), &json!("a-2.png"))
    );
}

/// Takes the numbering lock `files::free_name` takes for `scope` of `c`, waiting at most
/// `timeout`.
async fn numbering_lock(
    tx: &mut sqlx::PgConnection,
    c: &Collection,
    scope: &str,
    timeout: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL lock_timeout = '{timeout}'"
    )))
    .execute(&mut *tx)
    .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("files:{}:{scope}", c.id))
        .execute(&mut *tx)
        .await?;
    Ok(())
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_restore_takes_numbering_locks_in_scope_order(pool: PgPool) {
    let c = seed(&pool).await;
    // The `y` file sorts first by ID, the `x` file first by scope.
    let y = reserve(&pool, &c, "a.png", r#"["y"]"#, None).await.id;
    let x = reserve(&pool, &c, "a.png", r#"["x"]"#, None).await.id;
    let doc = draft_with(&pool, &c, y).await;
    publish(&pool, &c, doc).await;
    set_draft_file(&pool, &c, doc, x).await;
    documents::delete(&pool, c.id, doc).await.unwrap();

    // Another writer holds `x` and will want `y` next, as a restore in the other order would.
    let mut other = pool.begin().await.unwrap();
    numbering_lock(&mut other, &c, r#"["x"]"#, "5s")
        .await
        .unwrap();
    let restoring = tokio::spawn({
        let (pool, c) = (pool.clone(), c.clone());
        async move { restore(&pool, &c, doc).await }
    });
    // Wait until the restore is blocked on an advisory lock, failing if it never is.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'
               AND wait_event = 'advisory'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        assert!(
            !restoring.is_finished(),
            "the restore never waited for the `x` lock"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "the restore didn't block within 10 s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // The restore waits on `x` holding nothing else, so `y` is free.
    numbering_lock(&mut other, &c, r#"["y"]"#, "2s")
        .await
        .unwrap();
    other.commit().await.unwrap();
    restoring.await.unwrap().unwrap();
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn failed_cleanups_outlive_the_job_purge(pool: PgPool) {
    let c = seed(&pool).await;
    let gone = reserve(&pool, &c, "gone.png", "", None).await;
    let live = reserve(&pool, &c, "live.png", "", None).await;
    let mut tx = pool.begin().await.unwrap();
    files::go_live_in(&mut tx, c.id, live.id).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("UPDATE files SET state = 'gone' WHERE id = $1")
        .bind(gone.id)
        .execute(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let mut add = async |kind: &str, payload: Value, ran_out: bool| {
        let id = tachyon_db::jobs::add(
            &mut conn,
            &tachyon_db::jobs::NewJob {
                kind,
                document_id: None,
                collection_id: None,
                payload,
                run_at: time::OffsetDateTime::now_utc(),
                dedupe_key: None,
                created_by: None,
                key_id: None,
                max_attempts: None,
            },
        )
        .await
        .unwrap();
        // Failed for good, longer ago than jobs are kept.
        sqlx::query(
            "UPDATE jobs SET state = 'failed', finished_at = now() - interval '31 days',
                    attempts = CASE WHEN $2 THEN max_attempts ELSE 1 END WHERE id = $1",
        )
        .bind(id)
        .bind(ran_out)
        .execute(&pool)
        .await
        .unwrap();
        id
    };
    let for_gone = add(CLEANUP, json!({ "file": gone.id }), false).await;
    let for_keys = add(
        CLEANUP,
        json!({"keys": ["a/b/c.png"], "storage": "default"}),
        true,
    )
    .await;
    // Refused for its payload, on its first run: it would never be done.
    let refused = add(
        CLEANUP,
        json!({"keys": "a/b/c.png", "storage": "default"}),
        false,
    )
    .await;
    let for_live = add(CLEANUP, json!({ "file": live.id }), true).await;
    let other = add("publish", json!({}), true).await;
    drop(conn);
    tachyon_db::jobs::claim(&pool, std::time::Duration::from_secs(60))
        .await
        .unwrap();
    let left: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM jobs WHERE state = 'failed' ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        left,
        [for_gone, for_keys],
        "kept while their objects may be in storage"
    );
    assert!(!left.contains(&for_live) && !left.contains(&other) && !left.contains(&refused));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_size_being_written_holds_off_a_keys_cleanup(pool: PgPool) {
    let c = seed(&pool).await;
    let f = reserve(&pool, &c, "a.png", "", None).await;
    let doc = draft_with(&pool, &c, f.id).await;
    let key = format!("images/{}/a-4x3.png", f.id);
    let mut writing = pool.begin().await.unwrap();
    let locked = files::lock_for_size_in(&mut writing, c.id, f.id)
        .await
        .unwrap();
    assert!(matches!(locked, files::SizeLock::Live { ref file, .. } if file.id == f.id));
    let checking = {
        let (pool, key) = (pool.clone(), key.clone());
        tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            files::key_in_use_in(&mut tx, "default", &key)
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!checking.is_finished(), "the check waits on the size");
    let entry = json!({"width": 4, "height": 3, "mimeType": "image/png", "filesize": 1,
                       "filename": "a-4x3.png"});
    let written = files::fill_size_in(&mut writing, f.id, "thumb", &entry, &["a-4x3.png".into()])
        .await
        .unwrap();
    writing.commit().await.unwrap();
    assert!(checking.await.unwrap(), "it sees the size written");
    assert_eq!(written, 1, "the one document holding it");
    let row = files::get(&pool, f.id).await.unwrap();
    assert_eq!(row.names, ["a.png", "a-4x3.png"]);
    assert_eq!(row.sizes["thumb"], entry);
    // The same name again isn't listed twice.
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        files::lock_for_size_in(&mut tx, c.id, f.id).await.unwrap(),
        files::SizeLock::Live { .. }
    ));
    files::fill_size_in(&mut tx, f.id, "thumb", &entry, &["a-4x3.png".into()])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(files::get(&pool, f.id).await.unwrap().names.len(), 2);
    // Released, the file can't take a size.
    set_draft_file(
        &pool,
        &c,
        doc,
        reserve(&pool, &c, "b.png", "", Some(doc)).await.id,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        files::lock_for_size_in(&mut tx, c.id, f.id).await.unwrap(),
        files::SizeLock::Released
    ));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_size_write_waits_out_a_schema_change(pool: PgPool) {
    let c = seed(&pool).await;
    let f = reserve(&pool, &c, "a.png", "", None).await;
    draft_with(&pool, &c, f.id).await;
    // As a schema change's rewrite holds it while it reads and writes documents whole.
    let mut change = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM collections WHERE id = $1 FOR UPDATE")
        .bind(c.id)
        .execute(&mut *change)
        .await
        .unwrap();
    let writing = {
        let pool = pool.clone();
        let (c, f) = (c.id, f.id);
        tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            files::lock_for_size_in(&mut tx, c, f).await.unwrap()
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !writing.is_finished(),
        "the size waits for the schema change"
    );
    change.commit().await.unwrap();
    assert!(matches!(
        writing.await.unwrap(),
        files::SizeLock::Live { schema_version, .. } if schema_version == c.schema_version
    ));
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_size_write_never_waits_on_a_users_file_or_version_locks(pool: PgPool) {
    let c = seed(&pool).await;
    let f = reserve(&pool, &c, "a.png", "", None).await;
    let doc = draft_with(&pool, &c, f.id).await;
    let version = record(&pool, &c, doc, DocCopy::Draft).await;
    sqlx::query("UPDATE versions SET file_id = $2 WHERE id = $1")
        .bind(version)
        .bind(f.id)
        .execute(&pool)
        .await
        .unwrap();
    for sql in [
        // As `lock_in` does, taking a file a restore points the document at.
        "SELECT id FROM files WHERE id = $1 FOR UPDATE",
        // As a prune does, deleting the version holding it.
        "SELECT id FROM versions WHERE file_id = $1 FOR UPDATE",
    ] {
        let mut user = pool.begin().await.unwrap();
        sqlx::query(sql)
            .bind(f.id)
            .execute(&mut *user)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let lock = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            files::lock_for_size_in(&mut tx, c.id, f.id),
        )
        .await
        .expect("it doesn't wait")
        .unwrap();
        assert!(matches!(lock, files::SizeLock::Busy), "{sql}: {lock:?}");
        drop(tx);
        user.commit().await.unwrap();
    }
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_deferred_job_keeps_its_attempts_up_to_a_cap(pool: PgPool) {
    use tachyon_db::jobs::{self, MAX_DEFERRALS, NewJob};
    let job = NewJob {
        kind: "image_sizes",
        document_id: None,
        collection_id: None,
        payload: json!({"file": Uuid::now_v7()}),
        run_at: time::OffsetDateTime::now_utc(),
        dedupe_key: None,
        created_by: None,
        key_id: None,
        max_attempts: Some(1),
    };
    let id = jobs::enqueue(&mut pool.acquire().await.unwrap(), &job)
        .await
        .unwrap();
    let lease = std::time::Duration::from_secs(60);
    for n in 0..MAX_DEFERRALS {
        sqlx::query("UPDATE jobs SET run_at = now() WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        let claimed = jobs::claim(&pool, lease).await.unwrap().unwrap();
        assert_eq!(claimed.attempts, 1, "deferral {n} spent nothing");
        assert!(
            jobs::defer(&pool, &claimed, std::time::Duration::ZERO, "busy")
                .await
                .unwrap()
        );
    }
    sqlx::query("UPDATE jobs SET run_at = now() WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let claimed = jobs::claim(&pool, lease).await.unwrap().unwrap();
    assert!(
        !jobs::defer(&pool, &claimed, std::time::Duration::ZERO, "busy")
            .await
            .unwrap(),
        "past the cap it must spend an attempt"
    );
}

/// An edit takes the copy only while it still holds the file the edit was made from.
#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn an_edit_needs_the_file_it_was_made_from(pool: PgPool) {
    let c = seed(&pool).await;
    let a = reserve(&pool, &c, "a.png", "", None).await.id;
    let doc = draft_with(&pool, &c, a).await;
    assert_eq!(
        documents::file_of(&pool, c.id, doc, DocCopy::Draft)
            .await
            .unwrap(),
        Some(a)
    );
    assert!(matches!(
        documents::file_of(&pool, c.id, doc, DocCopy::Published).await,
        Err(DbError::NotFound)
    ));
    let b = reserve(&pool, &c, "a.png", "", Some(doc)).await.id;
    let other = reserve(&pool, &c, "a.png", "", Some(doc)).await.id;
    let (db, c) = (&pool, &c);
    let edit = move |from, to| async move {
        let mut tx = db.begin().await.unwrap();
        let done = documents::update_in::<DbError>(
            &mut tx,
            c.id,
            c.schema_version,
            doc,
            DocCopy::Draft,
            None,
            None,
            |_| Ok(with_file(FileChange::Edit { from, to })),
        )
        .await;
        if done.is_ok() {
            tx.commit().await.unwrap();
        }
        done.map(|_| ())
    };
    assert!(matches!(edit(other, b).await, Err(DbError::Refused(_))));
    assert_eq!(state(&pool, b).await, "pending", "not taken");
    edit(a, b).await.unwrap();
    assert_eq!(state(&pool, b).await, "live");
    assert_eq!(
        documents::file_of(&pool, c.id, doc, DocCopy::Draft)
            .await
            .unwrap(),
        Some(b)
    );
    assert_eq!(state(&pool, a).await, "gone", "nothing else held it");
}

/// This connection's subtransactions in its open transaction: how many, and whether
/// Postgres's cache of them overflowed.
async fn subxacts(conn: &mut sqlx::PgConnection) -> (i32, bool) {
    sqlx::query_as(
        "SELECT subxact_count, subxact_overflowed FROM pg_stat_get_backend_subxact(
             (SELECT b FROM pg_stat_get_backend_idset() b
              WHERE pg_stat_get_backend_pid(b) = pg_backend_pid()))",
    )
    .fetch_one(conn)
    .await
    .unwrap()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_collection_drops_its_files_without_a_savepoint_each(pool: PgPool) {
    let c = seed(&pool).await;
    let mut held = Vec::new();
    for i in 0..100 {
        let f = reserve(&pool, &c, &format!("{i}.png"), "", None).await;
        draft_with(&pool, &c, f.id).await;
        held.push(f.id);
    }
    // Live, though nothing holds it (as a past bug or a hand edit could leave one).
    let loose = reserve(&pool, &c, "loose.png", "", None).await.id;
    sqlx::query("UPDATE files SET state = 'live' WHERE id = $1")
        .bind(loose)
        .execute(&pool)
        .await
        .unwrap();
    let pending = reserve(&pool, &c, "pending.png", "", None).await.id;

    let mut tx = pool.begin().await.unwrap();
    files::drop_collection_in(&mut tx, c.id).await.unwrap();
    assert_eq!(subxacts(&mut tx).await, (0, false));
    tx.commit().await.unwrap();
    for id in held.iter().chain([&loose]) {
        assert_eq!(state(&pool, *id).await, "gone", "{id}");
        assert_eq!(cleanups(&pool, *id).await, 1, "{id}");
    }
    let due: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND state = 'queued' AND run_at <= now()",
    )
    .bind(CLEANUP)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(due, 101, "each released file's cleanup is due now");
    // A pending file keeps the cleanup it was reserved with, after its grace.
    assert_eq!(state(&pool, pending).await, "gone");
    let later: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND payload->>'file' = $2
           AND state = 'queued' AND run_at > now() + interval '50 minutes'",
    )
    .bind(CLEANUP)
    .bind(pending.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(later, 1);
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn a_newer_walk_cancels_a_batch_that_was_committing(pool: PgPool) {
    let c = seed(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let batch = tachyon_db::jobs::add(&mut conn, &files::regenerate_job(c.id, None))
        .await
        .unwrap();
    drop(conn);
    // The batch completes and queues the next one, and holds that open...
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE jobs SET state = 'done', finished_at = now() WHERE id = $1")
        .bind(batch)
        .execute(&mut *tx)
        .await
        .unwrap();
    let next = tachyon_db::jobs::add(&mut tx, &files::regenerate_job(c.id, Some(Uuid::now_v7())))
        .await
        .unwrap();
    // ...while a schema change queues a new walk.
    let newer = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut conn = pool.acquire().await.unwrap();
            tachyon_db::jobs::enqueue(&mut conn, &files::regenerate_job(c.id, None))
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    tx.commit().await.unwrap();
    let newer = newer.await.unwrap();
    let states: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, state FROM jobs WHERE kind = 'regenerate_sizes' ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        states,
        [
            (batch, "done".into()),
            (next, "cancelled".into()),
            (newer, "queued".into()),
        ],
        "one walk goes on, the newer, from the start"
    );
}
