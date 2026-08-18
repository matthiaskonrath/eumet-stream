//! Vector overlays: coastlines, country borders and the graticule.
//!
//! Nothing in a satellite product knows where one country stops and the next
//! begins, so political boundaries come from Natural Earth (public domain),
//! compacted by `cargo run --bin mkborders`. Two sets are embedded: a detailed
//! 1:50m one clipped to the regional windows, and a coarser global 1:110m one
//! (coastline and borders together) for the full-disc view.
//!
//! Everything is drawn by projecting each vertex through the canvas, so the
//! same code serves a latitude/longitude map and the satellite's own disc.

use crate::geo::Canvas;
use std::sync::OnceLock;

const REGIONAL: &str = include_str!("../assets/borders.txt");
const GLOBAL: &str = include_str!("../assets/globe.txt");

type Polyline = Vec<(f32, f32)>;

fn parse(data: &str) -> Vec<Polyline> {
    data.lines()
        .filter_map(|line| {
            let mut pts = Vec::new();
            let mut it = line.split_ascii_whitespace();
            while let (Some(a), Some(b)) = (it.next(), it.next()) {
                if let (Ok(lon), Ok(lat)) = (a.parse::<f32>(), b.parse::<f32>()) {
                    pts.push((lon, lat));
                }
            }
            (pts.len() >= 2).then_some(pts)
        })
        .collect()
}

fn regional() -> &'static Vec<Polyline> {
    static L: OnceLock<Vec<Polyline>> = OnceLock::new();
    L.get_or_init(|| parse(REGIONAL))
}

fn global() -> &'static Vec<Polyline> {
    static L: OnceLock<Vec<Polyline>> = OnceLock::new();
    L.get_or_init(|| parse(GLOBAL))
}

pub fn polyline_count() -> usize {
    regional().len() + global().len()
}

/// Meridians and parallels, as polylines dense enough to stay smooth once
/// projected onto a curved disc.
fn graticule() -> &'static Vec<Polyline> {
    static L: OnceLock<Vec<Polyline>> = OnceLock::new();
    L.get_or_init(|| {
        let mut out = Vec::new();
        let mut lon = -180.0f32;
        while lon <= 180.0 {
            let mut line = Vec::new();
            let mut lat = -85.0f32;
            while lat <= 85.0 {
                line.push((lon, lat));
                lat += 1.0;
            }
            out.push(line);
            lon += 10.0;
        }
        let mut lat = -80.0f32;
        while lat <= 80.0 {
            let mut line = Vec::new();
            let mut lon = -180.0f32;
            while lon <= 180.0 {
                line.push((lon, lat));
                lon += 1.0;
            }
            out.push(line);
            lat += 10.0;
        }
        out
    })
}

/// Draw a set of polylines through the canvas projection.
pub fn draw_lines(
    rgba: &mut [u8],
    w: usize,
    h: usize,
    canvas: &Canvas,
    sub_lon: f64,
    lines: &[Polyline],
    alpha: f32,
) {
    if w == 0 || h == 0 {
        return;
    }
    for line in lines {
        let mut prev: Option<(f64, f64)> = None;
        for &(lon, lat) in line {
            let here = canvas.project(lat as f64, lon as f64, w, h, sub_lon);
            if let (Some((x0, y0)), Some((x1, y1))) = (prev, here) {
                segment(rgba, w, h, x0, y0, x1, y1, alpha);
            }
            prev = here;
        }
    }
}

pub fn draw_borders(
    rgba: &mut [u8],
    w: usize,
    h: usize,
    canvas: &Canvas,
    sub_lon: f64,
    alpha: f32,
) {
    let set = if canvas.is_disc() {
        global()
    } else {
        regional()
    };
    draw_lines(rgba, w, h, canvas, sub_lon, set, alpha);
}

pub fn draw_graticule(
    rgba: &mut [u8],
    w: usize,
    h: usize,
    canvas: &Canvas,
    sub_lon: f64,
    alpha: f32,
) {
    draw_lines(rgba, w, h, canvas, sub_lon, graticule(), alpha);
}

/// Plot a straight run between two points, one pixel per step.
#[allow(clippy::too_many_arguments)]
fn segment(rgba: &mut [u8], w: usize, h: usize, x0: f64, y0: f64, x1: f64, y1: f64, alpha: f32) {
    let margin = 2.0;
    if (x0 < -margin && x1 < -margin)
        || (y0 < -margin && y1 < -margin)
        || (x0 > w as f64 + margin && x1 > w as f64 + margin)
        || (y0 > h as f64 + margin && y1 > h as f64 + margin)
    {
        return;
    }
    let dx = x1 - x0;
    let dy = y1 - y0;
    let steps = dx.abs().max(dy.abs()).ceil() as i64;
    if steps <= 0 {
        plot(rgba, w, h, x0, y0, alpha);
        return;
    }
    // A very long run means the line left the canvas and came back; skipping it
    // avoids drawing a streak straight across the image.
    if steps > (w + h) as i64 {
        return;
    }
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        plot(rgba, w, h, x0 + dx * t, y0 + dy * t, alpha);
    }
}

fn plot(rgba: &mut [u8], w: usize, h: usize, x: f64, y: f64, alpha: f32) {
    let (xi, yi) = (x.round(), y.round());
    if xi < 0.0 || yi < 0.0 {
        return;
    }
    let (xi, yi) = (xi as usize, yi as usize);
    if xi >= w || yi >= h {
        return;
    }
    let o = (yi * w + xi) * 4;
    // Leave empty space alone: an overlay only means something over imagery.
    if rgba[o + 3] == 0 {
        return;
    }
    crate::render::blend_ink(rgba, o, alpha, 200);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::BBox;

    #[test]
    fn both_datasets_parse() {
        assert!(regional().len() > 100, "regional: {}", regional().len());
        assert!(global().len() > 300, "global: {}", global().len());
        assert!(regional().iter().all(|l| l.len() >= 2));
        assert!(global().iter().all(|l| l.len() >= 2));
    }

    #[test]
    fn global_set_reaches_the_southern_hemisphere() {
        let south = global()
            .iter()
            .flatten()
            .filter(|(_, lat)| *lat < -20.0)
            .count();
        assert!(south > 100, "expected southern coastlines, found {south}");
    }

    #[test]
    fn transparent_pixels_are_left_alone() {
        let (w, h) = (64, 64);
        let mut rgba = vec![0u8; w * h * 4];
        let c = Canvas::LatLon(BBox::EUROPE);
        draw_borders(&mut rgba, w, h, &c, 0.0, 0.9);
        draw_graticule(&mut rgba, w, h, &c, 0.0, 0.9);
        assert!(rgba.iter().all(|&b| b == 0));
    }

    #[test]
    fn draws_over_imagery_on_both_canvases() {
        for canvas in [Canvas::LatLon(BBox::EUROPE), Canvas::FULL_DISC] {
            let (w, h) = (400, 300);
            let mut rgba = vec![120u8; w * h * 4];
            let before = rgba.clone();
            draw_borders(&mut rgba, w, h, &canvas, 0.0, 0.9);
            assert_ne!(rgba, before, "nothing drawn on {canvas:?}");
        }
    }
}
