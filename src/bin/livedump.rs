//! Render one natural-colour frame straight from the HRIT stream.
//!
//! usage: livedump <hrit dir> <out.png> [slot YYYYMMDDhhmm] [bbox]

use eumet_stream::geo::Canvas;
use eumet_stream::hrit;
use eumet_stream::live::{self, LiveOpts};
use std::path::Path;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .unwrap_or_else(|| r"E:\EUMETCast\received\bas\E1B-GEO-5".into());
    let out = args.next().unwrap_or_else(|| "live.png".into());
    let want = args.next();
    let bbox = args.next().unwrap_or_else(|| "europe".into());

    let slots = hrit::scan_slots(Path::new(&dir));
    println!("slots indexed: {}", slots.len());
    let expect = hrit::expected_segments(&slots, &live::REQUIRED);
    let usable: Vec<_> = slots
        .iter()
        .filter(|s| s.is_complete(&live::REQUIRED, expect))
        .collect();
    println!("segments per full image: {expect}");
    println!(
        "complete slots (all four channels + prologue): {}",
        usable.len()
    );
    if usable.is_empty() {
        eprintln!("nothing to render");
        std::process::exit(1);
    }

    let slot = match &want {
        Some(stamp) => usable
            .iter()
            .find(|s| s.stamp == *stamp)
            .unwrap_or_else(|| panic!("slot {stamp} not available")),
        None => usable.last().unwrap(),
    };
    println!("rendering slot {}", slot.stamp);

    let tool = hrit::find_decompressor();
    match &tool {
        Some(t) => println!("decompressor: {}", t.display()),
        None => {
            eprintln!("xRITDecompress not found - cannot read compressed pixel data");
            std::process::exit(1);
        }
    }

    let cache = std::env::temp_dir().join("eumet-stream-hrit");
    std::fs::create_dir_all(&cache).ok();

    // Report the segment plan, which controls how line numbers are anchored.
    if let Some(segs) = slot.segments.get(live::GREEN) {
        if let Some((seq, path)) = segs.iter().next() {
            let cache0 = std::env::temp_dir().join("eumet-stream-hrit");
            std::fs::create_dir_all(&cache0).ok();
            if let Ok(raw) = hrit::segment_bytes(path, &cache0, tool.as_deref()) {
                if let Ok(h) = hrit::Headers::parse(&raw) {
                    println!(
                        "segment {seq}: planned {}..{}, {} lines/segment, loff {}",
                        h.planned_start, h.planned_end, h.lines, h.loff
                    );
                }
            }
        }
    }

    let t0 = std::time::Instant::now();
    let scene = match live::load(slot, &cache, tool.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("load failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "loaded in {:?}: {} x {} px, lines {}..{}, sub_lon {:.2}deg, cfac {}, lfac {}, coff {}, loff {}",
        t0.elapsed(),
        scene.red.columns,
        scene.red.lines,
        scene.red.line_start,
        scene.red.line_start + scene.red.lines - 1,
        scene.sub_lon.to_degrees(),
        scene.red.cfac,
        scene.red.lfac,
        scene.red.coff,
        scene.red.loff
    );

    // A quick look at the counts, to catch a decode that produced noise.
    let mut lo = u16::MAX;
    let mut hi = 0u16;
    let mut sum = 0u64;
    for &c in &scene.green.counts {
        lo = lo.min(c);
        hi = hi.max(c);
        sum += c as u64;
    }
    println!(
        "VIS008 counts: min {lo} max {hi} mean {:.1}",
        sum as f64 / scene.green.counts.len() as f64
    );

    let opts = LiveOpts {
        canvas: Canvas::named(&bbox),
        width: 1200,
        height: 800,
        graticule: true,
        coastline: false,
        borders: true,
    };
    let t1 = std::time::Instant::now();
    match live::render_png(&scene, None, &opts) {
        Ok(png) => {
            std::fs::write(&out, &png).expect("write");
            println!(
                "rendered in {:?} -> {out} ({} bytes)",
                t1.elapsed(),
                png.len()
            );
        }
        Err(e) => {
            eprintln!("render failed: {e}");
            std::process::exit(1);
        }
    }
}
