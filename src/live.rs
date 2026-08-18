//! Live natural-colour Earth built from raw SEVIRI imagery.
//!
//! SEVIRI has no blue or green detector, so a literal true-colour photograph is
//! not physically available. This uses EUMETSAT's standard *Natural Colour*
//! recipe (red from the 1.6 um shortwave infrared, green from 0.8 um, blue from
//! 0.6 um) which renders vegetation green, bare ground in warm tones, cloud
//! white and snow or ice cyan.
//!
//! Reflected sunlight vanishes after dark, so the night side crossfades to
//! colourised 10.8 um infrared and the picture stays useful right through a
//! 48 hour window.

use crate::geo::{Canvas, GeosGrid};
use crate::hrit::{self, Calibration, ChannelImage, Slot};
use crate::product::{Conditions, Style, SURF_SPACE};
use crate::render::encode_png;
use crate::solar::Sun;
use std::path::Path;

/// Channels feeding red, green and blue, plus the infrared used after dark.
pub const RED: &str = "IR_016";
pub const GREEN: &str = "VIS008";
pub const BLUE: &str = "VIS006";
pub const NIGHT: &str = "IR_108";

pub const REQUIRED: [&str; 4] = [RED, GREEN, BLUE, NIGHT];

pub struct LiveScene {
    pub red: ChannelImage,
    pub green: ChannelImage,
    pub blue: ChannelImage,
    pub night: ChannelImage,
    pub cal: Calibration,
    pub epoch: i64,
    pub sub_lon: f64,
}

pub fn load(slot: &Slot, cache: &Path, tool: Option<&Path>) -> hrit::Result<LiveScene> {
    let pro = slot
        .prologue
        .as_ref()
        .ok_or_else(|| format!("slot {} has no prologue, so no calibration", slot.stamp))?;
    let cal = hrit::calibration_from_prologue(pro)?;

    // Decompress everything this slot needs at once rather than a segment at a
    // time; a full disc is 32 of them.
    hrit::warm_segments(&hrit::segment_paths(slot, &REQUIRED), cache, tool);

    let red = hrit::load_channel(slot, RED, cache, tool)?;
    let green = hrit::load_channel(slot, GREEN, cache, tool)?;
    let blue = hrit::load_channel(slot, BLUE, cache, tool)?;
    let night = hrit::load_channel(slot, NIGHT, cache, tool)?;
    let sub_lon = red.sub_lon;

    Ok(LiveScene {
        red,
        green,
        blue,
        night,
        cal,
        epoch: slot.epoch,
        sub_lon,
    })
}

pub struct LiveOpts {
    pub canvas: Canvas,
    pub width: usize,
    pub height: usize,
    pub graticule: bool,
    pub coastline: bool,
    pub borders: bool,
}

// Planck constants for the radiance-to-temperature conversion, in the units
// SEVIRI radiances are published in.
const C1: f64 = 1.19104e-5; // mW m^-2 sr^-1 (cm^-1)^-4
const C2: f64 = 1.43877; // K (cm^-1)^-1

// Spectral-band coefficients for the 10.8 um channel.
const NU_108: f64 = 930.659;
const ALPHA_108: f64 = 0.9983;
const BETA_108: f64 = 0.627;

fn brightness_temp(radiance: f64) -> f64 {
    if radiance <= 0.0 {
        return f64::NAN;
    }
    let t = C2 * NU_108 / (1.0 + C1 * NU_108.powi(3) / radiance).ln();
    (t - BETA_108) / ALPHA_108
}

/// Bidirectional reflectance in percent, corrected for the sun's angle.
///
/// The published band figures are total solar irradiance at the top of the
/// atmosphere, so converting a directional radiance into a reflectance needs
/// the factor of pi: `R = pi x L / (E0 x cos(theta))`. Dropping it leaves the
/// whole scene about three times too dark, with cloud tops never reaching white.
///
/// The cosine is clamped near the terminator, where dividing by a vanishing
/// value would otherwise amplify sensor noise into a bright rim.
fn reflectance(cal: &Calibration, idx: usize, count: u16, irradiance: f64, cos_sza: f64) -> f32 {
    let radiance = cal.radiance(idx, count);
    if radiance <= 0.0 {
        return 0.0;
    }
    let pct = 100.0 * std::f64::consts::PI * radiance / irradiance;
    (pct / cos_sza.max(0.12)) as f32
}

/// Map 0-100% reflectance onto 0-1 display, lifting the midtones.
fn stretch(pct: f32) -> f32 {
    (pct / 100.0).clamp(0.0, 1.0).powf(0.72)
}

/// Night-side infrared: cold high cloud bright, warm surface near black.
fn ir_colour(bt: f64) -> [f32; 3] {
    const STOPS: [(f64, [f32; 3]); 5] = [
        (190.0, [1.00, 1.00, 1.00]),
        (230.0, [0.67, 0.78, 0.90]),
        (260.0, [0.27, 0.37, 0.51]),
        (280.0, [0.12, 0.16, 0.24]),
        (300.0, [0.05, 0.07, 0.12]),
    ];
    if !bt.is_finite() {
        return [0.0, 0.0, 0.0];
    }
    if bt <= STOPS[0].0 {
        return STOPS[0].1;
    }
    for w in STOPS.windows(2) {
        let (a, ca) = w[0];
        let (b, cb) = w[1];
        if bt >= a && bt <= b {
            let f = ((bt - a) / (b - a)) as f32;
            return [
                ca[0] + (cb[0] - ca[0]) * f,
                ca[1] + (cb[1] - ca[1]) * f,
                ca[2] + (cb[2] - ca[2]) * f,
            ];
        }
    }
    STOPS[STOPS.len() - 1].1
}

pub fn render_png(
    scene: &LiveScene,
    geo_cond: Option<(&GeosGrid, &Conditions)>,
    opts: &LiveOpts,
) -> Result<Vec<u8>, String> {
    let rgba = render_rgba(scene, geo_cond, opts)?;
    encode_png(&rgba, opts.width, opts.height)
}

pub fn render_rgba(
    scene: &LiveScene,
    geo_cond: Option<(&GeosGrid, &Conditions)>,
    opts: &LiveOpts,
) -> Result<Vec<u8>, String> {
    let (w, h) = (opts.width, opts.height);
    let mut rgba = vec![0u8; w * h * 4];

    let sun = Sun::at(scene.epoch);

    let idx_red = hrit::channel_index(RED).unwrap();
    let idx_green = hrit::channel_index(GREEN).unwrap();
    let idx_blue = hrit::channel_index(BLUE).unwrap();
    let idx_night = hrit::channel_index(NIGHT).unwrap();

    let irr_red = hrit::solar_irradiance(RED).unwrap();
    let irr_green = hrit::solar_irradiance(GREEN).unwrap();
    let irr_blue = hrit::solar_irradiance(BLUE).unwrap();

    let want_surf = opts.coastline && geo_cond.is_some();
    let mut surf = vec![SURF_SPACE; w * h];

    crate::render::render_bands(&mut rgba, &mut surf, w, h, |y0, band, sband| {
        let rows = band.len() / (w * 4);
        for yy in 0..rows {
            let y = y0 + yy;
            for x in 0..w {
                let p = yy * w + x;
                // On a lat/lon canvas this is the pixel's own position; on the
                // disc it comes back through the inverse projection.
                let (lat, lon) = match opts.canvas.lonlat_at_sub(x, y, w, h, scene.sub_lon) {
                    Some(v) => v,
                    None => continue,
                };

                // The coastline comes from the NWC SAF geography mask,
                // reprojected through the same output pixel so it registers
                // with the imagery.
                if want_surf {
                    if let Some((grid, cond)) = geo_cond {
                        if let Some(i) = grid.sample_index(lat, lon) {
                            sband[p] = cond.surface.get(i).copied().unwrap_or(SURF_SPACE);
                        }
                    }
                }

                let Some((sx, sy)) = opts.canvas.scan_at(x, y, w, h, scene.sub_lon) else {
                    continue;
                };

                let cos_sza = sun.cos_zenith(lat, lon);
                // Crossfade across the terminator rather than switching abruptly.
                let day_weight = (((cos_sza + 0.05) / 0.20) as f32).clamp(0.0, 1.0);

                let mut rgb = [0f32; 3];
                let mut have = false;

                if day_weight > 0.0 {
                    if let (Some(r), Some(g), Some(b)) = (
                        scene.red.sample(sx, sy),
                        scene.green.sample(sx, sy),
                        scene.blue.sample(sx, sy),
                    ) {
                        let day = [
                            stretch(reflectance(&scene.cal, idx_red, r, irr_red, cos_sza)),
                            stretch(reflectance(&scene.cal, idx_green, g, irr_green, cos_sza)),
                            stretch(reflectance(&scene.cal, idx_blue, b, irr_blue, cos_sza)),
                        ];
                        for k in 0..3 {
                            rgb[k] += day[k] * day_weight;
                        }
                        have = true;
                    }
                }

                if day_weight < 1.0 {
                    if let Some(t) = scene.night.sample(sx, sy) {
                        let bt = brightness_temp(scene.cal.radiance(idx_night, t));
                        let night = ir_colour(bt);
                        for k in 0..3 {
                            rgb[k] += night[k] * (1.0 - day_weight);
                        }
                        have = true;
                    }
                }

                if !have {
                    continue;
                }
                let o = p * 4;
                for k in 0..3 {
                    band[o + k] = (rgb[k].clamp(0.0, 1.0) * 255.0) as u8;
                }
                band[o + 3] = 255;
            }
        }
    });
    if !want_surf {
        surf.clear();
    }

    crate::render::draw_overlays(
        &mut rgba,
        w,
        h,
        &opts.canvas,
        scene.sub_lon,
        (opts.coastline && !surf.is_empty()).then_some(surf.as_slice()),
        Style::Natural,
        opts.borders,
        opts.graticule,
    );
    Ok(rgba)
}

// ---------------------------------------------------------------------------
// Surface heat
// ---------------------------------------------------------------------------

/// Only the infrared channel is needed to see how warm the ground and sea are,
/// so this avoids decompressing the three visible channels.
pub struct SurfaceScene {
    pub ir: ChannelImage,
    pub cal: Calibration,
    pub epoch: i64,
    pub sub_lon: f64,
}

pub fn load_surface(slot: &Slot, cache: &Path, tool: Option<&Path>) -> hrit::Result<SurfaceScene> {
    let pro = slot
        .prologue
        .as_ref()
        .ok_or_else(|| format!("slot {} has no prologue, so no calibration", slot.stamp))?;
    let cal = hrit::calibration_from_prologue(pro)?;
    let ir = hrit::load_channel(slot, NIGHT, cache, tool)?;
    let sub_lon = ir.sub_lon;
    Ok(SurfaceScene {
        ir,
        cal,
        epoch: slot.epoch,
        sub_lon,
    })
}

/// Cloud-type classes that are a view of the surface rather than of cloud.
/// Snow and ice count: they are the ground, and their temperature is real.
fn is_clear_sky(class: u8) -> bool {
    matches!(class, 1..=4)
}

/// Colours for surface temperature, cold through to hot.
const HEAT_STOPS: [(f64, [f32; 3]); 8] = [
    (265.0, [0.16, 0.12, 0.36]),
    (275.0, [0.12, 0.35, 0.75]),
    (283.0, [0.12, 0.69, 0.75]),
    (290.0, [0.27, 0.78, 0.35]),
    (297.0, [0.92, 0.86, 0.27]),
    (304.0, [0.94, 0.55, 0.16]),
    (312.0, [0.84, 0.20, 0.16]),
    (320.0, [1.00, 0.92, 0.88]),
];

fn heat_colour(bt: f64) -> [f32; 3] {
    if !bt.is_finite() || bt <= HEAT_STOPS[0].0 {
        return HEAT_STOPS[0].1;
    }
    for w in HEAT_STOPS.windows(2) {
        let (a, ca) = w[0];
        let (b, cb) = w[1];
        if bt >= a && bt <= b {
            let f = ((bt - a) / (b - a)) as f32;
            return [
                ca[0] + (cb[0] - ca[0]) * f,
                ca[1] + (cb[1] - ca[1]) * f,
                ca[2] + (cb[2] - ca[2]) * f,
            ];
        }
    }
    HEAT_STOPS[HEAT_STOPS.len() - 1].1
}

/// Legend swatches for the heat ramp.
pub fn heat_swatches(n: usize) -> Vec<String> {
    let lo = HEAT_STOPS[0].0;
    let hi = HEAT_STOPS[HEAT_STOPS.len() - 1].0;
    (0..n)
        .map(|i| {
            let t = lo + (hi - lo) * i as f64 / (n - 1).max(1) as f64;
            let c = heat_colour(t);
            format!(
                "#{:02x}{:02x}{:02x}",
                (c[0] * 255.0) as u8,
                (c[1] * 255.0) as u8,
                (c[2] * 255.0) as u8
            )
        })
        .collect()
}

/// Where cloud hides the surface.
///
/// Drawn as a pale, cloud-like grey rather than a dark neutral: the point is to
/// read as "there is cloud in the way", not as "this pixel failed".
const UNDER_CLOUD: [u8; 3] = [176, 182, 192];

/// Render how warm the ground and sea are, with cloud masked out.
///
/// This is the 10.8 um brightness temperature seen from orbit. It tracks the
/// real skin temperature closely in clear air, but it is not an atmospherically
/// corrected land-surface-temperature product.
pub fn render_surface_png(
    scene: &SurfaceScene,
    cloud: Option<(&GeosGrid, &[u8])>,
    geo_cond: Option<(&GeosGrid, &Conditions)>,
    opts: &LiveOpts,
) -> Result<Vec<u8>, String> {
    let rgba = render_surface_rgba(scene, cloud, geo_cond, opts)?;
    encode_png(&rgba, opts.width, opts.height)
}

pub fn render_surface_rgba(
    scene: &SurfaceScene,
    cloud: Option<(&GeosGrid, &[u8])>,
    geo_cond: Option<(&GeosGrid, &Conditions)>,
    opts: &LiveOpts,
) -> Result<Vec<u8>, String> {
    let (w, h) = (opts.width, opts.height);
    let mut rgba = vec![0u8; w * h * 4];

    let idx_ir = hrit::channel_index(NIGHT).unwrap();

    let want_surf = opts.coastline && geo_cond.is_some();
    let mut surf = vec![SURF_SPACE; w * h];

    crate::render::render_bands(&mut rgba, &mut surf, w, h, |y0, band, sband| {
        let rows = band.len() / (w * 4);
        for yy in 0..rows {
            let y = y0 + yy;
            for x in 0..w {
                let p = yy * w + x;
                let (lat, lon) = match opts.canvas.lonlat_at_sub(x, y, w, h, scene.sub_lon) {
                    Some(v) => v,
                    None => continue,
                };

                if want_surf {
                    if let Some((grid, cond)) = geo_cond {
                        if let Some(i) = grid.sample_index(lat, lon) {
                            sband[p] = cond.surface.get(i).copied().unwrap_or(SURF_SPACE);
                        }
                    }
                }

                let Some((sx, sy)) = opts.canvas.scan_at(x, y, w, h, scene.sub_lon) else {
                    continue;
                };
                let Some(count) = scene.ir.sample(sx, sy) else {
                    continue;
                };

                // Cloud tops are cold and would read as freezing ground, so any
                // pixel the cloud mask flags is greyed out rather than coloured.
                let clear = match cloud {
                    Some((grid, ct)) => grid
                        .sample_index(lat, lon)
                        .and_then(|i| ct.get(i).copied())
                        .map(is_clear_sky)
                        .unwrap_or(false),
                    None => true,
                };

                let o = p * 4;
                if clear {
                    let bt = brightness_temp(scene.cal.radiance(idx_ir, count));
                    if !bt.is_finite() {
                        continue;
                    }
                    let c = heat_colour(bt);
                    for k in 0..3 {
                        band[o + k] = (c[k].clamp(0.0, 1.0) * 255.0) as u8;
                    }
                } else {
                    band[o..o + 3].copy_from_slice(&UNDER_CLOUD);
                }
                band[o + 3] = 255;
            }
        }
    });
    if !want_surf {
        surf.clear();
    }

    crate::render::draw_overlays(
        &mut rgba,
        w,
        h,
        &opts.canvas,
        scene.sub_lon,
        (opts.coastline && !surf.is_empty()).then_some(surf.as_slice()),
        Style::Classes,
        opts.borders,
        opts.graticule,
    );
    Ok(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heat_ramp_runs_cold_to_hot() {
        let cold = heat_colour(266.0);
        let hot = heat_colour(318.0);
        assert!(hot[0] > cold[0], "hot end should be redder");
        assert_eq!(heat_colour(200.0), HEAT_STOPS[0].1, "clamps below");
        assert_eq!(
            heat_colour(400.0),
            HEAT_STOPS[HEAT_STOPS.len() - 1].1,
            "clamps above"
        );
    }

    #[test]
    fn only_surface_classes_count_as_clear() {
        for c in [1u8, 2, 3, 4] {
            assert!(is_clear_sky(c), "class {c} is a view of the surface");
        }
        for c in [5u8, 8, 10, 15, 255] {
            assert!(!is_clear_sky(c), "class {c} is cloud");
        }
    }

    #[test]
    fn heat_swatches_are_distinct() {
        let s = heat_swatches(16);
        assert_eq!(s.len(), 16);
        assert_ne!(s.first(), s.last());
    }

    /// A round trip through the published SEVIRI constants should land on a
    /// sensible cloud-top temperature.
    #[test]
    fn brightness_temperature_is_physical() {
        // Radiance typical of a warm surface in the 10.8 um channel.
        let warm = brightness_temp(100.0);
        assert!((250.0..320.0).contains(&warm), "got {warm}");
        // Less radiance must mean a colder scene.
        assert!(brightness_temp(20.0) < warm);
    }

    #[test]
    fn ir_colour_is_brighter_when_colder() {
        let cold = ir_colour(200.0);
        let warm = ir_colour(295.0);
        assert!(cold[0] > warm[0] && cold[1] > warm[1] && cold[2] > warm[2]);
    }

    #[test]
    fn stretch_is_monotonic_and_bounded() {
        assert_eq!(stretch(0.0), 0.0);
        assert_eq!(stretch(100.0), 1.0);
        assert_eq!(stretch(500.0), 1.0);
        assert!(stretch(20.0) < stretch(60.0));
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    /// The channel names are looked up and unwrapped in the render paths, so a
    /// typo in one of these constants compiles cleanly and then panics on the
    /// first frame. `rgb.rs` already guards its recipes this way; this is the
    /// same guard for the constants used here.
    #[test]
    fn every_named_channel_exists() {
        for name in REQUIRED {
            assert!(
                crate::hrit::channel_index(name).is_some(),
                "{name} is not a SEVIRI channel"
            );
        }
        // The visible three are also calibrated against solar irradiance;
        // the infrared one is not and must not be.
        for name in [RED, GREEN, BLUE] {
            assert!(
                crate::hrit::solar_irradiance(name).is_some(),
                "{name} has no band irradiance, and reflectance needs one"
            );
        }
        assert!(
            crate::hrit::solar_irradiance(NIGHT).is_none(),
            "{NIGHT} is thermal infrared; a solar irradiance for it is a mistake"
        );
    }
}
