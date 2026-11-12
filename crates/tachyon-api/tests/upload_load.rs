//! Many large uploads at once: eight bodies near the 25 MB limit and two images at the
//! 50-megapixel cap, all in flight together. The spool directory and the process's memory
//! stay under stated bounds. Its own test binary, so no other test shares the process
//! whose memory it measures.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::Request;
use common::{ADMIN, BOUNDARY, PASSWORD, png, send, send_body, signed_in};
use serde_json::json;
use sqlx::PgPool;
use tachyon_api::{Options, UploadsConfig};
use tachyon_media::fixtures;
use tachyon_media::storage::{StorageConfig, Storages};

const MB: u64 = 1_000_000;

/// The bytes in every file under `dir`.
fn bytes_in(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => bytes_in(&e.path()),
            _ => e.metadata().map_or(0, |m| m.len()),
        })
        .sum()
}

/// This process's resident memory in bytes, as `ps` reports it.
fn rss() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .expect("ps prints kilobytes")
        * 1024
}

/// `file` as a multipart body, shared rather than copied for each request.
fn body(file: &[u8], name: &str) -> Bytes {
    let mut out = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
         Content-Type: image/png\r\n\r\n"
    )
    .into_bytes();
    out.extend_from_slice(file);
    out.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    out.into()
}

#[sqlx::test(migrator = "tachyon_db::MIGRATOR")]
async fn many_large_uploads_stay_bounded(pool: PgPool) {
    let base = tempfile::Builder::new()
        .prefix("tachyon-load-")
        .tempdir()
        .unwrap();
    let (temp, root) = (base.path().join("spool"), base.path().join("store"));
    for dir in [&temp, &root] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let storages = Storages::open(&BTreeMap::from([(
        "default".into(),
        StorageConfig::local(&root),
    )]))
    .unwrap();
    let options = Options {
        uploads: UploadsConfig {
            temp_dir: temp.clone(),
            storages: Arc::new(storages),
            ..UploadsConfig::default()
        },
        ..Options::default()
    };
    let app = tachyon_api::router_with(pool.clone(), options)
        .await
        .unwrap();
    let setup = json!({"email": ADMIN, "name": "Ada", "password": PASSWORD});
    let res = send(&app, "POST", "/api/setup", Some(&setup.to_string())).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let app = signed_in(app, ADMIN).await;
    let schema = json!({"slug": "images", "upload": true, "fields": []});
    let res = send(
        &app,
        "POST",
        "/api/admin/collections",
        Some(&schema.to_string()),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);

    // 4990×5000 grey, stored uncompressed: just under the 25 MB limit.
    let large = body(&png(4990, 5000), "large.png");
    assert!(large.len() as u64 > 24 * MB && (large.len() as u64) < 25 * MB);
    // 8000×6250, the cap exactly, compressed to almost nothing.
    let grey = image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(
        8000,
        6250,
        image::Luma([90]),
    ));
    let huge = body(
        &fixtures::encoded(&grey, image::ImageFormat::Png),
        "huge.png",
    );
    drop(grey);
    let bodies: Vec<Bytes> = (0..8)
        .map(|_| large.clone())
        .chain((0..2).map(|_| huge.clone()))
        .collect();
    let sent: u64 = bodies.iter().map(|b| b.len() as u64).sum();

    let before = rss();
    let watching = Arc::new(AtomicBool::new(true));
    let watcher = {
        let (watching, temp) = (watching.clone(), temp.clone());
        tokio::spawn(async move {
            let (mut spool, mut memory) = (0, 0);
            while watching.load(Ordering::Relaxed) {
                spool = spool.max(bytes_in(&temp));
                memory = memory.max(tokio::task::spawn_blocking(rss).await.unwrap());
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            (spool, memory)
        })
    };
    let started = Instant::now();
    let uploads = bodies.into_iter().enumerate().map(|(i, bytes)| {
        let app = app.clone();
        async move {
            let req = Request::builder().method("POST").uri("/api/images").header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            );
            (i, send_body(&app, req, Body::from(bytes)).await)
        }
    });
    let results = futures_util::future::join_all(uploads).await;
    let took = started.elapsed();
    watching.store(false, Ordering::Relaxed);
    let (spool, memory) = watcher.await.unwrap();
    let grew = memory.saturating_sub(before);
    println!(
        "{} uploads, {} MB sent, in {:.1} s: spool peak {} MB, RSS {} MB before, peak {} MB \
         (+{} MB)",
        results.len(),
        sent / MB,
        took.as_secs_f64(),
        spool / MB,
        before / MB,
        memory / MB,
        grew / MB
    );
    for (i, res) in &results {
        assert_eq!(res.status, 201, "{i}: {}", res.json);
        let dims = if *i < 8 { (4990, 5000) } else { (8000, 6250) };
        assert_eq!(
            (&res.json["width"], &res.json["height"]),
            (&json!(dims.0), &json!(dims.1)),
            "{i}"
        );
    }
    assert_eq!(
        tachyon_api::decodes_peak(),
        2,
        "decodes wait on the permits"
    );
    // Every body is spooled once, plus at most a working file for each of the two decodes
    // allowed at once. (Measured: exactly what was sent, 200 MB.)
    let spool_bound = sent + 2 * 25 * MB;
    assert!(
        spool <= spool_bound,
        "spool peak {spool} bytes, bound {spool_bound}"
    );
    // Two decodes at a time, each at most 50 MP with its working copies, and the bodies
    // streamed to disk rather than held: under 640 MB above where it started, 2.5 times
    // the most measured (164 to 242 MB on an M-series Mac). Holding all eight bodies would
    // add about 200 MB more.
    assert!(grew < 640 * MB, "RSS grew {} MB", grew / MB);
    assert_eq!(bytes_in(&temp), 0, "the spool is empty after");
}
