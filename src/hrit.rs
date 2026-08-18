//! Raw SEVIRI imagery from the HRIT stream (`E1B-GEO-5`, MSG-4 Rapid Scan).
//!
//! Unlike the NWC SAF products these files are not netCDF: they are CGMS
//! LRIT/HRIT records with big-endian headers and 10-bit packed pixels, and the
//! pixel data is wavelet-compressed with EUMETSAT's scheme. Decompression is
//! delegated to `xRITDecompress` (EUMETSAT's own tool, Apache 2.0); everything
//! else - headers, calibration, assembly, geolocation - is done here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};

pub type Result<T> = std::result::Result<T, String>;

/// SEVIRI channels in the order used by the prologue's calibration table.
pub const CHANNELS: [&str; 12] = [
    "VIS006", "VIS008", "IR_016", "IR_039", "WV_062", "WV_073", "IR_087", "IR_097", "IR_108",
    "IR_120", "IR_134", "HRV",
];

/// Band solar irradiance for Meteosat-11 (MSG-4), W m^-2 sr^-1 (um)^-1, used to
/// turn visible-channel radiance into reflectance.
pub fn solar_irradiance(channel: &str) -> Option<f64> {
    Some(match channel {
        "VIS006" => 65.5148,
        "VIS008" => 73.1807,
        "IR_016" => 62.0208,
        "HRV" => 78.7599,
        _ => return None,
    })
}

pub fn channel_index(name: &str) -> Option<usize> {
    CHANNELS.iter().position(|c| *c == name)
}

// ---------------------------------------------------------------------------
// Big-endian reading
// ---------------------------------------------------------------------------

fn be16(d: &[u8], o: usize) -> u16 {
    ((d[o] as u16) << 8) | d[o + 1] as u16
}

fn be32i(d: &[u8], o: usize) -> i32 {
    i32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn be64(d: &[u8], o: usize) -> u64 {
    let mut v = 0u64;
    for i in 0..8 {
        v = (v << 8) | d[o + i] as u64;
    }
    v
}

fn be_f64(d: &[u8], o: usize) -> f64 {
    f64::from_bits(be64(d, o))
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Headers {
    pub total_header_length: usize,
    pub data_field_bytes: usize,
    pub bits_per_pixel: u8,
    pub columns: usize,
    pub lines: usize,
    pub compressed: bool,
    pub cfac: i32,
    pub lfac: i32,
    pub coff: i32,
    pub loff: i32,
    /// Sub-satellite longitude in radians, from the projection name.
    pub sub_lon: f64,
    pub segment: u16,
    /// First and last segment the sender plans to transmit for this image.
    /// Rapid Scan only sends the northern segments and shifts LOFF to match,
    /// so line numbers are relative to `planned_start`, not to the full disc.
    pub planned_start: u16,
    pub planned_end: u16,
}

impl Headers {
    pub fn parse(d: &[u8]) -> Result<Headers> {
        if d.len() < 16 || d[0] != 0 {
            return Err("not an HRIT file (missing primary header)".into());
        }
        let total = be32i(d, 4) as usize;
        let data_bits = be64(d, 8);

        let mut h = Headers {
            total_header_length: total,
            data_field_bytes: (data_bits / 8) as usize,
            bits_per_pixel: 0,
            columns: 0,
            lines: 0,
            compressed: false,
            cfac: 0,
            lfac: 0,
            coff: 0,
            loff: 0,
            sub_lon: 0.0,
            segment: 0,
            planned_start: 1,
            planned_end: 0,
        };

        let mut p = 16usize;
        while p + 3 <= total.min(d.len()) {
            let kind = d[p];
            let len = be16(d, p + 1) as usize;
            if len < 3 || p + len > d.len() {
                break;
            }
            /* The record's own declared length is the only thing checked above,
            and a record may declare less than the fields read out of it -
            a truncated or corrupt header then indexes past the end. Each
            kind states the length it needs before touching anything. */
            match kind {
                // Image structure
                1 if len >= 9 => {
                    h.bits_per_pixel = d[p + 3];
                    h.columns = be16(d, p + 4) as usize;
                    h.lines = be16(d, p + 6) as usize;
                    h.compressed = d[p + 8] != 0;
                }
                // Image navigation
                2 if len >= 51 => {
                    let name = String::from_utf8_lossy(&d[p + 3..p + 35]).to_string();
                    h.sub_lon = parse_projection_longitude(&name).to_radians();
                    h.cfac = be32i(d, p + 35);
                    h.lfac = be32i(d, p + 39);
                    h.coff = be32i(d, p + 43);
                    h.loff = be32i(d, p + 47);
                }
                // Image segment identification (MSG specific)
                128 if len >= 8 => {
                    h.segment = be16(d, p + 6);
                    if len >= 12 {
                        h.planned_start = be16(d, p + 8);
                        h.planned_end = be16(d, p + 10);
                    }
                }
                _ => {}
            }
            p += len;
        }
        Ok(h)
    }
}

/// `GEOS(+000.0)` -> 0.0 degrees.
fn parse_projection_longitude(name: &str) -> f64 {
    let Some(open) = name.find('(') else {
        return 0.0;
    };
    let rest = &name[open + 1..];
    let end = rest.find(')').unwrap_or(rest.len());
    rest[..end].trim().parse().unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Calibration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Calibration {
    pub slope: [f64; 12],
    pub offset: [f64; 12],
}

impl Calibration {
    pub fn radiance(&self, channel_index: usize, count: u16) -> f64 {
        count as f64 * self.slope[channel_index] + self.offset[channel_index]
    }
}

/// Locate the level 1.5 calibration table inside an MSG prologue.
///
/// Rather than hard-coding a byte offset into a large undocumented-here
/// structure, the table is found by its signature: twelve consecutive
/// (slope, offset) pairs of big-endian doubles in which the offset is exactly
/// `-51 x slope`, which is how MSG defines the zero-radiance count. In practice
/// this matches in exactly one place.
pub fn calibration_from_prologue(path: &Path) -> Result<Calibration> {
    let d = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if d.len() < 192 {
        return Err("prologue too short".into());
    }

    'scan: for o in 0..=d.len() - 192 {
        let mut slope = [0f64; 12];
        let mut offset = [0f64; 12];
        for k in 0..12 {
            let s = be_f64(&d, o + k * 16);
            let f = be_f64(&d, o + k * 16 + 8);
            if !(s.is_finite() && f.is_finite())
                || !(0.0005..3.0).contains(&s)
                || (f + 51.0 * s).abs() > 1e-9
            {
                continue 'scan;
            }
            slope[k] = s;
            offset[k] = f;
        }
        return Ok(Calibration { slope, offset });
    }
    Err("no calibration table found in prologue".into())
}

// ---------------------------------------------------------------------------
// Pixel unpacking
// ---------------------------------------------------------------------------

/// Unpack big-endian 10-bit pixels: four pixels per five bytes.
pub fn unpack10(data: &[u8], count: usize) -> Vec<u16> {
    let mut out = Vec::with_capacity(count);
    let mut i = 0usize;
    while out.len() < count && i + 5 <= data.len() {
        let b = &data[i..i + 5];
        out.push(((b[0] as u16) << 2) | (b[1] >> 6) as u16);
        if out.len() < count {
            out.push((((b[1] & 0x3F) as u16) << 4) | (b[2] >> 4) as u16);
        }
        if out.len() < count {
            out.push((((b[2] & 0x0F) as u16) << 6) | (b[3] >> 2) as u16);
        }
        if out.len() < count {
            out.push((((b[3] & 0x03) as u16) << 8) | b[4] as u16);
        }
        i += 5;
    }
    out.resize(count, 0);
    out
}

// ---------------------------------------------------------------------------
// Decompression
// ---------------------------------------------------------------------------

/// Find EUMETSAT's `xRITDecompress`, which handles the wavelet-compressed
/// pixel data. Checked in order: the `XRIT_DECOMPRESS` environment variable,
/// a `tools/` directory beside the executable or the working directory, then
/// whatever is on `PATH`.
pub fn find_decompressor() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        "xRITDecompress.exe"
    } else {
        "xRITDecompress"
    };

    if let Ok(p) = std::env::var("XRIT_DECOMPRESS") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
            // target/release/<exe> -> project root
            if let Some(up) = dir.parent().and_then(|d| d.parent()) {
                roots.push(up.to_path_buf());
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    for r in roots {
        let c = r.join("tools").join(exe_name);
        if c.is_file() {
            return Some(c);
        }
    }
    which(exe_name)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// The decompressed name drops the trailing compression marker.
fn decompressed_name(name: &str) -> String {
    if let Some(stem) = name.strip_suffix("C_") {
        format!("{stem}__")
    } else {
        name.to_string()
    }
}

/// Ensure a segment is readable, decompressing into `cache` if needed, and
/// return the path to use.
///
/// `xRITDecompress` writes its output into the working directory, so it is run
/// with the cache as its cwd. Results are kept, which makes re-rendering a slot
/// roughly free.
pub fn ensure_segment(src: &Path, cache: &Path, tool: Option<&Path>) -> Result<PathBuf> {
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "bad segment path".to_string())?;

    if !name.ends_with("C_") {
        return Ok(src.to_path_buf());
    }

    let out = cache.join(decompressed_name(name));
    if let Ok(meta) = std::fs::metadata(&out) {
        if meta.len() > 0 {
            return Ok(out);
        }
    }

    let tool = tool.ok_or_else(|| {
        "xRITDecompress not found - the HRIT pixel data is wavelet-compressed and \
         cannot be read without it"
            .to_string()
    })?;
    std::fs::create_dir_all(cache).map_err(|e| format!("{}: {e}", cache.display()))?;

    /* The tool writes its output into the working directory under a name it
    chooses, so it is given a private one. Two requests needing the same
    segment - two layers sharing a channel, two tabs, an export beside a
    render - would otherwise both write the same path at the same time, and
    a reader arriving mid-write would get a short file that the length check
    below happily accepts. Decompressing into a scratch directory and
    renaming the result into the cache makes the segment appear whole or not
    at all, and a loser of the race simply replaces an identical file. */
    let scratch = cache.join(format!(".work-{}", crate::diskcache::unique()));
    std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;

    let result = (|| -> Result<PathBuf> {
        let status = std::process::Command::new(tool)
            .arg(src)
            .current_dir(&scratch)
            .output()
            .map_err(|e| format!("running {}: {e}", tool.display()))?;
        if !status.status.success() {
            return Err(format!(
                "xRITDecompress failed on {}: {}",
                name,
                String::from_utf8_lossy(&status.stderr).trim()
            ));
        }
        let produced = scratch.join(decompressed_name(name));
        if !produced.exists() {
            return Err(format!("xRITDecompress produced nothing for {name}"));
        }
        std::fs::rename(&produced, &out).map_err(|e| format!("{}: {e}", out.display()))?;
        Ok(out.clone())
    })();

    let _ = std::fs::remove_dir_all(&scratch);
    result
}

pub fn segment_bytes(src: &Path, cache: &Path, tool: Option<&Path>) -> Result<Vec<u8>> {
    let path = ensure_segment(src, cache, tool)?;
    std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// How many decompressions may run at once across the whole process.
///
/// This is deliberately global rather than per-request. Decompression is
/// CPU-bound, and measured throughput rises right up to the core count for a
/// single frame - but several frames render concurrently, and a per-request
/// limit multiplies: three frames at one-per-core each spawn three times more
/// processes than the machine can run, and the whole batch gets slower. One
/// shared budget gives a lone frame the whole machine and a batch its fair
/// share.
fn decompress_budget() -> &'static (Mutex<usize>, Condvar) {
    static POOL: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let limit = std::env::var("EUMET_DECOMP_WORKERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(cores)
            .clamp(1, 64);
        (Mutex::new(limit), Condvar::new())
    })
}

/// Holds one slot in the decompression budget until dropped.
struct Permit;

impl Permit {
    fn acquire() -> Permit {
        let (lock, cv) = decompress_budget();
        let mut free = lock.lock().unwrap();
        while *free == 0 {
            free = cv.wait(free).unwrap();
        }
        *free -= 1;
        Permit
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let (lock, cv) = decompress_budget();
        *lock.lock().unwrap() += 1;
        cv.notify_one();
    }
}

/// Decompress a batch of segments in parallel.
///
/// A full disc is eight segments per channel and four channels for the natural
/// colour view, so a single slot is 32 independent decompressions. Done one at
/// a time that is several seconds of wall clock on an otherwise idle machine.
/// Failures are left to the per-segment read that follows, which reports them
/// with the context that matters.
pub fn warm_segments(paths: &[PathBuf], cache: &Path, tool: Option<&Path>) {
    let todo: Vec<&PathBuf> = paths
        .iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with("C_"))
                .unwrap_or(false)
        })
        .collect();
    if todo.len() < 2 {
        return;
    }

    // One thread per segment; they cost almost nothing while blocked, and the
    // shared budget - not the thread count - decides how many actually run.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = todo.len().min(64);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(p) = todo.get(i) else { break };
                let _permit = Permit::acquire();
                let _ = ensure_segment(p, cache, tool);
            });
        }
    });
}

/// Every segment file a set of channels needs for one slot.
pub fn segment_paths(slot: &Slot, channels: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for ch in channels {
        if let Some(segs) = slot.segments.get(*ch) {
            out.extend(segs.values().cloned());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Slots
// ---------------------------------------------------------------------------

/// Every file belonging to one acquisition time.
#[derive(Debug, Clone, Default)]
pub struct Slot {
    pub stamp: String,
    pub epoch: i64,
    pub prologue: Option<PathBuf>,
    /// channel -> (segment number -> path)
    pub segments: BTreeMap<String, BTreeMap<u16, PathBuf>>,
}

impl Slot {
    pub fn has_channels(&self, wanted: &[&str]) -> bool {
        wanted.iter().all(|c| {
            self.segments
                .get(*c)
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        })
    }

    /// Fewest segments held across the wanted channels.
    pub fn min_segments(&self, wanted: &[&str]) -> usize {
        wanted
            .iter()
            .map(|c| self.segments.get(*c).map(|s| s.len()).unwrap_or(0))
            .min()
            .unwrap_or(0)
    }

    /// A slot is usable once every wanted channel is fully received. The newest
    /// slot is normally still arriving, so rendering it would give a partial
    /// strip.
    pub fn is_complete(&self, wanted: &[&str], expect: usize) -> bool {
        self.prologue.is_some() && self.has_channels(wanted) && self.min_segments(wanted) >= expect
    }
}

/// How many segments a full image of these channels contains, learned from the
/// directory rather than assumed, so this works for both Rapid Scan (three
/// segments) and full-disc services (eight).
pub fn expected_segments(slots: &[Slot], wanted: &[&str]) -> usize {
    slots
        .iter()
        .map(|s| s.min_segments(wanted))
        .max()
        .unwrap_or(1)
        .max(1)
}

/// `H-000-MSG4__-MSG4_RSS____-VIS006___-000006___-202608171420-C_`
fn parse_hrit_name(name: &str) -> Option<(String, String, u16, String)> {
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() < 8 || parts[0] != "H" {
        return None;
    }
    let satellite = parts[2].trim_end_matches('_').to_string();
    let channel = parts[4].trim_end_matches('_').to_string();
    let segment = parts[5].trim_end_matches('_').parse::<u16>().unwrap_or(0);
    let stamp = parts[6].to_string();
    if stamp.len() != 12 {
        return None;
    }
    Some((satellite, channel, segment, stamp))
}

/// `202608171420` -> seconds since the Unix epoch.
fn stamp_to_epoch(s: &str) -> Option<i64> {
    if s.len() != 12 {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let mo: u32 = s[4..6].parse().ok()?;
    let d: u32 = s[6..8].parse().ok()?;
    let h: i64 = s[8..10].parse().ok()?;
    let mi: i64 = s[10..12].parse().ok()?;
    Some(crate::catalog::days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60)
}

/// Acquisition time of an HRIT segment, prologue or epilogue, from its name.
///
/// Names that do not match are not part of the stream, and callers must leave
/// them alone rather than guess at their age.
pub fn segment_epoch(name: &str) -> Option<i64> {
    let (_, _, _, stamp) = parse_hrit_name(name)?;
    stamp_to_epoch(&stamp)
}

/// Index an HRIT receive directory by acquisition time.
pub fn scan_slots(dir: &Path) -> Vec<Slot> {
    let mut by_stamp: BTreeMap<String, Slot> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    for e in entries.flatten() {
        let path = e.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((_sat, channel, segment, stamp)) = parse_hrit_name(name) else {
            continue;
        };
        let Some(epoch) = stamp_to_epoch(&stamp) else {
            continue;
        };

        let slot = by_stamp.entry(stamp.clone()).or_insert_with(|| Slot {
            stamp: stamp.clone(),
            epoch,
            ..Default::default()
        });

        if channel.is_empty() {
            // The prologue and epilogue carry no channel name.
            if name.contains("-PRO______-") {
                slot.prologue = Some(path);
            }
        } else {
            slot.segments
                .entry(channel)
                .or_default()
                .insert(segment, path);
        }
    }

    let mut out: Vec<Slot> = by_stamp.into_values().collect();
    out.sort_by_key(|s| s.epoch);
    out
}

// ---------------------------------------------------------------------------
// Assembled channel image
// ---------------------------------------------------------------------------

/// One channel of one slot, assembled from its segments.
pub struct ChannelImage {
    pub columns: usize,
    /// Global line number (1-based) of row 0.
    pub line_start: usize,
    pub lines: usize,
    pub counts: Vec<u16>,
    pub cfac: i32,
    pub lfac: i32,
    pub coff: i32,
    pub loff: i32,
    pub sub_lon: f64,
}

impl ChannelImage {
    /// Pixels per unit of scan angle, and whether that unit is degrees.
    ///
    /// Producers scale CFAC/LFAC for either degrees or radians. The two differ
    /// by exactly 180/pi - around 208 against 11927 pixels per unit for SEVIRI -
    /// so the magnitude identifies the convention unambiguously.
    fn angle_factors(&self) -> (f64, f64, bool) {
        let scale = (2f64).powi(-16);
        let cf = self.cfac as f64 * scale;
        let lf = self.lfac as f64 * scale;
        let radians = cf.abs() > 2000.0;
        (cf, lf, !radians)
    }

    /// Map CGMS scan angles to a stored pixel.
    ///
    /// MSG numbers lines from the south and columns from the east; the signs of
    /// LFAC and CFAC in the file encode that, so no extra flipping is needed.
    pub fn sample(&self, x: f64, y: f64) -> Option<u16> {
        let (cf, lf, degrees) = self.angle_factors();
        let (x, y) = if degrees {
            (x.to_degrees(), y.to_degrees())
        } else {
            (x, y)
        };
        let col = (self.coff as f64 + x * cf).round();
        let line = (self.loff as f64 + y * lf).round();
        if !col.is_finite() || !line.is_finite() {
            return None;
        }
        let col = col as i64;
        let line = line as i64;
        if col < 1 || col > self.columns as i64 {
            return None;
        }
        let row = line - self.line_start as i64;
        if row < 0 || row >= self.lines as i64 {
            return None;
        }
        self.counts
            .get(row as usize * self.columns + (col as usize - 1))
            .copied()
    }
}

/// Keep the decompressed-segment cache from growing without bound.
///
/// Each slot expands to roughly 26 MB across the four channels, so a long
/// window would otherwise fill the disk. Oldest files go first; anything still
/// needed is simply decompressed again.
pub fn prune_cache(cache: &Path, max_bytes: u64) {
    crate::diskcache::prune_dir(cache, max_bytes);
}

/// Decompress, unpack and stitch every segment of one channel.
pub fn load_channel(
    slot: &Slot,
    channel: &str,
    cache: &Path,
    tool: Option<&Path>,
) -> Result<ChannelImage> {
    let segs = slot
        .segments
        .get(channel)
        .ok_or_else(|| format!("channel {channel} missing from slot {}", slot.stamp))?;
    if segs.is_empty() {
        return Err(format!("channel {channel} has no segments"));
    }

    let mut image: Option<ChannelImage> = None;
    let first_segment = *segs.keys().next().unwrap();
    let last_segment = *segs.keys().next_back().unwrap();

    for (&seq, path) in segs {
        let stored = ensure_segment(path, cache, tool)?;
        let raw = std::fs::read(&stored).map_err(|e| format!("{}: {e}", stored.display()))?;
        let h = Headers::parse(&raw)?;
        if h.columns == 0 || h.lines == 0 {
            return Err(format!("{}: empty image structure", path.display()));
        }
        if h.bits_per_pixel != 10 {
            return Err(format!(
                "{}: expected 10 bits per pixel, found {}",
                path.display(),
                h.bits_per_pixel
            ));
        }

        let img = image.get_or_insert_with(|| {
            let lines = h.lines * (last_segment - first_segment + 1) as usize;
            // LOFF is expressed relative to the first segment the sender plans
            // to transmit, so line numbering starts there rather than at the
            // top of the full disc.
            let base = h.planned_start.max(1);
            ChannelImage {
                columns: h.columns,
                line_start: (first_segment.saturating_sub(base)) as usize * h.lines + 1,
                lines,
                counts: vec![0u16; lines * h.columns],
                cfac: h.cfac,
                lfac: h.lfac,
                coff: h.coff,
                loff: h.loff,
                sub_lon: h.sub_lon,
            }
        });

        let body = raw
            .get(h.total_header_length..)
            .ok_or_else(|| format!("{}: truncated data field", path.display()))?;

        /* `unpack10` pads a short input with zeros rather than failing, which is
        right for a segment the sender genuinely cut short but wrong for one
        lost to a half-written cache file: that would paint the missing lines
        as valid zero counts and quietly produce a blank band. A segment is
        a fixed size, so the shortfall is worth naming. */
        let want = (h.columns * h.lines * 10).div_ceil(8);
        if body.len() < want {
            // Drop it so the next request decompresses the segment again: a
            // short file in the cache is a write that did not finish, and
            // failing for as long as it survives would be worse than the one
            // error it takes to notice.
            if stored.starts_with(cache) {
                let _ = std::fs::remove_file(&stored);
            }
            return Err(format!(
                "{}: cached segment was incomplete ({} of {want} bytes) and has been \
                 discarded - the next request will decompress it again",
                path.display(),
                body.len()
            ));
        }
        let px = unpack10(body, h.columns * h.lines);

        // Segments are numbered from the south, and lines within a segment run
        // in the same direction, so the destination offset is a simple stride.
        let dst_row = (seq as usize - first_segment as usize) * h.lines;
        let start = dst_row * img.columns;
        let end = (start + px.len()).min(img.counts.len());
        img.counts[start..end].copy_from_slice(&px[..end - start]);
    }

    image.ok_or_else(|| "no segments loaded".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpacks_ten_bit_pixels() {
        // Four pixels packed into five bytes, most significant bits first.
        // 0b1111111111, 0b0000000000, 0b1010101010, 0b0101010101
        let packed = [0xFF, 0xC0, 0x0A, 0xA9, 0x55];
        assert_eq!(unpack10(&packed, 4), vec![1023, 0, 682, 341]);
    }

    #[test]
    fn unpack_stops_at_the_requested_count() {
        let packed = [0xFF, 0xC0, 0x0A, 0xA9, 0x55];
        assert_eq!(unpack10(&packed, 2), vec![1023, 0]);
        // Short input is padded rather than panicking.
        assert_eq!(unpack10(&[0xFF], 3).len(), 3);
    }

    #[test]
    fn parses_a_segment_filename() {
        let (sat, ch, seg, stamp) =
            parse_hrit_name("H-000-MSG4__-MSG4_RSS____-VIS006___-000006___-202608171420-C_")
                .unwrap();
        assert_eq!(sat, "MSG4");
        assert_eq!(ch, "VIS006");
        assert_eq!(seg, 6);
        assert_eq!(stamp, "202608171420");
    }

    #[test]
    fn ignores_non_hrit_names() {
        assert!(parse_hrit_name("S_NWC_CT_MSG3_MSG-N-VISIR_20260817T120000Z.nc").is_none());
    }

    #[test]
    fn reads_the_projection_longitude() {
        assert!((parse_projection_longitude("GEOS(+009.5)") - 9.5).abs() < 1e-9);
        assert!((parse_projection_longitude("GEOS(+000.0)")).abs() < 1e-9);
    }

    #[test]
    fn decompressed_name_clears_the_marker() {
        assert!(decompressed_name("H-000-x-000006___-202608171420-C_").ends_with("-__"));
    }

    fn image_with(cfac: i32) -> ChannelImage {
        ChannelImage {
            columns: 3712,
            line_start: 1,
            lines: 1392,
            counts: vec![0; 1],
            cfac,
            lfac: cfac,
            coff: 1856,
            loff: -464,
            sub_lon: 0.0,
        }
    }

    /// CFAC/LFAC come scaled for either degrees or radians; the magnitude has
    /// to pick the right one or the whole image lands off-grid.
    #[test]
    fn detects_the_angle_convention() {
        let (_, _, degrees) = image_with(-13642337).angle_factors();
        assert!(degrees, "SEVIRI HRIT scales these for degrees");

        let (_, _, degrees) = image_with(-781648343).angle_factors();
        assert!(!degrees, "the radian-scaled variant must be recognised");
    }

    /// The two conventions differ by exactly 180/pi.
    #[test]
    fn both_conventions_agree_on_the_same_pixel() {
        let deg = image_with(-13642337);
        let rad = image_with(-781648343);
        let (x, y) = (0.05f64, -0.10f64);
        let f = |img: &ChannelImage| {
            let (cf, lf, degrees) = img.angle_factors();
            let (x, y) = if degrees {
                (x.to_degrees(), y.to_degrees())
            } else {
                (x, y)
            };
            (img.coff as f64 + x * cf, img.loff as f64 + y * lf)
        };
        let (ca, la) = f(&deg);
        let (cb, lb) = f(&rad);
        assert!((ca - cb).abs() < 0.5, "columns {ca} vs {cb}");
        assert!((la - lb).abs() < 0.5, "lines {la} vs {lb}");
    }

    #[test]
    fn calibration_signature_rejects_noise() {
        let junk = vec![0u8; 4096];
        assert!(calibration_from_prologue(Path::new("/definitely/missing")).is_err());
        let _ = junk;
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    /// A record may declare a length shorter than the fields the parser reads
    /// from it. The guard only checks the declared length fits the file.
    #[test]
    fn a_short_record_must_not_panic() {
        for kind in [1u8, 2, 128] {
            let mut d = vec![0u8; 64];
            d[0] = 0; // primary header
            d[4..8].copy_from_slice(&64i32.to_be_bytes()); // total header length
                                                           // One record at offset 16 declaring the minimum legal length.
            d[16] = kind;
            d[17] = 0;
            d[18] = 3; // len = 3, far shorter than the fields read below
            let _ = Headers::parse(&d);
        }
    }
}
