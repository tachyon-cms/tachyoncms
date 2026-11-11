//! Fetching a file from a URL the caller names (`pasteURL`) into a spool file, without
//! letting that URL reach the server's own network.
//!
//! Each hop is checked before it's sent: `http` or `https`, no user name or password, a
//! host, on the collection's `allowList`. Its host is then resolved once, through a resolver
//! that refuses a name with any non-public address, and the connection goes to the addresses
//! that resolver returned, so a name can't answer differently once checked. An address
//! written into the URL is checked the same way. Redirects are followed by hand, at most
//! `MAX_REDIRECTS`, each one a new hop. Proxies from the environment are ignored, bodies
//! aren't decompressed, and the whole fetch is bounded by one timeout.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::LOCATION;
use reqwest::redirect::Policy;
use reqwest::{Client, StatusCode};
use tachyon_core::net::is_public_addr;
use tachyon_core::uploads::{PasteUrl, SkipSafeFetch, Upload, UrlPattern, sanitize_filename};
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use url::{Host, Url};

/// How much of a file is kept in memory for sniffing.
pub const HEAD: usize = 8 * 1024;
/// The most redirects a fetch follows.
pub const MAX_REDIRECTS: usize = 5;
/// What a target on a non-public address is refused with. It never names the address.
pub const PRIVATE: &str = "this URL points at a private address";

/// A file written to a temporary file that's deleted when this is dropped.
pub struct Spool {
    pub file: NamedTempFile,
    pub len: u64,
    /// The first `HEAD` bytes, for sniffing.
    pub head: Vec<u8>,
    /// The name it came under, sanitised.
    pub name: String,
}

/// Why a fetch failed. None of them carry the resolved address or what the server sent.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The URL, or a redirect, isn't one this fetch may follow.
    #[error("{0}")]
    Refused(String),
    /// The file is over this many bytes.
    #[error("the file is over this collection's limit of {0} bytes")]
    TooLarge(u64),
    #[error("the server answered {0}")]
    Status(u16),
    #[error("the server didn't send the file in time")]
    Timeout,
    /// The connection couldn't be made, or broke.
    #[error("couldn't fetch this URL")]
    Unreachable,
    /// The spool file couldn't be written.
    #[error("can't spool a fetched file: {0}")]
    Io(String),
}

/// A client for fetches made for someone else; see the module docs.
#[derive(Clone)]
pub struct SafeFetch {
    /// Resolves through `Guard`.
    guarded: Client,
    /// For URLs `skipSafeFetch` names.
    open: Client,
    timeout: Duration,
}

impl SafeFetch {
    /// With the system's resolver, each fetch bounded by `timeout`.
    pub fn new(timeout: Duration) -> Result<Self, String> {
        Self::with_resolver(timeout, Arc::new(System))
    }

    /// With `resolver` in place of the system's, which tests use to give names addresses.
    pub fn with_resolver(timeout: Duration, resolver: Arc<dyn Resolve>) -> Result<Self, String> {
        let client = |r: Arc<dyn Resolve>| {
            Client::builder()
                .no_proxy()
                .redirect(Policy::none())
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .dns_resolver(r)
                .build()
                .map_err(|e| format!("can't set up the URL fetch client: {e}"))
        };
        Ok(Self {
            guarded: client(Arc::new(Guard(resolver.clone())))?,
            open: client(resolver)?,
            timeout,
        })
    }

    /// Fetches `url` for upload collection `u` into a new spool file in `dir`, named by
    /// `url`'s last path segment. Refused one byte past `cap`. On any failure the spool file
    /// is gone.
    pub async fn to_spool(
        &self,
        url: &Url,
        u: &Upload,
        cap: u64,
        dir: &Path,
    ) -> Result<Spool, FetchError> {
        // Dropped on timeout, the spool file with it.
        tokio::time::timeout(self.timeout, self.fetch(url, u, cap, dir))
            .await
            .map_err(|_| FetchError::Timeout)?
    }

    async fn fetch(
        &self,
        url: &Url,
        u: &Upload,
        cap: u64,
        dir: &Path,
    ) -> Result<Spool, FetchError> {
        let (mut at, mut hops) = (url.clone(), 0);
        let mut res = loop {
            let client = if guarded(&at, u)? {
                &self.guarded
            } else {
                &self.open
            };
            let res = client.get(at.clone()).send().await.map_err(failed)?;
            if !matches!(res.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                break res;
            }
            if hops == MAX_REDIRECTS {
                return Err(FetchError::Refused(format!(
                    "this URL redirects more than {MAX_REDIRECTS} times"
                )));
            }
            hops += 1;
            at = res
                .headers()
                .get(LOCATION)
                .and_then(|l| l.to_str().ok())
                .and_then(|l| at.join(l).ok())
                .ok_or_else(|| {
                    FetchError::Refused("this URL redirects without saying where".into())
                })?;
        };
        if res.status() != StatusCode::OK {
            return Err(FetchError::Status(res.status().as_u16()));
        }
        if res.content_length().is_some_and(|n| n > cap) {
            return Err(FetchError::TooLarge(cap));
        }
        let io = |e: std::io::Error| FetchError::Io(e.to_string());
        let file = NamedTempFile::new_in(dir).map_err(io)?;
        let mut out = tokio::fs::File::from_std(file.as_file().try_clone().map_err(io)?);
        let (mut head, mut len) = (Vec::with_capacity(HEAD), 0u64);
        // On any error `file` is dropped, which deletes it.
        while let Some(chunk) = res.chunk().await.map_err(failed)? {
            len += chunk.len() as u64;
            if len > cap {
                return Err(FetchError::TooLarge(cap));
            }
            let room = HEAD - head.len();
            head.extend_from_slice(&chunk[..room.min(chunk.len())]);
            out.write_all(&chunk).await.map_err(io)?;
        }
        out.flush().await.map_err(io)?;
        Ok(Spool {
            file,
            len,
            head,
            name: filename_of(url),
        })
    }
}

/// Whether `url` may be fetched for `u`, and whether through the guard: `false` when
/// `skipSafeFetch` names it.
fn guarded(url: &Url, u: &Upload) -> Result<bool, FetchError> {
    let refused = |m: &str| Err(FetchError::Refused(m.into()));
    if !matches!(url.scheme(), "http" | "https") {
        return refused("only http and https URLs can be fetched");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return refused("a URL with a user name or password can't be fetched");
    }
    let host = match url.host() {
        Some(Host::Domain("")) | None => return refused("this URL has no host"),
        Some(h) => h,
    };
    match &u.paste_url {
        PasteUrl::Off => return refused("this collection doesn't take URLs"),
        PasteUrl::AllowList(list) if !pattern_allows(list, url) => {
            return refused("this URL isn't on this collection's pasteURL allowList");
        }
        _ => {}
    }
    let skip = match &u.skip_safe_fetch {
        SkipSafeFetch::No => false,
        SkipSafeFetch::All => true,
        SkipSafeFetch::AllowList(list) => pattern_allows(list, url),
    };
    // A name is checked as it's resolved; an address written in the URL, here.
    let ip = match host {
        Host::Ipv4(ip) => Some(ip.into()),
        Host::Ipv6(ip) => Some(ip.into()),
        Host::Domain(_) => None,
    };
    if !skip && ip.is_some_and(|ip| !is_public_addr(ip)) {
        return refused(PRIVATE);
    }
    Ok(!skip)
}

/// What a failed request was: refused by the guard, too slow, or unreachable. Its own text,
/// which can name the address, isn't kept.
fn failed(e: reqwest::Error) -> FetchError {
    if e.is_timeout() {
        return FetchError::Timeout;
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(s) = source {
        if s.is::<PrivateAddress>() {
            return FetchError::Refused(PRIVATE.into());
        }
        source = s.source();
    }
    FetchError::Unreachable
}

/// Whether any of `patterns` matches `url`, as `allowList` and `skipSafeFetch` entries do.
/// `protocol` is `https` when unset; `hostname` takes a leading `*.` for one label or `**.`
/// for any number; `pathname` takes `*` within a segment, `**` across them, and a trailing
/// `/` that may be left off; `port` and `search` match exactly. An empty value is unset.
pub fn pattern_allows(patterns: &[UrlPattern], url: &Url) -> bool {
    let set = |v: &Option<String>| v.clone().filter(|v| !v.is_empty());
    patterns.iter().any(|p| {
        let protocol = set(&p.protocol).unwrap_or_else(|| "https".into());
        let port = url.port().map(|p| p.to_string()).unwrap_or_default();
        let search = url.query().map(|q| format!("?{q}")).unwrap_or_default();
        protocol.trim_end_matches(':') == url.scheme()
            && host_matches(&p.hostname, url.host_str().unwrap_or_default())
            && set(&p.port).is_none_or(|want| want == port)
            && set(&p.search).is_none_or(|want| want == search)
            && set(&p.pathname).is_none_or(|want| path_matches(&want, url.path()))
    })
}

fn host_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    if let Some(base) = pattern.strip_prefix("**.") {
        host.strip_suffix(base)
            .and_then(|s| s.strip_suffix('.'))
            .is_some_and(|labels| !labels.is_empty())
    } else if let Some(base) = pattern.strip_prefix("*.") {
        host.strip_suffix(base)
            .and_then(|s| s.strip_suffix('.'))
            .is_some_and(|label| !label.is_empty() && !label.contains('.'))
    } else {
        pattern == host
    }
}

/// `path` against a `pathname` glob.
fn path_matches(pattern: &str, path: &str) -> bool {
    let (pattern, slash) = match pattern.strip_suffix('/') {
        Some(p) if !p.is_empty() => (p, true),
        _ => (pattern, false),
    };
    let path = if slash {
        path.strip_suffix('/').unwrap_or(path)
    } else {
        path
    };
    glob(pattern.as_bytes(), path.as_bytes())
}

/// `*` is anything but `/`, `**` anything.
fn glob(p: &[u8], s: &[u8]) -> bool {
    match p {
        [] => s.is_empty(),
        [b'*', b'*', rest @ ..] => (0..=s.len()).any(|i| glob(rest, &s[i..])),
        [b'*', rest @ ..] => (0..=s.len())
            .take_while(|i| *i == 0 || s[i - 1] != b'/')
            .any(|i| glob(rest, &s[i..])),
        [c, rest @ ..] => s.first() == Some(c) && glob(rest, &s[1..]),
    }
}

/// `url`'s last path segment, percent-decoded and sanitised.
fn filename_of(url: &Url) -> String {
    let last = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or_default();
    let b = last.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .filter(|h| h.iter().all(u8::is_ascii_hexdigit));
        match (b[i], hex) {
            (b'%', Some(h)) => {
                out.push(
                    u8::from_str_radix(std::str::from_utf8(h).unwrap_or("0"), 16).unwrap_or(0),
                );
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    sanitize_filename(&String::from_utf8_lossy(&out))
}

/// The guard's refusal, found again in the request's error.
#[derive(Debug, thiserror::Error)]
#[error("{PRIVATE}")]
struct PrivateAddress;

/// Resolves through another resolver, refusing a name with any non-public address.
struct Guard(Arc<dyn Resolve>);

impl Resolve for Guard {
    fn resolve(&self, name: Name) -> Resolving {
        let inner = self.0.resolve(name);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = inner.await?.collect();
            if addrs.iter().any(|a| !is_public_addr(a.ip())) {
                return Err(Box::new(PrivateAddress) as _);
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// The system's resolver.
struct System;

impl Resolve for System {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(json: serde_json::Value) -> Vec<UrlPattern> {
        vec![serde_json::from_value(json).unwrap()]
    }

    fn allows(p: serde_json::Value, url: &str) -> bool {
        pattern_allows(&pattern(p), &Url::parse(url).unwrap())
    }

    #[test]
    fn patterns_match_as_documented() {
        let host = serde_json::json!({"hostname": "example.com"});
        assert!(allows(host.clone(), "https://example.com/a.png"));
        assert!(
            !allows(host.clone(), "http://example.com/a.png"),
            "https unless set"
        );
        assert!(!allows(host.clone(), "https://a.example.com/"));
        assert!(
            allows(host, "https://example.com:8443/"),
            "any port unless set"
        );
        let one = serde_json::json!({"hostname": "*.example.com"});
        assert!(allows(one.clone(), "https://a.example.com/"));
        assert!(!allows(one.clone(), "https://a.b.example.com/"));
        assert!(!allows(one.clone(), "https://example.com/"));
        assert!(!allows(one, "https://aexample.com/"));
        let any = serde_json::json!({"hostname": "**.example.com"});
        assert!(allows(any.clone(), "https://a.b.example.com/"));
        assert!(!allows(any.clone(), "https://example.com/"));
        assert!(!allows(any, "https://evil-example.com/"));
        let full = serde_json::json!({
            "protocol": "http", "hostname": "127.0.0.1", "port": "8080",
            "pathname": "/images/*", "search": "?v=1"
        });
        assert!(allows(
            full.clone(),
            "http://127.0.0.1:8080/images/a.png?v=1"
        ));
        assert!(!allows(
            full.clone(),
            "http://127.0.0.1:8081/images/a.png?v=1"
        ));
        assert!(!allows(
            full.clone(),
            "http://127.0.0.1:8080/images/a/b.png?v=1"
        ));
        assert!(!allows(full.clone(), "http://127.0.0.1:8080/images/a.png"));
        assert!(!allows(full, "http://127.0.0.1:8080/other/a.png?v=1"));
        let deep = serde_json::json!({"hostname": "x.io", "pathname": "/a/**"});
        assert!(allows(deep.clone(), "https://x.io/a/b/c.png"));
        assert!(!allows(deep, "https://x.io/b/c.png"));
        let slash = serde_json::json!({"hostname": "x.io", "pathname": "/a/"});
        assert!(allows(slash.clone(), "https://x.io/a"));
        assert!(allows(slash, "https://x.io/a/"));
        let empty = serde_json::json!({"hostname": "x.io", "port": "", "pathname": ""});
        assert!(allows(empty, "https://x.io:9/any"));
    }

    #[test]
    fn filenames_come_from_the_last_segment() {
        let name = |u: &str| filename_of(&Url::parse(u).unwrap());
        assert_eq!(name("https://x.io/a/my%20cat.png?x=1"), "my cat.png");
        assert_eq!(name("https://x.io/a/..%2F..%2Fetc%2Fpasswd"), "passwd");
        assert_eq!(name("https://x.io/"), "file");
        assert_eq!(name("https://x.io/%zz%2"), "%zz%2");
    }

    #[test]
    fn hops_are_checked_before_they_are_sent() {
        let u = Upload::default();
        let refusal = |url: &str| match guarded(&Url::parse(url).unwrap(), &u) {
            Err(FetchError::Refused(m)) => m,
            other => panic!("{url}: {other:?}"),
        };
        assert!(refusal("ftp://example.com/a").contains("http and https"));
        assert!(refusal("file:///etc/passwd").contains("http and https"));
        assert!(refusal("https://user:pw@example.com/").contains("user name"));
        assert!(refusal("https://user@example.com/").contains("user name"));
        for private in [
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://169.254.169.254/",
            "http://2130706433/",
            "http://0x7f.1/",
            "http://[::ffff:10.0.0.1]/",
            "http://0/",
        ] {
            assert_eq!(refusal(private), PRIVATE, "{private}");
        }
        assert!(matches!(
            guarded(&Url::parse("https://example.com/").unwrap(), &u),
            Ok(true)
        ));
        let skip = Upload {
            skip_safe_fetch: SkipSafeFetch::All,
            ..Upload::default()
        };
        assert!(matches!(
            guarded(&Url::parse("http://127.0.0.1/").unwrap(), &skip),
            Ok(false)
        ));
    }
}
