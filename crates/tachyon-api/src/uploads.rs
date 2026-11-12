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
use axum::extract::{FromRequest, Request, State};
use axum::http::header::CONTENT_TYPE;
use futures_util::{Stream, StreamExt as _};
use multer::{Constraints, Multipart, SizeLimit};
use serde_json::{Map, Value, json};
use tachyon_core::uploads::{
    CropRect, PasteUrl, SERVER_SET, UPLOAD_EDITS, Upload, UploadEdits, mime_allowed, restricted,
    sanitize_filename,
};
use tachyon_core::{Field, FieldError, FieldKind};
use tachyon_db::collections::Collection;
use tachyon_db::documents;
use tachyon_db::files::{self, FileRow, NewFile};
use tachyon_db::{DbError, DocCopy};
pub(crate) use tachyon_media::fetch::Spool;
use tachyon_media::fetch::{FetchError, HEAD, MAX_URL};
use tachyon_media::image::{self as picture, ImageError};
use tachyon_media::sniff::sniff;
use tachyon_media::storage::{Storage, StorageError, object_key, staging_key};
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

/// Where an upload's bytes come from: a file part already spooled, a URL to fetch
/// (`{"file": {"url": "…"}}` in the payload), or a file the client put in storage itself
/// (`{"file": {"filename": "…"}}`, as its upload instructions named it).
pub(crate) enum Incoming {
    Spooled(Spool),
    /// With the request's time limit, lengthened as the fetch starts.
    Url(String, Option<crate::Limit>),
    /// With the request's time limit, lengthened as its download starts.
    Client(ClientFile, Option<crate::Limit>),
}

/// A file put straight into storage, as a create or update names it: by the name its
/// instructions gave. That's all that's read: what it is comes from its bytes.
pub(crate) struct ClientFile {
    pub filename: String,
}

impl Incoming {
    /// The file spooled in `[uploads] temp_dir`, fetched first if it's a URL: a 400 when `u`
    /// takes no URLs, a 422 at `file.url` when this one can't be fetched, a 413 past the
    /// collection's limit.
    pub(crate) async fn spool(self, state: &AppState, u: &Upload) -> Result<Spool, ApiError> {
        let (raw, limit) = match self {
            Self::Spooled(spool) => return Ok(spool),
            Self::Url(raw, limit) => (raw, limit),
            Self::Client(..) => {
                return Err(ApiError::Internal(
                    "a file in storage isn't spooled; it's claimed".into(),
                ));
            }
        };
        if u.paste_url == PasteUrl::Off {
            return Err(ApiError::BadRequest(
                "this collection doesn't take URLs".into(),
            ));
        }
        if raw.len() > MAX_URL {
            return Err(at_url("this URL is too long".into()));
        }
        let url = Url::parse(&raw).map_err(|_| at_url("this isn't a URL".into()))?;
        let cap = u.filesize_limit.unwrap_or(state.media.max_file_size);
        let fetch = state.fetch.to_spool(&url, u, cap, &state.media.temp_dir);
        let fetched = match &limit {
            Some(limit) => {
                let until = limit.fetching();
                // No time left is a timeout at once; `timeout_at` would poll the fetch first.
                if tokio::time::Instant::now() >= until {
                    Err(FetchError::Timeout)
                } else {
                    tokio::time::timeout_at(until, fetch)
                        .await
                        .unwrap_or(Err(FetchError::Timeout))
                }
            }
            None => fetch.await,
        };
        fetched.map_err(|e| match e {
            FetchError::TooLarge(_) => ApiError::BodyTooLarge(e.to_string()),
            FetchError::Io(_) => ApiError::Internal(e.to_string()),
            e => at_url(e.to_string()),
        })
    }
}

impl Incoming {
    /// The file checked and made what's stored (`check_and_process`), from wherever it
    /// comes: one put in storage by the client is claimed for the caller first, for
    /// `document` when it's an update's. Its name is settled in the write's own scope when
    /// it's stored. A refusal after the claim gives that file up.
    pub(crate) async fn checked(
        self,
        state: &AppState,
        access: &crate::access::Access,
        c: &Collection,
        u: &Upload,
        document: Option<Uuid>,
    ) -> Result<Checked, ApiError> {
        match self {
            Self::Client(f, limit) => {
                claimed(state, access, c, u, &f, limit.as_ref(), document).await
            }
            other => {
                let spool = other.spool(state, u).await?;
                check_and_process(state, u, spool).await
            }
        }
    }
}

/// The 422 for a file in storage that isn't waiting for this caller: the same whether it
/// never existed, is someone else's, another collection's, past its grace or taken.
fn unclaimed() -> ApiError {
    at_file("no upload by this name is waiting for you; ask for upload instructions first".into())
}

/// `result`; when it's a refusal, the file a client put in storage that this request
/// claimed (`claimed`) is given up first, in its own transaction, with its cleanup due
/// now. A request cut off midway leaves its file pending, issued to nobody, for the cleanup
/// its reserve queued.
pub(crate) async fn or_give_up<T>(
    state: &AppState,
    claimed: Option<Uuid>,
    result: Result<T, ApiError>,
) -> Result<T, ApiError> {
    if result.is_err()
        && let Some(file) = claimed
    {
        abandon(state, file).await;
    }
    result
}

/// File `f`, put in storage by the client, claimed for the caller (for `document`, or for
/// none) and checked as
/// an upload is: past the limit by its stored size (413), then downloaded and sniffed,
/// with the time an upload gets. Any refusal from here on gives it up. What the client put
/// is deleted once the claim is done, either way; the file is only ever written by the
/// server, from what it checked.
async fn claimed(
    state: &AppState,
    access: &crate::access::Access,
    c: &Collection,
    u: &Upload,
    f: &ClientFile,
    limit: Option<&crate::Limit>,
    document: Option<Uuid>,
) -> Result<Checked, ApiError> {
    let Some(user) = access.user_id() else {
        return Err(unclaimed());
    };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let key = access.key_id();
    let row = files::claim_pending(&mut tx, c.id, &f.filename, user, key, document).await?;
    tx.commit().await.map_err(DbError::from)?;
    let row = row.ok_or_else(unclaimed)?;
    let id = row.id;
    let storage = state.media.storages.get(&row.storage).ok_or_else(|| {
        ApiError::Internal(format!("no storage named `{}` is configured", row.storage))
    });
    let staging = files::staging_key(&state.pool, id).await;
    let checked = match (storage, staging) {
        (Ok(storage), Ok(Some(staging))) => {
            let checked = checked_claim(state, u, storage, &staging, row, limit).await;
            // The sweep queued at issue is there should this fail, or the client put again.
            if let Err(error) = storage.delete_staging(&staging).await {
                tracing::warn!(%error, %staging, "couldn't delete a client's upload; it's swept when its URL runs out");
            }
            checked
        }
        (Err(e), _) => Err(e),
        (_, Err(e)) => Err(e.into()),
        (_, Ok(None)) => Err(ApiError::Internal(format!(
            "the issued file `{id}` has no staging key"
        ))),
    };
    or_give_up(state, Some(id), checked).await
}

/// `claimed`'s file `row`, put by the client at `staging` in `storage`, checked.
async fn checked_claim(
    state: &AppState,
    u: &Upload,
    storage: &Storage,
    staging: &str,
    row: FileRow,
    limit: Option<&crate::Limit>,
) -> Result<Checked, ApiError> {
    let cap = u.filesize_limit.unwrap_or(state.media.max_file_size);
    let too_big = || {
        ApiError::BodyTooLarge(format!(
            "the file is over this collection's limit of {cap} bytes"
        ))
    };
    let unread = |e: StorageError| ApiError::Internal(format!("can't read a stored file: {e}"));
    let size = match storage.head(staging).await {
        Ok(meta) => meta.size,
        // Never put, or swept: as unclaimable as any other.
        Err(e) if e.is_not_found() => return Err(unclaimed()),
        Err(e) => return Err(unread(e)),
    };
    if size > cap {
        return Err(too_big());
    }
    // Every check that needs no bytes is behind it: the download gets an upload's time.
    if let Some(limit) = limit {
        limit.lengthen();
    }
    let spool = download(storage, staging, &state.media.temp_dir, &row.filename, cap)
        .await
        .map_err(|e| match e.is_not_found() {
            // Swept between its `head` and its download: gone, like any other.
            true => unclaimed(),
            false => unread(e),
        })?
        .ok_or_else(too_big)?;
    let mut checked = check_and_process(state, u, spool).await?;
    checked.source = match checked.source {
        Source::Spool(spool) => Source::Claimed {
            spool,
            file: Box::new(row),
        },
        other => other,
    };
    Ok(checked)
}

/// The object at `key`, streamed into a new spool file in `dir` named `name`; `None`, and
/// the spool deleted, once it's past `cap` bytes.
pub(crate) async fn download(
    storage: &Storage,
    key: &str,
    dir: &Path,
    name: &str,
    cap: u64,
) -> Result<Option<Spool>, StorageError> {
    let got = storage.get_range(key, None).await?;
    let file = NamedTempFile::new_in(dir)?;
    let mut out = tokio::fs::File::from_std(file.reopen()?);
    let (mut head, mut len) = (Vec::with_capacity(HEAD), 0u64);
    let mut chunks = got.into_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        len += chunk.len() as u64;
        if len > cap {
            return Ok(None);
        }
        let room = HEAD - head.len();
        head.extend_from_slice(&chunk[..room.min(chunk.len())]);
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    Ok(Some(Spool {
        file,
        len,
        head,
        name: name.to_owned(),
    }))
}

fn at_url(message: String) -> ApiError {
    ApiError::Validation(vec![FieldError {
        path: "file.url".into(),
        message,
    }])
}

/// A file that passed every check, with what was learnt about it: one sent and spooled,
/// or an edit of the document's stored one.
pub(crate) struct Checked {
    /// The name it's stored under, before `reserve` numbers it.
    pub name: String,
    pub source: Source,
    pub mime: String,
    pub filesize: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub blurhash: Option<String>,
    /// For an edit, the file it was made from, which the document must still hold.
    pub edits: Option<Uuid>,
}

/// Where a checked file's bytes are.
pub(crate) enum Source {
    /// In a spool file, to be stored.
    Spool(Spool),
    /// Already stored, under `key` in `storage`, to be copied there: a focal point moved.
    Copy { storage: String, key: String },
    /// Put in storage by the client for pending `file`, claimed by this request, and
    /// downloaded into `spool` to be checked; stored from there at the file's own key.
    Claimed { spool: Spool, file: Box<FileRow> },
}

impl Checked {
    /// The file a client put in storage that this request claimed, if it's one.
    pub(crate) fn claimed(&self) -> Option<Uuid> {
        match &self.source {
            Source::Claimed { file, .. } => Some(file.id),
            _ => None,
        }
    }
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
    /// A URL in the body carries the request's time limit, for its fetch to lengthen.
    pub(crate) async fn read(
        req: Request,
        file_limit: Option<u64>,
        temp_dir: &Path,
    ) -> Result<Self, ApiError> {
        let limit = req.extensions().get::<crate::Limit>().cloned();
        let mut body = Self::read_body(req, file_limit, temp_dir).await?;
        if let Self::Upload {
            file: Incoming::Url(_, at) | Incoming::Client(_, at),
            ..
        } = &mut body
        {
            *at = limit;
        }
        Ok(body)
    }

    async fn read_body(
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

    /// `payload`, as an upload of the URL in its `file` when it's `{"url": "…"}`, or of a
    /// file put in storage when it's `{"filename": "…"}`, and the collection takes files
    /// (`file_limit`). Any other key beside them is a 422.
    fn pasted(mut payload: Value, file_limit: Option<u64>) -> Result<Self, ApiError> {
        let has = |k: &str| payload.get("file").is_some_and(|f| f.get(k).is_some());
        let pasting = file_limit.is_some() && has("url");
        let putting = file_limit.is_some() && !pasting && has("filename");
        let Some(Value::Object(file)) = (pasting || putting)
            .then(|| payload.as_object_mut()?.remove("file"))
            .flatten()
        else {
            return Ok(Self::Json(payload));
        };
        if putting {
            return Ok(Self::Upload {
                payload,
                file: Incoming::Client(client_file(&file)?, None),
            });
        }
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
                file: Incoming::Url(url.clone(), None),
            }),
            _ => Err(at_url("expected a URL".into())),
        }
    }
}

/// `file` of a payload naming a file put in storage: its `filename`, with the `mimeType`
/// and `filesize` its instructions gave allowed beside it. A 422 at what's wrong.
fn client_file(file: &Map<String, Value>) -> Result<ClientFile, ApiError> {
    let at = |k: &str, message: &str| FieldError {
        path: format!("file.{k}"),
        message: message.into(),
    };
    let mut errors: Vec<FieldError> = file
        .keys()
        .filter(|k| !["filename", "mimeType", "filesize"].contains(&k.as_str()))
        .map(|k| {
            at(
                k,
                "unknown field; a file put in storage takes `filename`, `mimeType` and `filesize`",
            )
        })
        .collect();
    let filename = file["filename"].as_str();
    if filename.is_none() {
        errors.push(at("filename", "expected text"));
    }
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    Ok(ClientFile {
        filename: filename.unwrap_or_default().to_owned(),
    })
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

/// A 415 unless `u` takes a file of type `mime` named `name`, saying what it looked like.
fn type_allowed(u: &Upload, mime: &str, name: &str) -> Result<(), ApiError> {
    if !mime_allowed(&u.mime_types, mime) {
        return Err(ApiError::UnsupportedMediaType(format!(
            "this file is {mime}; this collection takes {}",
            u.mime_types.join(", ")
        )));
    }
    if !u.allow_restricted_file_types && restricted(mime, name) {
        return Err(ApiError::UnsupportedMediaType(format!(
            "this file is {mime}, named `{name}`; this collection doesn't take executables or scripts"
        )));
    }
    Ok(())
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
    type_allowed(u, mime, &spool.name)?;
    let (mut mime, mut width, mut height, mut blurhash) = (mime, None, None, None);
    if mime == "image/svg+xml" {
        let path = spool.file.path().to_path_buf();
        // The spool moves into the blocking work and comes back with its result: should the
        // request be dropped meanwhile, the work still owns it and deletes it when done, so
        // nothing it writes outlives it.
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
        let (back, done) =
            processed(state, u, spool, image_refused, picture::process_original).await?;
        spool = back;
        if done.mime != mime {
            spool.name = with_extension(&spool.name, done.mime);
        }
        (mime, width, height, blurhash) = (
            done.mime,
            Some(done.width),
            Some(done.height),
            done.blurhash,
        );
    }
    Ok(Checked {
        name: spool.name.clone(),
        filesize: spool.len,
        source: Source::Spool(spool),
        mime: mime.to_owned(),
        width,
        height,
        blurhash,
        edits: None,
    })
}

/// The image in `spool` made into what's stored by `work`: read from its headers and held
/// to the pixel cap before anything is decoded, then decoded behind `DECODES`. The result
/// replaces the spool file, whose length it takes. `refused` says what an image error
/// answers.
async fn processed(
    state: &AppState,
    u: &Upload,
    mut spool: Spool,
    refused: fn(ImageError) -> ApiError,
    work: impl FnOnce(
        &Path,
        &picture::Probe,
        &Upload,
        u64,
        &Path,
    ) -> Result<picture::Processed, ImageError>
    + Send
    + 'static,
) -> Result<(Spool, picture::Processed), ApiError> {
    let path = spool.file.path().to_path_buf();
    let (at, head) = (path.clone(), spool.head.clone());
    let probe = blocking(move || picture::probe(&at, &head))
        .await?
        .map_err(refused)?;
    let max_pixels = state.media.max_pixels;
    picture::check_cap(&probe, picture::cap(u, max_pixels)).map_err(refused)?;
    let permit = Decoding::permit().await;
    let (u, out_dir) = (u.clone(), state.media.temp_dir.clone());
    // The spool moves into the blocking work and comes back with its result: should the
    // request be dropped meanwhile, the work still owns it and deletes it when done, so
    // nothing it writes outlives it.
    let (back, done) = blocking(move || {
        let _decoding = Decoding::running(permit);
        let done = work(&path, &probe, &u, max_pixels, &out_dir).and_then(|done| {
            // Over the spool, so a request never holds more than its spool once the permit
            // is given back.
            if done.path != path {
                std::fs::rename(&done.path, &path).map_err(|e| {
                    let _ = std::fs::remove_file(&done.path);
                    ImageError::Internal(format!("can't replace a spooled upload: {e}"))
                })?;
            }
            Ok(done)
        });
        (spool, done)
    })
    .await?;
    spool = back;
    let done = done.map_err(refused)?;
    spool.len = done.filesize;
    Ok((spool, done))
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
    out.insert("mimeType".into(), f.mime.clone().into());
    out.insert("filesize".into(), f.filesize.into());
    out.insert("width".into(), f.width.into());
    out.insert("height".into(), f.height.into());
    out.insert("sizes".into(), sizes.into());
    out.insert("blurhash".into(), f.blurhash.clone().into());
    out
}

/// The server-set fields of `fields` (a new file's) whose value differs from `stored`'s, as
/// a write for the field rules to judge: a write that brings a file writes these. `sizes`
/// is left out, as it's the server's alone and every new file empties it.
pub(crate) fn file_writes(
    fields: Option<&Map<String, Value>>,
    stored: Option<&Map<String, Value>>,
) -> Value {
    let was = |k: &str| stored.and_then(|s| s.get(k)).unwrap_or(&Value::Null);
    let changed: Map<String, Value> = fields
        .into_iter()
        .flatten()
        .filter(|(k, v)| *k != "sizes" && *v != was(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Value::Object(changed)
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

/// What a PATCH without a new file asks of the document's stored image: `uploadEdits`, or
/// a `focalX`/`focalY` other than the one stored. Either makes a new file (call 2); the
/// focal point it ends at is in the patch.
pub(crate) struct Edit {
    pub crop: Option<CropRect>,
    /// Only the focal fields changed: a document with no image to remake keeps its file.
    pub plain: bool,
}

fn at_edits(path: &str, message: &str) -> ApiError {
    ApiError::Validation(vec![FieldError {
        path: format!("{UPLOAD_EDITS}{path}"),
        message: message.into(),
    }])
}

/// The answer when the file an edit started from went away before it was stored.
fn gone_meanwhile() -> ApiError {
    ApiError::Conflict(
        "This document's file changed while it was being edited. Reload and try again.".into(),
    )
}

/// `uploadEdits`, taken out of `sent` (a body for `c`), read and checked against what `c`
/// allows: a 400 beside a new file, a 422 at each key it can't take. A collection without
/// files leaves it in, for validation to refuse like any unknown field.
pub(crate) fn take_edits(
    c: &Collection,
    sent: &mut Value,
    with_file: bool,
) -> Result<Option<UploadEdits>, ApiError> {
    let (Some(u), Value::Object(o)) = (&c.schema.upload, sent) else {
        return Ok(None);
    };
    let Some(v) = o.remove(UPLOAD_EDITS) else {
        return Ok(None);
    };
    if with_file {
        return Err(ApiError::BadRequest(format!(
            "`{UPLOAD_EDITS}` edits the stored file; send it without a new one"
        )));
    }
    let edits = UploadEdits::parse(&v).map_err(ApiError::Validation)?;
    // Asking for nothing is no edit: the patch is judged as if it never came.
    if edits.crop.is_none() && edits.focal_point.is_none() {
        return Ok(None);
    }
    if edits.crop.is_some() && !u.crop {
        return Err(at_edits(
            ".crop",
            "this collection's images can't be cropped",
        ));
    }
    if edits.focal_point.is_some() && !u.focal_point {
        return Err(at_edits(
            ".focalPoint",
            "this collection's images have no focal point",
        ));
    }
    Ok(Some(edits))
}

/// The focal point `data` holds, the default where it holds none or `null`.
fn focal_in(data: &Map<String, Value>) -> (f64, f64) {
    let at = |k: &str| data.get(k).and_then(Value::as_f64).unwrap_or(50.0);
    (at("focalX"), at("focalY"))
}

/// The edit `patch` (with `edits`, taken out of it) makes to `stored`'s image, when it
/// makes one. With `uploadEdits`, the focal point it ends at is written into `patch`, so
/// the field rules and validation judge it: `uploadEdits.focalPoint`, else what `patch`
/// sends, else 50/50 for a crop and the stored one otherwise. Without it, only a
/// collection with `focalPoint` remakes its file for a new focal point, and not when a new
/// file is sent; a focal value that isn't a number is left for validation to refuse. With
/// it, such a value is a 422 at its key before anything is made, since the edit would
/// otherwise write over it.
pub(crate) fn edit_of(
    c: &Collection,
    edits: Option<UploadEdits>,
    patch: &mut Value,
    stored: &Map<String, Value>,
    with_file: bool,
) -> Result<Option<Edit>, ApiError> {
    let (Some(u), Value::Object(p)) = (&c.schema.upload, patch) else {
        return Ok(None);
    };
    // `null` is the default, as on create; `None` inside for a value that isn't a number.
    let sent = |k: &str| p.get(k).map(|v| v.as_f64().or(v.is_null().then_some(50.0)));
    let Some(edits) = edits else {
        let (x, y) = (sent("focalX"), sent("focalY"));
        if !u.focal_point || with_file || x == Some(None) || y == Some(None) {
            return Ok(None);
        }
        let was = focal_in(stored);
        let now = (x.flatten().unwrap_or(was.0), y.flatten().unwrap_or(was.1));
        return Ok((now != was).then_some(Edit {
            crop: None,
            plain: true,
        }));
    };
    let bad: Vec<FieldError> = ["focalX", "focalY"]
        .into_iter()
        .filter(|k| sent(k) == Some(None))
        .map(|k| FieldError {
            path: k.into(),
            message: "expected a number".into(),
        })
        .collect();
    if !bad.is_empty() {
        return Err(ApiError::Validation(bad));
    }
    let (x, y) = match edits.focal_point {
        Some(f) => (f.x, f.y),
        None => {
            let base = match edits.crop {
                Some(_) => (50.0, 50.0),
                None => focal_in(stored),
            };
            let pick = |k, b: f64| sent(k).flatten().unwrap_or(b);
            (pick("focalX", base.0), pick("focalY", base.1))
        }
    };
    // Whole numbers as integers, as they'd be sent.
    let number = |v: f64| match v.fract() == 0.0 {
        true => Value::from(v as i64),
        false => Value::from(v),
    };
    p.insert("focalX".into(), number(x));
    p.insert("focalY".into(), number(y));
    Ok(Some(Edit {
        crop: edits.crop,
        plain: false,
    }))
}

/// A 409 when `patch` sends a focal point over `stored` but the locked document, `current`,
/// has another: its file was remade for that one meanwhile, and writing over it would
/// leave the document and its file disagreeing. Only where a focal point remakes the file
/// (`focalPoint` on, an image stored); elsewhere it's a field, and the last write wins.
pub(crate) fn focal_moved(
    c: &Collection,
    patch: &Value,
    stored: &Map<String, Value>,
    current: &Map<String, Value>,
) -> Result<(), ApiError> {
    let remakes = c.schema.upload.as_ref().is_some_and(|u| u.focal_point)
        && stored
            .get("mimeType")
            .and_then(Value::as_str)
            .is_some_and(|m| picture::size_format(m).is_some());
    let sends = patch.get("focalX").is_some() || patch.get("focalY").is_some();
    if remakes && sends && focal_in(stored) != focal_in(current) {
        return Err(gone_meanwhile());
    }
    Ok(())
}

/// The new file `e` makes from the image copy `copy` of document `id` holds: cropped from
/// the stored original (fetched, then cut behind `DECODES` like any upload), or for a new
/// focal point alone a copy of it made inside storage. `None` for a plain focal edit of a
/// document with no image; a 422 at `uploadEdits` for an edit of one.
pub(crate) async fn edited(
    state: &AppState,
    c: &Collection,
    u: &Upload,
    id: Uuid,
    copy: DocCopy,
    e: &Edit,
    limit: Option<&crate::Limit>,
) -> Result<Option<Checked>, ApiError> {
    let none = |message: &str| match e.plain {
        true => Ok(None),
        false => Err(at_edits("", message)),
    };
    let Some(from) = documents::file_of(&state.pool, c.id, id, copy).await? else {
        return none("this document has no file to edit");
    };
    let row = files::get(&state.pool, from).await?;
    if picture::size_format(&row.mime_type).is_none() {
        return none("only images can be cropped or given a focal point");
    }
    let dimension = |d: Option<i32>| d.and_then(|d| u32::try_from(d).ok());
    let mut checked = Checked {
        name: row.filename.clone(),
        source: Source::Copy {
            storage: row.storage.clone(),
            key: row.key.clone(),
        },
        mime: row.mime_type.clone(),
        filesize: u64::try_from(row.filesize).unwrap_or_default(),
        width: dimension(row.width),
        height: dimension(row.height),
        blurhash: row.blurhash.clone(),
        edits: Some(from),
    };
    let Some(crop) = &e.crop else {
        return Ok(Some(checked));
    };
    let (Some(w), Some(h)) = (checked.width, checked.height) else {
        return Err(at_edits(".crop", "this image's size isn't known"));
    };
    let rect = crop
        .pixels(w, h)
        .map_err(|e| ApiError::Validation(vec![e]))?;
    let storage = state.media.storages.get(&row.storage).ok_or_else(|| {
        ApiError::Internal(format!("no storage named `{}` is configured", row.storage))
    })?;
    // Every check is behind it: fetching and cutting the original gets the time an upload
    // does, as a pasted URL's fetch does. The field rules on what the cut file sets come
    // after, as they must: they need its new size, which only the cut gives.
    if let Some(limit) = limit {
        limit.lengthen();
    }
    let spool = fetch_spool(storage, &row.key, &state.media.temp_dir, &row.filename).await?;
    let crop_refused = |e: ImageError| match e {
        ImageError::Internal(what) => ApiError::Internal(what),
        e => at_edits(".crop", &e.to_string()),
    };
    let (spool, done) = processed(
        state,
        u,
        spool,
        crop_refused,
        move |path, probe, u, max, out| picture::crop(path, probe, rect, u, max, out),
    )
    .await?;
    checked.mime = done.mime.to_owned();
    checked.filesize = done.filesize;
    (checked.width, checked.height) = (Some(done.width), Some(done.height));
    checked.blurhash = done.blurhash;
    checked.source = Source::Spool(spool);
    Ok(Some(checked))
}

/// The object at `key` in a spool file in `dir`, named `name`: a 409 when it's gone.
async fn fetch_spool(
    storage: &Storage,
    key: &str,
    dir: &Path,
    name: &str,
) -> Result<Spool, ApiError> {
    download(storage, key, dir, name, u64::MAX)
        .await
        .map_err(|e| match e.is_not_found() {
            true => gone_meanwhile(),
            false => ApiError::Internal(format!("can't read a stored file: {e}")),
        })?
        .ok_or_else(|| ApiError::Internal("a stored file is bigger than any file".into()))
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
    // A copy stays in the storage it's copied within, and a claimed file where it was put.
    let name = match &f.source {
        Source::Copy { storage, .. } => storage,
        Source::Claimed { file, .. } => &file.storage,
        Source::Spool(_) => &u.storage,
    };
    let storage =
        state.media.storages.get(name).ok_or_else(|| {
            ApiError::Internal(format!("no storage named `{name}` is configured"))
        })?;
    let too_big = |what: &str| ApiError::Internal(format!("{what} doesn't fit a database integer"));
    let dimension = |d: Option<u32>| d.map(i32::try_from).transpose();
    let new = NewFile {
        collection_id: c.id,
        storage: name,
        filename: &f.name,
        scope,
        mime_type: &f.mime,
        filesize: i64::try_from(f.filesize).map_err(|_| too_big("the file size"))?,
        width: dimension(f.width).map_err(|_| too_big("the width"))?,
        height: dimension(f.height).map_err(|_| too_big("the height"))?,
        focal,
        blurhash: f.blurhash.as_deref(),
        issued_to: None,
        document,
    };
    let static_dir = u.static_dir.as_deref();
    let key_of = |id, name: &str| {
        object_key(static_dir, &c.schema.slug, id, name).map_err(|e| e.to_string())
    };
    let file = match &f.source {
        // Reserved when its instructions were given; now it's known what it is.
        Source::Claimed { file, .. } => {
            let mut tx = state.pool.begin().await.map_err(DbError::from)?;
            let settled = files::settle_in(&mut tx, file.id, &new, key_of).await?;
            tx.commit().await.map_err(DbError::from)?;
            settled
        }
        _ => files::reserve(&state.pool, &new, key_of).await?,
    };
    // Every file stored from a spool is written from the checked bytes, with the type
    // they were found to be.
    let stored = match &f.source {
        Source::Spool(spool) | Source::Claimed { spool, .. } => storage
            .put_file(&file.key, spool.file.path(), &f.mime)
            .await
            .map(|_| ()),
        Source::Copy { key, .. } => storage.copy(key, &file.key).await,
    };
    if let Err(e) = stored {
        abandon(state, file.id).await;
        return Err(match e.is_not_found() {
            true => gone_meanwhile(),
            false => ApiError::Internal(format!("can't store an upload: {e}")),
        });
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

/// What a client asks upload instructions for.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Ask {
    filename: String,
    mime_type: String,
    filesize: u64,
    /// The values the document will have, for a `filenameCompoundIndex` to number the name
    /// within; a replacement's are written over the stored document's.
    data: Option<Map<String, Value>>,
}

/// `POST /api/{collection}/upload-instructions`: where and how the caller puts a file in
/// storage itself, for a create or update to name after (see `upload_instructions_in`).
pub async fn instructions_for(
    State(state): State<AppState>,
    access: crate::access::Access,
    path: Result<axum::extract::Path<String>, axum::extract::rejection::PathRejection>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let axum::extract::Path(slug) = path.map_err(|_| ApiError::NotFound)?;
    let body = instructions_body(&access, &body)?;
    upload_instructions_in(&state, &access, &slug, body).await
}

/// `POST /api/upload-instructions`, its collection named by `collectionSlug`.
pub async fn upload_instructions(
    State(state): State<AppState>,
    access: crate::access::Access,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let mut body = instructions_body(&access, &body)?;
    let slug = match body
        .as_object_mut()
        .and_then(|o| o.remove("collectionSlug"))
    {
        Some(Value::String(slug)) => slug,
        _ => {
            return Err(ApiError::BadRequest(
                "name the collection in `collectionSlug`".into(),
            ));
        }
    };
    upload_instructions_in(&state, &access, &slug, body).await
}

/// An instructions request's body as JSON, read only once it's known someone is asking:
/// instructions are only ever for a signed-in caller, so a signed-out one is refused
/// first, whatever they sent.
fn instructions_body(access: &crate::access::Access, body: &[u8]) -> Result<Value, ApiError> {
    if access.user_id().is_none() {
        return Err(access.refuse());
    }
    if body.len() > JSON_LIMIT {
        return Err(ApiError::BodyTooLarge(format!(
            "the request body is over {JSON_LIMIT} bytes (10 MiB)"
        )));
    }
    serde_json::from_slice(body)
        .map_err(|e| ApiError::BadRequest(format!("the body isn't JSON: {e}")))
}

/// Instructions for a file of collection `slug`, as `body` describes it. Only for a caller
/// who may create there, signed in, and a storage that takes client uploads (else 400).
/// The size and type claimed are checked as an upload's are (413, 415), though only the
/// bytes put decide later. A `pending` file is reserved under the sanitised, numbered
/// name, issued to the caller and their key (429 while 100 wait), with a `PUT` URL that
/// lasts 15 minutes; the file waits an hour to be claimed.
async fn upload_instructions_in(
    state: &AppState,
    access: &crate::access::Access,
    slug: &str,
    mut body: Value,
) -> Result<Json<Value>, ApiError> {
    use tachyon_core::auth::Action;
    // A replacement is judged as the update that takes it, refusals and all; a new file as
    // the create. Both before the rest of the body is read.
    // `id`: the document whose file this replaces, for its update to take.
    let id = match body.as_object_mut().and_then(|o| o.remove("id")) {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().unwrap_or_default().to_owned()),
    };
    let (c, replaces) = match &id {
        Some(id) => {
            access.scope(Action::Update, slug)?;
            let id = crate::documents::parse_id(id)?;
            let c = crate::documents::collection(state, slug)?;
            access.may_write(Action::Update, &c)?;
            let (copy, ..) = crate::documents::may_update(state, access, &c, id).await?;
            (c, Some((id, copy)))
        }
        None => {
            let c = crate::documents::collection(state, slug)?;
            access.scope(Action::Create, &c.schema.slug)?;
            if !access.may_create(&c) {
                return Err(access.refuse());
            }
            (c, None)
        }
    };
    let Some(user) = access.user_id() else {
        return Err(access.refuse());
    };
    let Some(u) = &c.schema.upload else {
        return Err(ApiError::BadRequest(
            "this collection doesn't take files".into(),
        ));
    };
    let ask: Ask = serde_json::from_value(body)
        .map_err(|e| ApiError::BadRequest(format!("bad upload instructions request: {e}")))?;
    let storage = state.media.storages.get(&u.storage).ok_or_else(|| {
        ApiError::Internal(format!("no storage named `{}` is configured", u.storage))
    })?;
    if !storage.takes_client_uploads() {
        return Err(ApiError::BadRequest(format!(
            "storage `{}` doesn't take files straight from clients; send the file to /api/{slug}",
            u.storage
        )));
    }
    let cap = u.filesize_limit.unwrap_or(state.media.max_file_size);
    if ask.filesize > cap {
        return Err(ApiError::BodyTooLarge(format!(
            "the file is over this collection's limit of {cap} bytes"
        )));
    }
    let mime = ask.mime_type.trim().to_ascii_lowercase();
    if !media_type(&mime) {
        return Err(ApiError::UnsupportedMediaType(format!(
            "`{}` isn't a media type",
            ask.mime_type.escape_debug()
        )));
    }
    let name = sanitize_filename(&ask.filename);
    type_allowed(u, &mime, &name)?;
    // Numbered in the scope the write will store it in, so the name given is the one kept.
    // The document replaced must be there, as for the update; its values are the base.
    let mut values = match replaces {
        Some((id, copy)) => documents::get(&state.pool, c.id, id, copy).await?.data.0,
        None => Map::new(),
    };
    values.extend(ask.data.clone().unwrap_or_default());
    values.insert("filename".into(), name.clone().into());
    let scope = scope_of(u, &values);
    let filesize = i64::try_from(ask.filesize)
        .map_err(|_| ApiError::Internal("the file size doesn't fit a database integer".into()))?;
    let new = NewFile {
        collection_id: c.id,
        storage: &u.storage,
        filename: &name,
        scope: &scope,
        mime_type: &mime,
        filesize,
        width: None,
        height: None,
        focal: (50.0, 50.0),
        blurhash: None,
        issued_to: Some((user, access.key_id())),
        document: replaces.map(|(id, _)| id),
    };
    let static_dir = u.static_dir.as_deref();
    // Where the client puts it: a key of its own that nothing serves or links to. The
    // file's key is only ever written by the server, from what it checked.
    let staging = staging_key();
    let issued = files::issue(&state.pool, &new, &staging, |id, name| {
        object_key(static_dir, &c.schema.slug, id, name).map_err(|e| e.to_string())
    })
    .await?;
    let file = match issued {
        files::Issued::Reserved(file) => *file,
        files::Issued::Full { retry_after } => {
            return Err(ApiError::TooManyRequests {
                detail: format!(
                    "{} uploads wait for you to use them here; use or let some expire first",
                    files::ISSUED_CAP
                ),
                retry_after: retry_after.as_secs(),
            });
        }
    };
    let url = match storage
        .presign_put(&staging, &mime, files::ISSUED_PUT)
        .await
    {
        Ok(url) => url,
        Err(e) => {
            abandon(state, file.id).await;
            return Err(unsigned(storage.name(), "an upload", e));
        }
    };
    Ok(Json(json!({
        "type": "http",
        "request": {
            "url": url.as_str(),
            "method": "PUT",
            "headers": storage.put_headers(&mime),
        },
        "file": {"filename": file.filename, "mimeType": mime, "filesize": ask.filesize},
    })))
}

/// A signing failure at `storage`: the store's own (a revoked grant, the cloud down) is
/// worth trying again later; anything else is a fault here.
pub(crate) fn unsigned(storage: &str, what: &str, e: StorageError) -> ApiError {
    match e {
        StorageError::Store(cause) => {
            tracing::warn!(%cause, storage, "can't sign {what}");
            ApiError::Unavailable(format!(
                "storage `{storage}` can't sign links right now; try again later"
            ))
        }
        other => ApiError::Internal(format!("can't sign {what}: {other}")),
    }
}

/// Whether `mime` is a bare `type/subtype` of token characters, fit for a signed header.
fn media_type(mime: &str) -> bool {
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    mime.split_once('/')
        .is_some_and(|(t, sub)| token(t) && token(sub))
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
