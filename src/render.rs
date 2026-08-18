//! Turning a georeferenced field into a PNG of Europe.

use crate::geo::BBox;
use crate::product::{
    Conditions, Field, Scene, Style, LIGHT_NIGHT, LIGHT_TWILIGHT, SURF_COAST, SURF_LAND, SURF_SEA,
    SURF_SPACE,
};

pub struct RenderOpts {
    pub bbox: BBox,
    pub width: usize,
    pub height: usize,
    pub graticule: bool,
    pub coastline: bool,
    pub borders: bool,
    pub style: Style,
}

impl Default for RenderOpts {
    fn default() -> Self {
        RenderOpts {
            bbox: BBox::EUROPE,
            width: 1100,
            height: 800,
            graticule: true,
            coastline: true,
            borders: true,
            style: Style::Classes,
        }
    }
}

/// Colour stops interpolated to produce a continuous ramp.
type Stops = [(f32, [u8; 3])];

/// Cloud-top temperature, following the usual infrared enhancement: warm
/// surfaces stay dark and neutral, deep cold tops run through blue to white.
const TEMP_STOPS: &Stops = &[
    (0.00, [255, 245, 225]), // coldest / highest tops
    (0.12, [235, 130, 235]),
    (0.26, [120, 90, 230]),
    (0.42, [40, 150, 240]),
    (0.56, [60, 210, 215]),
    (0.70, [120, 200, 130]),
    (0.84, [190, 165, 110]),
    (1.00, [70, 55, 50]), // warmest / lowest
];

/// Cloud-top height: low cloud warm-toned, high cloud cold-toned.
const HEIGHT_STOPS: &Stops = &[
    (0.00, [60, 50, 45]),
    (0.18, [200, 150, 90]),
    (0.38, [235, 215, 130]),
    (0.58, [130, 210, 170]),
    (0.78, [80, 160, 235]),
    (1.00, [250, 250, 255]),
];

// Basemap colours for the natural view.
const SEA: [u8; 3] = [12, 38, 72];
const LAND: [u8; 3] = [52, 88, 46];

fn ramp(stops: &Stops, t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    for w in stops.windows(2) {
        let (a, ca) = w[0];
        let (b, cb) = w[1];
        if t >= a && t <= b {
            let f = if (b - a).abs() < f32::EPSILON {
                0.0
            } else {
                (t - a) / (b - a)
            };
            return [
                (ca[0] as f32 + (cb[0] as f32 - ca[0] as f32) * f) as u8,
                (ca[1] as f32 + (cb[1] as f32 - ca[1] as f32) * f) as u8,
                (ca[2] as f32 + (cb[2] as f32 - ca[2] as f32) * f) as u8,
            ];
        }
    }
    stops[stops.len() - 1].1
}

/// Pick the ramp that suits a field's units.
fn stops_for(units: &str) -> &'static Stops {
    match units {
        "m" => HEIGHT_STOPS,
        _ => TEMP_STOPS,
    }
}

/// Colour and opacity for a cloud-type class drawn over the basemap.
///
/// Opaque cloud hides the surface; thin cirrus lets it through. Snow and ice
/// are surface features rather than cloud, but they are genuinely white.
fn cloud_look(class: u8) -> Option<([u8; 3], f32)> {
    Some(match class {
        // Snow and ice are given a cyan cast rather than the white they really
        // are, so they stay distinguishable from cloud. This matches how ice
        // reads in the Live SEVIRI natural-colour view, where the 1.6 um band
        // is absorbed by ice and the same surfaces come out cyan for real.
        3 => ([146, 226, 236], 1.00),  // snow over land
        4 => ([104, 198, 224], 1.00),  // sea ice
        5 => ([224, 228, 233], 0.85),  // very low cloud
        6 => ([234, 238, 242], 0.90),  // low cloud
        7 => ([243, 245, 248], 0.93),  // mid-level cloud
        8 => ([250, 251, 253], 0.97),  // high opaque
        9 => ([255, 255, 255], 1.00),  // very high opaque
        10 => ([234, 238, 242], 0.45), // fractional
        11 => ([224, 232, 240], 0.30), // semitransparent thin
        12 => ([234, 240, 246], 0.50), // semitransparent moderately thick
        13 => ([243, 246, 250], 0.70), // semitransparent thick
        14 => ([240, 244, 248], 0.65), // semitransparent above low/medium
        15 => ([206, 232, 242], 0.60), // semitransparent above snow/ice
        _ => return None,              // 1 and 2 are cloud-free
    })
}

/// Brightness applied to the surface for each illumination state.
fn light_factor(light: u8) -> f32 {
    match light {
        LIGHT_NIGHT => 0.30,
        LIGHT_TWILIGHT => 0.60,
        _ => 1.0,
    }
}

/// Cloud stays more visible than the ground after dark.
fn cloud_light_factor(light: u8) -> f32 {
    match light {
        LIGHT_NIGHT => 0.45,
        LIGHT_TWILIGHT => 0.72,
        _ => 1.0,
    }
}

fn scale(c: [u8; 3], f: f32) -> [u8; 3] {
    [
        (c[0] as f32 * f) as u8,
        (c[1] as f32 * f) as u8,
        (c[2] as f32 * f) as u8,
    ]
}

/// Render a scene as an RGBA PNG. Areas with no data stay transparent so the
/// page background shows through.
pub fn render_png(
    scene: &Scene,
    cond: Option<&Conditions>,
    opts: &RenderOpts,
) -> Result<Vec<u8>, String> {
    let rgba = render_rgba(scene, cond, opts)?;
    encode_png(&rgba, opts.width, opts.height)
}

/// The same drawing, left as raw RGBA so an animation can pack many frames
/// into one file without a PNG round trip per frame.
pub fn render_rgba(
    scene: &Scene,
    cond: Option<&Conditions>,
    opts: &RenderOpts,
) -> Result<Vec<u8>, String> {
    let (w, h) = (opts.width, opts.height);
    let bb = opts.bbox;
    let mut rgba = vec![0u8; w * h * 4];

    let dlat = (bb.lat_max - bb.lat_min) / h as f64;
    let dlon = (bb.lon_max - bb.lon_min) / w as f64;
    let stops = stops_for(&scene.units);

    // Resolve every output pixel to a source cell once; the mapping is reused
    // for the field, the basemap and the coastline.
    let mut src: Vec<u32> = vec![u32::MAX; w * h];
    for y in 0..h {
        // Row 0 is the northern edge.
        let lat = bb.lat_max - (y as f64 + 0.5) * dlat;
        for x in 0..w {
            let lon = bb.lon_min + (x as f64 + 0.5) * dlon;
            if let Some(i) = scene.grid.sample_index(lat, lon) {
                src[y * w + x] = i as u32;
            }
        }
    }

    // Surface class per output pixel, for the basemap and coastline.
    let surf: Vec<u8> = match cond {
        Some(c) => src
            .iter()
            .map(|&i| {
                if i == u32::MAX {
                    SURF_SPACE
                } else {
                    c.surface.get(i as usize).copied().unwrap_or(SURF_SPACE)
                }
            })
            .collect(),
        None => Vec::new(),
    };

    for p in 0..w * h {
        let i = src[p];
        if i == u32::MAX {
            continue;
        }
        let i = i as usize;
        let o = p * 4;

        match &scene.field {
            Field::Natural { ct } => {
                let Some(c) = cond else { continue };
                let s = surf[p];
                if s == SURF_SPACE {
                    continue;
                }
                let light = c.light.get(i).copied().unwrap_or(0);
                let base = if s == SURF_SEA { SEA } else { LAND };
                let mut px = scale(base, light_factor(light));

                if let Some(&class) = ct.get(i) {
                    if let Some((cc, alpha)) = cloud_look(class) {
                        let cc = scale(cc, cloud_light_factor(light));
                        for k in 0..3 {
                            px[k] = (px[k] as f32 * (1.0 - alpha) + cc[k] as f32 * alpha) as u8;
                        }
                    }
                }
                rgba[o] = px[0];
                rgba[o + 1] = px[1];
                rgba[o + 2] = px[2];
                rgba[o + 3] = 255;
            }
            Field::Categorical {
                data,
                palette,
                fill,
            } => {
                if let Some(&v) = data.get(i) {
                    if v != *fill && v != 0 {
                        let c = palette[v as usize];
                        // The shipped palette paints cloud-free sea black,
                        // which is indistinguishable from space; give it a deep
                        // blue so the coastline reads.
                        let c = if v == 2 && c == [0, 0, 0] {
                            [10, 28, 68]
                        } else {
                            c
                        };
                        rgba[o] = c[0];
                        rgba[o + 1] = c[1];
                        rgba[o + 2] = c[2];
                        rgba[o + 3] = 255;
                    }
                }
            }
            Field::Continuous { data, lo, hi } => {
                if let Some(&v) = data.get(i) {
                    if v.is_finite() {
                        let c = ramp(stops, (v - lo) / (hi - lo));
                        rgba[o] = c[0];
                        rgba[o + 1] = c[1];
                        rgba[o + 2] = c[2];
                        rgba[o + 3] = 255;
                    }
                }
            }
        }
    }

    draw_overlays(
        &mut rgba,
        w,
        h,
        &crate::geo::Canvas::LatLon(bb),
        scene.grid.sub_lon,
        (opts.coastline && !surf.is_empty()).then_some(surf.as_slice()),
        opts.style,
        opts.borders,
        opts.graticule,
    );
    Ok(rgba)
}

/// Split an image into horizontal bands and fill them on separate threads.
///
/// Every output pixel is independent - its own inverse projection, sun angle
/// and channel samples - so the only shared state is the buffer, which is
/// handed out as disjoint slices. `f` receives the first row of its band and
/// the rows themselves.
/// `surf` carries one byte per pixel alongside the image, for the surface
/// classification the coastline is traced from. It is handed out in matching
/// bands so a worker can fill both without any sharing.
pub(crate) fn render_bands<F>(rgba: &mut [u8], surf: &mut [u8], w: usize, h: usize, f: F)
where
    F: Fn(usize, &mut [u8], &mut [u8]) + Sync,
{
    if w == 0 || h == 0 {
        return;
    }
    /* The two buffers are walked in lockstep, so a mismatch would not fail -
    `zip` stops at the shorter one and the rest of the image is silently left
    unpainted. Every caller allocates both from the same w and h; this is
    here so that a future one which does not fails a test rather than
    producing a picture with a blank band in it. */
    debug_assert_eq!(rgba.len(), w * h * 4, "rgba buffer does not match w * h");
    debug_assert_eq!(surf.len(), w * h, "surface buffer does not match w * h");
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 16);
    // One band per thread, but never so thin that the split costs more than it
    // saves on a small image.
    let rows_per_band = h.div_ceil(threads).max(16);

    if rows_per_band >= h {
        f(0, rgba, surf);
        return;
    }

    std::thread::scope(|s| {
        let mut y0 = 0usize;
        for (band, sband) in rgba
            .chunks_mut(rows_per_band * w * 4)
            .zip(surf.chunks_mut(rows_per_band * w))
        {
            let start = y0;
            y0 += band.len() / (w * 4);
            let f = &f;
            s.spawn(move || f(start, band, sband));
        }
    });
}

fn luminance(px: [u8; 3]) -> f32 {
    0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32
}

/// Ink that contrasts with whatever it is drawn over.
///
/// A single fixed line colour cannot work here: the cloud-type palette runs
/// from near-black sea to white cloud, and the temperature ramp is almost black
/// at its warm end, so any one choice disappears somewhere. Picking per pixel
/// guarantees the line stays visible over every background.
fn contrast_ink(px: [u8; 3]) -> [u8; 3] {
    if luminance(px) > 140.0 {
        [6, 10, 16]
    } else {
        [250, 252, 255]
    }
}

pub(crate) fn blend_ink(rgba: &mut [u8], o: usize, alpha: f32, min_alpha: u8) {
    let cur = [rgba[o], rgba[o + 1], rgba[o + 2]];
    let ink = contrast_ink(cur);
    for k in 0..3 {
        rgba[o + k] = (cur[k] as f32 * (1.0 - alpha) + ink[k] as f32 * alpha) as u8;
    }
    rgba[o + 3] = rgba[o + 3].max(min_alpha);
}

/// Trace the land/sea boundary in output space, so the line stays one pixel
/// wide whatever the zoom.
pub(crate) fn draw_coastline(rgba: &mut [u8], surf: &[u8], w: usize, h: usize, style: Style) {
    let is_land = |s: u8| s == SURF_LAND || s == SURF_COAST;
    let is_sea = |s: u8| s == SURF_SEA;

    // The natural basemap needs a lighter touch than the analysis palettes.
    let alpha = match style {
        Style::Natural => 0.62,
        _ => 0.88,
    };

    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            let p = y * w + x;
            if !is_land(surf[p]) {
                continue;
            }
            let touches_sea = is_sea(surf[p - 1])
                || is_sea(surf[p + 1])
                || is_sea(surf[p - w])
                || is_sea(surf[p + w]);
            if !touches_sea {
                continue;
            }
            blend_ink(rgba, p * 4, alpha, 210);
        }
    }
}

/// Apply every vector overlay to a finished frame.
///
/// Shared by all renderers so the regional maps and the full disc get the same
/// treatment, projected through whichever canvas they were drawn on.
#[allow(clippy::too_many_arguments)]
pub fn draw_overlays(
    rgba: &mut [u8],
    w: usize,
    h: usize,
    canvas: &crate::geo::Canvas,
    sub_lon: f64,
    surf: Option<&[u8]>,
    style: Style,
    borders: bool,
    graticule: bool,
) {
    if borders {
        crate::borders::draw_borders(rgba, w, h, canvas, sub_lon, 0.72);
    }
    // The data-derived coastline only exists for the regional grids; the disc
    // relies on the coastline carried in the global vector set instead.
    if let Some(surf) = surf {
        draw_coastline(rgba, surf, w, h, style);
    }
    if graticule {
        crate::borders::draw_graticule(rgba, w, h, canvas, sub_lon, 0.22);
    }
}

pub(crate) fn encode_png(rgba: &[u8], w: usize, h: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Colour swatches for the legend, as CSS-ready `#rrggbb` strings.
pub fn ramp_swatches(units: &str, n: usize) -> Vec<String> {
    let stops = stops_for(units);
    (0..n)
        .map(|i| {
            let c = ramp(stops, i as f32 / (n - 1).max(1) as f32);
            format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
        })
        .collect()
}

fn hex(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// Legend entries for the natural view.
///
/// Swatches are taken from `cloud_look` so the key always matches what is
/// actually painted.
pub fn natural_legend() -> Vec<(String, String)> {
    let look = |class: u8| hex(cloud_look(class).map(|(c, _)| c).unwrap_or([0, 0, 0]));
    vec![
        (hex(SEA), "Sea".into()),
        (hex(LAND), "Land".into()),
        (look(9), "Cloud".into()),
        (look(11), "Thin cirrus".into()),
        (look(3), "Snow over land".into()),
        (look(4), "Sea ice".into()),
        (hex(scale(LAND, 0.30)), "Night side".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_hits_its_endpoints() {
        assert_eq!(ramp(TEMP_STOPS, 0.0), TEMP_STOPS[0].1);
        assert_eq!(ramp(TEMP_STOPS, 1.0), TEMP_STOPS[TEMP_STOPS.len() - 1].1);
    }

    #[test]
    fn ramp_clamps_outside_range() {
        assert_eq!(ramp(TEMP_STOPS, -5.0), TEMP_STOPS[0].1);
        assert_eq!(ramp(TEMP_STOPS, 5.0), TEMP_STOPS[TEMP_STOPS.len() - 1].1);
    }

    #[test]
    fn cloud_free_classes_draw_no_cloud() {
        assert!(cloud_look(1).is_none());
        assert!(cloud_look(2).is_none());
        assert!(cloud_look(9).is_some());
    }

    /// Snow and ice must not be mistakable for cloud: give them a clear blue
    /// cast rather than the near-white they would otherwise share.
    #[test]
    fn ice_is_distinguishable_from_cloud() {
        let cloud = cloud_look(9).unwrap().0; // very high opaque cloud
        for ice in [3u8, 4] {
            let c = cloud_look(ice).unwrap().0;
            let cloud_cast = cloud[2] as i32 - cloud[0] as i32;
            let ice_cast = c[2] as i32 - c[0] as i32;
            assert!(
                ice_cast - cloud_cast > 40,
                "class {ice} is too close to cloud: {c:?} vs {cloud:?}"
            );
        }
    }

    /// Every legend swatch must be visually distinct, which is the whole point
    /// of a key.
    #[test]
    fn legend_swatches_are_distinct() {
        let entries = natural_legend();
        for i in 0..entries.len() {
            for j in i + 1..entries.len() {
                assert_ne!(
                    entries[i].0, entries[j].0,
                    "'{}' and '{}' share a swatch",
                    entries[i].1, entries[j].1
                );
            }
        }
    }

    #[test]
    fn night_darkens_the_surface() {
        assert!(light_factor(LIGHT_NIGHT) < light_factor(0));
    }

    /// A single land pixel surrounded by sea must be marked as coast.
    #[test]
    fn coastline_follows_the_land_sea_edge() {
        let (w, h) = (3, 3);
        let mut surf = vec![SURF_SEA; w * h];
        surf[4] = SURF_LAND;
        let mut rgba = vec![0u8; w * h * 4];
        draw_coastline(&mut rgba, &surf, w, h, Style::Classes);
        assert!(rgba[4 * 4 + 3] > 0, "centre land pixel should be drawn");
        assert_eq!(rgba[3], 0, "open sea should be untouched");
    }
}
