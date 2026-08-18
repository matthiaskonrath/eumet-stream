//! Multi-channel RGB composites from SEVIRI infrared imagery.
//!
//! These are the standard EUMETSAT recipes. Each colour gun is fed either a
//! channel's brightness temperature or the difference between two channels,
//! stretched over a published range. Because they are built from infrared they
//! work by day and by night alike, unlike the visible-light natural colour.

use crate::geo::Canvas;
use crate::hrit::{self, Calibration, ChannelImage, Slot};
use crate::product::{Conditions, SURF_SPACE};
use crate::render::encode_png;
use std::collections::BTreeMap;
use std::path::Path;

/// Planck constants in the units SEVIRI radiances are published in.
const C1: f64 = 1.19104e-5; // mW m^-2 sr^-1 (cm^-1)^-4
const C2: f64 = 1.43877; // K (cm^-1)^-1

/// Spectral-band coefficients (central wavenumber, then the two correction
/// terms) for converting radiance to brightness temperature.
///
/// These are the values published for MSG; they vary by a few hundredths of a
/// kelvin between the individual satellites, which does not matter for a
/// picture but would for a geophysical product.
fn planck(channel: &str) -> Option<(f64, f64, f64)> {
    Some(match channel {
        "IR_039" => (2569.094, 0.9959, 3.471),
        "WV_062" => (1598.566, 0.9963, 2.219),
        "WV_073" => (1362.142, 0.9991, 0.485),
        "IR_087" => (1149.083, 0.9996, 0.181),
        "IR_097" => (1034.345, 0.9999, 0.060),
        "IR_108" => (930.659, 0.9983, 0.627),
        "IR_120" => (839.661, 0.9988, 0.397),
        "IR_134" => (752.381, 0.9981, 0.576),
        _ => return None,
    })
}

pub fn brightness_temp(channel: &str, radiance: f64) -> f64 {
    let Some((nu, a, b)) = planck(channel) else {
        return f64::NAN;
    };
    if radiance <= 0.0 {
        return f64::NAN;
    }
    let t = C2 * nu / (1.0 + C1 * nu.powi(3) / radiance).ln();
    (t - b) / a
}

/// What feeds one colour gun.
#[derive(Debug, Clone, Copy)]
pub enum Component {
    /// A single channel's brightness temperature.
    Single {
        ch: &'static str,
        lo: f64,
        hi: f64,
        gamma: f64,
    },
    /// The difference between two channels, `a` minus `b`.
    Diff {
        a: &'static str,
        b: &'static str,
        lo: f64,
        hi: f64,
        gamma: f64,
    },
}

impl Component {
    fn channels(&self) -> Vec<&'static str> {
        match self {
            Component::Single { ch, .. } => vec![ch],
            Component::Diff { a, b, .. } => vec![a, b],
        }
    }

    /// Evaluate to 0..1, or NaN where any input is missing.
    fn value(&self, bt: &dyn Fn(&str) -> f64) -> f32 {
        let (raw, lo, hi, gamma) = match self {
            Component::Single { ch, lo, hi, gamma } => (bt(ch), *lo, *hi, *gamma),
            Component::Diff {
                a,
                b,
                lo,
                hi,
                gamma,
            } => (bt(a) - bt(b), *lo, *hi, *gamma),
        };
        if !raw.is_finite() {
            return f32::NAN;
        }
        // A reversed range (hi < lo) inverts the gun, which several of the
        // published recipes rely on.
        let t = ((raw - lo) / (hi - lo)).clamp(0.0, 1.0);
        (t.powf(1.0 / gamma)) as f32
    }
}

pub struct Recipe {
    pub id: &'static str,
    pub label: &'static str,
    pub red: Component,
    pub green: Component,
    pub blue: Component,
    /// What the colours mean, shown in the legend.
    pub key: &'static [(&'static str, &'static str)],
    pub note: &'static str,
}

impl Recipe {
    pub fn channels(&self) -> Vec<&'static str> {
        let mut v = self.red.channels();
        v.extend(self.green.channels());
        v.extend(self.blue.channels());
        v.sort_unstable();
        v.dedup();
        v
    }
}

pub const RECIPES: &[Recipe] = &[
    Recipe {
        id: "airmass",
        label: "Airmass",
        // Warm colours mark dry, ozone-rich stratospheric air descending behind
        // a jet; green marks moist tropical air.
        red: Component::Diff {
            a: "WV_062",
            b: "WV_073",
            lo: -25.0,
            hi: 0.0,
            gamma: 1.0,
        },
        green: Component::Diff {
            a: "IR_097",
            b: "IR_108",
            lo: -40.0,
            hi: 5.0,
            gamma: 1.0,
        },
        blue: Component::Single {
            ch: "WV_062",
            lo: 243.0,
            hi: 208.0, // reversed: colder is brighter
            gamma: 1.0,
        },
        key: &[
            ("#2f7fd0", "Cold, dry stratospheric air"),
            ("#d24a3a", "Warm, dry descending air"),
            ("#3fae62", "Moist tropical air"),
            ("#f2f2f2", "High cloud"),
        ],
        note: "Red streaks mark stratospheric intrusions; the sharp red/green boundary follows the jet stream.",
    },
    Recipe {
        id: "dust",
        label: "Dust",
        red: Component::Diff {
            a: "IR_120",
            b: "IR_108",
            lo: -4.0,
            hi: 2.0,
            gamma: 1.0,
        },
        green: Component::Diff {
            a: "IR_108",
            b: "IR_087",
            lo: 0.0,
            hi: 15.0,
            gamma: 2.5,
        },
        blue: Component::Single {
            ch: "IR_108",
            lo: 261.0,
            hi: 289.0,
            gamma: 1.0,
        },
        key: &[
            ("#d97be0", "Dust"),
            ("#5fd0e8", "Thin high cloud"),
            ("#8a5a2a", "Warm surface"),
            ("#e8e0d0", "Thick cloud"),
        ],
        note: "Saharan dust appears magenta, and volcanic ash a similar pink. Thick cloud stays neutral.",
    },
    Recipe {
        id: "nightfog",
        label: "Night microphysics",
        red: Component::Diff {
            a: "IR_120",
            b: "IR_108",
            lo: -4.0,
            hi: 2.0,
            gamma: 1.0,
        },
        green: Component::Diff {
            a: "IR_108",
            b: "IR_039",
            lo: 0.0,
            hi: 10.0,
            gamma: 1.0,
        },
        blue: Component::Single {
            ch: "IR_108",
            lo: 243.0,
            hi: 293.0,
            gamma: 1.0,
        },
        key: &[
            ("#4fe0c8", "Fog and low stratus"),
            ("#d8748c", "Thick mid-level cloud"),
            ("#2b3550", "Clear night surface"),
            ("#c8b4e8", "Thin cirrus"),
        ],
        note: "Built for darkness: fog and low stratus glow cyan-green where visible light shows nothing at all.",
    },
];

pub fn recipe(id: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|r| r.id == id)
}

/// Every channel a recipe needs, assembled for one slot.
pub struct Composite {
    pub channels: BTreeMap<&'static str, ChannelImage>,
    pub cal: Calibration,
    pub sub_lon: f64,
}

pub fn load(
    slot: &Slot,
    recipe: &Recipe,
    cache: &Path,
    tool: Option<&Path>,
) -> hrit::Result<Composite> {
    let pro = slot
        .prologue
        .as_ref()
        .ok_or_else(|| format!("slot {} has no prologue, so no calibration", slot.stamp))?;
    let cal = hrit::calibration_from_prologue(pro)?;

    // Decompress every channel's segments together, not one channel at a time.
    hrit::warm_segments(&hrit::segment_paths(slot, &recipe.channels()), cache, tool);

    let mut channels = BTreeMap::new();
    let mut sub_lon = 0.0;
    for ch in recipe.channels() {
        let img = hrit::load_channel(slot, ch, cache, tool)?;
        sub_lon = img.sub_lon;
        channels.insert(ch, img);
    }
    Ok(Composite {
        channels,
        cal,
        sub_lon,
    })
}

pub struct CompositeOpts {
    pub canvas: Canvas,
    pub width: usize,
    pub height: usize,
    pub graticule: bool,
    pub coastline: bool,
    pub borders: bool,
}

pub fn render_rgba(
    comp: &Composite,
    recipe: &Recipe,
    geo_cond: Option<(&crate::geo::GeosGrid, &Conditions)>,
    opts: &CompositeOpts,
) -> Result<Vec<u8>, String> {
    let (w, h) = (opts.width, opts.height);
    let mut rgba = vec![0u8; w * h * 4];

    let indices: BTreeMap<&str, usize> = recipe
        .channels()
        .into_iter()
        .filter_map(|c| hrit::channel_index(c).map(|i| (c, i)))
        .collect();

    let want_surf = opts.coastline && geo_cond.is_some() && !opts.canvas.is_disc();
    let mut surf = vec![SURF_SPACE; w * h];

    crate::render::render_bands(&mut rgba, &mut surf, w, h, |y0, band, sband| {
        let rows = band.len() / (w * 4);
        for yy in 0..rows {
            let y = y0 + yy;
            for x in 0..w {
                let p = yy * w + x;

                if want_surf {
                    if let (Some((grid, cond)), Some((lat, lon))) =
                        (geo_cond, opts.canvas.lonlat_at(x, y, w, h))
                    {
                        if let Some(i) = grid.sample_index(lat, lon) {
                            sband[p] = cond.surface.get(i).copied().unwrap_or(SURF_SPACE);
                        }
                    }
                }

                let Some((sx, sy)) = opts.canvas.scan_at(x, y, w, h, comp.sub_lon) else {
                    continue;
                };

                // Brightness temperature of a named channel at this pixel.
                let bt = |name: &str| -> f64 {
                    let (Some(img), Some(&idx)) = (comp.channels.get(name), indices.get(name))
                    else {
                        return f64::NAN;
                    };
                    match img.sample(sx, sy) {
                        Some(count) => brightness_temp(name, comp.cal.radiance(idx, count)),
                        None => f64::NAN,
                    }
                };

                let r = recipe.red.value(&bt);
                let g = recipe.green.value(&bt);
                let b = recipe.blue.value(&bt);
                if !r.is_finite() || !g.is_finite() || !b.is_finite() {
                    continue;
                }

                let o = p * 4;
                band[o] = (r * 255.0) as u8;
                band[o + 1] = (g * 255.0) as u8;
                band[o + 2] = (b * 255.0) as u8;
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
        comp.sub_lon,
        if surf.is_empty() {
            None
        } else {
            Some(surf.as_slice())
        },
        crate::product::Style::Classes,
        opts.borders,
        opts.graticule,
    );
    Ok(rgba)
}

pub fn render_png(
    comp: &Composite,
    recipe: &Recipe,
    geo_cond: Option<(&crate::geo::GeosGrid, &Conditions)>,
    opts: &CompositeOpts,
) -> Result<Vec<u8>, String> {
    let rgba = render_rgba(comp, recipe, geo_cond, opts)?;
    encode_png(&rgba, opts.width, opts.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightness_temperature_is_physical() {
        let t = brightness_temp("IR_108", 100.0);
        assert!((250.0..320.0).contains(&t), "got {t}");
        assert!(brightness_temp("IR_108", 20.0) < t);
        assert!(
            brightness_temp("VIS006", 50.0).is_nan(),
            "no Planck for VIS"
        );
    }

    #[test]
    fn every_recipe_names_known_channels() {
        for r in RECIPES {
            for ch in r.channels() {
                assert!(planck(ch).is_some(), "{} uses unknown channel {ch}", r.id);
                assert!(
                    hrit::channel_index(ch).is_some(),
                    "{ch} not a SEVIRI channel"
                );
            }
        }
    }

    #[test]
    fn a_reversed_range_inverts_the_gun() {
        // The airmass blue gun runs 243 K down to 208 K, so colder is brighter.
        let c = Component::Single {
            ch: "WV_062",
            lo: 243.0,
            hi: 208.0,
            gamma: 1.0,
        };
        let cold = c.value(&|_| 208.0);
        let warm = c.value(&|_| 243.0);
        assert!(cold > warm, "cold {cold} should exceed warm {warm}");
    }

    #[test]
    fn components_clamp_and_report_missing() {
        let c = Component::Diff {
            a: "IR_120",
            b: "IR_108",
            lo: -4.0,
            hi: 2.0,
            gamma: 1.0,
        };
        assert!(c.value(&|_| f64::NAN).is_nan());
        let v = c.value(&|ch| if ch == "IR_120" { 300.0 } else { 200.0 });
        assert_eq!(v, 1.0, "a large positive difference saturates");
    }
}
