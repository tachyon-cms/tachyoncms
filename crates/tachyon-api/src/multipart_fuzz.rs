//! Seeded fuzz for multipart bodies: random boundaries, truncated parts, huge headers,
//! repeated `file` parts, NUL in names and over-cap chunks through `UploadBody::read`. It
//! never panics, refuses only with what a client caused (400 or 413), spools only into the
//! spool directory, within the limit, and leaves nothing there once a body is dropped.

use std::path::Path;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::uploads::{Incoming, UploadBody};

/// xorshift64: the same cases every run.
struct Rng(u64);

impl Rng {
    fn n(&mut self, m: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % m as u64) as usize
    }

    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.n(xs.len())]
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.n(256) as u8).collect()
    }
}

const LIMIT: u64 = 2_000;

const BOUNDARIES: &[&str] = &[
    "x",
    "boundary",
    "----WebKitFormBoundary7MA4YWxkTrZu0gW",
    "a b",
    "\"quoted\"",
    "--",
    "0123456789012345678901234567890123456789012345678901234567890123456789",
];

const NAMES: &[&str] = &[
    "file",
    "_payload",
    "other",
    "fi\0le",
    "",
    "FILE",
    "_payload\0",
];

const FILENAMES: &[&str] = &[
    "a.png",
    "../../etc/passwd",
    "a\0b.png",
    "",
    "C:\\Windows\\a.exe",
    ".hidden",
    "\u{202e}gnp.exe",
];

const PAYLOADS: &[&str] = &[
    "{}",
    "{\"alt\":\"x\"}",
    "{nope",
    "[1,2]",
    "null",
    "\"\\u0000\"",
];

/// What a body that reads at all must give back: the first `file` part's bytes, and the
/// first `_payload` part's JSON. Known only when the parts use the boundary the header
/// names, and that boundary can't be mistaken for a longer one.
#[derive(Default)]
struct Expected {
    known: bool,
    file: Option<Vec<u8>>,
    payload: Option<Value>,
}

/// One random body, the content type it's sent with, and what reading it must give.
fn body(r: &mut Rng) -> (String, Vec<u8>, Expected) {
    let mut expected = Expected::default();
    let boundary = r.pick(BOUNDARIES).to_owned();
    // The boundary the parts use is sometimes not the one the header names.
    let used = if r.n(10) == 0 {
        r.pick(BOUNDARIES).to_owned()
    } else {
        boundary.clone()
    };
    let kind = match r.n(12) {
        0 => "application/json".to_owned(),
        1 => "multipart/form-data".to_owned(),
        2 => format!("multipart/form-data; boundary=\"{boundary}\""),
        _ => format!("multipart/form-data; boundary={boundary}"),
    };
    expected.known = used == boundary
        && boundary
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !boundary.starts_with('-');
    let mut out = Vec::new();
    if r.n(8) == 0 {
        let len = r.n(64);
        out.extend(r.bytes(len));
    }
    for _ in 0..r.n(5) {
        out.extend_from_slice(format!("--{used}\r\n").as_bytes());
        let name = match r.n(3) {
            0 => r.pick(NAMES),
            _ => r.pick(&["file", "_payload"]),
        };
        let mut disposition = format!("Content-Disposition: form-data; name=\"{name}\"");
        if r.n(2) == 0 {
            disposition.push_str(&format!("; filename=\"{}\"", r.pick(FILENAMES)));
        }
        let unnamed = r.n(6) == 0;
        if unnamed {
            disposition = "Content-Disposition: attachment".into();
        }
        out.extend_from_slice(disposition.as_bytes());
        out.extend_from_slice(b"\r\n");
        // Huge headers: one long line, or more headers than the parser keeps.
        match r.n(15) {
            0 => {
                let len = r.n(100_000);
                out.extend_from_slice(format!("X-Long: {}\r\n", "a".repeat(len)).as_bytes());
            }
            1 => {
                for i in 0..40 {
                    out.extend_from_slice(format!("X-{i}: v\r\n").as_bytes());
                }
            }
            _ => {}
        }
        out.extend_from_slice(b"\r\n");
        let content = match (name, r.n(4)) {
            ("_payload", 0..=2) => r.pick(PAYLOADS).as_bytes().to_vec(),
            // Over the cap now and then.
            _ => {
                let len = r.n(LIMIT as usize + 1_200);
                r.bytes(len)
            }
        };
        match name {
            _ if unnamed => {}
            "file" if expected.file.is_none() => expected.file = Some(content.clone()),
            "_payload" if expected.payload.is_none() => {
                expected.payload = serde_json::from_slice(&content).ok();
            }
            _ => {}
        }
        out.extend_from_slice(&content);
        out.extend_from_slice(b"\r\n");
    }
    if r.n(4) != 0 {
        out.extend_from_slice(format!("--{used}--\r\n").as_bytes());
    }
    // Truncated somewhere.
    if r.n(5) == 0 && !out.is_empty() {
        let at = r.n(out.len());
        out.truncate(at);
    }
    (kind, out, expected)
}

/// `bytes` as a body in random chunks, the stream sometimes failing partway.
fn chunked(r: &mut Rng, bytes: Vec<u8>) -> Body {
    let mut chunks: Vec<Result<Vec<u8>, std::io::Error>> = Vec::new();
    let mut rest = &bytes[..];
    while !rest.is_empty() {
        let n = (r.n(4_096) + 1).min(rest.len());
        chunks.push(Ok(rest[..n].to_vec()));
        rest = &rest[n..];
    }
    if r.n(10) == 0 {
        let at = r.n(chunks.len() + 1);
        chunks.insert(at, Err(std::io::Error::other("disconnected")));
    }
    Body::from_stream(futures_util::stream::iter(chunks))
}

fn is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).unwrap().next().is_none()
}

#[tokio::test]
async fn multipart_fuzz() {
    let temp =
        std::env::temp_dir().join(format!("tachyon-multipart-fuzz-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp).unwrap();
    let mut r = Rng(0x7AC4_1E55_0B0D_1E5D);
    let (mut json, mut files, mut bad, mut big) = (0, 0, 0, 0);
    for case in 0..5_000 {
        let (kind, bytes, expected) = body(&mut r);
        let multipart = kind.starts_with("multipart");
        let req = Request::builder()
            .method("POST")
            .uri("/api/files")
            .header("content-type", &kind)
            .body(chunked(&mut r, bytes))
            .unwrap();
        let payload = || expected.payload.clone().unwrap_or_else(|| json!({}));
        match UploadBody::read(req, Some(LIMIT), &temp).await {
            Ok(UploadBody::Json(v)) => {
                json += 1;
                if multipart && expected.known {
                    assert!(expected.file.is_none(), "case {case}: its file was dropped");
                    assert_eq!(v, payload(), "case {case}");
                }
            }
            Ok(UploadBody::Upload {
                file: Incoming::Url(..),
                ..
            }) => panic!("case {case}: a URL nobody sent"),
            Ok(UploadBody::Upload {
                payload: got,
                file: Incoming::Spooled(s),
            }) => {
                files += 1;
                assert_eq!(s.file.path().parent(), Some(temp.as_path()), "case {case}");
                let written = std::fs::read(s.file.path()).unwrap();
                if expected.known {
                    assert_eq!(Some(&written), expected.file.as_ref(), "case {case}");
                    assert_eq!(got, payload(), "case {case}");
                }
                assert_eq!(written.len() as u64, s.len, "case {case}");
                assert_eq!(s.head, written[..written.len().min(8192)], "case {case}");
                assert!(!s.name.is_empty() && !s.name.contains(['/', '\\', '\0']));
            }
            Err(e) => {
                let (status, _) = e.problem();
                match status {
                    StatusCode::BAD_REQUEST => bad += 1,
                    StatusCode::PAYLOAD_TOO_LARGE => big += 1,
                    _ => panic!("case {case}: {status}"),
                }
            }
        }
        assert!(is_empty(&temp), "case {case} left a spool file");
    }
    // Every outcome is reached often enough to mean something.
    for (what, n) in [("json", json), ("file", files), ("400", bad), ("413", big)] {
        assert!(n >= 50, "only {n} cases came out {what}");
    }
    std::fs::remove_dir(&temp).unwrap();
}

#[tokio::test]
async fn a_cancelled_read_leaves_no_spool_file() {
    let temp =
        std::env::temp_dir().join(format!("tachyon-multipart-cancel-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp).unwrap();
    let head = "--x\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\n";
    let mut first = head.as_bytes().to_vec();
    first.extend_from_slice(&[b'a'; 1_000]);
    // Half a file, then nothing more, ever: the client went quiet.
    let stream = futures_util::stream::iter([Ok::<_, std::io::Error>(first)])
        .chain(futures_util::stream::pending());
    let req = Request::builder()
        .method("POST")
        .uri("/api/files")
        .header("content-type", "multipart/form-data; boundary=x")
        .body(Body::from_stream(stream))
        .unwrap();
    let mut read = Box::pin(UploadBody::read(req, Some(LIMIT), &temp));
    let late = tokio::time::timeout(std::time::Duration::from_millis(50), &mut read).await;
    assert!(late.is_err(), "still waiting");
    assert!(!is_empty(&temp), "it was spooling");
    drop(read);
    assert!(is_empty(&temp), "the cancelled read left its spool file");
    std::fs::remove_dir(&temp).unwrap();
}
