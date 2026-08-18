//! Web server for browsing recent EUMETCast imagery of Europe.

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use bytes::Bytes;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eumet_stream::anim::Animation;
use eumet_stream::catalog::{civil_from_epoch, now_epoch, Catalog};
use eumet_stream::geo::{self, BBox, Canvas, GeosGrid};
use eumet_stream::live::{self, LiveOpts};
use eumet_stream::product::{self, Conditions, Style, VIEWS};
use eumet_stream::render::{self, RenderOpts};
use eumet_stream::{borders, hrit, rgb};

const DEFAULT_DIR: &str = r"C:\EUMETCast\received\bas\E1B-GEO-4";
const DEFAULT_HRIT_DIR: &str = r"C:\EUMETCast\received\bas\E1B-GEO-5";
/// Rapid Scan only transmits the northern third of the disc, so the global view
/// comes from the 0-degree full-disc service instead.
const DEFAULT_DISC_DIR: &str = r"C:\EUMETCast\received\bas\E1B-GEO-3";
/// The full-disc service repeats every 15 minutes, not every 5.
const DISC_STEPS: [i64; 6] = [15, 30, 60, 120, 180, 360];
const WINDOWS: [i64; 4] = [6, 12, 24, 48];

/// Frame intervals offered, in minutes. Rapid Scan arrives every 5 minutes and
/// the NWC SAF products every 15, so each layer offers what it can supply.
///
/// The three coarse ones exist for ranges rather than for windows: a month at
/// hourly is 720 frames, past the ceiling below, and the range would have been
/// cut short instead of thinned. At two-hourly a month is 360, which fits - so
/// the ladder now reaches far enough to thin the longest span anyone can ask
/// for. Nobody picks 6-hourly for a 6-hour window, and nothing stops them.
const LIVE_STEPS: [i64; 8] = [5, 10, 15, 30, 60, 120, 180, 360];
const PRODUCT_STEPS: [i64; 6] = [15, 30, 60, 120, 180, 360];

/// A hard ceiling on frames per window, so an unlucky combination cannot ask
/// the browser to hold a thousand images.
const MAX_FRAMES: usize = 400;

/// Decompressed HRIT segments are cached on disk; a slot costs about 26 MB.
const HRIT_CACHE_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// Rendered PNGs are cached on disk too, and unlike the segments they are never
/// rewritten, so without a ceiling the directory grows for as long as the
/// server is used. Anything evicted is simply rendered again.
///
/// Sixteen gigabytes because a long range is the demanding case: 400 full-disc
/// frames at 2400 square are 10.4 MB each, so one such window is 4 GB on its
/// own. At the old 2 GB the cache evicted frames from the start of a pass
/// before playback reached them, and re-rendered what it had just built. This
/// holds several large windows at once and still costs half a percent of the
/// free space on the drive it lives on.
const RENDER_CACHE_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// How often the rendered-frame directory is measured. Pruning walks the
/// directory, so it is not worth doing on every frame of a render pass.
const RENDER_PRUNE_EVERY: Duration = Duration::from_secs(120);

/// A ceiling on the rendered frames held in memory.
///
/// The bound is bytes rather than a count: a globe frame at full size is
/// several megabytes and a small European one a few hundred kilobytes, so a
/// fixed number of entries means anything between a few tens of megabytes and
/// most of a gigabyte.
///
/// Four gigabytes holds a whole 400-frame full-disc window, so replaying one a
/// second time touches no disk at all. This is the only cache that occupies
/// RAM - the sixteen gigabytes above is disk - and it is a small fraction of
/// any machine that would be asked to render a month of imagery.
const MEMORY_CACHE_BYTES: usize = 4 * 1024 * 1024 * 1024;

/// Rendered frames kept in memory, bounded by total size.
struct Cache {
    map: HashMap<String, Bytes>,
    order: VecDeque<String>,
    bytes: usize,
    cap_bytes: usize,
}

impl Cache {
    fn new(cap_bytes: usize) -> Self {
        Cache {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            cap_bytes,
        }
    }

    fn get(&self, k: &str) -> Option<Bytes> {
        self.map.get(k).cloned()
    }

    fn put(&mut self, k: String, v: Bytes) {
        if self.map.contains_key(&k) {
            return;
        }
        // A single frame larger than the whole budget would evict everything
        // and still not fit, so it is simply not kept.
        if v.len() > self.cap_bytes {
            return;
        }
        while self.bytes + v.len() > self.cap_bytes {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(gone) = self.map.remove(&old) {
                self.bytes -= gone.len();
            }
        }
        self.bytes += v.len();
        self.order.push_back(k.clone());
        self.map.insert(k, v);
    }
}

/// How long a directory listing is trusted before being rebuilt.
///
/// The HRIT directories hold tens of thousands of files and were being walked
/// on every single request, including each frame of a render pass. Slots arrive
/// every five minutes, so a few seconds of staleness costs nothing.
const INDEX_TTL: Duration = Duration::from_secs(20);

struct Indexes {
    slots: HashMap<PathBuf, (Instant, Arc<Vec<hrit::Slot>>)>,
    catalog: Option<(Instant, Arc<Catalog>)>,
    /// The NWC SAF geography and cloud mask, which cost an HDF5 decode each.
    aux: Option<(PathBuf, Arc<Auxiliary>)>,
}

struct AppState {
    dir: PathBuf,
    hrit_dir: PathBuf,
    disc_dir: PathBuf,
    hrit_cache: PathBuf,
    render_cache: PathBuf,
    decompressor: Option<PathBuf>,
    cache: Mutex<Cache>,
    index: Mutex<Indexes>,
    /// Identifies the code that produced a cached frame; see `build_stamp`.
    build: String,
    /// One lock per frame being rendered, so concurrent requests for the same
    /// frame wait for the first rather than each repeating the work.
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// A short token that changes whenever the executable does.
///
/// It goes into every cache key. The rendered-frame cache is on disk and
/// outlives the process, so without this a change to a palette, a ramp or the
/// projection would keep serving pictures drawn by the old code - for as long
/// as those files survive, which is days. The executable's modification time
/// changes on every rebuild and needs no manual bookkeeping.
fn build_stamp() -> String {
    let secs = std::env::current_exe()
        .and_then(|p| p.metadata())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}.{secs:x}", env!("CARGO_PKG_VERSION"))
}

impl AppState {
    /// Which HRIT service serves a given area.
    fn hrit_dir_for(&self, region: &str) -> &PathBuf {
        if region == "globe" {
            &self.disc_dir
        } else {
            &self.hrit_dir
        }
    }

    /// Where a rendered frame is kept on disk.
    ///
    /// Rendering a raw-imagery frame costs seconds, so finished PNGs outlive
    /// the process: a restart, or coming back to a window looked at yesterday,
    /// serves from disk instead of decompressing and reprojecting again.
    fn render_path(&self, key: &str) -> PathBuf {
        // FNV-1a keeps the filename short and stable without a hashing crate.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in key.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        self.render_cache.join(format!("{h:016x}.png"))
    }

    /// A finished frame, from memory or from disk.
    fn cached_frame(&self, key: &str) -> Option<Bytes> {
        if let Some(v) = self.cache.lock().unwrap().get(key) {
            return Some(v);
        }
        let bytes = std::fs::read(self.render_path(key)).ok()?;
        // A PNG is at least a signature and an IEND; anything shorter is not a
        // frame someone finished writing.
        if bytes.len() < 8 || bytes[..8] != [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
            return None;
        }
        let bytes = Bytes::from(bytes);
        self.cache
            .lock()
            .unwrap()
            .put(key.to_string(), bytes.clone());
        Some(bytes)
    }

    /// Slots in a directory, rebuilt at most every `INDEX_TTL`.
    fn slots_in(&self, dir: &Path) -> Arc<Vec<hrit::Slot>> {
        {
            let idx = self.index.lock().unwrap();
            if let Some((at, slots)) = idx.slots.get(dir) {
                if at.elapsed() < INDEX_TTL {
                    return slots.clone();
                }
            }
        }
        // Scanning outside the lock: it takes long enough that holding it would
        // serialise every concurrent render behind one directory walk.
        let scanned = Arc::new(hrit::scan_slots(dir));
        let mut idx = self.index.lock().unwrap();
        idx.slots
            .insert(dir.to_path_buf(), (Instant::now(), scanned.clone()));
        scanned
    }

    fn catalog(&self) -> Arc<Catalog> {
        {
            let idx = self.index.lock().unwrap();
            if let Some((at, c)) = &idx.catalog {
                if at.elapsed() < INDEX_TTL {
                    return c.clone();
                }
            }
        }
        let scanned = Arc::new(Catalog::scan(&self.dir));
        let mut idx = self.index.lock().unwrap();
        idx.catalog = Some((Instant::now(), scanned.clone()));
        scanned
    }

    /// The NWC SAF auxiliary for one slot, kept for as long as that slot is the
    /// one being asked about. Every regional frame needs it, and decoding it is
    /// a full HDF5 read.
    fn auxiliary(&self, path: &Path) -> Option<Arc<Auxiliary>> {
        {
            let idx = self.index.lock().unwrap();
            if let Some((p, a)) = &idx.aux {
                if p == path {
                    return Some(a.clone());
                }
            }
        }
        let loaded = Arc::new(load_auxiliary(path)?);
        let mut idx = self.index.lock().unwrap();
        idx.aux = Some((path.to_path_buf(), loaded.clone()));
        Some(loaded)
    }

    fn store_frame(&self, key: String, png: Bytes) {
        let path = self.render_path(&key);
        // A failed write only costs the reuse, so it is not worth failing over.
        let _ = eumet_stream::diskcache::write_atomic(&path, &png);
        self.cache.lock().unwrap().put(key, png);
    }

    /// Hold the rendered-frame directory to its ceiling.
    ///
    /// Pruning measures the whole directory, which costs about 95 ms at 1700
    /// files and scales with the count - at the 16 GB ceiling that is closer to
    /// half a second. This used to run from `store_frame`, which put that on
    /// the end of an unlucky frame render every couple of minutes. It belongs
    /// on its own timer instead, where it delays nothing.
    fn prune_renders(&self) {
        eumet_stream::diskcache::prune_dir(&self.render_cache, RENDER_CACHE_BYTES);
    }

    /// The lock guarding renders of one frame, creating it if this is the first
    /// request for that key.
    fn render_slot(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.inflight.lock().unwrap();
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Forget a render lock once nobody else is holding it. Keeping the map
    /// from growing matters: every distinct frame ever requested would
    /// otherwise leave an entry behind.
    fn release_slot(&self, key: &str) {
        let mut map = self.inflight.lock().unwrap();
        if let Some(slot) = map.get(key) {
            // Two references: the map's and the caller's.
            if Arc::strong_count(slot) <= 2 {
                map.remove(key);
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let mut dir = PathBuf::from(DEFAULT_DIR);
    let mut hrit_dir = PathBuf::from(DEFAULT_HRIT_DIR);
    let mut disc_dir = PathBuf::from(DEFAULT_DISC_DIR);
    let mut port: u16 = 8787;
    let mut bind = LOOPBACK.to_string();
    let mut retain_days: Option<i64> = None;
    let mut purge_dry_run = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dir" => {
                if let Some(v) = args.next() {
                    dir = PathBuf::from(v);
                }
            }
            "--hrit-dir" => {
                if let Some(v) = args.next() {
                    hrit_dir = PathBuf::from(v);
                }
            }
            "--disc-dir" => {
                if let Some(v) = args.next() {
                    disc_dir = PathBuf::from(v);
                }
            }
            "--port" => {
                if let Some(v) = args.next() {
                    port = v.parse().unwrap_or(port);
                }
            }
            /* Localhost stays the default: the server has no authentication, so
            reaching it from the network is a decision to make rather than
            one to inherit. `all` is spelled out because `0.0.0.0` is easy to
            mistype into something that binds nothing. */
            "--bind" => {
                if let Some(v) = args.next() {
                    bind = match v.as_str() {
                        "all" => "0.0.0.0".to_string(),
                        "localhost" => LOOPBACK.to_string(),
                        other => other.to_string(),
                    };
                }
            }
            /* Off unless asked for. Received slots cannot be rebuilt, and a
            viewer is not the natural owner of the receiver's output, so
            deleting any of it is a decision the operator makes. */
            "--retain-days" => {
                if let Some(v) = args.next() {
                    retain_days = v.parse().ok();
                }
            }
            "--purge-dry-run" => purge_dry_run = true,
            "--help" | "-h" => {
                println!("usage: eumet-stream [options]");
                println!();
                println!("  --dir <path>       NWC SAF products (CT, CTTH, CMA)");
                println!("                     default {DEFAULT_DIR}");
                println!("  --hrit-dir <path>  SEVIRI HRIT, Rapid Scan - Europe and Wide");
                println!("                     default {DEFAULT_HRIT_DIR}");
                println!("  --disc-dir <path>  SEVIRI HRIT, full disc - the Globe area");
                println!("                     default {DEFAULT_DISC_DIR}");
                println!("  --port <port>      default {port}");
                println!("  --retain-days <n>  delete received data older than n days.");
                println!("                     Off unless given. Applies to the directories");
                println!(
                    "                     above and to both caches. Minimum {}.",
                    eumet_stream::purge::MIN_RETAIN_DAYS
                );
                println!("  --purge-dry-run    report what --retain-days would delete, and");
                println!("                     delete nothing.");
                println!("  --bind <addr>      default {LOOPBACK}, this machine only.");
                println!("                     'all' listens on every interface, so other");
                println!("                     machines on your network can open it. There is");
                println!("                     no password on the server, so only do this on a");
                println!("                     network you trust.");
                println!();
                println!("Any directory may be omitted; the layers it feeds are then withdrawn.");
                return;
            }
            _ => {}
        }
    }

    let catalog = Catalog::scan(&dir);
    println!("EUMETCast Europe viewer");
    println!("  NWC SAF directory : {}", dir.display());
    println!("  frames indexed    : {}", catalog.frames.len());
    for (p, n) in catalog.counts() {
        println!("    {p:<6} {n} slots");
    }

    let slots = hrit::scan_slots(&hrit_dir);
    let expect = hrit::expected_segments(&slots, &live::REQUIRED);
    let complete = slots
        .iter()
        .filter(|s| s.is_complete(&live::REQUIRED, expect))
        .count();
    println!("  HRIT directory    : {}", hrit_dir.display());
    println!(
        "  complete slots    : {complete} of {} indexed",
        slots.len()
    );
    println!("  border polylines  : {}", borders::polyline_count());

    let decompressor = hrit::find_decompressor();
    match &decompressor {
        Some(p) => println!("  decompressor      : {}", p.display()),
        None => println!(
            "  decompressor      : NOT FOUND - the SEVIRI layers need xRITDecompress\n\
                                   (run tools\\build-decompressor.ps1, or set XRIT_DECOMPRESS)"
        ),
    }

    let render_cache = std::env::temp_dir().join("eumet-stream-frames");
    std::fs::create_dir_all(&render_cache).ok();
    // Hold it to its ceiling now rather than waiting for the first render: a
    // server left running for weeks would otherwise start with whatever the
    // last one left behind.
    let freed = eumet_stream::diskcache::prune_dir(&render_cache, RENDER_CACHE_BYTES);
    let kept = std::fs::read_dir(&render_cache)
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    print!(
        "  frame cache       : {} ({kept} rendered",
        render_cache.display()
    );
    if freed > 0 {
        print!(", {} MB pruned", freed / (1024 * 1024));
    }
    println!(")");

    let hrit_cache = std::env::temp_dir().join("eumet-stream-hrit");
    std::fs::create_dir_all(&hrit_cache).ok();
    // Decompression works in a private subdirectory and removes it afterwards;
    // one left behind means a process that did not get to finish.
    sweep_scratch(&hrit_cache);

    let disc = hrit::scan_slots(&disc_dir);
    let disc_ok = disc
        .iter()
        .filter(|s| s.is_complete(&live::REQUIRED, 8))
        .count();
    println!("  full-disc dir     : {}", disc_dir.display());
    println!("  full-disc slots   : {disc_ok} of {} indexed", disc.len());

    let state = Arc::new(AppState {
        dir,
        hrit_dir,
        disc_dir,
        hrit_cache,
        render_cache,
        decompressor,
        cache: Mutex::new(Cache::new(MEMORY_CACHE_BYTES)),
        index: Mutex::new(Indexes {
            slots: HashMap::new(),
            catalog: None,
            aux: None,
        }),
        build: build_stamp(),
        inflight: Mutex::new(HashMap::new()),
    });

    {
        // Off the request path: a directory sweep must never land on the end of
        // someone's frame render.
        let owner = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RENDER_PRUNE_EVERY).await;
                let st = owner.clone();
                let _ = tokio::task::spawn_blocking(move || st.prune_renders()).await;
            }
        });
    }

    if let Some(days) = retain_days {
        run_purge(&state, days, purge_dry_run);
        if !purge_dry_run {
            let owner = state.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(PURGE_EVERY).await;
                    let st = owner.clone();
                    // Off the async runtime: it is a directory walk and a lot
                    // of unlinks, and it must not stall a render.
                    let _ = tokio::task::spawn_blocking(move || run_purge(&st, days, false)).await;
                }
            });
        }
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/api/init", get(api_init))
        .route("/api/status", get(api_status))
        .route("/api/frames", get(api_frames))
        .route("/api/range", get(api_range))
        .route("/api/legend", get(api_legend))
        .route("/api/frame.png", get(api_frame))
        .route("/api/native", get(api_native))
        .route("/api/animation.png", get(api_animation))
        .with_state(state);

    let addr = format!("{bind}:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind {addr}: {e}");
            return;
        }
    };

    println!();
    if bind == LOOPBACK {
        println!("  listening on http://127.0.0.1:{port}");
        println!("  (this machine only - pass --bind all to reach it from the network)");
    } else {
        println!("  listening on http://{addr}");
        if let Some(ip) = local_address() {
            println!("  on this network: http://{ip}:{port}");
        }
        println!();
        println!("  Reachable from other machines. There is no password on it: anyone who");
        println!("  can open that address sees the imagery and can make the server render.");
        println!("  Windows Firewall will most likely block it until you allow the port -");
        println!("  once, from an administrator PowerShell:");
        println!();
        // One line, so it can be copied and pasted as it stands.
        println!("    New-NetFirewallRule -DisplayName 'eumet-stream' -Direction Inbound -Action Allow -Protocol TCP -LocalPort {port} -Profile Private");
    }
    println!();

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("server error: {e}");
    }
}

const LOOPBACK: &str = "127.0.0.1";

/// This machine's address on the network it would use to reach the outside.
///
/// Printed so the address can be typed into a phone without going looking for
/// it. A connected UDP socket sends nothing - it only makes the operating
/// system choose a route and bind a local address - so this works with no
/// traffic and no name lookup. It returns nothing when there is no route at
/// all, which is not an error worth reporting.
fn local_address() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?; // TEST-NET-1, reserved and unrouted
    let ip = sock.local_addr().ok()?.ip();
    if ip.is_unspecified() || ip.is_loopback() {
        return None;
    }
    Some(ip)
}

/// How often received data is checked against the retention period. Slots
/// arrive every few minutes but age out over days, so this is not urgent work.
const PURGE_EVERY: Duration = Duration::from_secs(3600);

/// One retention pass, reported on the console.
///
/// Received data and cache are reported separately: deleting a cache entry
/// costs a re-render, deleting a received slot is permanent.
fn run_purge(st: &AppState, days: i64, dry_run: bool) {
    let targets = eumet_stream::purge::Targets {
        products: &st.dir,
        hrit: &st.hrit_dir,
        disc: &st.disc_dir,
        hrit_cache: &st.hrit_cache,
        render_cache: &st.render_cache,
    };
    let (received, cached) = eumet_stream::purge::purge_all(&targets, days, now_epoch(), dry_run);

    if received.is_empty() && cached.is_empty() {
        return;
    }
    let verb = if dry_run { "would delete" } else { "deleted" };
    println!(
        "  purge ({days} days)  : {verb} {} received files ({} MB), {} cached ({} MB)",
        received.files,
        received.megabytes(),
        cached.files,
        cached.megabytes()
    );
    let stuck = received.failed + cached.failed;
    if stuck > 0 {
        println!("                      {stuck} could not be removed - most likely still open");
    }
}

/// Remove decompression scratch directories a previous run left behind.
fn sweep_scratch(cache: &Path) {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        if name.to_string_lossy().starts_with(".work-") && e.path().is_dir() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("web/index.html"))
}

async fn app_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("web/app.js"),
    )
}

async fn style_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("web/style.css"),
    )
}

fn style_name(s: Style) -> &'static str {
    match s {
        Style::Live => "live",
        Style::Composite => "composite",
        Style::Surface => "surface",
        Style::Natural => "natural",
        Style::Classes => "classes",
        Style::Ramp => "ramp",
    }
}

/// Layers reading the raw HRIT stream rather than the NWC SAF products.
fn is_hrit(style: Style) -> bool {
    matches!(style, Style::Live | Style::Surface | Style::Composite)
}

/// Channels a layer needs from the HRIT stream.
fn channels_for(v: &product::View) -> Vec<&'static str> {
    match v.style {
        Style::Composite => rgb::recipe(v.variable)
            .map(|r| r.channels())
            .unwrap_or_default(),
        Style::Surface => vec![live::NIGHT],
        _ => live::REQUIRED.to_vec(),
    }
}

fn label_for(epoch: i64) -> String {
    let (y, mo, d, h, mi) = civil_from_epoch(epoch);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}Z")
}

fn iso_for(epoch: i64) -> String {
    let (y, mo, d, h, mi) = civil_from_epoch(epoch);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:00Z")
}

/// Complete HRIT slots for an area and channel set, newest last.
fn hrit_slots(st: &AppState, region: &str, channels: &[&str]) -> Vec<hrit::Slot> {
    let slots = st.slots_in(st.hrit_dir_for(region));
    let expect = hrit::expected_segments(&slots, channels);
    slots
        .iter()
        .filter(|s| s.is_complete(channels, expect))
        .cloned()
        .collect()
}

/// Shorthand for the natural-colour channel set on the regional service.
fn live_slots(st: &AppState) -> Vec<hrit::Slot> {
    hrit_slots(st, "europe", &live::REQUIRED)
}

/// Keep frames landing on a whole multiple of the requested interval.
///
/// Slots are already aligned to the service cadence, so this picks exact times
/// rather than an arbitrary stride: an hourly view really is on the hour.
fn align(mut times: Vec<i64>, step_minutes: i64) -> Vec<i64> {
    let step = step_minutes.max(1) * 60;
    if step > 60 {
        let aligned: Vec<i64> = times.iter().copied().filter(|t| t % step == 0).collect();
        if !aligned.is_empty() {
            times = aligned;
        }
    }
    times
}

/// Align to the interval, then keep the newest `MAX_FRAMES`.
fn thin(times: Vec<i64>, step_minutes: i64) -> Vec<i64> {
    let mut times = align(times, step_minutes);
    if times.len() > MAX_FRAMES {
        let drop = times.len() - MAX_FRAMES;
        times.drain(0..drop);
    }
    times
}

#[derive(Serialize)]
struct ViewInfo {
    id: String,
    label: String,
    units: String,
    style: String,
    steps: Vec<i64>,
}

#[derive(Serialize)]
struct InitResponse {
    views: Vec<ViewInfo>,
    windows: Vec<i64>,
}

async fn api_init(State(st): State<Arc<AppState>>) -> Json<InitResponse> {
    let catalog = st.catalog();
    let available = catalog.products();
    /* Either HRIT service is enough to offer the raw-imagery layers: the
    full-disc directory alone still draws the globe. Testing only the Rapid
    Scan directory withdrew every one of them when just it was missing. */
    let hrit_ready = st.decompressor.is_some()
        && (!live_slots(&st).is_empty() || !hrit_slots(&st, "globe", &live::REQUIRED).is_empty());

    let views = VIEWS
        .iter()
        .filter(|v| {
            if is_hrit(v.style) {
                hrit_ready
            } else {
                available.iter().any(|p| p == v.product)
            }
        })
        .map(|v| ViewInfo {
            id: v.id.into(),
            label: v.label.into(),
            units: v.units.into(),
            style: style_name(v.style).into(),
            steps: if is_hrit(v.style) {
                LIVE_STEPS.to_vec()
            } else {
                PRODUCT_STEPS.to_vec()
            },
        })
        .collect();

    Json(InitResponse {
        views,
        windows: WINDOWS.to_vec(),
    })
}

#[derive(Serialize)]
struct StatusResponse {
    now: i64,
    /// Newest complete Rapid Scan slot.
    live: Option<i64>,
    /// Newest NWC SAF slot.
    product: Option<i64>,
}

/// How fresh the data is. Polled by the page so it can show the age of the
/// newest image and pick up new slots without a reload.
async fn api_status(State(st): State<Arc<AppState>>) -> Json<StatusResponse> {
    let product = st
        .catalog()
        .frames
        .iter()
        .filter(|f| f.product == "CT")
        .map(|f| f.epoch)
        .max();
    let live = live_slots(&st).last().map(|s| s.epoch);
    Json(StatusResponse {
        now: now_epoch(),
        live,
        product,
    })
}

#[derive(Serialize)]
struct FrameInfo {
    t: i64,
    iso: String,
    label: String,
}

#[derive(Serialize)]
struct FramesResponse {
    frames: Vec<FrameInfo>,
    hours: i64,
    /// The interval actually used, which a range may have coarsened.
    step: i64,
    /// Intervals valid for this layer and area, so the picker can rebuild.
    steps: Vec<i64>,
    /// Echoed back when the selection was a range rather than a window.
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<i64>,
}

async fn api_frames(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<FramesResponse>, (StatusCode, String)> {
    let view_id = q.get("view").map(|s| s.as_str()).unwrap_or("live");
    let v = product::view(view_id)
        .ok_or((StatusCode::BAD_REQUEST, format!("unknown view {view_id}")))?;
    let hours: i64 = q
        .get("hours")
        .and_then(|h| h.parse().ok())
        .filter(|h| WINDOWS.contains(h))
        .unwrap_or(24);

    let Some(region) = region_from(&q) else {
        return Err((StatusCode::BAD_REQUEST, "unknown area".to_string()));
    };
    let allowed: &[i64] = steps_for(v, region);
    let step: i64 = q
        .get("step")
        .and_then(|s| s.parse().ok())
        .filter(|s| allowed.contains(s))
        .unwrap_or(allowed[0]);

    /* An explicit range replaces the rolling window. Both are honoured so a
    bookmark or an export keeps working either way. */
    let range = match range_from(&q) {
        Ok(r) => r,
        Err(e) => return Err((StatusCode::BAD_REQUEST, e)),
    };

    let (times, step) = match range {
        Some((from, to)) => {
            let times: Vec<i64> = available_times(&st, v, region)
                .into_iter()
                .filter(|t| *t >= from && *t <= to)
                .collect();
            fit_range(times, allowed, step)
        }
        None => {
            let all = available_times(&st, v, region);
            let times = match all.last().copied() {
                // The window is measured back from the newest frame held, not
                // from the wall clock, so the view still fills if reception has
                // paused.
                Some(newest) => all
                    .into_iter()
                    .filter(|t| *t >= newest - hours * 3600)
                    .collect(),
                None => Vec::new(),
            };
            (thin(times, step), step)
        }
    };

    let frames = times
        .into_iter()
        .map(|t| FrameInfo {
            t,
            iso: iso_for(t),
            label: label_for(t),
        })
        .collect();

    Ok(Json(FramesResponse {
        frames,
        hours,
        step,
        steps: allowed.to_vec(),
        from: range.map(|r| r.0),
        to: range.map(|r| r.1),
    }))
}

/// Every slot time a layer can be drawn for in an area, oldest first.
fn available_times(st: &AppState, v: &product::View, region: &str) -> Vec<i64> {
    if is_hrit(v.style) {
        hrit_slots(st, region, &channels_for(v))
            .iter()
            .map(|s| s.epoch)
            .collect()
    } else {
        let catalog = st.catalog();
        let mut times: Vec<i64> = catalog
            .frames
            .iter()
            .filter(|f| f.product == v.product)
            .map(|f| f.epoch)
            .collect();
        times.sort_unstable();
        times.dedup();
        times
    }
}

/// The longest span that may be asked for at once.
///
/// Two days at five minutes is already 576 frames, so the interval does most
/// of the limiting; this is the outer bound on the scan and on how much a
/// single request can be made to consider. It is deliberately wider than the
/// retention period, so it never becomes the thing that stops you replaying
/// what you still have.
const MAX_RANGE_DAYS: i64 = 31;

/// The `from`/`to` pair, if the request carries a valid one.
///
/// Both must be present: half a range is a mistake worth reporting rather than
/// silently interpreting as a window.
fn range_from(q: &HashMap<String, String>) -> std::result::Result<Option<(i64, i64)>, String> {
    let parse = |k: &str| q.get(k).map(|s| s.parse::<i64>());
    match (parse("from"), parse("to")) {
        (None, None) => Ok(None),
        (Some(Ok(from)), Some(Ok(to))) => {
            if to <= from {
                return Err("the end of the range must be after its start".into());
            }
            /* Checked, because both ends come off the query string. `to - from`
            on i64::MIN and i64::MAX wraps in a release build, and the
            wrapped value is small enough to pass the span test - so the
            limit below was bypassed by asking for a range wide enough to
            overflow it. The same subtraction panics in a debug build. */
            let span = to
                .checked_sub(from)
                .ok_or("that range is not a span of time")?;
            if span > MAX_RANGE_DAYS * 86400 {
                return Err(format!("a range may span at most {MAX_RANGE_DAYS} days"));
            }
            Ok(Some((from, to)))
        }
        (Some(Err(_)), _) | (_, Some(Err(_))) => {
            Err("from and to must be whole seconds since the epoch".into())
        }
        _ => Err("a range needs both from and to".into()),
    }
}

/// Fit a chosen range into the frame ceiling by coarsening, not by truncating.
///
/// A rolling window drops its oldest frames when there are too many, which is
/// right there - you asked for "the last N hours" and the newest end is the
/// point. A range is different: you named both ends, so cutting one off would
/// answer a different question. Stepping up the interval instead keeps the
/// whole span and reports the interval it settled on.
fn fit_range(times: Vec<i64>, allowed: &[i64], wanted: i64) -> (Vec<i64>, i64) {
    let mut coarsest = wanted;
    for candidate in allowed.iter().copied().filter(|s| *s >= wanted) {
        // Counted before truncation. Asking `thin` would be no use here: it
        // caps the length itself, so the answer would always look as though it
        // fitted and the interval would never step up.
        let kept = align(times.clone(), candidate);
        if kept.len() <= MAX_FRAMES {
            return (kept, candidate);
        }
        coarsest = candidate;
    }
    // Even the coarsest interval holds too many, so the newest that fit win -
    // the same rule the rolling window uses.
    (thin(times, coarsest), coarsest)
}

#[derive(Serialize)]
struct DayInfo {
    /// `YYYY-MM-DD`, UTC.
    day: String,
    /// How many slots that day holds, which is what makes a day worth offering:
    /// reception gaps leave days with a handful of frames rather than none.
    slots: usize,
    /// First and last slot within the day, so selecting it covers exactly what
    /// is there rather than a nominal midnight-to-midnight.
    first: i64,
    last: i64,
}

#[derive(Serialize)]
struct RangeResponse {
    /// Oldest and newest slot held for this layer and area, or null when the
    /// layer has nothing at all.
    first: Option<i64>,
    last: Option<i64>,
    first_iso: Option<String>,
    last_iso: Option<String>,
    slots: usize,
    max_days: i64,
    /// Every day that holds imagery, oldest first. The calendar marks these and
    /// refuses the rest, so a range cannot be drawn across emptiness.
    days: Vec<DayInfo>,
}

/// Group slot times into calendar days, UTC.
fn days_of(times: &[i64]) -> Vec<DayInfo> {
    let mut out: Vec<DayInfo> = Vec::new();
    for &t in times {
        let (y, mo, d, _, _) = civil_from_epoch(t);
        let day = format!("{y:04}-{mo:02}-{d:02}");
        match out.last_mut() {
            // The times arrive sorted, so a day is always still the last one.
            Some(prev) if prev.day == day => {
                prev.slots += 1;
                prev.last = t;
            }
            _ => out.push(DayInfo {
                day,
                slots: 1,
                first: t,
                last: t,
            }),
        }
    }
    out
}

/// What span of time this layer can actually be replayed over.
///
/// The date pickers are bounded by this rather than left open: the receiver
/// only holds a few days, so offering the whole calendar would mostly offer
/// emptiness.
async fn api_range(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<RangeResponse>, (StatusCode, String)> {
    let view_id = q.get("view").map(|s| s.as_str()).unwrap_or("live");
    let v = product::view(view_id)
        .ok_or((StatusCode::BAD_REQUEST, format!("unknown view {view_id}")))?;
    let Some(region) = region_from(&q) else {
        return Err((StatusCode::BAD_REQUEST, "unknown area".to_string()));
    };

    let times = available_times(&st, v, region);
    Ok(Json(RangeResponse {
        first: times.first().copied(),
        last: times.last().copied(),
        first_iso: times.first().map(|t| iso_for(*t)),
        last_iso: times.last().map(|t| iso_for(*t)),
        slots: times.len(),
        max_days: MAX_RANGE_DAYS,
        days: days_of(&times),
    }))
}

/// Intervals a layer can offer in a given area. The full-disc service repeats
/// every 15 minutes, so the globe cannot do 5.
fn steps_for(v: &product::View, region: &str) -> &'static [i64] {
    if !is_hrit(v.style) {
        &PRODUCT_STEPS
    } else if region == "globe" {
        &DISC_STEPS
    } else {
        &LIVE_STEPS
    }
}

#[derive(Serialize)]
struct LegendItem {
    color: String,
    label: String,
}

#[derive(Serialize)]
struct LegendResponse {
    kind: &'static str,
    items: Vec<LegendItem>,
    swatches: Vec<String>,
    lo: f32,
    hi: f32,
    units: String,
    title: String,
    note: String,
}

async fn api_legend(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<LegendResponse>, (StatusCode, String)> {
    let view_id = q.get("view").map(|s| s.as_str()).unwrap_or("live");
    let v = product::view(view_id)
        .ok_or((StatusCode::BAD_REQUEST, format!("unknown view {view_id}")))?;

    let blank = |kind, items, swatches, title: String, note: String| LegendResponse {
        kind,
        items,
        swatches,
        lo: v.lo,
        hi: v.hi,
        units: v.units.into(),
        title,
        note,
    };

    match v.style {
        Style::Composite => {
            let r = rgb::recipe(v.variable)
                .ok_or((StatusCode::BAD_REQUEST, "unknown recipe".to_string()))?;
            Ok(Json(blank(
                "categorical",
                r.key
                    .iter()
                    .map(|(color, label)| LegendItem {
                        color: (*color).into(),
                        label: (*label).into(),
                    })
                    .collect(),
                Vec::new(),
                r.label.into(),
                r.note.into(),
            )))
        }
        Style::Live => Ok(Json(blank(
            "categorical",
            live_legend()
                .into_iter()
                .map(|(color, label)| LegendItem {
                    color: color.into(),
                    label: label.into(),
                })
                .collect(),
            Vec::new(),
            "Natural colour".into(),
            "Ice cloud reads cyan: the 1.6 um band is absorbed by ice.".into(),
        ))),
        Style::Surface => Ok(Json(blank(
            "ramp",
            vec![LegendItem {
                color: "#2e323a".into(),
                label: "Hidden by cloud".into(),
            }],
            live::heat_swatches(32),
            "Surface temperature (K)".into(),
            "Clear-sky 10.8 um brightness temperature of the ground and sea.".into(),
        ))),
        Style::Ramp => Ok(Json(blank(
            "ramp",
            Vec::new(),
            render::ramp_swatches(v.units, 32),
            format!("Scale ({})", v.units),
            String::new(),
        ))),
        Style::Natural => Ok(Json(blank(
            "categorical",
            render::natural_legend()
                .into_iter()
                .map(|(color, label)| LegendItem { color, label })
                .collect(),
            Vec::new(),
            "Natural view".into(),
            String::new(),
        ))),
        Style::Classes => {
            let catalog = st.catalog();
            let newest = catalog
                .window(v.product, 48)
                .into_iter()
                .last()
                .map(|f| f.path.clone());

            let mut items = Vec::new();
            if let Some(path) = newest {
                let labels = product::class_labels(&path, v.variable);
                if let Ok(scene) = product::load(&path, v) {
                    if let product::Field::Categorical { palette, .. } = &scene.field {
                        for (i, name) in labels.iter().enumerate() {
                            let class = i + 1; // flag_values start at 1
                                               // The label list and the palette are separate
                                               // arrays in the file; nothing guarantees they
                                               // agree, so a short palette must not panic.
                            let Some(&entry) = palette.get(class) else {
                                continue;
                            };
                            let mut c = entry;
                            if class == 2 && c == [0, 0, 0] {
                                c = [10, 28, 68];
                            }
                            items.push(LegendItem {
                                color: format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]),
                                label: name.clone(),
                            });
                        }
                    }
                }
            }
            Ok(Json(blank(
                "categorical",
                items,
                Vec::new(),
                "Cloud type".into(),
                String::new(),
            )))
        }
    }
}

/// What the natural-colour recipe renders each surface as.
fn live_legend() -> Vec<(&'static str, &'static str)> {
    vec![
        ("#0d1a2e", "Sea"),
        ("#4c6b32", "Vegetation"),
        ("#a98a5e", "Bare ground / desert"),
        ("#f2f4f7", "Water cloud"),
        ("#6fe3e0", "Ice cloud / snow"),
        ("#2a3446", "Night (10.8 um infrared)"),
    ]
}

async fn api_frame(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let view_id = q.get("view").map(|s| s.as_str()).unwrap_or("live");
    let Some(v) = product::view(view_id) else {
        return (StatusCode::BAD_REQUEST, "unknown view").into_response();
    };
    let Some(t) = q.get("t").and_then(|s| s.parse::<i64>().ok()) else {
        return (StatusCode::BAD_REQUEST, "missing t").into_response();
    };
    let Some(region) = region_from(&q) else {
        return (StatusCode::BAD_REQUEST, "unknown area").into_response();
    };
    let coast = q.get("coast").map(|s| s != "0").unwrap_or(true);
    let borders = q.get("borders").map(|s| s != "0").unwrap_or(true);
    let w = snap(
        q.get("w").and_then(|s| s.parse().ok()).unwrap_or(1100),
        200,
        3200,
    );
    let h = snap(
        q.get("h").and_then(|s| s.parse().ok()).unwrap_or(800),
        100,
        2400,
    );

    let canvas = canvas_from(region);
    let key = frame_key(&st, view_id, t, region, w, h, coast, borders);
    if let Some(png) = st.cached_frame(&key) {
        return png_response(png);
    }

    /* Rendering the same frame twice at once is pure waste - seconds of
    decompression each - and it happens easily: a second tab, a reload, or an
    export running beside the page. The first request through renders; the
    others wait on this lock and then find the frame in the cache. */
    let slot = st.render_slot(&key);
    let _guard = slot.lock().await;
    if let Some(png) = st.cached_frame(&key) {
        st.release_slot(&key);
        return png_response(png);
    }

    let opts = LiveOpts {
        canvas,
        width: w,
        height: h,
        graticule: true,
        coastline: coast,
        borders,
    };

    let rendered = match v.style {
        Style::Live => render_live(&st, t, region, opts).await,
        Style::Surface => render_surface(&st, t, region, opts).await,
        Style::Composite => render_composite(&st, v, t, region, opts).await,
        _ => render_product(&st, v, t, opts).await,
    };

    let response = match rendered {
        Ok(png) => {
            let png = Bytes::from(png);
            st.store_frame(key.clone(), png.clone());
            png_response(png)
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    drop(_guard);
    st.release_slot(&key);
    response
}

/// Everything the NWC SAF products contribute to an HRIT frame: the geography
/// mask for coastlines and the cloud classification for the surface view.
struct Auxiliary {
    grid: GeosGrid,
    ct: Vec<u8>,
    conditions: Conditions,
}

fn load_auxiliary(path: &Path) -> Option<Auxiliary> {
    let view = product::view("cloudtype")?;
    let scene = product::load(path, view).ok()?;
    let product::Field::Categorical { data, .. } = scene.field else {
        return None;
    };
    let conditions = product::load_conditions(path).ok()?;
    Some(Auxiliary {
        grid: scene.grid,
        ct: data,
        conditions,
    })
}

/// How far from an HRIT time a NWC SAF slot may be and still describe it.
///
/// The products arrive every 15 minutes against Rapid Scan's 5, so a perfect
/// match is not guaranteed and some tolerance is needed. But the two services
/// fail independently: this receiver's own history has a 40-hour hole in the
/// products while the imagery kept arriving. Without a bound the nearest slot
/// could be a day away, and the surface view would mask cloud using a cloud
/// mask from a different day - wrong, and with nothing on screen to say so.
/// Beyond this, no mask is better than the wrong mask.
const MAX_CT_DISTANCE: i64 = 30 * 60;

/// The NWC SAF slot nearest an HRIT time, if one is close enough to describe
/// the same sky.
fn nearest_ct(st: &AppState, t: i64) -> Option<PathBuf> {
    let catalog = st.catalog();
    let times: Vec<(i64, &Path)> = catalog
        .frames
        .iter()
        .filter(|f| f.product == "CT")
        .map(|f| (f.epoch, f.path.as_path()))
        .collect();
    nearest_within(&times, t, MAX_CT_DISTANCE).map(|p| p.to_path_buf())
}

/// The entry closest to `t`, or nothing if even the closest is too far.
fn nearest_within<'a>(times: &[(i64, &'a Path)], t: i64, max: i64) -> Option<&'a Path> {
    times
        .iter()
        .min_by_key(|(epoch, _)| (epoch - t).abs())
        .filter(|(epoch, _)| (epoch - t).abs() <= max)
        .map(|(_, p)| *p)
}

/// Multi-channel infrared composites: airmass, dust, night microphysics.
async fn render_composite(
    st: &Arc<AppState>,
    v: &'static product::View,
    t: i64,
    region: &str,
    opts: LiveOpts,
) -> Result<Vec<u8>, String> {
    let recipe = rgb::recipe(v.variable).ok_or_else(|| format!("unknown recipe {}", v.variable))?;
    let slots = hrit_slots(st, region, &recipe.channels());
    let Some(slot) = slots.into_iter().find(|s| s.epoch == t) else {
        return Err("no such slot".into());
    };
    // The data-derived coastline only exists for the regional grid.
    let ct_path = if opts.coastline && !opts.canvas.is_disc() {
        nearest_ct(st, t)
    } else {
        None
    };
    let cache = st.hrit_cache.clone();
    let tool = st.decompressor.clone();
    let owner = st.clone();

    tokio::task::spawn_blocking(move || {
        let comp = rgb::load(&slot, recipe, &cache, tool.as_deref())?;
        hrit::prune_cache(&cache, HRIT_CACHE_BYTES);
        let aux = ct_path.as_deref().and_then(|p| owner.auxiliary(p));
        let pair = aux.as_ref().map(|a| (&a.grid, &a.conditions));
        rgb::render_png(
            &comp,
            recipe,
            pair,
            &rgb::CompositeOpts {
                canvas: opts.canvas,
                width: opts.width,
                height: opts.height,
                graticule: opts.graticule,
                coastline: opts.coastline,
                borders: opts.borders,
            },
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn render_live(
    st: &Arc<AppState>,
    t: i64,
    region: &str,
    opts: LiveOpts,
) -> Result<Vec<u8>, String> {
    let slots = hrit_slots(st, region, &live::REQUIRED);
    let Some(slot) = slots.into_iter().find(|s| s.epoch == t) else {
        return Err("no such slot".into());
    };
    let ct_path = if opts.coastline && !opts.canvas.is_disc() {
        nearest_ct(st, t)
    } else {
        None
    };
    let cache = st.hrit_cache.clone();
    let tool = st.decompressor.clone();
    let owner = st.clone();

    tokio::task::spawn_blocking(move || {
        let scene = live::load(&slot, &cache, tool.as_deref())?;
        hrit::prune_cache(&cache, HRIT_CACHE_BYTES);
        let aux = ct_path.as_deref().and_then(|p| owner.auxiliary(p));
        let pair = aux.as_ref().map(|a| (&a.grid, &a.conditions));
        live::render_png(&scene, pair, &opts)
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn render_surface(
    st: &Arc<AppState>,
    t: i64,
    region: &str,
    opts: LiveOpts,
) -> Result<Vec<u8>, String> {
    let slots = hrit_slots(st, region, &[live::NIGHT]);
    let Some(slot) = slots.into_iter().find(|s| s.epoch == t) else {
        return Err("no such slot".into());
    };
    // The cloud mask is not optional here: without it, cold cloud tops would be
    // painted as freezing ground.
    let ct_path = nearest_ct(st, t);
    let cache = st.hrit_cache.clone();
    let tool = st.decompressor.clone();
    let owner = st.clone();

    tokio::task::spawn_blocking(move || {
        let scene = live::load_surface(&slot, &cache, tool.as_deref())?;
        hrit::prune_cache(&cache, HRIT_CACHE_BYTES);
        let aux = ct_path.as_deref().and_then(|p| owner.auxiliary(p));
        let cloud = aux.as_ref().map(|a| (&a.grid, a.ct.as_slice()));
        let geo = aux.as_ref().map(|a| (&a.grid, &a.conditions));
        live::render_surface_png(&scene, cloud, geo, &opts)
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn render_product(
    st: &Arc<AppState>,
    v: &'static product::View,
    t: i64,
    opts: LiveOpts,
) -> Result<Vec<u8>, String> {
    let catalog = st.catalog();
    let Some(frame) = catalog
        .frames
        .iter()
        .find(|f| f.product == v.product && f.epoch == t)
        .cloned()
    else {
        return Err("no such frame".into());
    };

    let needs_conditions = opts.coastline || v.style == Style::Natural;
    let ct_path: Option<PathBuf> = if needs_conditions {
        catalog
            .frames
            .iter()
            .find(|f| f.product == "CT" && f.epoch == t)
            .map(|f| f.path.clone())
    } else {
        None
    };

    let render_opts = RenderOpts {
        bbox: opts.canvas.bbox(),
        width: opts.width,
        height: opts.height,
        graticule: opts.graticule,
        coastline: opts.coastline,
        borders: opts.borders,
        style: v.style,
    };

    let path = frame.path.clone();
    let view = *v;
    tokio::task::spawn_blocking(move || {
        let scene = product::load(&path, &view).map_err(|e| e.to_string())?;
        let cond = ct_path.as_deref().and_then(load_conditions_quietly);
        if view.style == Style::Natural && cond.is_none() {
            return Err("the natural view needs ct_conditions from the CT product".into());
        }
        render::render_png(&scene, cond.as_ref(), &render_opts)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The coastline is an enhancement; a file that will not yield conditions
/// should not take the whole frame down with it.
fn load_conditions_quietly(path: &Path) -> Option<Conditions> {
    match product::load_conditions(path) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("conditions unavailable for {}: {e}", path.display());
            None
        }
    }
}

#[derive(Serialize)]
struct NativeResponse {
    w: usize,
    h: usize,
    lat_min: f64,
    lat_max: f64,
    lon_min: f64,
    lon_max: f64,
    /// Sub-satellite longitude in degrees, and the disc window when there is
    /// one, so the page can place a marker with the same projection the server
    /// drew with.
    sub_lon: f64,
    disc: Option<[f64; 3]>,
}

/// The resolution at which one output pixel equals one source sample.
async fn api_native(Query(q): Query<HashMap<String, String>>) -> Json<NativeResponse> {
    let region = region_from(&q).unwrap_or("europe");
    let view_id = q.get("view").map(|s| s.as_str()).unwrap_or("live");
    // Rapid Scan sits at 9.5 degE; the full-disc service and the NWC SAF
    // products both come from 0 deg.
    let sub_lon = match product::view(view_id) {
        Some(v) if is_hrit(v.style) && region != "globe" => 9.5f64.to_radians(),
        _ => 0.0,
    };
    // A free window reports its own extent, so "Native" and the location marker
    // follow the viewer wherever it has panned to.
    let canvas = canvas_from(region);
    let (bb, disc) = match canvas {
        Canvas::LatLon(bb) => (bb, None),
        Canvas::Disc {
            half_deg,
            cx_deg,
            cy_deg,
        } => (
            BBox {
                lat_min: -80.0,
                lat_max: 80.0,
                lon_min: -80.0,
                lon_max: 80.0,
            },
            Some([cx_deg, cy_deg, half_deg]),
        ),
    };
    let (w, h) = geo::native_span(&bb, sub_lon, geo::SEVIRI_PX_PER_RAD);
    Json(NativeResponse {
        w,
        h,
        lat_min: bb.lat_min,
        lat_max: bb.lat_max,
        lon_min: bb.lon_min,
        lon_max: bb.lon_max,
        sub_lon: sub_lon.to_degrees(),
        disc,
    })
}

/// Frames in an export are capped: an animated PNG holds every frame in full,
/// so the file grows quickly.
const MAX_ANIM_FRAMES: usize = 120;

/// How much RGBA the export may hold at once.
///
/// Frames are rendered and encoded a batch at a time rather than all together,
/// so this bounds the peak whatever the window: at full size a single frame is
/// 30 MB, and the old all-at-once path would have wanted gigabytes.
const ANIM_BATCH_BYTES: usize = 256 * 1024 * 1024;

/// Render a window as one animated PNG.
async fn api_animation(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let view_id = q.get("view").map(|s| s.as_str()).unwrap_or("live");
    let Some(v) = product::view(view_id) else {
        return (StatusCode::BAD_REQUEST, "unknown view").into_response();
    };
    let hours: i64 = q
        .get("hours")
        .and_then(|s| s.parse().ok())
        .filter(|h| WINDOWS.contains(h))
        .unwrap_or(6);
    let Some(region) = region_from(&q) else {
        return (StatusCode::BAD_REQUEST, "unknown area").into_response();
    };
    let allowed: &[i64] = steps_for(v, region);
    let step: i64 = q
        .get("step")
        .and_then(|s| s.parse().ok())
        .filter(|s| allowed.contains(s))
        .unwrap_or(allowed[allowed.len() - 1]);
    // GIF plays everywhere; APNG keeps full colour but only animates in a
    // browser, so it is opt-in.
    let want_apng = q.get("format").map(|s| s == "apng").unwrap_or(false);
    let coast = q.get("coast").map(|s| s != "0").unwrap_or(true);
    let borders = q.get("borders").map(|s| s != "0").unwrap_or(true);
    /* The same bounds as a single frame, deliberately. They used to be lower,
    which meant a page rendering at 2000 x 1100 silently got a 1600-wide
    export - and, worse, matched none of the frames already in the cache, so
    every one was rendered again. Sharing the bounds keeps the export the
    size that is on screen and lets it reuse those renders. */
    let w = snap(
        q.get("w").and_then(|s| s.parse().ok()).unwrap_or(900),
        200,
        3200,
    );
    let h = snap(
        q.get("h").and_then(|s| s.parse().ok()).unwrap_or(600),
        100,
        2400,
    );
    let fps: u16 = q
        .get("fps")
        .and_then(|s| s.parse().ok())
        .unwrap_or(8)
        .clamp(1, 24);

    // Selected the same way as the timeline, so an export saves the span that
    // is on screen rather than a differently-chosen one.
    let range = match range_from(&q) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let times: Vec<i64> = match range {
        Some((from, to)) => {
            let within: Vec<i64> = available_times(&st, v, region)
                .into_iter()
                .filter(|t| *t >= from && *t <= to)
                .collect();
            fit_range(within, allowed, step).0
        }
        None => {
            let all = available_times(&st, v, region);
            match all.last().copied() {
                Some(newest) => thin(
                    all.into_iter()
                        .filter(|t| *t >= newest - hours * 3600)
                        .collect(),
                    step,
                ),
                None => Vec::new(),
            }
        }
    };

    if times.is_empty() {
        return (StatusCode::NOT_FOUND, "no frames in this window").into_response();
    }
    // Keep the most recent frames when the window holds more than the cap.
    let times: Vec<i64> = times
        .into_iter()
        .rev()
        .take(MAX_ANIM_FRAMES)
        .rev()
        .collect();

    let opts = LiveOpts {
        canvas: canvas_from(region),
        width: w,
        height: h,
        graticule: true,
        coastline: coast,
        borders,
    };

    let st2 = st.clone();
    let view = *v;
    let built = tokio::task::spawn_blocking(move || {
        let anim = Animation {
            width: w,
            height: h,
            delay_ms: 1000 / fps.max(1),
        };
        let format = if want_apng {
            eumet_stream::anim::Format::Apng
        } else {
            eumet_stream::anim::Format::Gif
        };
        let mut writer = anim.writer(format, times.len())?;

        // How many frames may be in hand at once, and therefore how many are
        // rendered in parallel.
        let per_frame = eumet_stream::anim::frame_bytes(w, h).max(1);
        let batch = (ANIM_BATCH_BYTES / per_frame).clamp(1, 16);

        for chunk in times.chunks(batch) {
            let mut rgba: Vec<Option<Result<Vec<u8>, String>>> =
                (0..chunk.len()).map(|_| None).collect();
            /* Frames already on screen are in the cache as encoded PNGs.
            Decoding one back to RGBA costs milliseconds against seconds to
            re-render it, so an export of what you were just watching is
            nearly free - and the ones that do need rendering are built
            across the cores rather than one after another. */
            std::thread::scope(|scope| {
                for (slot, &t) in rgba.iter_mut().zip(chunk) {
                    let st = &st2;
                    let opts = &opts;
                    let view = &view;
                    scope.spawn(move || {
                        let key = frame_key(st, view.id, t, region, w, h, coast, borders);
                        let cached = st.cached_frame(&key);
                        *slot = Some(match cached.and_then(|png| decode_rgba(&png, w, h)) {
                            Some(r) => Ok(r),
                            None => render_frame_rgba(st, view, t, opts),
                        });
                    });
                }
            });

            let mut ready = Vec::with_capacity(chunk.len());
            for r in rgba {
                ready.push(r.unwrap_or_else(|| Err("frame was not rendered".into()))?);
            }
            writer.push(&mut ready)?;
            // Freed before the next batch is built, which is what keeps the
            // peak to one batch rather than the whole window.
            drop(ready);
        }
        writer.finish()
    })
    .await;

    let (mime, ext) = if want_apng {
        ("image/apng", "png")
    } else {
        ("image/gif", "gif")
    };

    match built {
        Ok(Ok(bytes)) => (
            [
                (header::CONTENT_TYPE, mime.to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"eumet-{view_id}-{region}-{hours}h.{ext}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// The canvas a request asks for.
fn canvas_from(region: &str) -> Canvas {
    Canvas::named(region)
}

/// The areas that exist. Anything else is a mistake worth reporting.
const REGIONS: [&str; 3] = ["europe", "wide", "globe"];

/// The area a request names, as one of the known ones.
///
/// An unknown name used to fall through to Europe. That hid typos, and - since
/// the name goes into the cache key verbatim - it also meant `bbox=anything`
/// minted a fresh render and a fresh cache file for a picture already held
/// under a different name. Unbounded distinct keys is a poor property for a
/// server that renders on demand and is reachable from the network.
fn region_from(q: &HashMap<String, String>) -> Option<&'static str> {
    let asked = q.get("bbox").map(|s| s.as_str()).unwrap_or("europe");
    REGIONS.iter().copied().find(|r| *r == asked)
}

/// Output sizes are snapped to this grid.
///
/// The page already asks in multiples of 100 so its requests are unchanged.
/// Doing it here as well is what bounds the work: without it the accepted range
/// is about 3000 by 2300 distinct sizes, each one a full render of seconds and
/// its own multi-megabyte cache entry, all of the same picture. Snapping takes
/// that to a few hundred combinations.
const SIZE_STEP: usize = 100;

fn snap(v: usize, lo: usize, hi: usize) -> usize {
    let rounded = ((v + SIZE_STEP / 2) / SIZE_STEP) * SIZE_STEP;
    rounded.clamp(lo, hi)
}

/// Everything that decides what a rendered frame looks like.
///
/// The build stamp is part of it so that changing the rendering code retires
/// the frames the old code left on disk instead of serving them for days.
#[allow(clippy::too_many_arguments)]
fn frame_key(
    st: &AppState,
    view_id: &str,
    t: i64,
    region: &str,
    w: usize,
    h: usize,
    coast: bool,
    borders: bool,
) -> String {
    format!(
        "{}|{view_id}|{t}|{region}|{w}x{h}|c{}b{}",
        st.build, coast as u8, borders as u8
    )
}

/// Decode one of our own cached PNGs back to RGBA.
fn decode_rgba(png: &[u8], w: usize, h: usize) -> Option<Vec<u8>> {
    let decoder = png::Decoder::new(png);
    let mut reader = decoder.read_info().ok()?;
    let info = reader.info();
    if info.width as usize != w || info.height as usize != h {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).ok()?;
    if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(frame.buffer_size());
    Some(buf)
}

/// Render one frame as raw RGBA, for the animation encoder.
fn render_frame_rgba(
    st: &AppState,
    v: &product::View,
    t: i64,
    opts: &LiveOpts,
) -> Result<Vec<u8>, String> {
    match v.style {
        Style::Live | Style::Surface | Style::Composite => {
            let region = if opts.canvas.is_disc() {
                "globe"
            } else {
                "europe"
            };
            let slots = hrit_slots(st, region, &channels_for(v));
            let slot = slots
                .into_iter()
                .find(|s| s.epoch == t)
                .ok_or_else(|| format!("no HRIT slot at {t}"))?;
            let aux = if opts.canvas.is_disc() {
                None
            } else {
                nearest_ct(st, t).as_deref().and_then(|p| st.auxiliary(p))
            };
            let geo_pair = aux.as_ref().map(|a| (&a.grid, &a.conditions));

            match v.style {
                Style::Live => {
                    let scene = live::load(&slot, &st.hrit_cache, st.decompressor.as_deref())?;
                    live::render_rgba(&scene, geo_pair, opts)
                }
                Style::Surface => {
                    let scene =
                        live::load_surface(&slot, &st.hrit_cache, st.decompressor.as_deref())?;
                    let cloud = aux.as_ref().map(|a| (&a.grid, a.ct.as_slice()));
                    live::render_surface_rgba(&scene, cloud, geo_pair, opts)
                }
                _ => {
                    let recipe = rgb::recipe(v.variable)
                        .ok_or_else(|| format!("unknown recipe {}", v.variable))?;
                    let comp =
                        rgb::load(&slot, recipe, &st.hrit_cache, st.decompressor.as_deref())?;
                    rgb::render_rgba(
                        &comp,
                        recipe,
                        geo_pair,
                        &rgb::CompositeOpts {
                            canvas: opts.canvas,
                            width: opts.width,
                            height: opts.height,
                            graticule: opts.graticule,
                            coastline: opts.coastline,
                            borders: opts.borders,
                        },
                    )
                }
            }
        }
        _ => {
            let catalog = st.catalog();
            let frame = catalog
                .frames
                .iter()
                .find(|f| f.product == v.product && f.epoch == t)
                .ok_or_else(|| format!("no frame at {t}"))?;
            let ct_path = catalog
                .frames
                .iter()
                .find(|f| f.product == "CT" && f.epoch == t)
                .map(|f| f.path.clone());
            let scene = product::load(&frame.path, v).map_err(|e| e.to_string())?;
            let cond = ct_path.as_deref().and_then(load_conditions_quietly);
            let render_opts = RenderOpts {
                bbox: opts.canvas.bbox(),
                width: opts.width,
                height: opts.height,
                graticule: opts.graticule,
                coastline: opts.coastline,
                borders: opts.borders,
                style: v.style,
            };
            render::render_rgba(&scene, cond.as_ref(), &render_opts)
        }
    }
}

/// `Bytes` is handed to the response body by reference count, so a frame served
/// from the cache is never copied however many times it goes out.
fn png_response(png: Bytes) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        png,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[test]
    fn the_nearest_product_slot_wins() {
        let times = [(1000, p("early.nc")), (2000, p("late.nc"))];
        assert_eq!(
            nearest_within(&times, 1900, MAX_CT_DISTANCE),
            Some(p("late.nc"))
        );
        assert_eq!(
            nearest_within(&times, 1100, MAX_CT_DISTANCE),
            Some(p("early.nc"))
        );
    }

    /// The guard that matters: the two services fail independently, so a
    /// product slot from hours away must not be used to mask an image.
    #[test]
    fn a_distant_product_slot_is_refused() {
        let times = [(0, p("yesterday.nc"))];
        // Fifteen minutes is the product cadence and has to be accepted.
        assert!(nearest_within(&times, 15 * 60, MAX_CT_DISTANCE).is_some());
        assert!(nearest_within(&times, MAX_CT_DISTANCE, MAX_CT_DISTANCE).is_some());
        // A minute past the bound, and a whole day, are not.
        assert!(nearest_within(&times, MAX_CT_DISTANCE + 60, MAX_CT_DISTANCE).is_none());
        assert!(nearest_within(&times, 24 * 3600, MAX_CT_DISTANCE).is_none());
        // Symmetric: a slot from the future is no better.
        assert!(nearest_within(&times, -24 * 3600, MAX_CT_DISTANCE).is_none());
    }

    /// The memory cache is bounded by bytes, not entries: a fixed count means
    /// anything between a few tens of megabytes and most of a gigabyte,
    /// depending on whether the frames are small European ones or full discs.
    #[test]
    fn the_memory_cache_holds_a_byte_budget() {
        let mut c = Cache::new(1000);
        let frame = |n: usize| Bytes::from(vec![0u8; n]);
        c.put("a".into(), frame(400));
        c.put("b".into(), frame(400));
        assert!(c.get("a").is_some() && c.get("b").is_some());

        // The third eviction-worthy frame pushes the oldest out.
        c.put("c".into(), frame(400));
        assert!(c.get("a").is_none(), "the oldest should have gone");
        assert!(c.get("b").is_some() && c.get("c").is_some());
        assert!(c.bytes <= 1000);

        // One frame larger than the whole budget is simply not kept, rather
        // than emptying the cache to make room it still would not fit in.
        c.put("huge".into(), frame(5000));
        assert!(c.get("huge").is_none());
        assert!(c.get("c").is_some(), "the cache should be untouched");
    }

    #[test]
    fn no_product_slots_at_all_is_not_an_error() {
        assert!(nearest_within(&[], 1000, MAX_CT_DISTANCE).is_none());
    }

    /// A range names both ends, so fitting it to the frame ceiling must
    /// coarsen the interval rather than cut an end off. The first version
    /// asked `thin`, which caps the length itself - so the count always looked
    /// as though it fitted and the interval never stepped up.
    #[test]
    fn an_oversized_range_coarsens_instead_of_truncating() {
        let allowed = [5i64, 10, 15, 30, 60];
        // Five days at five minutes: 1441 slots, well over the ceiling.
        let times: Vec<i64> = (0..1441).map(|i| i * 300).collect();

        let (kept, step) = fit_range(times.clone(), &allowed, 5);
        assert!(
            step > 5,
            "the interval should have stepped up, stayed at {step}"
        );
        assert!(
            kept.len() <= MAX_FRAMES,
            "{} frames is over the ceiling",
            kept.len()
        );
        assert_eq!(
            kept.first(),
            times.first(),
            "the start of the range was cut off"
        );
        assert_eq!(
            kept.last(),
            times.last(),
            "the end of the range was cut off"
        );
    }

    /// The longest span anyone can ask for must actually come back whole. It
    /// did not before the coarse intervals existed: the ladder stopped at
    /// hourly, a month of full-disc slots is 720 frames, and the range was cut
    /// to the newest 400 - 16.6 days presented as 30.
    #[test]
    fn the_longest_range_spans_its_whole_self() {
        for (name, ladder) in [
            ("globe", &DISC_STEPS[..]),
            ("regional", &LIVE_STEPS[..]),
            ("products", &PRODUCT_STEPS[..]),
        ] {
            let cadence = ladder[0] * 60;
            let slots = MAX_RANGE_DAYS * 86400 / cadence;
            let times: Vec<i64> = (0..slots).map(|i| i * cadence).collect();
            let (kept, step) = fit_range(times.clone(), ladder, ladder[0]);

            assert!(
                kept.len() <= MAX_FRAMES,
                "{name}: {} frames is over the ceiling",
                kept.len()
            );
            /* Aligning to a coarse interval moves each end inward by less than
            one interval - that is quantisation, and unavoidable. What must
            not happen is truncation: before the coarse steps existed a
            month came back as its newest 16.6 days. */
            let slack = step * 60;
            let lost_at_start = kept.first().unwrap() - times.first().unwrap();
            let lost_at_end = times.last().unwrap() - kept.last().unwrap();
            assert!(
                lost_at_start < slack && lost_at_end < slack,
                "{name}: a {MAX_RANGE_DAYS}-day range at {step} min lost {} h at the start \n                 and {} h at the end",
                lost_at_start / 3600,
                lost_at_end / 3600
            );
        }
    }

    /// A range that already fits keeps the interval that was asked for.
    #[test]
    fn a_range_that_fits_is_left_alone() {
        let allowed = [5i64, 10, 15, 30, 60];
        let times: Vec<i64> = (0..100).map(|i| i * 300).collect();
        let (kept, step) = fit_range(times.clone(), &allowed, 5);
        assert_eq!(step, 5);
        assert_eq!(kept.len(), times.len());
    }

    /// Frames must not outlive the code that drew them: the key carries a
    /// build stamp, so a rebuilt server cannot serve the old renders.
    #[test]
    fn the_cache_key_separates_builds() {
        let key = |build: &str| format!("{build}|live|1000|europe|800x600|c1b1");
        assert_ne!(key("0.1.0.aaaa"), key("0.1.0.bbbb"));
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;

    fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Both ends come off the query string, so the span between them has to be
    /// computed without trusting them to be sane. Subtracting the extremes
    /// wraps in a release build, and the wrapped value is small enough to slip
    /// past the limit - which is how a request for the whole of representable
    /// time was served.
    #[test]
    fn a_span_that_overflows_is_refused() {
        let extremes = q(&[
            ("from", &i64::MIN.to_string()),
            ("to", &i64::MAX.to_string()),
        ]);
        assert!(
            range_from(&extremes).is_err(),
            "an overflowing span was accepted"
        );

        // Large but representable is refused by the limit itself.
        let wide = q(&[("from", "0"), ("to", "8000000000")]);
        assert!(range_from(&wide).is_err());

        // And an ordinary range still works.
        let ok = q(&[("from", "1786611600"), ("to", "1786698000")]);
        assert_eq!(range_from(&ok).unwrap(), Some((1786611600, 1786698000)));
    }

    #[test]
    fn half_a_range_is_a_mistake_not_a_window() {
        assert!(range_from(&q(&[("from", "1000")])).is_err());
        assert!(range_from(&q(&[("to", "1000")])).is_err());
        assert!(range_from(&q(&[])).unwrap().is_none());
        assert!(range_from(&q(&[("from", "x"), ("to", "1")])).is_err());
        assert!(range_from(&q(&[("from", "9"), ("to", "9")])).is_err());
    }
}
