//! `tachyon schema export | diff | apply`: keep content types in a file under version control.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use sqlx::PgPool;
use tachyon_core::diff::{Change, Renames};
use tachyon_core::{CollectionKind, CollectionSchema};
use tachyon_db::schema_change::{self, ChangeError, Preview};
use tachyon_db::{collections, indexes};

/// Every collection's schema as a pretty JSON bundle.
pub async fn export(pool: &PgPool) -> anyhow::Result<String> {
    let all = collections::list(pool).await?;
    let schemas: Vec<&CollectionSchema> = all.iter().map(|c| &c.schema.0).collect();
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
    items
        .iter()
        .map(|item| {
            let slug = item.get("slug").and_then(Value::as_str).unwrap_or("?");
            let schema: CollectionSchema = serde_json::from_value(item.clone())
                .with_context(|| format!("{slug}: can't read this collection"))?;
            schema
                .reject_unknown_options(item)
                .with_context(|| slug.to_owned())?;
            schema.check().with_context(|| slug.to_owned())?;
            Ok(schema)
        })
        .collect()
}

fn describe(slug: &str, p: &Preview) -> String {
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
            Change::Relinked { path, from, to } => {
                format!("change what {path} links to from {from} to {to}, keeping its links")
            }
            Change::Retargeted { path, from, to } => {
                format!("point {path} at {to} instead of {from}, clearing links that no longer fit")
            }
        })
        .collect();
    let mut line = format!("~ change {slug}: {}", changes.join("; "));
    if p.invalid > 0 {
        line += &format!(
            "\n  {} of {} documents won't validate against the new fields",
            p.invalid, p.documents
        );
    }
    if !p.duplicates.is_empty() {
        line += &format!("\n  duplicate values in: {}", p.duplicates.join(", "));
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

    let none = Renames::new();
    // Collections the file creates count as valid link targets for the changed ones.
    let pending: Vec<&str> = wanted
        .iter()
        .filter(|s| is_new(&s.slug))
        .map(|s| s.slug.as_str())
        .collect();
    let mut report = Vec::new();
    let mut previews = Vec::new();
    for s in &wanted {
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
        for s in wanted.iter().filter(|s| is_new(&s.slug)) {
            let c = collections::create(pool, s).await?;
            indexes::reconcile_indexes(pool, c.id).await?;
            done.push(format!("+ created {}", s.slug));
        }
        for s in wanted.iter().filter(|s| !is_new(&s.slug)) {
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
}
