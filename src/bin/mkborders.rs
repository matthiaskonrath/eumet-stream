//! One-off converter: Natural Earth GeoJSON -> the compact form the viewer
//! embeds.
//!
//! The published files are global and full precision. Coordinates are rounded
//! to about 100 m and the result is written as plain text, so the server needs
//! no JSON parsing at run time.
//!
//!   # regional borders, clipped to the area the products cover
//!   cargo run --bin mkborders -- assets/borders.txt --clip \
//!       assets/ne_50m_admin_0_boundary_lines_land.geojson
//!
//!   # global coastline and borders for the full-disc view
//!   cargo run --bin mkborders -- assets/globe.txt \
//!       assets/ne_110m_coastline.geojson \
//!       assets/ne_110m_admin_0_boundary_lines_land.geojson

use serde_json::Value;
use std::fmt::Write as _;

/// Generous clip around the regional windows.
const LON_MIN: f64 = -72.0;
const LON_MAX: f64 = 72.0;
const LAT_MIN: f64 = 18.0;
const LAT_MAX: f64 = 86.0;

fn inside(lon: f64, lat: f64) -> bool {
    (LON_MIN..=LON_MAX).contains(&lon) && (LAT_MIN..=LAT_MAX).contains(&lat)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: mkborders <out.txt> [--clip] <in.geojson>...");
        std::process::exit(2);
    }
    let dst = args[0].clone();
    let clip = args.iter().any(|a| a == "--clip");
    let sources: Vec<&String> = args[1..].iter().filter(|a| !a.starts_with("--")).collect();

    let mut lines: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut features = 0usize;

    for src in &sources {
        let text = match std::fs::read_to_string(src) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{src}: {e}");
                std::process::exit(1);
            }
        };
        let root: Value = serde_json::from_str(&text).expect("parse geojson");
        let feats = root["features"].as_array().expect("features array");
        features += feats.len();

        for f in feats {
            let geom = &f["geometry"];
            let coords = &geom["coordinates"];
            match geom["type"].as_str().unwrap_or("") {
                "LineString" => collect(coords, clip, &mut lines),
                "MultiLineString" => {
                    if let Some(parts) = coords.as_array() {
                        for part in parts {
                            collect(part, clip, &mut lines);
                        }
                    }
                }
                other => eprintln!("skipping geometry type {other}"),
            }
        }
    }

    let mut out = String::new();
    let mut vertices = 0usize;
    for line in &lines {
        for (i, (lon, lat)) in line.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let _ = write!(out, "{lon:.3} {lat:.3}");
        }
        out.push('\n');
        vertices += line.len();
    }

    std::fs::write(&dst, &out).expect("write");
    println!(
        "{} source(s), {features} features -> {} polylines, {vertices} vertices, {} KB",
        sources.len(),
        lines.len(),
        out.len() / 1024
    );
}

/// Split a line at the clip boundary, keeping the runs that fall inside.
fn collect(coords: &Value, clip: bool, out: &mut Vec<Vec<(f64, f64)>>) {
    let Some(points) = coords.as_array() else {
        return;
    };
    let mut run: Vec<(f64, f64)> = Vec::new();
    for p in points {
        let Some(pair) = p.as_array() else { continue };
        let (Some(lon), Some(lat)) = (
            pair.first().and_then(|v| v.as_f64()),
            pair.get(1).and_then(|v| v.as_f64()),
        ) else {
            continue;
        };
        if !clip || inside(lon, lat) {
            run.push((lon, lat));
        } else if !run.is_empty() {
            // Keep one point past the edge so the line reaches the border.
            run.push((lon, lat));
            if run.len() >= 2 {
                out.push(std::mem::take(&mut run));
            } else {
                run.clear();
            }
        }
    }
    if run.len() >= 2 {
        out.push(run);
    }
}
