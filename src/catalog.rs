//! Index of the NWC SAF products sitting in the EUMETCast receive directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct Frame {
    pub path: PathBuf,
    /// Product short name, e.g. `CT` or `CTTH`.
    pub product: String,
    /// Nominal slot time, seconds since the Unix epoch.
    pub epoch: i64,
}

impl Frame {
    /// `2026-08-17 12:15Z`, for display.
    pub fn label(&self) -> String {
        let (y, mo, d, h, mi) = civil_from_epoch(self.epoch);
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}Z")
    }

    pub fn iso(&self) -> String {
        let (y, mo, d, h, mi) = civil_from_epoch(self.epoch);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:00Z")
    }
}

#[derive(Debug, Default)]
pub struct Catalog {
    pub frames: Vec<Frame>,
}

impl Catalog {
    /// Scan a directory for `S_NWC_<product>_<sat>_<region>_<time>Z.nc` files.
    pub fn scan(dir: &Path) -> Catalog {
        let mut frames = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Catalog::default(),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("nc") {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if let Some(f) = parse_name(name) {
                frames.push(Frame {
                    path: path.clone(),
                    product: f.0,
                    epoch: f.1,
                });
            }
        }
        frames.sort_by_key(|f| f.epoch);
        Catalog { frames }
    }

    pub fn products(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for f in &self.frames {
            if !seen.contains(&f.product) {
                seen.push(f.product.clone());
            }
        }
        seen.sort();
        seen
    }

    /// Frames of one product within the last `hours`, oldest first.
    ///
    /// The window is measured back from the newest frame held rather than from
    /// the wall clock, so the view still fills if reception has paused.
    pub fn window(&self, product: &str, hours: i64) -> Vec<&Frame> {
        let newest = match self
            .frames
            .iter()
            .filter(|f| f.product == product)
            .map(|f| f.epoch)
            .max()
        {
            Some(t) => t,
            None => return Vec::new(),
        };
        let cutoff = newest - hours * 3600;
        self.frames
            .iter()
            .filter(|f| f.product == product && f.epoch >= cutoff)
            .collect()
    }

    pub fn counts(&self) -> HashMap<String, usize> {
        let mut m = HashMap::new();
        for f in &self.frames {
            *m.entry(f.product.clone()).or_insert(0) += 1;
        }
        m
    }
}

/// Acquisition time of a NWC SAF product file, from its name.
///
/// Names that do not match are not products, and callers must leave them
/// alone rather than guess at their age.
pub fn product_epoch(name: &str) -> Option<i64> {
    parse_name(name).map(|(_, epoch)| epoch)
}

/// `S_NWC_CT_MSG3_MSG-N-VISIR_20260817T120000Z.nc` -> ("CT", epoch)
fn parse_name(name: &str) -> Option<(String, i64)> {
    let stem = name.strip_suffix(".nc")?;
    let rest = stem.strip_prefix("S_NWC_")?;
    let parts: Vec<&str> = rest.split('_').collect();
    if parts.len() < 4 {
        return None;
    }
    let product = parts[0].to_string();
    let stamp = parts[parts.len() - 1];
    let epoch = parse_stamp(stamp)?;
    Some((product, epoch))
}

/// `20260817T120000Z` -> seconds since the epoch.
fn parse_stamp(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (date, time) = s.split_once('T')?;
    if date.len() != 8 || time.len() < 6 {
        return None;
    }
    let y: i64 = date[0..4].parse().ok()?;
    let mo: u32 = date[4..6].parse().ok()?;
    let d: u32 = date[6..8].parse().ok()?;
    let h: i64 = time[0..2].parse().ok()?;
    let mi: i64 = time[2..4].parse().ok()?;
    let se: i64 = time[4..6].parse().ok()?;
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se)
}

/// Days since 1970-01-01 (Howard Hinnant's civil calendar algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of `days_from_civil`, returning `(year, month, day, hour, minute)`.
pub fn civil_from_epoch(epoch: i64) -> (i64, u32, u32, u32, u32) {
    let days = epoch.div_euclid(86400);
    let secs = epoch.rem_euclid(86400);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = y + if m <= 2 { 1 } else { 0 };
    (y, m, d, (secs / 3600) as u32, ((secs % 3600) / 60) as u32)
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_product_filename() {
        let (p, e) = parse_name("S_NWC_CT_MSG3_MSG-N-VISIR_20260817T120000Z.nc").unwrap();
        assert_eq!(p, "CT");
        assert_eq!(civil_from_epoch(e), (2026, 8, 17, 12, 0));
    }

    #[test]
    fn epoch_round_trips() {
        for t in [0i64, 1_000_000_000, 1_780_000_000] {
            let (y, m, d, h, mi) = civil_from_epoch(t);
            let back = days_from_civil(y, m, d) * 86400 + h as i64 * 3600 + mi as i64 * 60;
            assert_eq!(back, t - t.rem_euclid(60));
        }
    }

    #[test]
    fn ignores_unrelated_files() {
        assert!(parse_name("W_XX-EUMETSAT-Darmstadt,SING+LEV+SAT.bin").is_none());
    }
}
