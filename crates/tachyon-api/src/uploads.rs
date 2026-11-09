//! Files sent with a create or update as `multipart/form-data`: the body read into a spool
//! file, what the file is checked against, and the reserve and store that come before the
//! document's own transaction.
//!
//! The order matters for what a failure leaves behind: everything that can refuse the
//! request without touching storage runs first (the size, the type, the payload's
//! validation). Only then is a `pending` row reserved and the object stored, and a document
//! write that fails after that gives the row up, so its cleanup deletes the object at once.

use std::path::Path;

use axum::Json;
use axum::extract::{FromRequest, Request};
use axum::http::header::CONTENT_TYPE;
use futures_util::StreamExt;
use multer::{Constraints, Multipart, SizeLimit};
use serde_json::{Map, Value, json};
use tachyon_core::uploads::{SERVER_SET, Upload, mime_allowed, restricted, sanitize_filename};
use tachyon_core::{Field, FieldError, FieldKind};
use tachyon_db::collections::Collection;
use tachyon_db::files::{self, FileRow, NewFile};
use tachyon_media::sniff::sniff;
use tachyon_media::storage::object_key;
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::documents::{JSON_LIMIT, json_body};
use crate::{ApiError, AppState};

/// How much of a file is kept in memory for sniffing.
const HEAD: usize = 8 * 1024;
/// Room for the parts' boundaries and headers, past the file and the payload.
const OVERHEAD: u64 = 64 * 1024;
/// The most a part's headers may take; the parser buffers them whole, and keeps 32 at most.
const HEADERS_MAX: usize = 16 * 1024;

/// A create or update body: JSON, or a multipart body with a file. A multipart body without
/// a `file` part is its `_payload` as JSON.
pub(crate) enum UploadBody {
    Json(Value),
    Upload { payload: Value, file: Incoming },
}

/// Where an upload's bytes come from.
pub(crate) enum Incoming {
    Spooled(Spool),
}

/// A file part, written to a temporary file in `[uploads] temp_dir` that's deleted when
/// this is dropped.
pub(crate) struct Spool {
    pub file: NamedTempFile,
    pub len: u64,
    /// The first 8 KiB, for sniffing.
    pub head: Vec<u8>,
    /// The name it was sent under, sanitised.
    pub name: String,
}

/// A spooled file that passed every check, with what was learnt about it.
pub(crate) struct Checked {
    pub spool: Spool,
    pub mime: &'static str,
    pub filesize: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub blurhash: Option<String>,
}

/// A reserved and stored file, with the server-set fields the document takes from it.
pub(crate) struct Stored {
    pub file: FileRow,
    pub fields: Map<String, Value>,
}

impl UploadBody {
    /// Reads `req`'s body. JSON keeps its 10 MiB cap. A multipart body holds at most one
    /// `file` part, streamed to a spool file in `temp_dir` and refused one byte past
    /// `file_limit` (no `file` part at all when `None`), and at most one `_payload` part of
    /// JSON; any other part is a 400 naming it. A body cut off midway is a 400, and leaves no
    /// spool file behind.
    pub(crate) async fn read(
        req: Request,
        file_limit: Option<u64>,
        temp_dir: &Path,
    ) -> Result<Self, ApiError> {
        let kind = req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !kind
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("multipart/form-data")
        {
            let body = Json::<Value>::from_request(req, &()).await;
            return json_body(body).map(Self::Json);
        }
        let boundary = multer::parse_boundary(kind)
            .map_err(|e| ApiError::BadRequest(format!("bad multipart content type: {e}")))?;
        let mut allowed = vec!["_payload"];
        let mut sizes = SizeLimit::new()
            .for_field("_payload", JSON_LIMIT as u64)
            .whole_stream(
                file_limit
                    .unwrap_or(0)
                    .saturating_add(JSON_LIMIT as u64 + OVERHEAD),
            );
        if let Some(limit) = file_limit {
            allowed.push("file");
            sizes = sizes.for_field("file", limit);
        }
        let constraints = Constraints::new().allowed_fields(allowed).size_limit(sizes);
        let mut guard = HeaderGuard::new(&boundary);
        let stream = req
            .into_body()
            .into_data_stream()
            .map(move |chunk| match chunk {
                Ok(bytes) if !guard.feed(&bytes) => Err(BoxError::from(HeadersTooLong)),
                Ok(bytes) => Ok(bytes),
                Err(e) => Err(BoxError::from(e)),
            });
        let mut parts = Multipart::with_constraints(stream, boundary, constraints);
        let (mut payload, mut spool) = (None, None);
        while let Some(mut part) = parts.next_field().await.map_err(refused)? {
            if part.name() == Some("file") {
                if spool.is_some() {
                    return Err(ApiError::BadRequest(
                        "send one `file` part; this body has more".into(),
                    ));
                }
                spool = Some(spool_part(&mut part, temp_dir).await?);
            } else {
                if payload.is_some() {
                    return Err(ApiError::BadRequest(
                        "send one `_payload` part; this body has more".into(),
                    ));
                }
                let bytes = part.bytes().await.map_err(refused)?;
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| ApiError::BadRequest(format!("`_payload` isn't JSON: {e}")))?;
                payload = Some(value);
            }
        }
        let payload = payload.unwrap_or_else(|| json!({}));
        Ok(match spool {
            Some(s) => Self::Upload {
                payload,
                file: Incoming::Spooled(s),
            },
            None => Self::Json(payload),
        })
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
#[error("a part's headers are over {HEADERS_MAX} bytes")]
struct HeadersTooLong;

/// Watches a multipart body go by and refuses it once a part's headers run past
/// `HEADERS_MAX` bytes, since the parser would buffer them until the whole-stream cap.
struct HeaderGuard {
    /// `\r\n--boundary`: where a part's headers start.
    delimiter: Vec<u8>,
    /// The last bytes seen, enough to find a delimiter or `\r\n\r\n` split across chunks.
    recent: Vec<u8>,
    /// Bytes of headers seen since the last delimiter, while inside them.
    in_headers: Option<usize>,
    /// Past the closing delimiter: the rest is an epilogue the parser ignores.
    done: bool,
}

impl HeaderGuard {
    fn new(boundary: &str) -> Self {
        Self {
            delimiter: format!("\r\n--{boundary}").into_bytes(),
            // The first delimiter may open the body, with no line break before it.
            recent: b"\r\n".to_vec(),
            in_headers: None,
            done: false,
        }
    }

    /// False once some part's headers are too long.
    fn feed(&mut self, chunk: &[u8]) -> bool {
        let keep = self.delimiter.len().max(4);
        let last = *self.delimiter.last().expect("never empty");
        for &b in chunk {
            if self.done {
                return true;
            }
            self.recent.push(b);
            if self.recent.len() > 2 * keep {
                self.recent.drain(..self.recent.len() - keep);
            }
            match &mut self.in_headers {
                Some(n) => {
                    *n += 1;
                    if *n == 2 && self.recent.ends_with(b"--") {
                        self.done = true;
                    } else if self.recent.ends_with(b"\r\n\r\n") {
                        self.in_headers = None;
                    } else if *n > HEADERS_MAX {
                        return false;
                    }
                }
                None if b == last && self.recent.ends_with(&self.delimiter) => {
                    self.in_headers = Some(0);
                }
                None => {}
            }
        }
        true
    }
}

/// The multipart parser's refusal as the API's.
fn refused(e: multer::Error) -> ApiError {
    match e {
        multer::Error::FieldSizeExceeded { limit, field_name } => match field_name.as_deref() {
            Some("file") => ApiError::BodyTooLarge(format!(
                "the file is over this collection's limit of {limit} bytes"
            )),
            name => ApiError::BodyTooLarge(format!(
                "`{}` is over its limit of {limit} bytes",
                name.unwrap_or_default()
            )),
        },
        multer::Error::StreamSizeExceeded { limit } => {
            ApiError::BodyTooLarge(format!("the request body is over {limit} bytes"))
        }
        multer::Error::UnknownField {
            field_name: Some(name),
        } => ApiError::BadRequest(format!("unknown part `{name}`; send `file` and `_payload`")),
        multer::Error::UnknownField { field_name: None } => {
            ApiError::BadRequest("a part has no name; send `file` and `_payload`".into())
        }
        multer::Error::StreamReadFailed(e) if e.is::<HeadersTooLong>() => {
            ApiError::BadRequest(e.to_string())
        }
        multer::Error::StreamReadFailed(_) => {
            ApiError::BadRequest("the upload was cut off before it ended".into())
        }
        e => ApiError::BadRequest(format!("bad multipart body: {e}")),
    }
}

/// Streams `part` to a new spool file in `temp_dir`.
async fn spool_part(part: &mut multer::Field<'_>, temp_dir: &Path) -> Result<Spool, ApiError> {
    let name = sanitize_filename(part.file_name().unwrap_or_default());
    let internal = |e: std::io::Error| ApiError::Internal(format!("can't spool an upload: {e}"));
    let file = NamedTempFile::new_in(temp_dir).map_err(internal)?;
    let mut out = tokio::fs::File::from_std(file.as_file().try_clone().map_err(internal)?);
    let (mut head, mut len) = (Vec::with_capacity(HEAD), 0u64);
    // On any error `file` is dropped, which deletes it.
    while let Some(chunk) = part.chunk().await.map_err(refused)? {
        let room = HEAD - head.len();
        head.extend_from_slice(&chunk[..room.min(chunk.len())]);
        len += chunk.len() as u64;
        out.write_all(&chunk).await.map_err(internal)?;
    }
    out.flush().await.map_err(internal)?;
    Ok(Spool {
        file,
        len,
        head,
        name,
    })
}

/// The most bytes a file of `c` may have: its `filesizeLimit`, else `[uploads]
/// max_file_size`. `None` for a collection that takes no files.
pub(crate) fn file_limit(state: &AppState, c: &Collection) -> Option<u64> {
    let u = c.schema.upload.as_ref()?;
    Some(u.filesize_limit.unwrap_or(state.media.max_file_size))
}

/// Checks what `spool` is, by its bytes alone, against what `u` takes: a 415 saying what
/// it looked like.
pub(crate) fn check_and_process(u: &Upload, spool: Spool) -> Result<Checked, ApiError> {
    let mime = sniff(&spool.head);
    if !mime_allowed(&u.mime_types, mime) {
        return Err(ApiError::UnsupportedMediaType(format!(
            "this file is {mime}; this collection takes {}",
            u.mime_types.join(", ")
        )));
    }
    if !u.allow_restricted_file_types && restricted(mime, &spool.name) {
        return Err(ApiError::UnsupportedMediaType(format!(
            "this file is {mime}, named `{}`; this collection doesn't take executables or scripts",
            spool.name
        )));
    }
    Ok(Checked {
        filesize: spool.len,
        spool,
        mime,
        width: None,
        height: None,
        blurhash: None,
    })
}

/// The fields of `u` the server sets from file `f` named `filename`.
pub(crate) fn server_fields(u: &Upload, f: &Checked, filename: &str) -> Map<String, Value> {
    let sizes: Map<String, Value> = u
        .image_sizes
        .iter()
        .map(|s| (s.name.clone(), Value::Null))
        .collect();
    let mut out = Map::new();
    out.insert("filename".into(), filename.into());
    out.insert("mimeType".into(), f.mime.into());
    out.insert("filesize".into(), f.filesize.into());
    out.insert("width".into(), f.width.into());
    out.insert("height".into(), f.height.into());
    out.insert("sizes".into(), sizes.into());
    out.insert("blurhash".into(), f.blurhash.clone().into());
    out
}

/// `input` with `fields` written over it, for validating what the document will hold.
pub(crate) fn with_fields(input: &Value, fields: Option<&Map<String, Value>>) -> Value {
    let mut out = input.clone();
    if let (Value::Object(o), Some(fields)) = (&mut out, fields) {
        o.extend(fields.clone());
    }
    out
}

/// `input` without the server-set fields of `c`, which only the server writes; a 422 at
/// each one it sends with a value other than the one `stored` holds, so a read written
/// back unchanged passes. Missing and `null` count the same, and so do the values filled
/// on read (`url`s), which are never stored.
pub(crate) fn settle_server_set(
    c: &Collection,
    input: &Value,
    stored: Option<&Map<String, Value>>,
) -> Result<Value, ApiError> {
    let (Some(_), Value::Object(fields)) = (&c.schema.upload, input) else {
        return Ok(input.clone());
    };
    let field = |k: &str| c.schema.data().iter().find(|f| f.name == k);
    let errors: Vec<FieldError> = SERVER_SET
        .iter()
        .filter(|k| {
            let was = stored.and_then(|s| s.get(**k)).unwrap_or(&Value::Null);
            fields
                .get(**k)
                .is_some_and(|v| comparable(field(k), v) != comparable(field(k), was))
        })
        .map(|k| FieldError {
            path: (*k).into(),
            message: "set by the server".into(),
        })
        .collect();
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    let mut out = fields.clone();
    out.retain(|k, _| !SERVER_SET.contains(&k.as_str()));
    Ok(Value::Object(out))
}

/// `v` without what's filled on read or `null`; a group left empty is `null`.
fn comparable(f: Option<&Field>, v: &Value) -> Value {
    match (f.map(|f| &f.kind), v) {
        (Some(FieldKind::Group { fields }), Value::Object(o)) => {
            let out: Map<String, Value> = o
                .iter()
                .filter_map(|(k, v)| {
                    let sub = fields.iter().find(|s| &s.name == k);
                    if sub.is_some_and(|s| s.computed) {
                        return None;
                    }
                    let v = comparable(sub, v);
                    (!v.is_null()).then(|| (k.clone(), v))
                })
                .collect();
            if out.is_empty() {
                Value::Null
            } else {
                Value::Object(out)
            }
        }
        _ => v.clone(),
    }
}

/// Writes the server-set fields of `from` into `into` as they are, `null`s and absences
/// included: they describe the file the write takes from `from`.
pub(crate) fn keep_server_set(
    c: &Collection,
    from: &Map<String, Value>,
    into: &mut Map<String, Value>,
) {
    if c.schema.upload.is_none() {
        return;
    }
    for k in SERVER_SET {
        match from.get(*k) {
            Some(v) => into.insert((*k).into(), v.clone()),
            None => into.remove(*k),
        };
    }
}

/// A 422 at `file` when `c` needs a file to create a document and none came.
pub(crate) fn require_file(c: &Collection, sent: bool) -> Result<(), ApiError> {
    match &c.schema.upload {
        Some(u) if u.files_required_on_create && !sent => {
            Err(ApiError::Validation(vec![FieldError {
                path: "file".into(),
                message: "a file is required".into(),
            }]))
        }
        _ => Ok(()),
    }
}

/// The `filenameCompoundIndex` scope of `data`: those fields' values as JSON, or `""`.
pub(crate) fn scope_of(u: &Upload, data: &Map<String, Value>) -> String {
    u.filename_compound_index
        .as_ref()
        .map(|names| {
            let values: Vec<&Value> = names
                .iter()
                .map(|n| data.get(n).unwrap_or(&Value::Null))
                .collect();
            json!(values).to_string()
        })
        .unwrap_or_default()
}

/// The focal point `data` holds, the default where it holds none.
pub(crate) fn focal_of(data: &Map<String, Value>) -> (f32, f32) {
    let at = |k: &str| data.get(k).and_then(Value::as_f64).unwrap_or(50.0) as f32;
    (at("focalX"), at("focalY"))
}

/// Reserves `f` for `document` (the one being updated, if any) under its sanitised name,
/// numbered on a clash within `scope`, and stores it. A store that fails gives the
/// reservation up.
pub(crate) async fn persist(
    state: &AppState,
    c: &Collection,
    u: &Upload,
    f: &Checked,
    scope: &str,
    focal: (f32, f32),
    document: Option<Uuid>,
) -> Result<Stored, ApiError> {
    let storage = state.media.storages.get(&u.storage).ok_or_else(|| {
        ApiError::Internal(format!("no storage named `{}` is configured", u.storage))
    })?;
    let too_big = |what: &str| ApiError::Internal(format!("{what} doesn't fit a database integer"));
    let dimension = |d: Option<u32>| d.map(i32::try_from).transpose();
    let new = NewFile {
        collection_id: c.id,
        storage: &u.storage,
        filename: &f.spool.name,
        scope,
        mime_type: f.mime,
        filesize: i64::try_from(f.filesize).map_err(|_| too_big("the file size"))?,
        width: dimension(f.width).map_err(|_| too_big("the width"))?,
        height: dimension(f.height).map_err(|_| too_big("the height"))?,
        focal,
        blurhash: f.blurhash.as_deref(),
        issued_to: None,
        document,
    };
    let static_dir = u.static_dir.as_deref();
    let file = files::reserve(&state.pool, &new, |id, name| {
        object_key(static_dir, &c.schema.slug, id, name).map_err(|e| e.to_string())
    })
    .await?;
    if let Err(e) = storage
        .put_file(&file.key, f.spool.file.path(), f.mime)
        .await
    {
        abandon(state, file.id).await;
        return Err(ApiError::Internal(format!("can't store an upload: {e}")));
    }
    let fields = server_fields(u, f, &file.filename);
    Ok(Stored { file, fields })
}

/// Gives up the file `stored` reserved, when the document write that was to take it
/// failed, so its object is deleted now rather than after the grace. Should even that
/// fail, the cleanup `reserve` queued still runs once the grace is over.
async fn abandon(state: &AppState, file: Uuid) {
    if let Err(error) = files::abandon(&state.pool, file).await {
        tracing::warn!(%error, %file, "couldn't give up an upload; it's cleaned up within the hour");
    }
}

/// `write`'s result; when it failed, the file `stored` reserved for it is given up.
pub(crate) async fn taken_or_abandoned<T>(
    state: &AppState,
    stored: Option<&Stored>,
    write: impl Future<Output = Result<T, ApiError>>,
) -> Result<T, ApiError> {
    let result = write.await;
    if result.is_err()
        && let Some(s) = stored
    {
        abandon(state, s.file.id).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `body` fed to a guard for boundary `x` in `step`-byte chunks.
    fn passes(body: &[u8], step: usize) -> bool {
        let mut g = HeaderGuard::new("x");
        body.chunks(step).all(|c| g.feed(c))
    }

    #[test]
    fn only_long_part_headers_are_refused() {
        let long = "a".repeat(HEADERS_MAX);
        let part = |headers: &str, content: &str| {
            format!(
                "--x\r\nContent-Disposition: form-data; name=\"file\"{headers}\r\n\r\n{content}\r\n"
            )
        };
        // Long contents and a long epilogue are fine; so are delimiters split across chunks.
        let fine = format!(
            "{}--x--\r\n{long}{long}",
            part("", &format!("{long}{long}"))
        );
        let bad = format!("{}--x--\r\n", part(&format!("\r\nX-Long: {long}"), "hi"));
        for step in [1, 3, 4096] {
            assert!(passes(fine.as_bytes(), step), "{step}");
            assert!(!passes(bad.as_bytes(), step), "{step}");
        }
        // The second part's headers count on their own.
        let second = format!(
            "{}{}--x--\r\n",
            part("", "hi"),
            part(&format!("\r\nX: {long}"), "")
        );
        assert!(!passes(second.as_bytes(), 7));
    }
}
