//! The guarded URL fetch, against servers on 127.0.0.1 and a resolver the tests control.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tachyon_core::uploads::{PasteUrl, SkipSafeFetch, Upload, UrlPattern};
use tachyon_media::fetch::{FetchError, PRIVATE, SafeFetch, Spool};
use tachyon_media::fixtures::{response, serve};
use url::Url;

fn ok(body: &[u8]) -> Vec<Vec<u8>> {
    vec![response(
        "200 OK",
        &[&format!("Content-Length: {}", body.len())],
        body,
    )]
}

fn redirect(to: &str) -> Vec<Vec<u8>> {
    vec![response(
        "302 Found",
        &[&format!("Location: {to}"), "Content-Length: 0"],
        b"",
    )]
}

/// Skips the guard for every URL, so servers on 127.0.0.1 can be reached.
fn open() -> Upload {
    Upload {
        skip_safe_fetch: SkipSafeFetch::All,
        ..Upload::default()
    }
}

/// An `http` pattern for the server at `addr`.
fn at(addr: SocketAddr) -> UrlPattern {
    UrlPattern {
        protocol: Some("http".into()),
        hostname: addr.ip().to_string(),
        port: Some(addr.port().to_string()),
        pathname: None,
        search: None,
    }
}

fn spool_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("tachyon-fetch-")
        .tempdir()
        .unwrap()
}

fn empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).unwrap().next().is_none()
}

async fn fetch_with(
    client: &SafeFetch,
    url: &str,
    u: &Upload,
    cap: u64,
) -> (Result<Spool, FetchError>, tempfile::TempDir) {
    let dir = spool_dir();
    let out = client
        .to_spool(&Url::parse(url).unwrap(), u, cap, dir.path())
        .await;
    if out.is_err() {
        assert!(empty(dir.path()), "{url}: a spool file was left behind");
    }
    (out, dir)
}

async fn fetch(url: &str, u: &Upload) -> Result<Spool, FetchError> {
    let client = SafeFetch::new(Duration::from_secs(10)).unwrap();
    fetch_with(&client, url, u, 1_000_000).await.0
}

fn refusal(out: Result<Spool, FetchError>) -> String {
    match out {
        Err(FetchError::Refused(m)) => m,
        Err(e) => panic!("not refused: {e:?}"),
        Ok(_) => panic!("fetched"),
    }
}

/// A resolver giving each name the addresses `table` holds, counting its lookups.
struct Stub {
    table: Vec<(&'static str, Vec<IpAddr>)>,
    lookups: AtomicUsize,
}

impl Resolve for Stub {
    fn resolve(&self, name: Name) -> Resolving {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        let addrs: Vec<SocketAddr> = self
            .table
            .iter()
            .find(|(n, _)| *n == name.as_str())
            .map(|(_, ips)| ips.iter().map(|ip| SocketAddr::new(*ip, 0)).collect())
            .unwrap_or_default();
        Box::pin(async move { Ok(Box::new(addrs.into_iter()) as Addrs) })
    }
}

fn stub(table: Vec<(&'static str, Vec<&str>)>) -> Arc<Stub> {
    Arc::new(Stub {
        table: table
            .into_iter()
            .map(|(n, ips)| (n, ips.iter().map(|ip| ip.parse().unwrap()).collect()))
            .collect(),
        lookups: AtomicUsize::new(0),
    })
}

#[tokio::test]
async fn fetches_through_redirects_as_sent() {
    // Gzip's magic number, sent as gzip: kept as it came, not decompressed.
    let body = b"\x1f\x8b\x08\x00 not really gzip".to_vec();
    let sent = body.clone();
    let addr = serve(move |path| match path {
        "/files/my%20cat.txt" => redirect("/next"),
        "/next" => vec![response(
            "200 OK",
            &[
                "Content-Encoding: gzip",
                &format!("Content-Length: {}", sent.len()),
            ],
            &sent,
        )],
        _ => vec![response("404 Not Found", &[], b"")],
    })
    .await;
    let client = SafeFetch::new(Duration::from_secs(10)).unwrap();
    let url = format!("http://{addr}/files/my%20cat.txt");
    let (spool, _dir) = fetch_with(&client, &url, &open(), 100).await;
    let spool = spool.unwrap();
    assert_eq!(spool.name, "my cat.txt", "named by the URL it was sent");
    assert_eq!(spool.len, body.len() as u64);
    assert_eq!(spool.head, body);
    assert_eq!(std::fs::read(spool.file.path()).unwrap(), body);
}

#[tokio::test]
async fn redirects_are_bounded_and_checked() {
    let addr = serve(|path| {
        let n: usize = path.trim_start_matches("/r/").parse().unwrap_or(0);
        match path {
            "/none" => vec![response("302 Found", &["Content-Length: 0"], b"")],
            "/ftp" => redirect("ftp://example.com/a"),
            "/file" => redirect("file:///etc/passwd"),
            "/userinfo" => redirect("http://me:pw@example.com/a"),
            // Over the cap, and not a URL: refused for its length, unparsed.
            "/long" => redirect(&format!("http://[{}", "a".repeat(9 * 1024))),
            p if p.starts_with("/to/") => ok(b"done"),
            _ => redirect(&format!("/r/{}", n + 1)),
        }
    })
    .await;
    // Five redirects and then the file; six are too many.
    let five = serve(move |path| match path {
        "/r/5" => ok(b"five"),
        p => {
            let n: i64 = p.trim_start_matches("/r/").parse().unwrap();
            redirect(&format!("/r/{}", n + 1))
        }
    })
    .await;
    let spool = fetch(&format!("http://{five}/r/0"), &open()).await.unwrap();
    assert_eq!(spool.head, b"five");
    let m = refusal(fetch(&format!("http://{five}/r/-1"), &open()).await);
    assert!(m.contains("more than 5"), "{m}");
    let m = refusal(fetch(&format!("http://{addr}/r/0"), &open()).await);
    assert!(m.contains("more than 5"), "{m}");
    let m = refusal(fetch(&format!("http://{addr}/none"), &open()).await);
    assert!(m.contains("without saying where"), "{m}");
    for path in ["/ftp", "/file"] {
        let m = refusal(fetch(&format!("http://{addr}{path}"), &open()).await);
        assert!(m.contains("http and https"), "{path}: {m}");
    }
    let m = refusal(fetch(&format!("http://{addr}/long"), &open()).await);
    assert_eq!(m, "this URL is too long");
    let m = refusal(fetch(&format!("http://{addr}/userinfo"), &open()).await);
    assert!(m.contains("user name"), "{m}");
}

/// A name resolving to a private address is refused without the address in the message;
/// one with any private address among public ones too.
#[tokio::test]
async fn names_resolving_privately_are_refused() {
    let resolver = stub(vec![
        ("evil.test", vec!["10.0.0.1"]),
        ("mixed.test", vec!["8.8.8.8", "10.0.0.1"]),
        ("meta.test", vec!["::ffff:169.254.169.254"]),
    ]);
    let client = SafeFetch::with_resolver(Duration::from_secs(10), resolver.clone()).unwrap();
    for name in ["evil.test", "mixed.test", "meta.test"] {
        let (out, _dir) = fetch_with(
            &client,
            &format!("http://{name}/a.png"),
            &Upload::default(),
            100,
        )
        .await;
        let m = refusal(out);
        assert_eq!(m, PRIVATE, "{name}");
        assert!(!m.contains("10.0.0.1") && !m.contains("169.254"), "{m}");
    }
    assert_eq!(resolver.lookups.load(Ordering::Relaxed), 3);
}

/// The connection goes to the address the resolver gave, looked up once: a name only the
/// stub knows reaches the server it names.
#[tokio::test]
async fn connects_to_the_resolved_address() {
    let addr = serve(|_| ok(b"reached")).await;
    let resolver = stub(vec![("site.test", vec!["127.0.0.1"])]);
    let client = SafeFetch::with_resolver(Duration::from_secs(10), resolver.clone()).unwrap();
    let url = format!("http://site.test:{}/a.txt", addr.port());
    let (out, _dir) = fetch_with(&client, &url, &open(), 100).await;
    assert_eq!(out.unwrap().head, b"reached");
    assert_eq!(resolver.lookups.load(Ordering::Relaxed), 1);
    // Guarded, the same name is refused before anything is sent.
    let (out, _dir) = fetch_with(&client, &url, &Upload::default(), 100).await;
    assert_eq!(refusal(out), PRIVATE);
}

/// A server the guard is skipped for can't redirect to one it isn't.
#[tokio::test]
async fn each_hop_is_guarded_on_its_own() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let inside = serve(move |_| {
        counted.fetch_add(1, Ordering::Relaxed);
        ok(b"secret")
    })
    .await;
    let outside = serve(move |_| redirect(&format!("http://{inside}/"))).await;
    let skip_outside = Upload {
        skip_safe_fetch: SkipSafeFetch::AllowList(vec![at(outside)]),
        ..Upload::default()
    };
    let m = refusal(fetch(&format!("http://{outside}/"), &skip_outside).await);
    assert_eq!(m, PRIVATE);
    assert_eq!(
        hits.load(Ordering::Relaxed),
        0,
        "the private hop was never sent"
    );
    // The allowList is matched on every hop too.
    let listed = Upload {
        paste_url: PasteUrl::AllowList(vec![at(outside)]),
        ..open()
    };
    let m = refusal(fetch(&format!("http://{outside}/"), &listed).await);
    assert!(m.contains("allowList"), "{m}");
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    let m = refusal(fetch(&format!("http://{inside}/"), &listed).await);
    assert!(m.contains("allowList"), "{m}");
}

#[tokio::test]
async fn the_cap_holds_with_and_without_a_length() {
    let addr = serve(|path| match path {
        "/said" => vec![response("200 OK", &["Content-Length: 101"], &[b'a'; 101])],
        "/unsaid" => vec![response("200 OK", &[], &[b'a'; 50]), vec![b'a'; 51]],
        _ => ok(&[b'a'; 100]),
    })
    .await;
    let client = SafeFetch::new(Duration::from_secs(10)).unwrap();
    for path in ["/said", "/unsaid"] {
        let (out, _dir) = fetch_with(&client, &format!("http://{addr}{path}"), &open(), 100).await;
        assert!(
            matches!(out, Err(FetchError::TooLarge(100))),
            "{path}: {out:?}",
            out = out.err()
        );
    }
    let (out, _dir) = fetch_with(&client, &format!("http://{addr}/exact"), &open(), 100).await;
    assert_eq!(out.unwrap().len, 100);
}

#[tokio::test]
async fn a_slow_body_times_out() {
    let addr = serve(|_| {
        let mut pieces = vec![response("200 OK", &["Content-Length: 10"], b"a")];
        pieces.extend((0..9).map(|_| b"a".to_vec()));
        pieces
    })
    .await;
    let client = SafeFetch::new(Duration::from_millis(500)).unwrap();
    let (out, _dir) = fetch_with(&client, &format!("http://{addr}/"), &open(), 100).await;
    assert!(matches!(out, Err(FetchError::Timeout)), "{:?}", out.err());
}

#[tokio::test]
async fn upstream_errors_say_nothing_the_server_sent() {
    let addr = serve(|_| {
        vec![response(
            "404 Not Found",
            &["Content-Length: 14"],
            b"secret: hunter",
        )]
    })
    .await;
    let out = fetch(&format!("http://{addr}/"), &open()).await;
    let e = out.err().unwrap();
    assert!(matches!(e, FetchError::Status(404)), "{e:?}");
    assert!(!e.to_string().contains("hunter"), "{e}");
    // Nothing listening.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let e = fetch(&format!("http://{closed}/"), &open())
        .await
        .err()
        .unwrap();
    assert!(matches!(e, FetchError::Unreachable), "{e:?}");
    assert!(!e.to_string().contains("127.0.0.1"), "{e}");
}

/// Proxies named by the environment are ignored: run again as a child with them set, the
/// fetch goes straight to the server and the proxy sees nothing.
#[tokio::test]
async fn proxy_variables_are_ignored() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let proxy = serve(move |_| {
        counted.fetch_add(1, Ordering::Relaxed);
        ok(b"proxied")
    })
    .await;
    let target = serve(|_| ok(b"direct")).await;
    let proxy_url = format!("http://{proxy}");
    let out = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "proxy_child", "--nocapture"])
        .env("TACHYON_PROXY_CHILD", format!("http://{target}/a.txt"))
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("HTTPS_PROXY", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"));
    assert_eq!(hits.load(Ordering::Relaxed), 0, "the proxy was used");
}

/// Run by `proxy_variables_are_ignored`; does nothing on its own.
#[tokio::test]
async fn proxy_child() {
    let Ok(url) = std::env::var("TACHYON_PROXY_CHILD") else {
        return;
    };
    let spool = fetch(&url, &open()).await.unwrap();
    assert_eq!(spool.head, b"direct");
}
