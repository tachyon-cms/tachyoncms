//! HTTP API for TachyonCMS.

mod access;
pub mod auth;
mod collections;
mod documents;
mod error;
mod files;
mod globals;
mod jobs;
mod joins;
mod media_jobs;
#[cfg(test)]
mod multipart_fuzz;
mod params;
mod populate;
mod request_id;
mod schedule;
mod schemas;
mod trash;
mod uploads;
mod versions;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use std::any::Any;

use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{HeaderValue, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use sqlx::PgPool;
use tachyon_db::DbError;
use tachyon_media::storage::Storages;
use tower_http::catch_panic::CatchPanicLayer;

pub use collections::check_upload;
pub use error::ApiError;
pub use jobs::{JobsConfig, RunningJobs};
use schemas::Schemas;
pub use uploads::decodes_peak;

#[derive(Clone)]
pub(crate) struct AppState {
    pub pool: PgPool,
    pub schemas: Arc<Schemas>,
    pub auth: Arc<auth::AuthState>,
    /// See `Options::schema_inline_rows`.
    pub inline_rows: i64,
    pub media: Arc<UploadsConfig>,
    /// For files sent as a URL.
    pub fetch: tachyon_media::fetch::SafeFetch,
}

/// `[uploads]` and the `[storage.*]` they go to.
#[derive(Debug, Clone)]
pub struct UploadsConfig {
    /// Bytes; a collection's `filesizeLimit` when it sets none.
    pub max_file_size: u64,
    /// The most pixels an image may decode to, all its frames counted.
    pub max_pixels: u64,
    /// Where uploads are spooled while they're checked.
    pub temp_dir: PathBuf,
    /// How long an upload request may take.
    pub timeout: Duration,
    pub storages: Arc<Storages>,
}

/// What `check_upload` reads from the server's config: each storage by name, with whether
/// it's local, and the pixel cap. Built without opening anything, for `tachyon schema`.
#[derive(Debug, Clone)]
pub struct UploadLimits {
    pub storages: BTreeMap<String, bool>,
    pub max_pixels: u64,
}

impl UploadsConfig {
    pub fn limits(&self) -> UploadLimits {
        UploadLimits {
            storages: self
                .storages
                .0
                .iter()
                .map(|(name, s)| (name.clone(), s.is_local()))
                .collect(),
            max_pixels: self.max_pixels,
        }
    }
}

impl Default for UploadsConfig {
    /// For tests: the system temp dir, and `default` storage in `./uploads`.
    fn default() -> Self {
        Self {
            max_file_size: 25_000_000,
            max_pixels: 50_000_000,
            temp_dir: std::env::temp_dir(),
            timeout: Duration::from_secs(600),
            storages: Arc::default(),
        }
    }
}

/// How the router is set up. `Default` suits development and tests.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub auth: auth::AuthConfig,
    pub jobs: JobsConfig,
    /// The most rows a schema change or preview may scan inside its request; a bigger one
    /// runs on a job worker. `None` is 2,000; tests lower it.
    pub schema_inline_rows: Option<i64>,
    pub uploads: UploadsConfig,
}

/// Why the API couldn't start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("{0}")]
    Config(String),
}

/// The API with default options; see `router_with`. The defaults suit development and tests:
/// the public URL is `http://localhost:3000` and the password breach check is off. A server
/// facing people should pass real settings to `router_with`.
pub async fn router(pool: PgPool) -> Result<Router, StartError> {
    router_with(pool, Options::default()).await
}

/// The API with `options`. It also starts `options.jobs.workers` job workers, which run until
/// `pool` closes: dropping the router doesn't stop them, and a second router on the same pool
/// starts as many again.
pub async fn router_with(pool: PgPool, options: Options) -> Result<Router, StartError> {
    let auth = Arc::new(auth::AuthState::new(options.auth).map_err(StartError::Config)?);
    // Users are a collection like any other, so make sure it's there before serving.
    tachyon_db::auth::ensure_users_collection(&pool).await?;
    let schemas = Arc::new(Schemas::load(&pool).await?);
    // Builds any index a failed change left missing, in the background: a big build, or one
    // waiting on another server's, mustn't keep this one from answering.
    let collections = tachyon_db::collections::list(&pool).await?;
    let heal = pool.clone();
    tokio::spawn(async move {
        for c in collections {
            if let Err(error) = tachyon_db::indexes::reconcile_indexes(&heal, c.id).await {
                tracing::warn!(%error, collection = %c.schema.slug, "couldn't build indexes");
            }
        }
    });
    schemas::spawn_listener(pool.clone(), schemas.clone());
    let routes = Router::new()
        .route(
            "/api/admin/collections",
            get(collections::list).post(collections::create),
        )
        .route(
            "/api/admin/collections/{slug}",
            get(collections::get_one)
                .put(collections::update)
                .delete(collections::delete),
        )
        .route(
            "/api/admin/collections/{slug}/preview",
            post(collections::preview),
        )
        .route("/api/admin/jobs/{id}", get(collections::job))
        .route("/api/access", get(access::report))
        .route(
            "/api/globals/{slug}",
            get(globals::get).patch(globals::update),
        )
        .route(
            "/api/globals/{slug}/publish",
            post(versions::publish_global),
        )
        .route(
            "/api/globals/{slug}/unpublish",
            post(versions::unpublish_global),
        )
        .route("/api/globals/{slug}/versions", get(versions::list_global))
        .route(
            "/api/globals/{slug}/versions/{vid}",
            get(versions::get_global),
        )
        .route(
            "/api/globals/{slug}/versions/{vid}/restore",
            post(versions::restore_global),
        )
        .route("/api/globals/{slug}/diff", get(versions::diff_global))
        .route(
            "/api/globals/{slug}/schedule",
            put(schedule::put_global)
                .get(schedule::get_global)
                .delete(schedule::delete_global),
        )
        .route(
            "/api/{collection}",
            get(documents::list).post(documents::create),
        )
        .route(
            "/api/{collection}/trash",
            get(trash::list).delete(trash::empty),
        )
        .route("/api/{collection}/trash/{id}", delete(trash::purge))
        // `GET` answers `HEAD` too.
        .route("/api/{collection}/file/{filename}", get(files::serve))
        .route("/api/{collection}/trash/{id}/restore", post(trash::restore))
        .route(
            "/api/{collection}/{id}",
            get(documents::get)
                .patch(documents::update)
                .delete(documents::delete),
        )
        .route(
            "/api/{collection}/{id}/references",
            get(documents::references),
        )
        .route("/api/{collection}/{id}/publish", post(versions::publish))
        .route(
            "/api/{collection}/{id}/unpublish",
            post(versions::unpublish),
        )
        .route("/api/{collection}/{id}/versions", get(versions::list))
        .route("/api/{collection}/{id}/versions/{vid}", get(versions::get))
        .route(
            "/api/{collection}/{id}/versions/{vid}/restore",
            post(versions::restore),
        )
        .route("/api/{collection}/{id}/diff", get(versions::diff))
        .route(
            "/api/{collection}/{id}/schedule",
            put(schedule::put)
                .get(schedule::get)
                .delete(schedule::delete),
        )
        .merge(auth::routes())
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed });
    let state = AppState {
        pool,
        schemas,
        auth,
        inline_rows: options
            .schema_inline_rows
            .unwrap_or(collections::INLINE_ROWS),
        fetch: tachyon_media::fetch::SafeFetch::new(options.uploads.timeout)
            .map_err(StartError::Config)?,
        media: Arc::new(options.uploads),
    };
    let storages: Vec<String> = state.media.storages.0.keys().cloned().collect();
    media_jobs::seed_sweep(&state.pool, &storages).await?;
    let limits = TimeLimits {
        normal: Duration::from_secs(30),
        uploads: state.media.timeout,
    };
    let routes = routes
        .layer(axum::middleware::from_fn_with_state(limits, anonymous_wait))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::principal::resolve,
        ));
    jobs::spawn_workers(state.clone(), &options.jobs);
    Ok(layers(routes, limits).with_state(state))
}

/// How long a request may take: `uploads` for one that may carry a file or waits for its
/// sizes, `normal` for the rest.
#[derive(Debug, Clone, Copy)]
struct TimeLimits {
    normal: Duration,
    uploads: Duration,
}

/// Why a request may take the upload time limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadTime {
    /// A multipart create or update of a document.
    File,
    /// A create or update of a document that waits for what its file makes (`?wait=true`),
    /// once the caller is known to be signed in (`anonymous_wait`).
    Wait,
}

/// Whether `req` may take the upload time limit, and why. Decided by the route it matched,
/// so a route added later gets 30 s unless it's listed here.
fn upload_time(req: &Request) -> Option<UploadTime> {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str);
    let document = matches!(
        (req.method(), route),
        (&Method::POST, Some("/api/{collection}"))
            | (&Method::PATCH, Some("/api/{collection}/{id}"))
    );
    if !document {
        return None;
    }
    let multipart = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.trim_start()
                .to_ascii_lowercase()
                .starts_with("multipart/form-data")
        });
    if multipart {
        Some(UploadTime::File)
    } else if params::wants_wait(req.uri().query()) {
        Some(UploadTime::Wait)
    } else {
        None
    }
}

/// What every route sits behind: a body limit, a time limit and a panic guard, all answering
/// in problem+json with the request's ID. Outermost last.
fn layers<S: Clone + Send + Sync + 'static>(routes: Router<S>, limit: TimeLimits) -> Router<S> {
    routes
        .fallback(|| async { ApiError::NotFound })
        .layer(DefaultBodyLimit::max(documents::JSON_LIMIT))
        .layer(CatchPanicLayer::custom(|panic: Box<dyn Any + Send>| {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            ApiError::Internal(format!("handler panicked: {message}")).into_response()
        }))
        .layer(axum::middleware::from_fn_with_state(limit, time_limit))
        .layer(axum::middleware::from_fn(request_id::middleware))
        // Outermost: also decorate responses created by panic/timeout guards.
        .layer(axum::middleware::map_response(add_security_headers))
}

async fn add_security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

/// When the request's time is up: the handler's own waits stop short of it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline(pub(crate) tokio::time::Instant);

async fn time_limit(State(limits): State<TimeLimits>, mut req: Request, next: Next) -> Response {
    let now = tokio::time::Instant::now();
    let limit = match upload_time(&req) {
        Some(_) => limits.uploads,
        None => limits.normal,
    };
    let until = Limit::new(now, now + limit, limits);
    req.extensions_mut().insert(Deadline(now + limit));
    req.extensions_mut().insert(until.clone());
    let run = next.run(req);
    tokio::pin!(run);
    loop {
        let at = until.at();
        tokio::select! {
            res = &mut run => return res,
            () = tokio::time::sleep_until(at) => {
                if until.at() <= at {
                    return ApiError::Timeout.into_response();
                }
            }
            () = until.0.moved.notified() => {}
        }
    }
}

/// When `time_limit` ends a request. A URL found in the body moves it while it's fetched,
/// past every check before it (`fetching`, then `fetched`). However it moves, a request
/// ends by arrival + `[uploads] timeout` + the normal limit.
#[derive(Clone)]
pub(crate) struct Limit(Arc<LimitAt>);

struct LimitAt {
    at: std::sync::Mutex<tokio::time::Instant>,
    arrival: tokio::time::Instant,
    limits: TimeLimits,
    moved: tokio::sync::Notify,
}

impl Limit {
    fn new(arrival: tokio::time::Instant, at: tokio::time::Instant, limits: TimeLimits) -> Self {
        Self(Arc::new(LimitAt {
            at: std::sync::Mutex::new(at),
            arrival,
            limits,
            moved: tokio::sync::Notify::new(),
        }))
    }

    /// The latest any request may end.
    fn last(&self) -> tokio::time::Instant {
        self.0.arrival + self.0.limits.uploads + self.0.limits.normal
    }

    fn at(&self) -> tokio::time::Instant {
        *self.0.at.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, at: tokio::time::Instant) {
        *self.0.at.lock().unwrap_or_else(|e| e.into_inner()) = at;
        self.0.moved.notify_one();
    }

    /// For a fetch starting now: the request may run to its last moment, and the fetch
    /// must end by arrival + `[uploads] timeout`, which is returned, so its own timeout
    /// always comes first.
    pub(crate) fn fetching(&self) -> tokio::time::Instant {
        self.set(self.last());
        self.0.arrival + self.0.limits.uploads
    }

    /// After the fetch, what follows gets the normal limit.
    pub(crate) fn fetched(&self) {
        let normal = tokio::time::Instant::now() + self.0.limits.normal;
        self.set(normal.min(self.last()));
    }
}

/// Inside `principal::resolve`: a signed-out caller's `?wait=true` keeps the normal limit,
/// so a parameter alone can't hold a connection open for the upload limit. With a file, the
/// upload itself still gets the upload limit, but the wait for its sizes doesn't.
async fn anonymous_wait(
    State(limits): State<TimeLimits>,
    mut req: Request,
    next: Next,
) -> Response {
    let signed_in = req
        .extensions()
        .get::<auth::principal::Principal>()
        .is_some_and(|p| p.identity().is_some());
    let upload = upload_time(&req);
    if signed_in || upload.is_none() || !params::wants_wait(req.uri().query()) {
        return next.run(req).await;
    }
    let normal = tokio::time::Instant::now() + limits.normal;
    let until = req
        .extensions()
        .get::<Deadline>()
        .map_or(normal, |d| d.0.min(normal));
    req.extensions_mut().insert(Deadline(until));
    // A wait alone ends the request at the normal limit; a URL found in its body still
    // lengthens it, as a file part would have.
    if upload == Some(UploadTime::Wait)
        && let Some(limit) = req.extensions().get::<Limit>()
    {
        limit.set(normal.min(limit.at()));
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    async fn call(app: Router, uri: &str) -> (StatusCode, String, Value) {
        call_with(app, Request::get(uri)).await
    }

    async fn call_with(
        app: Router,
        req: axum::http::request::Builder,
    ) -> (StatusCode, String, Value) {
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let kind = res
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            kind,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn app() -> Router {
        let routes = Router::new()
            .route("/ok", get(|| async { "ok" }))
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "late"
                }),
            )
            .route(
                "/panic",
                get(|| async {
                    if true {
                        panic!("boom");
                    }
                    "unreachable"
                }),
            );
        let slow = || async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "late"
        };
        let routes = routes
            .route("/api/{collection}", post(slow).get(slow))
            .route("/api/{collection}/{id}", axum::routing::patch(slow))
            .route("/api/globals/{slug}", axum::routing::patch(slow))
            .route("/api/auth/login", post(slow));
        let limits = TimeLimits {
            normal: Duration::from_millis(50),
            uploads: Duration::from_secs(5),
        };
        // No `resolve` here, so every caller is signed out.
        let routes = routes.layer(axum::middleware::from_fn_with_state(limits, anonymous_wait));
        layers(routes, limits)
    }

    #[tokio::test]
    async fn uploads_get_the_upload_time_limit() {
        let multipart = "multipart/form-data; boundary=x";
        for (method, uri, kind, status) in [
            ("POST", "/api/images", multipart, StatusCode::OK),
            ("PATCH", "/api/images/1", multipart, StatusCode::OK),
            ("POST", "/api/images?wait=true", multipart, StatusCode::OK),
            (
                "POST",
                "/api/images",
                "application/json",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            // Signed out, `wait` alone earns nothing.
            (
                "POST",
                "/api/images?wait=true",
                "application/json",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "GET",
                "/api/images?wait=true",
                "",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "PATCH",
                "/api/globals/site",
                multipart,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "POST",
                "/api/auth/login?wait=true",
                multipart,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "POST",
                "/api/nowhere/at/all",
                multipart,
                StatusCode::NOT_FOUND,
            ),
        ] {
            let mut req = Request::builder().method(method).uri(uri);
            if !kind.is_empty() {
                req = req.header("content-type", kind);
            }
            let (got, _, _) = call_with(app(), req).await;
            assert_eq!(got, status, "{method} {uri} {kind}");
        }
    }

    #[tokio::test]
    async fn a_signed_in_wait_gets_the_upload_time_limit() {
        let (_, _, abilities) = tachyon_core::auth::builtin_roles()[0].clone();
        let user = auth::principal::Principal::User {
            identity: tachyon_db::auth::Identity {
                user_id: uuid::Uuid::now_v7(),
                role: "admin".into(),
                abilities,
            },
            session: uuid::Uuid::now_v7(),
            cookie: false,
        };
        let slow = || async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "late"
        };
        let limits = TimeLimits {
            normal: Duration::from_millis(50),
            uploads: Duration::from_secs(5),
        };
        let routes = Router::new()
            .route("/api/{collection}", post(slow))
            .layer(axum::middleware::from_fn_with_state(limits, anonymous_wait))
            .layer(axum::middleware::map_request(
                move |mut req: Request<Body>| {
                    req.extensions_mut().insert(user.clone());
                    async move { req }
                },
            ));
        let app = layers(routes, limits);
        for (uri, status) in [
            ("/api/images?wait=true", StatusCode::OK),
            ("/api/images", StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let req = Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json");
            assert_eq!(call_with(app.clone(), req).await.0, status, "{uri}");
        }
    }

    #[tokio::test]
    async fn a_signed_out_wait_ends_with_the_normal_limit() {
        // Answers how long the handler has left, in ms.
        let left = |req: Request<Body>| async move {
            let d = req.extensions().get::<Deadline>().unwrap().0;
            (d - tokio::time::Instant::now()).as_millis().to_string()
        };
        let limits = TimeLimits {
            normal: Duration::from_millis(50),
            uploads: Duration::from_secs(5),
        };
        let routes = Router::new()
            .route("/api/{collection}", post(left))
            .layer(axum::middleware::from_fn_with_state(limits, anonymous_wait));
        let app = layers(routes, limits);
        for (uri, most) in [("/api/images?wait=true", 50), ("/api/images", 5_000)] {
            let req = Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "multipart/form-data; boundary=x")
                .body(Body::empty())
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            let body = res.into_body().collect().await.unwrap().to_bytes();
            let ms: u128 = std::str::from_utf8(&body).unwrap().parse().unwrap();
            assert!(ms <= most && ms + 1_000 > most, "{uri}: {ms} ms");
        }
    }

    /// A pasted URL lifts the request's limit only as its fetch starts, past every check
    /// before it. The whole request ends by arrival + `[uploads] timeout` + the normal
    /// limit: the fetch gets until arrival + `timeout`, so a slow upstream is its 422, not
    /// the request's 503, and what follows the fetch gets the normal limit. Each route is a
    /// collection: `on` fetches and then works 10 ms, `work` works 300 ms after its fetch,
    /// `off` takes no URLs, `refused` is turned away before the fetch (as a field-write
    /// refusal is), `slow` fetches from a server too slow.
    #[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
    async fn a_pasted_url_gets_the_upload_time_limit(pool: sqlx::PgPool) {
        use futures_util::StreamExt;
        use tachyon_core::uploads::{PasteUrl, SkipSafeFetch, Upload};
        use tachyon_media::fixtures::{response, serve};
        let temp = tempfile::Builder::new()
            .prefix("tachyon-limit-")
            .tempdir()
            .unwrap();
        let limits = TimeLimits {
            normal: Duration::from_millis(100),
            uploads: Duration::from_millis(500),
        };
        let state = AppState {
            pool: pool.clone(),
            schemas: Arc::new(Schemas::load(&pool).await.unwrap()),
            auth: Arc::new(auth::AuthState::new(Default::default()).unwrap()),
            inline_rows: collections::INLINE_ROWS,
            media: Arc::new(UploadsConfig {
                temp_dir: temp.path().to_path_buf(),
                timeout: limits.uploads,
                ..UploadsConfig::default()
            }),
            fetch: tachyon_media::fetch::SafeFetch::new(limits.uploads).unwrap(),
        };
        let fast = serve(|_| vec![response("200 OK", &["Content-Length: 2"], b"hi")]).await;
        let slow = serve(|_| {
            let mut pieces = vec![response("200 OK", &["Content-Length: 20"], b"a")];
            pieces.extend((0..19).map(|_| b"a".to_vec()));
            pieces
        })
        .await;
        let pasted = move |req: Request<Body>| {
            let state = state.clone();
            async move {
                let route = req.uri().path().trim_start_matches("/api/").to_owned();
                let u = Upload {
                    paste_url: if route == "off" {
                        PasteUrl::Off
                    } else {
                        PasteUrl::On
                    },
                    skip_safe_fetch: SkipSafeFetch::All,
                    ..Upload::default()
                };
                let work = |ms| tokio::time::sleep(Duration::from_millis(ms));
                let body = uploads::UploadBody::read(req, Some(1_000), &state.media.temp_dir).await;
                let Ok(uploads::UploadBody::Upload { file, .. }) = body else {
                    work(300).await;
                    return "not pasted".into_response();
                };
                if route == "refused" {
                    work(300).await;
                    return ApiError::Forbidden("no".into()).into_response();
                }
                match file.spool(&state, &u).await {
                    Ok(_) => {
                        work(if route == "work" { 300 } else { 10 }).await;
                        "fetched".into_response()
                    }
                    Err(e) if route == "slow" => e.into_response(),
                    Err(e) => {
                        work(300).await;
                        e.into_response()
                    }
                }
            }
        };
        let routes = Router::new()
            .route("/api/{collection}", post(pasted.clone()))
            .route("/api/{collection}/{id}", axum::routing::patch(pasted))
            .layer(axum::middleware::from_fn_with_state(limits, anonymous_wait));
        let app = layers(routes, limits);
        let json =
            |addr: std::net::SocketAddr| format!(r#"{{"file": {{"url": "http://{addr}/a.txt"}}}}"#);
        let part = |addr| {
            format!(
                "--x\r\nContent-Disposition: form-data; name=\"_payload\"\r\n\r\n{}\r\n--x--\r\n",
                json(addr)
            )
        };
        let (kind, multipart) = ("application/json", "multipart/form-data; boundary=x");
        let ok = StatusCode::OK;
        let late = StatusCode::SERVICE_UNAVAILABLE;
        for (method, uri, kind, body, status) in [
            ("POST", "/api/on", kind, json(fast), ok),
            ("PATCH", "/api/on/1", kind, json(fast), ok),
            ("POST", "/api/on?wait=true", kind, json(fast), ok),
            ("POST", "/api/on", multipart, part(fast), ok),
            ("POST", "/api/work", kind, json(fast), late),
            ("POST", "/api/off", kind, json(fast), late),
            ("POST", "/api/refused", kind, json(fast), late),
            ("POST", "/api/on", kind, r#"{"alt": "x"}"#.into(), late),
            (
                "POST",
                "/api/slow",
                kind,
                json(slow),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", kind)
                .body(Body::from(body))
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), status, "{method} {uri} {kind}");
        }
        // A body dripped for most of the upload limit leaves its fetch only the rest: the
        // fetch's 422 comes by arrival + `timeout`, not a whole `timeout` after it starts.
        let body = part(slow).into_bytes();
        let (head, rest) = body.split_at(10);
        let pieces = vec![(0, head.to_vec()), (350, rest.to_vec())];
        let dripped = futures_util::stream::iter(pieces).then(|(ms, bytes)| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok::<_, std::io::Error>(bytes)
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/slow")
            .header("content-type", multipart)
            .body(Body::from_stream(dripped))
            .unwrap();
        let started = tokio::time::Instant::now();
        let res = app.clone().oneshot(req).await.unwrap();
        let took = started.elapsed();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let detail = res.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&detail).contains("file.url"));
        assert!(
            took < limits.uploads + Duration::from_millis(100),
            "{took:?}"
        );
        assert!(std::fs::read_dir(temp.path()).unwrap().next().is_none());
    }

    #[test]
    fn wait_is_read_as_the_handlers_read_it() {
        let wait = |uri: &str| {
            let req = Request::post(uri).body(Body::empty()).unwrap();
            params::wants_wait(req.uri().query())
        };
        assert!(wait("/api/images?wait=true"));
        assert!(wait("/api/images?a=1&wait=%74rue"));
        assert!(!wait("/api/images?wait=false"));
        assert!(!wait("/api/images?wait=truly"));
        assert!(!wait("/api/images"));
    }

    #[tokio::test]
    async fn slow_requests_get_503_problem_json() {
        let (status, kind, body) = call(app(), "/slow").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(kind, "application/problem+json");
        assert!(!body["requestId"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn panics_get_500_problem_json_with_request_id() {
        let (status, kind, body) = call(app(), "/panic").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(kind, "application/problem+json");
        assert!(!body["requestId"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn security_headers_cover_success_not_found_and_panics() {
        for (uri, expected_status) in [
            ("/ok", StatusCode::OK),
            ("/missing", StatusCode::NOT_FOUND),
            ("/panic", StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            let res = app()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), expected_status, "{uri}");
            assert_eq!(res.headers()["x-content-type-options"], "nosniff", "{uri}");
            assert_eq!(res.headers()["x-frame-options"], "DENY", "{uri}");
            assert_eq!(res.headers()["referrer-policy"], "no-referrer", "{uri}");
        }
    }
}
