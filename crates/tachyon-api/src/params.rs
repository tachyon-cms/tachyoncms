//! Query parameters for listing and reading documents.

use std::collections::HashMap;

use serde_json::{Map, Value};
use tachyon_core::query::{Cursor, Filter, Sort, parse_filter_with, parse_query, parse_sort};
use tachyon_core::schema::NEAR_IN_JOIN;
use tachyon_core::select::{Select, parse_select};
use tachyon_core::{CollectionKind, Field};
use tachyon_db::collections::Collection;

use crate::ApiError;
use crate::populate::Shapes;
use crate::schemas::Schemas;

const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 100;
const DEFAULT_DEPTH: u8 = 1;
const MAX_DEPTH: u8 = 5;

pub(crate) struct ListParams {
    pub limit: i64,
    pub page: Option<i64>,
    pub after: Option<Cursor>,
    pub count: bool,
    pub depth: u8,
    pub filter: Option<Filter>,
    pub sort: Option<Sort>,
    pub select: Option<Select>,
    pub populate: Shapes,
    pub joins: Joins,
}

/// How a single read is shaped.
pub(crate) struct ReadParams {
    pub depth: u8,
    pub select: Option<Select>,
    pub populate: Shapes,
    pub joins: Joins,
}

/// `joins=false`, or per join field: `joins[posts][limit]=5` and the like.
pub(crate) enum Joins {
    Off,
    On(HashMap<String, JoinRequest>),
}

impl Joins {
    pub fn all() -> Self {
        Joins::On(HashMap::new())
    }
}

#[derive(Default, Clone)]
pub(crate) struct JoinRequest {
    pub limit: Option<i64>,
    pub page: Option<i64>,
    pub sort: Option<Sort>,
    pub filter: Option<Filter>,
    pub count: bool,
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::BadRequest(message.into())
}

fn params(raw: Option<&str>, allowed: &[&str]) -> Result<Map<String, Value>, ApiError> {
    let params = parse_query(raw.unwrap_or_default()).map_err(bad)?;
    if let Some(unknown) = params.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(bad(format!("unknown query parameter `{unknown}`")));
    }
    Ok(params)
}

fn single<'a>(params: &'a Map<String, Value>, name: &str) -> Result<Option<&'a str>, ApiError> {
    match params.get(name) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad(format!("{name} must be a single value"))),
    }
}

fn number(params: &Map<String, Value>, name: &str) -> Result<Option<i64>, ApiError> {
    single(params, name)?
        .map(|s| {
            s.parse::<i64>()
                .map_err(|_| bad(format!("{name} must be a whole number")))
        })
        .transpose()
}

fn select(params: &Map<String, Value>, fields: &[Field]) -> Result<Option<Select>, ApiError> {
    params
        .get("select")
        .map(|s| parse_select(fields, s, "select"))
        .transpose()
        .map_err(|e| bad(e.to_string()))
}

/// `populate[authors][name]=true`: a `select` for each collection's populated documents.
fn populate(params: &Map<String, Value>, schemas: &Schemas) -> Result<Shapes, ApiError> {
    let Some(input) = params.get("populate") else {
        return Ok(Shapes::new());
    };
    let shape = || bad("populate: expected populate[collection][field]=true");
    let Value::Object(by_slug) = input else {
        return Err(shape());
    };
    let mut out = Shapes::new();
    for (slug, tree) in by_slug {
        let c = schemas
            .get(slug)
            .filter(|c| c.schema.kind == CollectionKind::Collection)
            .ok_or_else(|| bad(format!("populate[{slug}]: no such collection")))?;
        if !tree.is_object() {
            return Err(bad(format!(
                "populate[{slug}]: expected populate[{slug}][field]=true"
            )));
        }
        let select = parse_select(c.schema.data(), tree, &format!("populate[{slug}]"))
            .map_err(|e| bad(e.to_string()))?;
        out.insert(slug.clone(), select);
    }
    Ok(out)
}

fn joins(
    params: &Map<String, Value>,
    c: &Collection,
    schemas: &Schemas,
) -> Result<Joins, ApiError> {
    let by_name = match params.get("joins") {
        None => return Ok(Joins::all()),
        Some(Value::String(s)) if s == "false" => return Ok(Joins::Off),
        Some(Value::Object(by_name)) => by_name,
        Some(_) => return Err(bad("joins: use joins=false, or joins[field][option]")),
    };
    let fields = c.schema.join_fields();
    // A join inside a group can be named `joins[meta.recent]` or `joins[meta][recent]`.
    fn named<'a>(
        prefix: &str,
        obj: &'a Map<String, Value>,
        paths: &[String],
        out: &mut Vec<(String, &'a Value)>,
    ) -> Result<(), ApiError> {
        for (key, value) in obj {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            if paths.contains(&path) {
                out.push((path, value));
            } else if let Value::Object(inner) = value
                && paths.iter().any(|p| p.starts_with(&format!("{path}.")))
            {
                named(&path, inner, paths, out)?;
            } else {
                return Err(bad(format!("joins[{path}]: no such join field")));
            }
        }
        Ok(())
    }
    let paths: Vec<String> = fields.iter().map(|j| j.path.clone()).collect();
    let mut requested = Vec::new();
    named("", by_name, &paths, &mut requested)?;
    let mut out = HashMap::new();
    for (name, options) in requested {
        let here = format!("joins[{name}]");
        let j = fields
            .iter()
            .find(|j| j.path == name)
            .expect("named only returns join paths");
        let Value::Object(options) = options else {
            return Err(bad(format!("{here}: expected {here}[option]=value")));
        };
        // The joined collection's fields, for its sort and filter. A join whose collection
        // has gone comes back empty, so its options only need to be well formed.
        let target = schemas.get(&j.collection).map(|t| t.schema.data().to_vec());
        let target = target.as_deref().unwrap_or_default();
        let mut req = JoinRequest::default();
        for (option, value) in options {
            let param = format!("{here}[{option}]");
            let text = || {
                value
                    .as_str()
                    .ok_or_else(|| bad(format!("{param}: must be a single value")))
            };
            let whole = || {
                text()?
                    .parse::<i64>()
                    .map_err(|_| bad(format!("{param}: must be a whole number")))
            };
            match option.as_str() {
                "limit" => {
                    let limit = whole()?;
                    if !(1..=MAX_LIMIT).contains(&limit) {
                        return Err(bad(format!("{param}: must be between 1 and {MAX_LIMIT}")));
                    }
                    req.limit = Some(limit);
                }
                "page" => {
                    let page = whole()?;
                    if page < 1 {
                        return Err(bad(format!("{param}: must be at least 1")));
                    }
                    req.page = Some(page);
                }
                "count" => {
                    req.count = match text()? {
                        "true" => true,
                        "false" => false,
                        _ => return Err(bad(format!("{param}: must be true or false"))),
                    }
                }
                "sort" => {
                    req.sort = Some(
                        parse_sort(target, text()?)
                            .map_err(|e| bad(format!("{param}: {}", e.message)))?,
                    );
                }
                "where" => {
                    // `where[title][equals]` from the parser becomes `joins[posts][where][title][equals]`.
                    let lookup = |slug: &str| schemas.lookup(slug);
                    let filter = parse_filter_with(target, value, &lookup).map_err(|e| {
                        let rest = e.param.strip_prefix("where").unwrap_or(&e.param);
                        let hint = e.message.replace("where[", &format!("{param}["));
                        bad(format!("{param}{rest}: {hint}"))
                    })?;
                    if filter.near().is_some() {
                        return Err(bad(format!("{param}: {NEAR_IN_JOIN}")));
                    }
                    req.filter = Some(filter);
                }
                _ => {
                    return Err(bad(format!(
                        "{param}: unknown option; use limit, page, sort, where or count"
                    )));
                }
            }
        }
        out.insert(name, req);
    }
    Ok(Joins::On(out))
}

fn depth(params: &Map<String, Value>) -> Result<u8, ApiError> {
    match single(params, "depth")? {
        None => Ok(DEFAULT_DEPTH),
        Some(s) => s
            .parse::<u8>()
            .ok()
            .filter(|d| *d <= MAX_DEPTH)
            .ok_or_else(|| bad("depth must be between 0 and 5")),
    }
}

pub(crate) fn list_params(
    raw: Option<&str>,
    c: &Collection,
    schemas: &Schemas,
) -> Result<ListParams, ApiError> {
    let params = params(
        raw,
        &[
            "limit", "page", "after", "count", "depth", "where", "sort", "select", "populate",
            "joins",
        ],
    )?;
    let fields = c.schema.data();

    let limit = number(&params, "limit")?.unwrap_or(DEFAULT_LIMIT);
    if limit < 1 {
        return Err(bad("limit must be at least 1"));
    }
    let page = number(&params, "page")?;
    if page.is_some_and(|p| p < 1) {
        return Err(bad("page must be at least 1"));
    }
    let count = match single(&params, "count")? {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(bad("count must be true or false")),
    };
    let filter = params
        .get("where")
        .map(|w| parse_filter_with(fields, w, &|slug: &str| schemas.lookup(slug)))
        .transpose()
        .map_err(|e| bad(e.to_string()))?;
    let sort = single(&params, "sort")?
        .map(|s| parse_sort(fields, s))
        .transpose()
        .map_err(|e| bad(e.to_string()))?;
    let after = single(&params, "after")?
        .map(|s| Cursor::decode(s).ok_or_else(|| bad("after isn't a valid cursor")))
        .transpose()?;
    if let Some(cursor) = &after {
        if page.is_some() {
            return Err(bad("use either page or after, not both"));
        }
        let fits = match &sort {
            Some(s) => cursor.fits(&s.target),
            None => cursor.value.is_none(),
        };
        if !fits {
            return Err(bad("after doesn't match this sort; start again without it"));
        }
    }
    Ok(ListParams {
        limit: limit.min(MAX_LIMIT),
        page,
        after,
        count,
        depth: depth(&params)?,
        select: select(&params, fields)?,
        populate: populate(&params, schemas)?,
        joins: joins(&params, c, schemas)?,
        filter,
        sort,
    })
}

pub(crate) fn read_params(
    raw: Option<&str>,
    c: &Collection,
    schemas: &Schemas,
) -> Result<ReadParams, ApiError> {
    let params = params(raw, &["depth", "select", "populate", "joins"])?;
    Ok(ReadParams {
        depth: depth(&params)?,
        select: select(&params, c.schema.data())?,
        populate: populate(&params, schemas)?,
        joins: joins(&params, c, schemas)?,
    })
}
