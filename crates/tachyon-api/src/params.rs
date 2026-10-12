//! Query parameters for listing and reading documents.

use serde_json::{Map, Value};
use tachyon_core::query::{Cursor, Filter, Sort, parse_filter, parse_query, parse_sort};
use tachyon_db::collections::Collection;

use crate::ApiError;

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

pub(crate) fn list_params(raw: Option<&str>, c: &Collection) -> Result<ListParams, ApiError> {
    let params = params(
        raw,
        &["limit", "page", "after", "count", "depth", "where", "sort"],
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
        .map(|w| parse_filter(fields, w))
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
        filter,
        sort,
    })
}

pub(crate) fn depth_param(raw: Option<&str>) -> Result<u8, ApiError> {
    depth(&params(raw, &["depth"])?)
}
