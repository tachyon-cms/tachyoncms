//! Files sent with a create or update as `multipart/form-data`: the body read into a spool
//! file, what the file is checked against, and the reserve and store that come before the
//! document's own transaction.
//!
//! The order matters for what a failure leaves behind: everything that can refuse the
//! request without touching storage runs first (the size, the type, the payload's
//! validation). Only then is a `pending` row reserved and the object stored, and a document
//! write that fails after that gives the row up, so its cleanup deletes the object at once.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::header::CONTENT_TYPE;
use futures_util::Stream;
use multer::{Constraints, Multipart, SizeLimit};
use serde_json::{Map, Value, json};
use tachyon_core::uploads::{
    PasteUrl, SERVER_SET, Upload, mime_allowed, restricted, sanitize_filename,
};
use tachyon_core::{Field, FieldError, FieldKind};
use tachyon_db::collections::Collection;
use tachyon_db::files::{self, FileRow, NewFile};
pub(crate) use tachyon_media::fetch::Spool;
use tachyon_media::fetch::{FetchError, HEAD};
use tachyon_media::image::{self as picture, ImageError};
use tachyon_media::sniff::sniff;
use tachyon_media::storage::object_key;
use tachyon_media::svg::{self, SvgError};
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Semaphore, SemaphorePermit};
use url::Url;
use uuid::Uuid;

use crate::documents::{JSON_LIMIT, json_body};
use crate::{ApiError, AppState};

/// Room for the parts' boundaries and headers, past the file and the payload.
const OVERHEAD: u64 = 64 * 1024;
/// The most a part's headers may take, with the preamble and delimiter before them; the
/// parser buffers them whole, and keeps 32 headers at most.
const HEADERS_MAX: usize = 16 * 1024;
/// The most the body hands the parser at a time. It reads all that's ready at once, so this
/// bounds how far past a part it reads while that part is read.
const CHUNK: usize = 16 * 1024;

/// A create or update body: JSON, or a multipart body with a file. A multipart body without
/// a `file` part is its `_payload` as JSON.
pub(crate) enum UploadBody {
    Json(Value),
    Upload { payload: Value, file: Incoming },
}

/// Where an upload's bytes come from: a file part already spooled, or a URL to fetch
/// (`{"file": {"url": "…"}}` in the payload).
pub(crate) enum Incoming {
    Spooled(Spool),
    Url(String),
}

impl Incoming {
    /// The file spooled in `[uploads] temp_dir`, fetched first if it's a URL: a 400 when `u`
    /// takes no URLs, a 422 at `file.url` when this one can't be fetched, a 413 past the
    /// collection's limit.
    pub(crate) async fn spool(self, state: &AppState, u: &Upload) -> Result<Spool, ApiError> {
        let raw = match self {
            Self::Spooled(spool) => return Ok(spool),
            Self::Url(raw) => raw,
        };
        if u.paste_url == PasteUrl::Off {
            return Err(ApiError::BadRequest(
                "this collection doesn't take URLs".into(),
            ));
        }
        let url = Url::parse(&raw).map_err(|_| at_url("this isn't a URL".into()))?;
        let cap = u.filesize_limit.unwrap_or(state.media.max_file_size);
        state
            .fetch
            .to_spool(&url, u, cap, &state.media.temp_dir)
            .await
            .map_err(|e| match e {
                FetchError::TooLarge(_) => ApiError::BodyTooLarge(e.to_string()),
                FetchError::Io(_) => ApiError::Internal(e.to_string()),
                e => at_url(e.to_string()),
            })
    }
}

fn at_url(message: String) -> ApiError {
    ApiError::Validation(vec![FieldError {
        path: "file.url".into(),
        message,
    }])
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
            return Self::pasted(json_body(body)?, file_limit);
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
        let between = Arc::new(Between::default());
        // What's pulled while a part is looked for: the rest of its delimiter line, its
        // header lines and blank line, and what's left of the last piece. The exact limit is
        // checked on the parsed lines below; this bounds what the parser buffers meanwhile.
        let budget = HEADERS_MAX + CHUNK + boundary.len() + 16;
        let stream = Paced::new(req.into_body().into_data_stream(), between.clone(), budget);
        let mut parts = Multipart::with_constraints(stream, boundary, constraints);
        let (mut payload, mut spool) = (None, None);
        // Each part is read to its end, or refused, before the next is asked for, so only
        // what comes between parts is counted.
        while let Some(mut part) = between.count(parts.next_field()).await.map_err(refused)? {
            let lines: usize = part
                .headers()
                .iter()
                .map(|(name, value)| name.as_str().len() + ": ".len() + value.len() + 2)
                .sum();
            if lines > HEADERS_MAX {
                return Err(ApiError::BadRequest(HeadersTooLong.to_string()));
            }
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
        match spool {
            Some(s) => Ok(Self::Upload {
                payload,
                file: Incoming::Spooled(s),
            }),
            None => Self::pasted(payload, file_limit),
        }
    }

    /// `payload`, as an upload of the URL in its `file` when it's `{"url": "…"}` and the
    /// collection takes files (`file_limit`); any other key beside `url` is a 422.
    fn pasted(mut payload: Value, file_limit: Option<u64>) -> Result<Self, ApiError> {
        let pasting =
            file_limit.is_some() && payload.get("file").is_some_and(|f| f.get("url").is_some());
        let Some(Value::Object(file)) = pasting
            .then(|| payload.as_object_mut()?.remove("file"))
            .flatten()
        else {
            return Ok(Self::Json(payload));
        };
        let unknown: Vec<FieldError> = file
            .keys()
            .filter(|k| *k != "url")
            .map(|k| FieldError {
                path: format!("file.{k}"),
                message: "unknown field; a file from a URL takes `url` alone".into(),
            })
            .collect();
        if !unknown.is_empty() {
            return Err(ApiError::Validation(unknown));
        }
        match &file["url"] {
            Value::String(url) => Ok(Self::Upload {
                payload,
                file: Incoming::Url(url.clone()),
            }),
            _ => Err(at_url("expected a URL".into())),
        }
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
#[error("a part's header lines are over {HEADERS_MAX} bytes")]
struct HeadersTooLong;

/// Bytes the parser pulled while looking for the next part: its preamble, delimiter and
/// headers, which it buffers whole.
#[derive(Default)]
struct Between {
    counting: AtomicBool,
    pulled: AtomicUsize,
}

impl Between {
    /// `next`, counting from nothing what the body hands over meanwhile.
    async fn count<T>(&self, next: impl Future<Output = T>) -> T {
        self.pulled.store(0, Ordering::Relaxed);
        self.counting.store(true, Ordering::Relaxed);
        let _stop = Stop(self);
        next.await
    }
}

/// Stops counting when `count` ends, finished or dropped midway.
struct Stop<'a>(&'a Between);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.counting.store(false, Ordering::Relaxed);
    }
}

/// The body in pieces of at most `CHUNK` bytes, one per poll, so the parser, which takes all
/// that's ready, never holds more than one piece past what it asked for. Fails once more than
/// `budget` bytes arrive between parts.
struct Paced<S> {
    inner: S,
    rest: Bytes,
    /// A piece was just handed over; the next poll yields first.
    handed: bool,
    between: Arc<Between>,
    budget: usize,
}

impl<S> Paced<S> {
    fn new(inner: S, between: Arc<Between>, budget: usize) -> Self {
        Self {
            inner,
            rest: Bytes::new(),
            handed: false,
            between,
            budget,
        }
    }
}

impl<S, E> Stream for Paced<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<BoxError>,
{
    type Item = Result<Bytes, BoxError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.handed {
            this.handed = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        if this.rest.is_empty() {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => this.rest = bytes,
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e.into()))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
        let piece = this.rest.split_to(this.rest.len().min(CHUNK));
        if this.between.counting.load(Ordering::Relaxed) {
            let pulled = this
                .between
                .pulled
                .fetch_add(piece.len(), Ordering::Relaxed);
            if pulled + piece.len() > this.budget {
                return Poll::Ready(Some(Err(BoxError::from(HeadersTooLong))));
            }
        }
        this.handed = true;
        Poll::Ready(Some(Ok(piece)))
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

/// Images decoding at once, each holding up to `[uploads] max_pixels` in memory.
/// shortcut: fixed count; make it an [uploads] key when a big server needs more
pub(crate) static DECODES: Semaphore = Semaphore::const_new(2);
/// Decodes running now, and the most there have been.
static BUSY: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The most decodes that have run at once in this process.
#[doc(hidden)]
pub fn decodes_peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// A `DECODES` permit, counted while the blocking work holding it runs. It's moved into
/// that work, so a request dropped meanwhile can't give it back early.
pub(crate) struct Decoding(#[allow(dead_code)] SemaphorePermit<'static>);

impl Decoding {
    pub(crate) async fn permit() -> SemaphorePermit<'static> {
        DECODES.acquire().await.expect("DECODES is never closed")
    }

    /// Called first thing in the blocking work.
    pub(crate) fn running(permit: SemaphorePermit<'static>) -> Self {
        let now = BUSY.fetch_add(1, Ordering::Relaxed) + 1;
        PEAK.fetch_max(now, Ordering::Relaxed);
        Self(permit)
    }
}

impl Drop for Decoding {
    fn drop(&mut self) {
        BUSY.fetch_sub(1, Ordering::Relaxed);
    }
}

/// `work` on a blocking thread.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ApiError::Internal(format!("an upload's processing failed: {e}")))
}

fn at_file(message: String) -> ApiError {
    ApiError::Validation(vec![FieldError {
        path: "file".into(),
        message,
    }])
}

fn image_refused(e: ImageError) -> ApiError {
    match e {
        ImageError::Unsupported(what) => ApiError::UnsupportedMediaType(what.into()),
        ImageError::Internal(what) => ApiError::Internal(what),
        e => at_file(e.to_string()),
    }
}

/// Checks what `spool` is, by its bytes alone, against what `u` takes: a 415 saying what
/// it looked like. Then makes it what's stored: an image's size is read from its headers
/// and refused past the pixel cap before anything is decoded, then it's decoded behind
/// `DECODES` and stripped and transformed as `u` says; an SVG is sanitised. The spool file
/// is replaced by the result.
pub(crate) async fn check_and_process(
    state: &AppState,
    u: &Upload,
    mut spool: Spool,
) -> Result<Checked, ApiError> {
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
    let path = spool.file.path().to_path_buf();
    let (mut mime, mut width, mut height, mut blurhash) = (mime, None, None, None);
    // The spool moves into the blocking work and comes back with its result: should the
    // request be dropped meanwhile, the work still owns it and deletes it when done, so
    // nothing it writes outlives it.
    if mime == "image/svg+xml" {
        let (back, clean) = blocking(move || {
            let clean = (|| {
                let bytes = std::fs::read(&path)
                    .map_err(|e| ApiError::Internal(format!("can't read a spooled upload: {e}")))?;
                let clean = svg::sanitize(&bytes).map_err(|e| match e {
                    SvgError::NotSvg => ApiError::UnsupportedMediaType(e.to_string()),
                    SvgError::Malformed(_) => at_file(e.to_string()),
                })?;
                std::fs::write(&path, &clean.bytes).map_err(|e| {
                    ApiError::Internal(format!("can't write a spooled upload: {e}"))
                })?;
                Ok::<_, ApiError>(clean)
            })();
            (spool, clean)
        })
        .await?;
        spool = back;
        let clean = clean?;
        spool.len = clean.bytes.len() as u64;
        (width, height) = (clean.width, clean.height);
    } else if picture::decodes(mime) {
        let (at, head) = (path.clone(), spool.head.clone());
        let probe = blocking(move || picture::probe(&at, &head))
            .await?
            .map_err(image_refused)?;
        let max_pixels = state.media.max_pixels;
        picture::check_cap(&probe, picture::cap(u, max_pixels)).map_err(image_refused)?;
        let permit = Decoding::permit().await;
        let (u, out_dir) = (u.clone(), state.media.temp_dir.clone());
        let (back, done) = blocking(move || {
            let _decoding = Decoding::running(permit);
            let done = picture::process_original(&path, &probe, &u, max_pixels, &out_dir).and_then(
                |done| {
                    // Over the spool, so a request never holds more than its spool once
                    // the permit is given back.
                    if done.path != path {
                        std::fs::rename(&done.path, &path).map_err(|e| {
                            let _ = std::fs::remove_file(&done.path);
                            ImageError::Internal(format!("can't replace a spooled upload: {e}"))
                        })?;
                    }
                    Ok(done)
                },
            );
            (spool, done)
        })
        .await?;
        spool = back;
        let done = done.map_err(image_refused)?;
        if done.mime != mime {
            spool.name = with_extension(&spool.name, done.mime);
        }
        spool.len = done.filesize;
        (mime, width, height, blurhash) = (
            done.mime,
            Some(done.width),
            Some(done.height),
            done.blurhash,
        );
    }
    Ok(Checked {
        filesize: spool.len,
        spool,
        mime,
        width,
        height,
        blurhash,
    })
}

/// `name` with the extension of `mime`, for an original re-encoded in another format.
fn with_extension(name: &str, mime: &str) -> String {
    let ext = match mime {
        "image/jpeg" => "jpg",
        other => other.rsplit('/').next().unwrap_or("bin"),
    };
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    format!("{stem}.{ext}")
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
/// on read (`url`s), which are never stored. `sizes` is the server's alone, like `url`:
/// dropped unread, since a job may fill it between a client's read and its write.
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
        .filter(|k| **k != "sizes")
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
    use axum::body::Body;

    /// What reading `body`, sent with boundary `x`, refuses it with: its detail.
    async fn refusal(body: Vec<u8>) -> Option<String> {
        let temp = std::env::temp_dir().join(format!("tachyon-guard-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&temp).unwrap();
        let req = Request::builder()
            .header("content-type", "multipart/form-data; boundary=x")
            .body(Body::from(body))
            .unwrap();
        let out = match UploadBody::read(req, Some(1_000), &temp).await {
            Ok(_) => None,
            Err(e) => Some(
                e.problem().1["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            ),
        };
        std::fs::remove_dir(&temp).unwrap();
        out
    }

    fn long() -> Vec<u8> {
        vec![b'a'; HEADERS_MAX + 3 * CHUNK]
    }

    /// Each way the parser can be kept buffering before a part begins is cut off at the budget.
    #[tokio::test]
    async fn every_header_block_is_bounded() {
        let headers = format!("{HEADERS_MAX} bytes");
        let cases: [(&str, Vec<u8>); 5] = [
            ("a long header line", [b"--x\r\nContent-Disposition: form-data; name=\"file\"\r\nX: ".to_vec(), long()].concat()),
            ("an empty header block, then no end", [b"--x\r\n\r\n".to_vec(), long()].concat()),
            ("a preamble, then a long header", [b"junk--x\r\nX: ".to_vec(), long()].concat()),
            ("a preamble and no boundary", long()),
            ("a second part's headers", [
                b"--x\r\nContent-Disposition: form-data; name=\"_payload\"\r\n\r\n{}\r\n--x\r\nX: ".to_vec(),
                long(),
            ].concat()),
        ];
        for (what, body) in cases {
            let detail = refusal(body).await.unwrap_or_default();
            assert!(detail.contains(&headers), "{what}: {detail}");
        }
    }

    /// A part whose header lines (`Name: value\r\n` each) take exactly `bytes`.
    fn headers_of(bytes: usize) -> Vec<u8> {
        let disposition = "Content-Disposition: form-data; name=\"_payload\"\r\n";
        let pad = bytes - disposition.len() - "X-Pad: \r\n".len();
        let block = format!("{disposition}X-Pad: {}\r\n", "a".repeat(pad));
        assert_eq!(block.len(), bytes);
        format!("--x\r\n{block}\r\n{{}}\r\n--x--\r\n").into_bytes()
    }

    #[tokio::test]
    async fn the_header_limit_is_exact() {
        assert_eq!(refusal(headers_of(HEADERS_MAX)).await, None);
        let detail = refusal(headers_of(HEADERS_MAX + 1))
            .await
            .unwrap_or_default();
        assert!(detail.contains("over 16384 bytes"), "{detail}");
    }

    #[tokio::test]
    async fn a_dropped_wait_stops_counting() {
        let between = Between::default();
        let wait = between.count(std::future::pending::<()>());
        let late = tokio::time::timeout(std::time::Duration::from_millis(10), wait).await;
        assert!(late.is_err());
        assert!(!between.counting.load(Ordering::Relaxed));
    }

    /// Contents past the budget, one part after another, read fine.
    #[tokio::test]
    async fn contents_dont_count() {
        let payload = format!("{{\"alt\": \"{}\"}}", "a".repeat(3 * HEADERS_MAX));
        let body = format!(
            "--x\r\nContent-Disposition: form-data; name=\"_payload\"\r\n\r\n{payload}\r\n\
             --x\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhi\r\n--x--\r\n"
        );
        assert_eq!(refusal(body.into_bytes()).await, None);
    }
}
