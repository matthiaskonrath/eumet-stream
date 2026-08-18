//! Keeping the on-disk caches from growing without bound.
//!
//! Two directories accumulate: decompressed HRIT segments and rendered PNGs.
//! Both are pure caches - anything deleted is simply rebuilt - so the policy is
//! a byte ceiling with oldest-first eviction.
//!
//! Eviction is by modification time, which here is creation time: nothing
//! rewrites these files. That makes it first-in-first-out rather than
//! least-recently-used. Read times would be a better signal, but Windows
//! disables last-access updates by default, so they cannot be relied on. In
//! practice the two orders agree: the oldest frames are also the ones that have
//! scrolled out of every window anyone is watching.

use std::path::{Path, PathBuf};

/// Delete oldest-first until the directory fits in `max_bytes`.
///
/// Returns the number of bytes removed, which is zero when the directory was
/// already inside its budget - the common case, and one that costs a single
/// directory listing.
pub fn prune_dir(dir: &Path, max_bytes: u64) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            if !m.is_file() {
                return None;
            }
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();

    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= max_bytes {
        return 0;
    }
    let before = total;
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
    before - total
}

/// Write a file so that no reader ever sees a partial one.
///
/// Both caches are read back by checking that a file exists and is non-empty,
/// which a half-written file also satisfies. A process killed mid-write, a full
/// disk, or two requests racing on the same key would otherwise leave a
/// truncated file that is then trusted for as long as it survives. Writing to a
/// unique temporary name and renaming into place makes the file appear whole or
/// not at all; on both Windows and Unix the rename replaces any existing entry
/// atomically.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("frame"),
        unique()
    ));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// A value no other write in this process will reuse. The process id keeps it
/// distinct from any other instance sharing the same cache directory.
pub fn unique() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("eumet-diskcache-test-{tag}-{}", unique()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn prunes_oldest_first() {
        let d = tmpdir("prune");
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            let p = d.join(name);
            std::fs::write(&p, vec![0u8; 1000]).unwrap();
            // Space the modification times so the ordering is unambiguous.
            let t = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_700_000_000 + i as u64 * 60);
            filetime_set(&p, t);
        }
        let freed = prune_dir(&d, 2000);
        assert!(freed >= 1000, "expected at least one file removed");
        assert!(!d.join("a").exists(), "the oldest file should go first");
        assert!(d.join("c").exists(), "the newest file should survive");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn leaves_a_directory_inside_its_budget_alone() {
        let d = tmpdir("budget");
        std::fs::write(d.join("a"), vec![0u8; 100]).unwrap();
        assert_eq!(prune_dir(&d, 10_000), 0);
        assert!(d.join("a").exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temporary() {
        let d = tmpdir("atomic");
        let p = d.join("frame.png");
        write_atomic(&p, b"first").unwrap();
        write_atomic(&p, b"second").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
        let leftovers = std::fs::read_dir(&d).unwrap().flatten().count();
        assert_eq!(leftovers, 1, "the temporary file should not survive");
        std::fs::remove_dir_all(&d).ok();
    }

    /// Set a file's modification time without pulling in a crate for it.
    fn filetime_set(path: &Path, t: std::time::SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }
}
