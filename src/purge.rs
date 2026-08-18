//! Deleting received data once it is older than a retention period.
//!
//! A EUMETCast receiver writes continuously and never tidies up after itself,
//! so the receive directories grow without bound. This removes what has aged
//! out, and is deliberately conservative about it, because unlike the caches
//! these files cannot be rebuilt - once a slot is gone it is gone.
//!
//! Three rules keep it safe:
//!
//! - **Only files it recognises.** A name has to parse as a NWC SAF product or
//!   an HRIT segment before it is a candidate. Anything else in the directory -
//!   another service's output, a note to self, a subdirectory - is not touched
//!   and not even counted.
//! - **Age comes from the name, not the file.** The timestamp in the filename
//!   is when the satellite made the observation, which is the age that was
//!   meant. Modification time is when it happened to land on this disk, which a
//!   re-transmission, a copy or a backup restore all change.
//! - **Nothing recent, ever.** The retention floor is a day, so no setting can
//!   reach data the receiver may still be assembling.

use std::path::Path;

/// What a purge did, or would have done.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub files: usize,
    pub bytes: u64,
    /// Files that were recognised and old enough but could not be removed -
    /// most often because the receiver still had one open.
    pub failed: usize,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.files == 0 && self.failed == 0
    }

    fn add(&mut self, other: Report) {
        self.files += other.files;
        self.bytes += other.bytes;
        self.failed += other.failed;
    }

    pub fn megabytes(&self) -> u64 {
        self.bytes / (1024 * 1024)
    }
}

/// The shortest retention that may be configured, in days.
///
/// A window can be 48 hours, the products lag the imagery, and the receiver is
/// writing the newest slot as this runs. One day is comfortably clear of all
/// three, and it means no combination of flags can be turned into "delete
/// everything".
pub const MIN_RETAIN_DAYS: i64 = 1;

/// How a file's age is decided.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// NWC SAF products: `S_NWC_<product>_..._<time>Z.nc`.
    Products,
    /// HRIT segments, prologues and epilogues: `H-000-MSG...-<time>-C_`.
    Hrit,
    /// Rendered frames, named by content hash. Nothing in the name says when
    /// the picture is from, so the file's own age is all there is - which is
    /// fine, because these are rebuilt on demand.
    Rendered,
}

/// Delete recognised files older than `cutoff` (a Unix time).
///
/// `dry_run` reports what would go without removing anything.
pub fn purge_dir(dir: &Path, kind: Kind, cutoff: i64, dry_run: bool) -> Report {
    let mut report = Report::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return report;
    };

    for entry in entries.flatten() {
        let meta = match entry.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue, // subdirectories and anything unreadable are left alone
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };

        let Some(age_epoch) = file_epoch(name, kind, &meta) else {
            continue; // not ours to delete
        };
        if age_epoch >= cutoff {
            continue;
        }

        // A dry run counts what it would have removed, so the two paths agree
        // on the total by construction rather than by being kept in step.
        let removed = dry_run || std::fs::remove_file(entry.path()).is_ok();
        if removed {
            report.files += 1;
            report.bytes += meta.len();
        } else {
            // Usually the receiver holding it open. It will age out again on
            // the next pass, so this is worth counting but not worth failing.
            report.failed += 1;
        }
    }
    report
}

fn file_epoch(name: &str, kind: Kind, meta: &std::fs::Metadata) -> Option<i64> {
    match kind {
        Kind::Products => crate::catalog::product_epoch(name),
        Kind::Hrit => crate::hrit::segment_epoch(name),
        Kind::Rendered => {
            if !name.ends_with(".png") {
                return None;
            }
            meta.modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        }
    }
}

/// One pass over every directory the server knows about.
pub struct Targets<'a> {
    pub products: &'a Path,
    pub hrit: &'a Path,
    pub disc: &'a Path,
    pub hrit_cache: &'a Path,
    pub render_cache: &'a Path,
}

/// Purge every directory, returning what was received data and what was cache.
///
/// The two are reported apart because they are not the same kind of loss: a
/// cache entry costs a re-render, a received slot is unrecoverable.
pub fn purge_all(t: &Targets, retain_days: i64, now: i64, dry_run: bool) -> (Report, Report) {
    let days = retain_days.max(MIN_RETAIN_DAYS);
    let cutoff = now - days * 86400;

    let mut received = Report::default();
    received.add(purge_dir(t.products, Kind::Products, cutoff, dry_run));
    received.add(purge_dir(t.hrit, Kind::Hrit, cutoff, dry_run));
    // The two HRIT services may be configured to the same directory.
    if t.disc != t.hrit {
        received.add(purge_dir(t.disc, Kind::Hrit, cutoff, dry_run));
    }

    let mut cached = Report::default();
    // Decompressed segments carry the same names as the originals, so they age
    // on observation time too rather than on when they were expanded.
    cached.add(purge_dir(t.hrit_cache, Kind::Hrit, cutoff, dry_run));
    cached.add(purge_dir(t.render_cache, Kind::Rendered, cutoff, dry_run));

    (received, cached)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "eumet-purge-test-{tag}-{}",
            crate::diskcache::unique()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    /// 2026-08-18 00:00:00Z, matching the stamps used below.
    const NOW: i64 = 1786665600 + 86400 * 5;

    #[test]
    fn old_products_go_and_recent_ones_stay() {
        let d = tmpdir("products");
        write(&d, "S_NWC_CT_MSG3_MSG-N-VISIR_20260801T120000Z.nc"); // old
        write(&d, "S_NWC_CTTH_MSG3_MSG-N-VISIR_20260801T121500Z.nc"); // old
        write(&d, "S_NWC_CT_MSG3_MSG-N-VISIR_20260817T120000Z.nc"); // recent

        let r = purge_dir(&d, Kind::Products, NOW - 10 * 86400, false);
        assert_eq!(r.files, 2);
        assert!(d
            .join("S_NWC_CT_MSG3_MSG-N-VISIR_20260817T120000Z.nc")
            .exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn old_hrit_segments_go_and_recent_ones_stay() {
        let d = tmpdir("hrit");
        write(
            &d,
            "H-000-MSG4__-MSG4_RSS____-VIS006___-000006___-202608011420-C_",
        );
        write(
            &d,
            "H-000-MSG4__-MSG4_RSS____-PRO______-000006___-202608011420-__",
        );
        write(
            &d,
            "H-000-MSG4__-MSG4_RSS____-VIS006___-000006___-202608171420-C_",
        );

        let r = purge_dir(&d, Kind::Hrit, NOW - 10 * 86400, false);
        assert_eq!(r.files, 2, "the prologue ages with its slot");
        assert!(d
            .join("H-000-MSG4__-MSG4_RSS____-VIS006___-000006___-202608171420-C_")
            .exists());
        std::fs::remove_dir_all(&d).ok();
    }

    /// The rule that matters most: this runs over directories the user did not
    /// give us exclusively, so anything unrecognised has to survive.
    #[test]
    fn unrecognised_files_are_never_touched() {
        let d = tmpdir("strangers");
        for name in [
            "notes.txt",
            "important.db",
            "S_NWC_CT_nonsense.nc",
            "H-000-truncated",
            ".hidden",
            "20260801T120000Z.nc",
        ] {
            write(&d, name);
        }
        std::fs::create_dir_all(d.join("subdir")).unwrap();
        write(
            &d.join("subdir"),
            "S_NWC_CT_MSG3_MSG-N-VISIR_20260101T120000Z.nc",
        );

        for kind in [Kind::Products, Kind::Hrit, Kind::Rendered] {
            let r = purge_dir(&d, kind, NOW, false);
            assert_eq!(r.files, 0, "nothing here should have been recognised");
        }
        assert_eq!(std::fs::read_dir(&d).unwrap().flatten().count(), 7);
        assert!(
            d.join("subdir")
                .join("S_NWC_CT_MSG3_MSG-N-VISIR_20260101T120000Z.nc")
                .exists(),
            "subdirectories are not searched"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_dry_run_deletes_nothing() {
        let d = tmpdir("dry");
        write(&d, "S_NWC_CT_MSG3_MSG-N-VISIR_20260801T120000Z.nc");
        let r = purge_dir(&d, Kind::Products, NOW - 10 * 86400, true);
        assert_eq!(r.files, 1);
        assert_eq!(std::fs::read_dir(&d).unwrap().flatten().count(), 1);
        std::fs::remove_dir_all(&d).ok();
    }

    /// No configuration may reach data the receiver could still be writing.
    #[test]
    fn retention_has_a_floor() {
        let d = tmpdir("floor");
        // Six hours old: inside every window the viewer offers.
        let recent = NOW - 6 * 3600;
        let (y, mo, dd, h, mi) = crate::catalog::civil_from_epoch(recent);
        let name = format!("S_NWC_CT_MSG3_MSG-N-VISIR_{y:04}{mo:02}{dd:02}T{h:02}{mi:02}00Z.nc");
        write(&d, &name);

        let t = Targets {
            products: &d,
            hrit: &d,
            disc: &d,
            hrit_cache: &d,
            render_cache: &d,
        };
        for asked in [-100, 0, 1] {
            let (received, _) = purge_all(&t, asked, NOW, true);
            assert_eq!(received.files, 0, "retention {asked} reached recent data");
        }
        assert!(d.join(&name).exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_shared_directory_is_only_swept_once() {
        let d = tmpdir("shared");
        write(
            &d,
            "H-000-MSG4__-MSG4_RSS____-VIS006___-000006___-202608011420-C_",
        );
        let t = Targets {
            products: &d,
            hrit: &d,
            disc: &d, // same directory for both services
            hrit_cache: &d,
            render_cache: &d,
        };
        let (received, _) = purge_all(&t, 10, NOW, true);
        assert_eq!(received.files, 1, "counted once, not twice");
        std::fs::remove_dir_all(&d).ok();
    }
}
