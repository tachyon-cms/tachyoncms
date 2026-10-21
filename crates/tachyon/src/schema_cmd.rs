//! `tachyon schema export | diff | apply`: keep content types in a file under version control.

use std::collections::BTreeMap;

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_core::access::check_rules;
use tachyon_core::diff::{Change, Renames};
use tachyon_core::{CollectionKind, CollectionSchema, explain_parse_error};
use tachyon_db::auth::{USERS, check_users_schema};
use tachyon_db::schema_change::{self, ChangeError, Preview};
use tachyon_db::{collections, indexes};

/// Every collection's schema as a pretty JSON bundle.
pub async fn export(pool: &PgPool) -> anyhow::Result<String> {
    let all = collections::list(pool).await?;
    let schemas: Vec<Value> = all.iter().map(|c| c.schema.to_compact_json()).collect();
    Ok(serde_json::to_string_pretty(&json!({ "collections": schemas }))? + "\n")
}

/// `posts:seo.title=headline` means: in posts, rename seo.title to headline.
pub fn parse_renames(args: &[String]) -> anyhow::Result<BTreeMap<String, Renames>> {
    let mut out: BTreeMap<String, Renames> = BTreeMap::new();
    for arg in args {
        let parsed = arg
            .split_once(':')
            .and_then(|(slug, rest)| Some((slug, rest.split_once('=')?)));
        let Some((slug, (from, to))) = parsed else {
            bail!("--rename {arg}: expected collection:field=new_name, like posts:title=headline");
        };
        out.entry(slug.to_owned())
            .or_default()
            .insert(from.to_owned(), to.to_owned());
    }
    Ok(out)
}

fn parse_bundle(text: &str) -> anyhow::Result<Vec<CollectionSchema>> {
    let bundle: Value = serde_json::from_str(text).context("the schema file isn't valid JSON")?;
    let items = bundle
        .get("collections")
        .and_then(Value::as_array)
        .context("the schema file needs a \"collections\" list")?;
    let mut seen = std::collections::HashSet::new();
    for slug in items
        .iter()
        .filter_map(|i| i.get("slug").and_then(Value::as_str))
    {
        if !seen.insert(slug) {
            bail!("`{slug}` appears more than once in the file");
        }
    }
    items
        .iter()
        .map(|item| {
            let slug = item.get("slug").and_then(Value::as_str).unwrap_or("?");
            let schema = CollectionSchema::deserialize(item)
                .map_err(|e| anyhow!("{slug}: {}", explain_parse_error(item, &e)))?;
            schema
                .reject_unknown_options(item)
                .with_context(|| slug.to_owned())?;
            // One naming shared blocks is checked once they're in place, in `run`.
            if schema.block_refs().is_empty() {
                schema.check().with_context(|| slug.to_owned())?;
            }
            Ok(schema)
        })
        .collect()
}

/// A preview's changes, in words.
fn changes(p: &Preview) -> String {
    let changes: Vec<String> = p
        .diff
        .changes
        .iter()
        .map(|c| match c {
            Change::Added { path, kind } => format!("add {path} ({kind})"),
            Change::Removed { path, .. } => format!("remove {path}, deleting its data"),
            Change::Renamed { from, to } => format!("rename {from} to {to}"),
            Change::TypeChanged { path, from, to } => format!("change {path} from {from} to {to}"),
            Change::OptionsChanged { path } => format!("update options on {path}"),
            Change::LayoutChanged => "rearrange fields in the admin".to_owned(),
            Change::CollectionOptionsChanged => "update collection options".to_owned(),
            Change::VirtualRemoved { path } => format!("remove {path} (it stored nothing)"),
            Change::Relinked { path, from, to } => {
                format!("change what {path} links to from {from} to {to}, keeping its links")
            }
            Change::Retargeted { path, from, to } => {
                format!("point {path} at {to} instead of {from}, clearing links that no longer fit")
            }
        })
        .collect();
    changes.join("; ")
}

fn describe(slug: &str, p: &Preview) -> String {
    let mut line = format!("~ change {slug}: {}", changes(p));
    // A shared block's change, as each schema naming it sees it.
    for (other, sub) in &p.collections {
        line += &format!("\n  in {other}: {}", changes(sub));
        if sub.invalid > 0 {
            line += &format!(
                "\n    {} of {} documents won't validate against the new fields",
                sub.invalid, sub.documents
            );
        }
    }
    if p.invalid > 0 && p.collections.is_empty() {
        line += &format!(
            "\n  {} of {} documents won't validate against the new fields",
            p.invalid, p.documents
        );
    }
    if !p.duplicates.is_empty() {
        line += &format!("\n  duplicate values in: {}", p.duplicates.join(", "));
    }
    let mut indexes = Vec::new();
    if !p.indexes.create.is_empty() {
        indexes.push(format!("create {}", p.indexes.create.join(", ")));
    }
    if !p.indexes.drop.is_empty() {
        indexes.push(format!("drop {}", p.indexes.drop.join(", ")));
    }
    if !indexes.is_empty() {
        line += &format!("\n  indexes: {}", indexes.join("; "));
    }
    line
}

/// Compares a schema file with the database. `apply: None` only reports; `Some(yes)` applies,
/// refusing destructive changes unless `yes`. Collections missing from the file are left alone.
pub async fn run(
    pool: &PgPool,
    text: &str,
    renames: &BTreeMap<String, Renames>,
    apply: Option<bool>,
) -> anyhow::Result<String> {
    let wanted = parse_bundle(text)?;
    let existing = collections::list(pool).await?;
    let is_new = |slug: &str| !existing.iter().any(|c| c.schema.0.slug == slug);

    for slug in renames.keys() {
        if !wanted.iter().any(|s| &s.slug == slug) {
            bail!("--rename {slug}: that collection isn't in the file");
        }
        if is_new(slug) {
            bail!("--rename {slug}: the file creates {slug}, so it has no fields to rename yet");
        }
    }
    for s in &wanted {
        for target in s.relation_targets() {
            let mut known = wanted.iter().chain(existing.iter().map(|c| &c.schema.0));
            if !known.any(|o| o.slug == target && o.kind == CollectionKind::Collection) {
                bail!(
                    "{}: relationTo `{target}` doesn't match a collection in the file or the database",
                    s.slug
                );
            }
        }
    }

    // Every collection must work with the file's schemas in place of the database's, those
    // left out of the file included: virtual and join fields read other collections, and
    // access rules filter through them or read `$user` fields from `users`. Each sees its
    // shared blocks as the file leaves them.
    let raw = |slug: &str| {
        wanted.iter().find(|o| o.slug == slug).or_else(|| {
            existing
                .iter()
                .map(|c| &c.schema.0)
                .find(|o| o.slug == slug)
        })
    };
    let mut world = Vec::new();
    for s in &wanted {
        let expanded = s.expand(raw).with_context(|| s.slug.clone())?;
        expanded.check().with_context(|| s.slug.clone())?;
        world.push(expanded);
    }
    for c in &existing {
        let other = &c.schema.0;
        if !wanted.iter().any(|s| s.slug == other.slug) {
            world.push(
                other
                    .expand(raw)
                    .with_context(|| format!("{} (not in the file) would break", other.slug))?,
            );
        }
    }
    let lookup = |slug: &str| world.iter().find(|o| o.slug == slug).cloned();
    let roles = tachyon_db::auth::roles_list(pool).await?;
    let roles: Vec<&str> = roles.iter().map(|r| r.slug.as_str()).collect();
    let users = lookup(USERS).map(|u| u.fields).unwrap_or_default();
    let works = |s: &CollectionSchema| {
        s.check_virtual_targets(lookup)
            .and_then(|()| s.check_join_targets(lookup))
            .and_then(|()| check_rules(s, &users, &roles, &lookup))
    };
    for s in &world {
        // `check_users_schema` here too for a `users` the file creates, which skips the
        // change's own check.
        if wanted.iter().any(|w| w.slug == s.slug) {
            check_users_schema(s)
                .and_then(|()| works(s))
                .with_context(|| s.slug.clone())?;
        } else {
            works(s).with_context(|| format!("{} (not in the file) would break", s.slug))?;
        }
    }
    // Shared blocks first, so the schemas naming them find them.
    let is_block = |s: &&CollectionSchema| s.kind == CollectionKind::Block;
    let ordered: Vec<&CollectionSchema> = wanted
        .iter()
        .filter(is_block)
        .chain(wanted.iter().filter(|s| !is_block(s)))
        .collect();

    let none = Renames::new();
    // What the file creates counts for the changed ones: as link targets, and shared blocks.
    let pending: Vec<&CollectionSchema> = wanted.iter().filter(|s| is_new(&s.slug)).collect();
    let mut report = Vec::new();
    let mut previews = Vec::new();
    for &s in &ordered {
        if is_new(&s.slug) {
            report.push(format!("+ create {}", s.slug));
            continue;
        }
        match schema_change::preview(
            pool,
            &s.slug,
            s,
            renames.get(&s.slug).unwrap_or(&none),
            &pending,
        )
        .await
        {
            Ok(p) if p.diff.is_empty() => report.push(format!("= {} unchanged", s.slug)),
            Ok(p) => {
                report.push(describe(&s.slug, &p));
                previews.push(p);
            }
            Err(ChangeError::Invalid(e)) => bail!("{}: {e}", s.slug),
            Err(e) => return Err(e.into()),
        }
    }
    for c in &existing {
        if !wanted.iter().any(|s| s.slug == c.schema.0.slug) {
            report.push(format!(
                "  {} isn't in the file; left as it is",
                c.schema.0.slug
            ));
        }
    }
    let report = report.join("\n");

    let Some(yes) = apply else { return Ok(report) };
    if previews.iter().any(|p| !p.duplicates.is_empty()) {
        bail!("{report}\n\nSome unique fields have duplicate values. Fix those documents first.");
    }
    if !yes && previews.iter().any(|p| p.needs_confirmation) {
        bail!(
            "{report}\n\nThese changes remove or retype fields, or leave documents that won't validate. Re-run with --yes to apply them."
        );
    }

    // Each collection commits on its own. If one fails, say exactly what already went in.
    let mut done = Vec::new();
    let result: anyhow::Result<()> = async {
        // New collections first, so changed ones can point at them.
        for s in ordered.iter().filter(|s| is_new(&s.slug)) {
            let c = collections::create(pool, s).await?;
            indexes::reconcile_indexes(pool, c.id).await?;
            done.push(format!("+ created {}", s.slug));
        }
        for s in ordered.iter().filter(|s| !is_new(&s.slug)) {
            let r = renames.get(&s.slug).unwrap_or(&none);
            // `yes` again here: the in-transaction check decides, not the earlier preview.
            let (c, _) = schema_change::apply(pool, &s.slug, s, r, yes).await?;
            indexes::reconcile_indexes(pool, c.id).await?;
            done.push(format!("~ changed {}", s.slug));
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let applied = if done.is_empty() {
            "  nothing".to_owned()
        } else {
            done.join("\n")
        };
        bail!("{report}\n\nStopped part-way: {e}\nAlready applied:\n{applied}");
    }
    Ok(format!("{report}\n\nApplied."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn posts(fields: Value) -> CollectionSchema {
        serde_json::from_value(json!({"slug": "posts", "fields": fields})).unwrap()
    }

    #[test]
    fn rename_arguments_parse() {
        let r = parse_renames(&[
            "posts:seo.title=headline".into(),
            "posts:body=content".into(),
        ])
        .unwrap();
        assert_eq!(r["posts"]["seo.title"], "headline");
        assert_eq!(r["posts"]["body"], "content");
        assert!(parse_renames(&["posts-title".into()]).is_err());
        assert!(parse_renames(&["posts:title".into()]).is_err());
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn export_then_apply_changes_nothing(pool: PgPool) {
        collections::create(&pool, &posts(json!([{"name": "title", "type": "text"}])))
            .await
            .unwrap();
        let file = export(&pool).await.unwrap();
        assert!(!file.contains("false") && !file.contains("null"), "{file}");
        let out = run(&pool, &file, &BTreeMap::new(), Some(false))
            .await
            .unwrap();
        assert!(out.contains("= posts unchanged"), "{out}");
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn creates_new_collections_and_changes_existing_ones(pool: PgPool) {
        collections::create(&pool, &posts(json!([{"name": "title", "type": "text"}])))
            .await
            .unwrap();
        let file = json!({"collections": [
            {"slug": "posts", "fields": [
                {"name": "headline", "type": "text"},
                {"name": "author", "type": "relationship", "relationTo": "authors"}
            ]},
            {"slug": "authors", "fields": [{"name": "name", "type": "text"}]}
        ]})
        .to_string();
        let renames = parse_renames(&["posts:title=headline".into()]).unwrap();

        let plan = run(&pool, &file, &renames, None).await.unwrap();
        assert!(
            plan.contains("~ change posts: rename title to headline; add author (relationship)"),
            "{plan}"
        );
        assert!(plan.contains("+ create authors"), "{plan}");
        assert_eq!(
            collections::list(&pool).await.unwrap().len(),
            1,
            "diff changes nothing"
        );

        run(&pool, &file, &renames, Some(false)).await.unwrap();
        let slugs: Vec<String> = collections::list(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.schema.0.slug)
            .collect();
        assert_eq!(slugs, ["authors", "posts"]);
        let posts = collections::get_by_slug(&pool, "posts").await.unwrap();
        assert_eq!(posts.schema.0.fields[0].name, "headline");
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn destructive_changes_need_yes(pool: PgPool) {
        collections::create(
            &pool,
            &posts(json!([
                {"name": "title", "type": "text"}, {"name": "summary", "type": "textarea"}
            ])),
        )
        .await
        .unwrap();
        let file = json!({"collections": [{"slug": "posts", "fields": [{"name": "title", "type": "text"}]}]}).to_string();

        let err = run(&pool, &file, &BTreeMap::new(), Some(false))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("remove summary") && err.contains("--yes"),
            "{err}"
        );
        assert_eq!(
            collections::get_by_slug(&pool, "posts")
                .await
                .unwrap()
                .schema
                .0
                .fields
                .len(),
            2
        );

        run(&pool, &file, &BTreeMap::new(), Some(true))
            .await
            .unwrap();
        assert_eq!(
            collections::get_by_slug(&pool, "posts")
                .await
                .unwrap()
                .schema
                .0
                .fields
                .len(),
            1
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn diffs_say_which_indexes_change(pool: PgPool) {
        collections::create(&pool, &posts(json!([{"name": "title", "type": "text"}])))
            .await
            .unwrap();
        let file = json!({"collections": [{"slug": "posts", "fields": [
            {"name": "title", "type": "text", "unique": true}
        ]}]})
        .to_string();
        let plan = run(&pool, &file, &BTreeMap::new(), None).await.unwrap();
        assert!(
            plan.contains("\n  indexes: create title (unique)"),
            "{plan}"
        );
    }

    #[test]
    fn the_access_slug_is_reserved() {
        for kind in ["collection", "global"] {
            let file = json!({"collections": [{"slug": "access", "kind": kind, "fields": []}]});
            let err = parse_bundle(&file.to_string()).unwrap_err();
            assert_eq!(
                format!("{err:#}"),
                "access: collection slug `access` is reserved"
            );
        }
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn duplicate_slugs_and_renames_on_new_collections_are_refused(pool: PgPool) {
        let twice = json!({"collections": [
            {"slug": "posts", "fields": []}, {"slug": "posts", "fields": []}
        ]})
        .to_string();
        let err = run(&pool, &twice, &BTreeMap::new(), None)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "`posts` appears more than once in the file");

        let new = json!({"collections": [{"slug": "posts", "fields": [
            {"name": "headline", "type": "text"}
        ]}]})
        .to_string();
        let renames = parse_renames(&["posts:title=headline".into()]).unwrap();
        let err = run(&pool, &new, &renames, None)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "--rename posts: the file creates posts, so it has no fields to rename yet"
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn virtual_fields_must_read_a_real_field(pool: PgPool) {
        let file = |read: &str| {
            json!({"collections": [
                {"slug": "authors", "fields": [{"name": "name", "type": "text"}]},
                {"slug": "posts", "fields": [
                    {"name": "author", "type": "relationship", "relationTo": "authors"},
                    {"name": "authorName", "type": "text", "virtual": read}
                ]}
            ]})
            .to_string()
        };
        let err = run(&pool, &file("author.nope"), &BTreeMap::new(), None)
            .await
            .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "posts: field `authorName`: authors has no field `nope` to read"
        );
        run(&pool, &file("author.name"), &BTreeMap::new(), Some(false))
            .await
            .unwrap();
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn joins_must_read_a_field_that_links_here(pool: PgPool) {
        let file = |on: &str| {
            json!({"collections": [
                {"slug": "categories", "fields": [
                    {"name": "posts", "type": "join", "collection": "posts", "on": on}
                ]},
                {"slug": "posts", "fields": [
                    {"name": "title", "type": "text"},
                    {"name": "category", "type": "relationship", "relationTo": "categories"}
                ]}
            ]})
            .to_string()
        };
        let err = run(&pool, &file("title"), &BTreeMap::new(), None)
            .await
            .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "categories: field `posts`: posts.title isn't a relationship field"
        );
        run(&pool, &file("category"), &BTreeMap::new(), Some(false))
            .await
            .unwrap();
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn changes_that_break_a_join_elsewhere_are_refused(pool: PgPool) {
        let both = json!({"collections": [
            {"slug": "categories", "fields": [
                {"name": "posts", "type": "join", "collection": "posts", "on": "category"}
            ]},
            {"slug": "posts", "fields": [
                {"name": "category", "type": "relationship", "relationTo": "categories"}
            ]}
        ]})
        .to_string();
        run(&pool, &both, &BTreeMap::new(), Some(false))
            .await
            .unwrap();
        let only_posts = json!({"collections": [{"slug": "posts", "fields": [
            {"name": "title", "type": "text"}
        ]}]})
        .to_string();
        let err = run(&pool, &only_posts, &BTreeMap::new(), None)
            .await
            .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "categories (not in the file) would break: field `posts`: posts has no field `category`"
        );
        let plan = run(
            &pool,
            &json!({"collections": [{"slug": "categories", "fields": []}]}).to_string(),
            &BTreeMap::new(),
            None,
        )
        .await
        .unwrap();
        assert!(plan.contains("remove posts (it stored nothing)"), "{plan}");
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn rules_round_trip_through_export_and_apply(pool: PgPool) {
        let abilities =
            serde_json::from_value(json!({"content": "read", "schemas": false, "users": false}))
                .unwrap();
        tachyon_db::auth::role_upsert(&pool, "reviewer", "Reviewer", &abilities)
            .await
            .unwrap();
        let posts = |read: Value| {
            json!({"slug": "posts",
            "access": {"read": read, "create": {"signedIn": true}, "update": {"author": "$user.id"}},
            "fields": [
                {"name": "title", "type": "text", "access": {"update": {"role": ["editor", "reviewer"]}}},
                {"name": "author", "type": "relationship", "relationTo": "users"},
                {"name": "notes", "type": "group", "fields": [
                    {"name": "body", "type": "text", "access": {"read": {"or": [{"signedIn": true}, false]}}}
                ]}
            ]})
        };
        let users = json!({"slug": "users", "fields": [
            {"name": "name", "type": "text", "required": true},
            {"name": "team", "type": "text"}
        ]});
        let read = json!({"or": [{"author.team": "$user.team"}, {"title": {"like": "public"}}]});
        let file = json!({"collections": [users, posts(read)]});
        run(&pool, &file.to_string(), &BTreeMap::new(), Some(false))
            .await
            .unwrap();

        let exported: Value = serde_json::from_str(&export(&pool).await.unwrap()).unwrap();
        let saved = exported["collections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["slug"] == "posts")
            .unwrap();
        assert_eq!(saved["access"], file["collections"][1]["access"]);
        assert_eq!(
            saved["fields"][0]["access"],
            json!({"update": {"role": ["editor", "reviewer"]}})
        );
        assert_eq!(
            saved["fields"][2]["fields"][0]["access"],
            json!({"read": {"or": [{"signedIn": true}, false]}})
        );
        let out = run(&pool, &exported.to_string(), &BTreeMap::new(), Some(false))
            .await
            .unwrap();
        assert!(out.contains("= posts unchanged"), "{out}");

        for (read, message) in [
            (
                json!(1),
                "posts: access.read: a rule is true, false, {\"role\": …}, {\"signedIn\": …}, {\"and\": […]}, {\"or\": […]} or a where filter",
            ),
            (
                json!({"role": "ghost"}),
                "posts: access.read: there's no role `ghost`",
            ),
            (
                json!({"title": "$user.nope"}),
                "posts: access.read: `$user.nope` isn't id, role or a users field",
            ),
        ] {
            let file = json!({"collections": [posts(read)]}).to_string();
            let err = run(&pool, &file, &BTreeMap::new(), Some(false))
                .await
                .unwrap_err();
            assert_eq!(format!("{err:#}"), message);
        }

        // A change that would break a rule in a collection left out of the file.
        let no_team = json!({"collections": [{"slug": "users", "fields": [
            {"name": "name", "type": "text", "required": true}
        ]}]});
        let err = run(&pool, &no_team.to_string(), &BTreeMap::new(), Some(true))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").starts_with("posts (not in the file) would break: access.read: "),
            "{err:#}"
        );

        // Rules on users itself aren't enforced, so they're refused.
        let mut ruled = file["collections"][0].clone();
        ruled["access"] = json!({"read": true});
        let err = run(
            &pool,
            &json!({"collections": [ruled]}).to_string(),
            &BTreeMap::new(),
            Some(false),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").starts_with("users: access: rules on users aren't enforced yet"),
            "{err:#}"
        );
    }

    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn shared_blocks_round_trip_through_export_and_apply(pool: PgPool) {
        let cta = |fields: Value| json!({"slug": "cta", "kind": "block", "fields": fields});
        let pages = json!({"slug": "pages", "fields": [
            {"name": "layout", "type": "blocks", "blocks": [{"blockRef": "cta"}]}
        ]});
        let label = json!([{"name": "label", "type": "text"}]);
        // The block comes after the collection in the file, and is still applied first.
        let file = json!({"collections": [pages, cta(label.clone())]}).to_string();
        let plan = run(&pool, &file, &BTreeMap::new(), Some(false))
            .await
            .unwrap();
        assert!(
            plan.contains("+ create cta") && plan.contains("+ create pages"),
            "{plan}"
        );

        let exported: Value = serde_json::from_str(&export(&pool).await.unwrap()).unwrap();
        let saved = |slug: &str| {
            exported["collections"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["slug"] == slug)
                .cloned()
                .unwrap()
        };
        assert_eq!(saved("cta"), cta(label));
        assert_eq!(
            saved("pages")["fields"][0]["blocks"],
            json!([{"blockRef": "cta"}])
        );
        let out = run(&pool, &exported.to_string(), &BTreeMap::new(), Some(false))
            .await
            .unwrap();
        assert!(
            out.contains("= cta unchanged") && out.contains("= pages unchanged"),
            "{out}"
        );

        // Changing the block reports the change in each collection that uses it.
        let changed = json!({"collections": [cta(json!([
            {"name": "label", "type": "text"}, {"name": "url", "type": "text"}
        ]))]})
        .to_string();
        let plan = run(&pool, &changed, &BTreeMap::new(), Some(false))
            .await
            .unwrap();
        assert!(plan.contains("~ change cta: add url (text)"), "{plan}");
        assert!(
            plan.contains("in pages: add layout.cta.url (text)"),
            "{plan}"
        );

        // A rename names the field within the block.
        let renamed = json!({"collections": [cta(json!([
            {"name": "text", "type": "text"}, {"name": "url", "type": "text"}
        ]))]})
        .to_string();
        let r = parse_renames(&["cta:label=text".into()]).unwrap();
        let plan = run(&pool, &renamed, &r, None).await.unwrap();
        assert!(
            plan.contains("in pages: rename layout.cta.label to layout.cta.text"),
            "{plan}"
        );

        // A reference must name a block in the file or the database.
        let missing = json!({"collections": [{"slug": "posts", "fields": [
            {"name": "layout", "type": "blocks", "blocks": [{"blockRef": "nope"}]}
        ]}]})
        .to_string();
        let err = run(&pool, &missing, &BTreeMap::new(), None)
            .await
            .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "posts: field `layout`: blockRef `nope` doesn't match a shared block"
        );
    }
}
