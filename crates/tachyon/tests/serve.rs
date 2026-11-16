//! `tachyon serve` as a process: what it refuses before it touches the database.

use std::process::Command;

/// More workers than the pool can hold is refused from the config alone, before any
/// connection or migration: the database here doesn't exist, and it's never asked.
#[test]
fn too_many_workers_for_the_pool_refuse_before_connecting() {
    let out = Command::new(env!("CARGO_BIN_EXE_tachyon"))
        .arg("serve")
        // No tachyon.toml here, so only these settings count.
        .current_dir(std::env::temp_dir())
        .env(
            "TACHYON_DATABASE_URL",
            "postgres://tachyon:tachyon@127.0.0.1:1/nowhere",
        )
        .env("TACHYON_DATABASE_POOL", "10")
        .env("TACHYON_JOBS__WORKERS", "5")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("jobs.workers is 5, but the database pool has 10 connections"),
        "{stderr}"
    );
    assert!(!stderr.contains("couldn't connect"), "{stderr}");
}
