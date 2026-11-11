//! Serving stored files, `GET`/`HEAD /api/{collection}/file/{filename}`, and the `url`s
//! documents carry to them.
//!
//! A file is served to whoever may read a copy of a document holding it now, and the field
//! that names it: what a version alone holds, or a trashed document, is served to nobody.
//! Every refusal is the 404 a missing file gets, so a name can't be probed.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::SystemTime;

use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, RawQuery, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE,
    LAST_MODIFIED, RANGE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};
use tachyon_core::auth::Action;
use tachyon_core::query::DocCopy;
use tachyon_core::uploads::PROTECTED_HEADERS;
use tachyon_db::collections::Collection;
use tachyon_db::files::{self, FileRow};
use tachyon_media::storage::StorageError;
use url::Url;
use uuid::Uuid;

use crate::access::Access;
use crate::documents::collection;
use crate::params::file_params;
use crate::{ApiError, AppState};

/// A public file may be cached anywhere, but asked about again each time (a cheap 304): a
/// deleted document's name can come back later as other bytes.
const PUBLIC: &str = "public, no-cache";
const PRIVATE: &str = "private, no-store";

/// `GET`/`HEAD /api/{collection}/file/{filename}`: the original, a size or a size's other
/// format named `filename`, from a document's published copy or else, for who may read it,
/// its draft. `?id` picks the document, where `filenameCompoundIndex` lets several share a
/// name. A refusal is never cached: the same URL may answer someone else, or later, with
/// the file.
pub async fn serve(
    state: State<AppState>,
    access: Access,
    method: Method,
    path: Result<Path<(String, String)>, PathRejection>,
    raw: RawQuery,
    headers: HeaderMap,
) -> Response {
    match serve_file(state, access, method, path, raw, headers).await {
        Ok(res) => res,
        Err(e) => {
            let mut res = e.into_response();
            res.headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res
        }
    }
}

async fn serve_file(
    State(state): State<AppState>,
    access: Access,
    method: Method,
    path: Result<Path<(String, String)>, PathRejection>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Path((slug, name)) = path.map_err(|_| ApiError::NotFound)?;
    access.scope(Action::Read, &slug)?;
    let c = collection(&state, &slug)?;
    let Some(u) = &c.schema.upload else {
        return Err(ApiError::NotFound);
    };
    if !access.permits_any(Action::Read, &c) {
        return Err(access.hidden());
    }
    let (id, draft) = file_params(raw.as_deref(), &c)?;
    let found = find(&state, &access, &c, &name, id, draft)
        .await?
        .ok_or(ApiError::NotFound)?;
    let storage = state
        .media
        .storages
        .get(&found.file.storage)
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "no storage named `{}` is configured",
                found.file.storage
            ))
        })?;
    let failed = |e: StorageError| match e.is_not_found() {
        // The row says it's there and the store doesn't: worth knowing, but the caller
        // gets what a missing file gets.
        true => {
            tracing::error!(file = %found.file.id, key = %found.key, "a live file's object is missing from storage");
            ApiError::NotFound
        }
        false => ApiError::Internal(format!("can't read a stored file: {e}")),
    };

    let mut out = HeaderMap::new();
    let etag = format!("\"{}-{}\"", found.file.id, encoded(&name, etag_char));
    // Whole seconds, as the header has them, so `If-Modified-Since` compares like for like.
    let modified = httpdate::fmt_http_date(SystemTime::from(found.file.created_at));
    let at_second = httpdate::parse_http_date(&modified).ok();
    let disposition = format!(
        "inline; filename=\"{}\"; filename*=UTF-8''{}",
        ascii_name(&name),
        encoded(&name, attr_char)
    );
    let mut set = |k: HeaderName, v: &str| {
        if let Ok(v) = HeaderValue::from_str(v) {
            out.insert(k, v);
        }
    };
    set(ETAG, &etag);
    set(LAST_MODIFIED, &modified);
    // A `?draft=true` answer is never stored, whoever asks and whichever copy it gives: a
    // shared cache could otherwise hand one caller's answer to another.
    set(
        CACHE_CONTROL,
        if found.public && !draft {
            PUBLIC
        } else {
            PRIVATE
        },
    );
    set(CONTENT_TYPE, &found.mime);
    set(CONTENT_DISPOSITION, &disposition);
    set(ACCEPT_RANGES, "bytes");
    if sandboxed(&found.mime) {
        set(CONTENT_SECURITY_POLICY, "sandbox");
    }
    // Last, and never over a header the server sets (those are refused at save already).
    for (k, v) in &u.modify_response_headers {
        if PROTECTED_HEADERS.iter().any(|p| p.eq_ignore_ascii_case(k)) {
            continue;
        }
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) && !out.contains_key(&k)
        {
            out.insert(k, v);
        }
    }

    if fresh(&headers, &etag, at_second) {
        return Ok(answer(StatusCode::NOT_MODIFIED, out, Body::empty()));
    }
    let size = storage.head(&found.key).await.map_err(failed)?.size;
    let wanted = match method {
        // A `HEAD` answers as the whole file's `GET` would.
        // An `If-Range` naming another copy wants the whole of this one (RFC 9110 13.1.5).
        Method::GET if !same_copy(headers.get(IF_RANGE), &etag, &modified) => Wanted::All,
        Method::GET => range(headers.get(RANGE), size),
        _ => Wanted::All,
    };
    let (status, part) = match wanted {
        Wanted::All => (StatusCode::OK, None),
        Wanted::Part(r) => (StatusCode::PARTIAL_CONTENT, Some(r)),
        Wanted::Unsatisfiable => return Err(ApiError::RangeNotSatisfiable(size)),
    };
    let length = part.as_ref().map_or(size, |r| r.end - r.start);
    if let Some(r) = &part
        && let Ok(v) = HeaderValue::from_str(&format!("bytes {}-{}/{size}", r.start, r.end - 1))
    {
        out.insert(CONTENT_RANGE, v);
    }
    out.insert(CONTENT_LENGTH, HeaderValue::from(length));
    if method == Method::HEAD {
        return Ok(answer(status, out, Body::empty()));
    }
    let got = storage.get_range(&found.key, part).await.map_err(failed)?;
    Ok(answer(status, out, Body::from_stream(got.into_stream())))
}

fn answer(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut res = Response::new(body);
    *res.status_mut() = status;
    *res.headers_mut() = headers;
    res
}

/// A file the caller may have, as it's served.
struct Found {
    file: FileRow,
    /// Where the bytes asked for are: the original's key, or a size's beside it.
    key: String,
    mime: String,
    /// Anyone could read it from where it's served, so caches may share it.
    public: bool,
}

/// The file named `name` the caller may read: held by the published copy of a document
/// they may read, or else by the draft of one whose draft they may read (`read_copy`
/// judges each copy), document `id` when given, through the field that names it. With
/// `draft`, drafts they may read come first, as a draft request reads (T4-1); one they
/// can't read is passed over, so they get what they'd get without it. Trashed documents
/// hold nothing here (`holders`).
async fn find(
    state: &AppState,
    access: &Access,
    c: &Collection,
    name: &str,
    id: Option<Uuid>,
    draft: bool,
) -> Result<Option<Found>, ApiError> {
    let candidates = files::by_name(&state.pool, c.id, name).await?;
    // An original's name wins over a size of another file that happens to share it.
    let originals = candidates.iter().any(|f| f.filename == name);
    let anonymous = access.anonymous();
    // Every published holder of every file with the name before any draft holder, or the
    // other way round with `draft`: a replace or an edit keeps the name, so the draft's
    // file and the published one can share it.
    let mut held = Vec::new();
    for file in candidates {
        let Some((field, mime)) = served_as(&file, name, originals) else {
            continue;
        };
        if !access.reads(c, &field) {
            continue;
        }
        let holders = files::holders(&state.pool, file.id).await?;
        let file = Arc::new(file);
        for (doc, copy) in holders {
            if id.is_none_or(|id| id == doc) {
                held.push((file.clone(), field.clone(), mime.clone(), doc, copy));
            }
        }
    }
    // Stable, so within each copy files keep their order by ID.
    let first = if draft {
        DocCopy::Draft
    } else {
        DocCopy::Published
    };
    held.sort_by_key(|(_, _, _, _, copy)| *copy != first);
    for (file, field, mime, doc, copy) in held {
        if access.read_copy(&state.pool, c, doc, copy).await?.is_none() {
            continue;
        }
        let public = copy == DocCopy::Published
            && anonymous.reads_every_document(c)
            && anonymous.reads(c, &field);
        let key = match name == file.filename {
            true => file.key.clone(),
            false => beside(&file.key, name),
        };
        return Ok(Some(Found {
            file: (*file).clone(),
            key,
            mime,
            public,
        }));
    }
    Ok(None)
}

/// What `name` is in `file`, as the field that carries it and its type: the original
/// (`filename`), a size (`sizes.<size>`) or a size's other format (`sizes.<size>.formats`).
/// A size's name counts only when no file has `name` as its original's (`originals`).
fn served_as(file: &FileRow, name: &str, originals: bool) -> Option<(String, String)> {
    if file.filename == name {
        return Some(("filename".into(), file.mime_type.clone()));
    }
    if originals {
        return None;
    }
    let named = |e: &Value| e.get("filename").and_then(Value::as_str) == Some(name);
    let mime = |e: &Value| e.get("mimeType").and_then(Value::as_str).map(str::to_owned);
    for (size, entry) in file.sizes.iter() {
        if named(entry) {
            return Some((format!("sizes.{size}"), mime(entry)?));
        }
        let formats = entry.get("formats").and_then(Value::as_object);
        if let Some(f) = formats
            .into_iter()
            .flatten()
            .map(|(_, f)| f)
            .find(|f| named(f))
        {
            return Some((format!("sizes.{size}.formats"), mime(f)?));
        }
    }
    None
}

/// The key of `name` in the directory of `key`, where a file's sizes sit.
fn beside(key: &str, name: &str) -> String {
    match key.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.to_owned(),
    }
}

/// Whether the client's copy is still good: `If-None-Match` names this `etag` (or `*`),
/// or, when it isn't sent, `If-Modified-Since` is exactly this file's `modified`. Not
/// "no earlier": a restored version puts an older file back under the same name.
fn fresh(req: &HeaderMap, etag: &str, modified: Option<SystemTime>) -> bool {
    if let Some(tags) = req.get(IF_NONE_MATCH) {
        return tags.to_str().is_ok_and(|tags| {
            tags.split(',')
                .map(str::trim)
                .any(|t| t == "*" || t.trim_start_matches("W/") == etag)
        });
    }
    let since = req
        .get(IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| httpdate::parse_http_date(v).ok());
    matches!((since, modified), (Some(since), Some(m)) if m == since)
}

/// Whether `if_range`, when sent, names this copy: its entity tag, compared strongly, or
/// exactly its `Last-Modified`.
fn same_copy(if_range: Option<&HeaderValue>, etag: &str, modified: &str) -> bool {
    if_range.is_none_or(|v| {
        v.to_str()
            .is_ok_and(|v| v.trim() == etag || v.trim() == modified)
    })
}

#[derive(Debug, PartialEq)]
enum Wanted {
    All,
    Part(Range<u64>),
    Unsatisfiable,
}

/// The bytes a `Range` header asks for of a file of `size` bytes. One range is served as
/// asked; several, another unit or a malformed header get the whole file, as a server may
/// answer them. A range starting past the end can't be served.
fn range(header: Option<&HeaderValue>, size: u64) -> Wanted {
    let Some(spec) = header
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.trim().strip_prefix("bytes="))
        .filter(|s| !s.contains(','))
    else {
        return Wanted::All;
    };
    let Some((first, last)) = spec.trim().split_once('-') else {
        return Wanted::All;
    };
    let number = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    match (first, last) {
        ("", n) => match number(n) {
            None => Wanted::All,
            Some(0) => Wanted::Unsatisfiable,
            Some(_) if size == 0 => Wanted::Unsatisfiable,
            Some(n) => Wanted::Part(size.saturating_sub(n)..size),
        },
        (a, b) => {
            // `a-` runs to the end.
            let last = match b {
                "" => Some(u64::MAX),
                b => number(b),
            };
            match (number(a), last) {
                (Some(a), Some(last)) if last >= a => match a >= size {
                    true => Wanted::Unsatisfiable,
                    false => Wanted::Part(a..last.saturating_add(1).min(size)),
                },
                _ => Wanted::All,
            }
        }
    }
}

/// Whether `mime` could run script or show active content on the CMS origin: everything
/// but raster images, audio, video and PDF (call 7).
fn sandboxed(mime: &str) -> bool {
    let m = mime
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let inert = m.starts_with("audio/")
        || m.starts_with("video/")
        || m == "application/pdf"
        || (m.starts_with("image/") && m != "image/svg+xml");
    !inert
}

/// `name` for a quoted `filename=`: printable ASCII, with anything else as `_`.
fn ascii_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '"' | '\\' => '_',
            c if c == ' ' || c.is_ascii_graphic() => c,
            _ => '_',
        })
        .collect()
}

/// RFC 8187's `attr-char`, what `filename*` keeps as it is.
fn attr_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b)
}

/// What an entity tag may hold, less `%`, which marks what's encoded, and `,`, which
/// `If-None-Match` splits tags on.
fn etag_char(b: u8) -> bool {
    b.is_ascii_graphic() && !matches!(b, b'"' | b'%' | b',')
}

/// `s` with each byte `keep` refuses percent-encoded.
fn encoded(s: &str, keep: fn(u8) -> bool) -> String {
    s.bytes()
        .map(|b| match keep(b) {
            true => char::from(b).to_string(),
            false => format!("%{b:02X}"),
        })
        .collect()
}

/// Whether documents of `c` link straight to the bucket (call 6): only when a signed-out
/// caller may read every document and its `filename` and `sizes`, and `c` has no drafts,
/// so a draft's object never has a public URL.
pub(crate) fn direct_urls(access: &Access, c: &Collection) -> bool {
    let anonymous = access.anonymous();
    !c.schema.drafts()
        && anonymous.reads_every_document(c)
        && anonymous.reads(c, "filename")
        && anonymous.reads(c, "sizes")
}

/// Fills `url`, `thumbnailURL` and each size's and format's `url` into `doc`, a document of
/// `c` with ID `id` as the caller may read it (after `Access::strip`). `key` is the stored
/// original's, given when its bucket URLs may be used (`direct_urls`); else they lead to the
/// file route. Never stored: worked out again on every read.
pub(crate) fn fill_upload_urls(
    state: &AppState,
    access: &Access,
    c: &Collection,
    id: Uuid,
    doc: &mut Map<String, Value>,
    key: Option<&str>,
) {
    let Some(u) = &c.schema.upload else { return };
    let storage = state.media.storages.get(&u.storage);
    let shared = u.filename_compound_index.is_some();
    // Read as a draft, a document's file is the draft's, which may share its name with the
    // published one's: its urls ask for the draft's.
    let draft = access.drafts() && c.schema.drafts();
    let link = |name: &str| -> Value {
        let direct = key
            .zip(storage)
            .and_then(|(key, s)| s.public_url(&beside(key, name)));
        direct
            .or_else(|| {
                route_url(
                    &state.auth.config.public_url,
                    &c.schema.slug,
                    name,
                    shared.then_some(id),
                    draft,
                )
            })
            .map_or(Value::Null, Value::String)
    };
    // A file's own `url`, from its `filename`; `null` while it has none.
    let url_of = |entry: &Map<String, Value>| {
        entry
            .get("filename")
            .and_then(Value::as_str)
            .map_or(Value::Null, link)
    };
    if let Some(Value::Object(sizes)) = doc.get_mut("sizes") {
        for (size, entry) in sizes.iter_mut() {
            let Value::Object(entry) = entry else {
                continue;
            };
            if access.reads(c, &format!("sizes.{size}.url")) {
                let url = url_of(entry);
                entry.insert("url".into(), url);
            }
            if let Some(Value::Object(formats)) = entry.get_mut("formats") {
                for (format, f) in formats.iter_mut() {
                    let Value::Object(f) = f else { continue };
                    if access.reads(c, &format!("sizes.{size}.formats.{format}.url")) {
                        let url = url_of(f);
                        f.insert("url".into(), url);
                    }
                }
            }
        }
    }
    // Hidden from the caller, the url can't come back as the thumbnail either.
    let url = match access.reads(c, "url") {
        true => url_of(doc),
        false => Value::Null,
    };
    if access.reads(c, "thumbnailURL") {
        let image = doc
            .get("mimeType")
            .and_then(Value::as_str)
            .is_some_and(|m| m.starts_with("image/"));
        let thumbnail = u
            .admin_thumbnail
            .as_ref()
            .and_then(|t| doc.get("sizes")?.get(t)?.get("url"))
            .filter(|v| v.is_string())
            .cloned()
            .or_else(|| (image && url.is_string()).then(|| url.clone()))
            .unwrap_or(Value::Null);
        doc.insert("thumbnailURL".into(), thumbnail);
    }
    if access.reads(c, "url") {
        doc.insert("url".into(), url);
    }
}

/// `{public_url}/api/{collection}/file/{name}`, with `?id=` when documents may share names
/// and `?draft=true` for a draft's file.
fn route_url(base: &str, slug: &str, name: &str, id: Option<Uuid>, draft: bool) -> Option<String> {
    let mut url = Url::parse(base).ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(["api", slug, "file", name]);
    if let Some(id) = id {
        url.query_pairs_mut().append_pair("id", &id.to_string());
    }
    if draft {
        url.query_pairs_mut().append_pair("draft", "true");
    }
    Some(url.into())
}

/// `fill_upload_urls` for each document of `items` (each with its `id`), looking up in one
/// query the stored keys of those that link straight to the bucket.
pub(crate) async fn fill_urls(
    state: &AppState,
    access: &Access,
    items: &mut [(Value, Arc<Collection>)],
) -> Result<(), ApiError> {
    let id_of = |v: &Value| v.get("id")?.as_str()?.parse::<Uuid>().ok();
    let mut direct: HashMap<Uuid, bool> = HashMap::new();
    let mut wanted = Vec::new();
    for (v, c) in items.iter() {
        if c.schema.upload.is_none() {
            continue;
        }
        let d = *direct.entry(c.id).or_insert_with(|| direct_urls(access, c));
        if d && let Some(id) = id_of(v) {
            wanted.push(id);
        }
    }
    let keys = match wanted.is_empty() {
        true => HashMap::new(),
        false => files::published_keys(&state.pool, &wanted).await?,
    };
    for (v, c) in items.iter_mut() {
        let (Some(id), Value::Object(doc)) = (id_of(v), &mut *v) else {
            continue;
        };
        let key = match direct.get(&c.id) {
            Some(true) => keys.get(&id).map(String::as_str),
            _ => None,
        };
        fill_upload_urls(state, access, c, id, doc, key);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(v: &str) -> HeaderValue {
        HeaderValue::from_str(v).unwrap()
    }

    #[test]
    fn ranges_are_read_as_one_or_ignored() {
        let r = |v: &str| range(Some(&h(v)), 100);
        assert_eq!(range(None, 100), Wanted::All);
        assert_eq!(r("bytes=0-9"), Wanted::Part(0..10));
        assert_eq!(r("bytes=90-"), Wanted::Part(90..100));
        assert_eq!(r("bytes=-5"), Wanted::Part(95..100));
        assert_eq!(r("bytes=-500"), Wanted::Part(0..100));
        assert_eq!(r("bytes=50-500"), Wanted::Part(50..100));
        assert_eq!(r("bytes=100-"), Wanted::Unsatisfiable);
        assert_eq!(r("bytes=-0"), Wanted::Unsatisfiable);
        // Several ranges, another unit, or nonsense: the whole file.
        assert_eq!(r("bytes=0-1,5-6"), Wanted::All);
        assert_eq!(r("items=0-1"), Wanted::All);
        assert_eq!(r("bytes=9-1"), Wanted::All);
        assert_eq!(r("bytes=a-b"), Wanted::All);
        assert_eq!(r("bytes=+1-2"), Wanted::All);
        assert_eq!(r("bytes=-"), Wanted::All);
        assert_eq!(range(Some(&h("bytes=0-")), 0), Wanted::Unsatisfiable);
    }

    #[test]
    fn active_types_are_sandboxed() {
        for m in [
            "image/png",
            "image/webp",
            "audio/mpeg",
            "video/mp4",
            "application/pdf",
        ] {
            assert!(!sandboxed(m), "{m}");
        }
        for m in [
            "image/svg+xml",
            "text/html",
            "text/plain",
            "application/xml",
            "IMAGE/SVG+XML",
        ] {
            assert!(sandboxed(m), "{m}");
        }
    }

    #[test]
    fn names_are_encoded_for_headers() {
        assert_eq!(ascii_name("café \"x\".png"), "caf_ _x_.png");
        assert_eq!(encoded("café 1.png", attr_char), "caf%C3%A9%201.png");
        assert_eq!(encoded("a\"b%.png", etag_char), "a%22b%25.png");
        // A comma separates tags in `If-None-Match`.
        assert_eq!(encoded("a,b.png", etag_char), "a%2Cb.png");
    }

    #[test]
    fn route_urls_encode_the_name() {
        let id = Uuid::nil();
        assert_eq!(
            route_url("http://localhost:3000", "media", "a b#.png", None, false).unwrap(),
            "http://localhost:3000/api/media/file/a%20b%23.png"
        );
        assert_eq!(
            route_url("https://x.test/cms/", "media", "a.png", Some(id), false).unwrap(),
            format!("https://x.test/cms/api/media/file/a.png?id={id}")
        );
        assert_eq!(
            route_url("https://x.test/", "media", "a.png", Some(id), true).unwrap(),
            format!("https://x.test/api/media/file/a.png?id={id}&draft=true")
        );
    }
}
