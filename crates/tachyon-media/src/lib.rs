pub mod fetch;
#[doc(hidden)]
pub mod fixtures;
pub mod image;
pub mod sniff;
pub mod storage;
pub mod svg;
#[cfg(test)]
mod svg_fuzz;

use std::path::Path;
use std::time::Duration;

/// What every spooled upload and decode output in `temp_dir` is named from, so a sweep
/// after a crash takes only those.
pub const SPOOL_PREFIX: &str = ".tachyon-spool-";

/// A new file in `dir` for a spool, deleted when dropped.
pub fn spool_in(dir: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    tempfile::Builder::new()
        .prefix(SPOOL_PREFIX)
        .tempfile_in(dir)
}

/// Deletes the spools in `dir` last written more than `age` ago: what a process killed
/// mid-upload left, since a live one is younger than its request's time limit. Other files,
/// and directories, stay. How many went.
pub fn sweep_spools(dir: &Path, age: Duration) -> std::io::Result<usize> {
    let mut gone = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let old = meta.modified()?.elapsed().is_ok_and(|e| e > age);
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(SPOOL_PREFIX));
        if ours && old && meta.is_file() && std::fs::remove_file(entry.path()).is_ok() {
            gone += 1;
        }
    }
    Ok(gone)
}

#[cfg(test)]
mod spool_tests {
    use std::time::{Duration, SystemTime};

    #[test]
    fn a_sweep_takes_only_old_spools() {
        let d = tempfile::Builder::new()
            .prefix("tachyon-sweep-")
            .tempdir()
            .unwrap();
        let aged = |name: &str, ago: u64| {
            let p = d.path().join(name);
            let f = std::fs::File::create(&p).unwrap();
            f.set_modified(SystemTime::now() - Duration::from_secs(ago))
                .unwrap();
            p
        };
        let (_young, old) = (
            crate::spool_in(d.path()).unwrap(),
            crate::spool_in(d.path()).unwrap(),
        );
        let old = old.keep().unwrap().1;
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(7_200))
            .unwrap();
        let foreign = aged("someone-elses.tmp", 7_200);
        std::fs::create_dir(d.path().join(format!("{}dir", crate::SPOOL_PREFIX))).unwrap();
        assert_eq!(
            crate::sweep_spools(d.path(), Duration::from_secs(3_600)).unwrap(),
            1
        );
        assert!(!old.exists());
        assert!(foreign.exists(), "not ours");
        let left = std::fs::read_dir(d.path()).unwrap().count();
        assert_eq!(
            left, 3,
            "the young spool, the other file and the directory stay"
        );
    }
}
