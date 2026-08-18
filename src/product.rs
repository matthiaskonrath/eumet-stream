//! Loading NWC SAF fields into something renderable.

use crate::geo::GeosGrid;
use crate::hdf5::{read_ints, AttrValue, Error, H5File, Result};
use std::path::Path;

/// How a layer should be drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Discrete classes painted from the palette shipped in the file.
    Classes,
    /// A physical quantity mapped through a colour ramp.
    Ramp,
    /// A natural-looking Earth: land, sea and white cloud.
    Natural,
    /// Real imagery straight from the SEVIRI HRIT stream.
    Live,
    /// Temperature of the ground and sea itself, with cloud masked away.
    Surface,
    /// A multi-channel infrared RGB recipe; `variable` names it.
    Composite,
}

/// A field ready to colour.
pub enum Field {
    Categorical {
        data: Vec<u8>,
        /// 256 RGB entries indexed directly by class value.
        palette: Vec<[u8; 3]>,
        /// Class value meaning "no data".
        fill: u8,
    },
    Continuous {
        /// NaN marks missing data.
        data: Vec<f32>,
        lo: f32,
        hi: f32,
    },
    /// Cloud-type classes, to be composited over a land/sea basemap.
    Natural { ct: Vec<u8> },
}

pub struct Scene {
    pub grid: GeosGrid,
    pub field: Field,
    pub title: String,
    pub units: String,
}

// ---------------------------------------------------------------------------
// Geography and illumination
// ---------------------------------------------------------------------------

pub const SURF_SPACE: u8 = 0;
pub const SURF_LAND: u8 = 1;
pub const SURF_SEA: u8 = 2;
pub const SURF_COAST: u8 = 3;

pub const LIGHT_UNKNOWN: u8 = 0;
pub const LIGHT_NIGHT: u8 = 1;
pub const LIGHT_DAY: u8 = 2;
pub const LIGHT_TWILIGHT: u8 = 3;

/// Per-pixel geography and illumination, unpacked from `ct_conditions`.
///
/// The surface classification is fixed geography, so it doubles as the
/// coastline source; the illumination changes slot to slot and gives the
/// day/night terminator.
pub struct Conditions {
    pub surface: Vec<u8>,
    pub light: Vec<u8>,
}

/// Read `ct_conditions` out of a CT product file.
pub fn load_conditions(ct_path: &Path) -> Result<Conditions> {
    let f = H5File::open(ct_path)?;
    let links = f.links(f.root_addr)?;
    let addr = links
        .iter()
        .find(|(n, _)| n == "ct_conditions")
        .map(|(_, a)| *a)
        .ok_or_else(|| Error("ct_conditions not present".into()))?;

    let ds = f.dataset(addr)?;
    let raw = f.read_raw(&ds)?;
    let vals = read_ints(&raw, &ds.dtype, ds.elem_count());

    let mut surface = Vec::with_capacity(vals.len());
    let mut light = Vec::with_capacity(vals.len());
    for v in vals {
        // Bits 4-5 hold the surface type, bits 1-2 the illumination.
        surface.push(match v & 48 {
            16 => SURF_LAND,
            32 => SURF_SEA,
            48 => SURF_COAST,
            _ => SURF_SPACE,
        });
        light.push(match v & 6 {
            2 => LIGHT_NIGHT,
            4 => LIGHT_DAY,
            6 => LIGHT_TWILIGHT,
            _ => LIGHT_UNKNOWN,
        });
    }
    Ok(Conditions { surface, light })
}

// ---------------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------------

/// One selectable layer in the UI.
#[derive(Debug, Clone, Copy)]
pub struct View {
    pub id: &'static str,
    pub label: &'static str,
    /// NWC SAF product the data lives in.
    pub product: &'static str,
    /// Variable inside that product.
    pub variable: &'static str,
    pub units: &'static str,
    /// Display range for continuous fields.
    pub lo: f32,
    pub hi: f32,
    pub style: Style,
}

pub const VIEWS: &[View] = &[
    View {
        id: "live",
        label: "Live SEVIRI",
        // Not an NWC SAF product: this one comes from the raw HRIT stream.
        product: "HRIT",
        variable: "",
        units: "natural",
        lo: 0.0,
        hi: 0.0,
        style: Style::Live,
    },
    View {
        id: "surface",
        label: "Surface heat",
        product: "HRIT",
        variable: "",
        units: "K",
        lo: 265.0,
        hi: 320.0,
        style: Style::Surface,
    },
    View {
        id: "airmass",
        label: "Airmass",
        product: "HRIT",
        variable: "airmass",
        units: "rgb",
        lo: 0.0,
        hi: 0.0,
        style: Style::Composite,
    },
    View {
        id: "dust",
        label: "Dust",
        product: "HRIT",
        variable: "dust",
        units: "rgb",
        lo: 0.0,
        hi: 0.0,
        style: Style::Composite,
    },
    View {
        id: "nightfog",
        label: "Night microphysics",
        product: "HRIT",
        variable: "nightfog",
        units: "rgb",
        lo: 0.0,
        hi: 0.0,
        style: Style::Composite,
    },
    View {
        id: "earth",
        label: "Earth (natural)",
        product: "CT",
        variable: "ct",
        units: "natural",
        lo: 0.0,
        hi: 0.0,
        style: Style::Natural,
    },
    View {
        id: "cloudtype",
        label: "Cloud type",
        product: "CT",
        variable: "ct",
        units: "class",
        lo: 0.0,
        hi: 0.0,
        style: Style::Classes,
    },
    View {
        id: "cloudtop_temp",
        label: "Cloud top temperature",
        product: "CTTH",
        variable: "ctth_tempe",
        units: "K",
        lo: 200.0,
        hi: 300.0,
        style: Style::Ramp,
    },
    View {
        id: "cloudtop_height",
        label: "Cloud top height",
        product: "CTTH",
        variable: "ctth_alti",
        units: "m",
        lo: 0.0,
        hi: 13000.0,
        style: Style::Ramp,
    },
];

pub fn view(id: &str) -> Option<&'static View> {
    VIEWS.iter().find(|v| v.id == id)
}

/// Read one variable out of a product file and prepare it for drawing.
pub fn load(path: &Path, v: &View) -> Result<Scene> {
    let f = H5File::open(path)?;
    let links = f.links(f.root_addr)?;
    let attrs = f.attributes(f.root_addr)?;

    let addr = links
        .iter()
        .find(|(n, _)| n == v.variable)
        .map(|(_, a)| *a)
        .ok_or_else(|| Error(format!("{} not found in {}", v.variable, path.display())))?;

    let ds = f.dataset(addr)?;
    if ds.dims.len() != 2 {
        return Err(Error(format!("expected a 2-D field, got {:?}", ds.dims)));
    }
    let (ny, nx) = (ds.dims[0] as usize, ds.dims[1] as usize);
    let grid = GeosGrid::from_attrs(&attrs, nx, ny)
        .ok_or_else(|| Error("file has no usable georeferencing".into()))?;

    let title = ds
        .attrs
        .get("long_name")
        .and_then(|a| a.as_text())
        .unwrap_or(v.label)
        .to_string();

    let field = match v.style {
        // These layers read the HRIT stream, not an NWC SAF file.
        Style::Live | Style::Surface | Style::Composite => {
            return Err(Error(
                "this layer is not loaded through the NWC SAF path".into(),
            ))
        }
        Style::Natural => Field::Natural {
            ct: f.read_raw(&ds)?,
        },
        Style::Classes => {
            let data = f.read_raw(&ds)?;
            let palette = read_palette(&f, &links, &format!("{}_pal", v.variable))?;
            Field::Categorical {
                data,
                palette,
                fill: 255,
            }
        }
        Style::Ramp => {
            let raw = f.read_raw(&ds)?;
            let n = ds.elem_count();
            let ints = read_ints(&raw, &ds.dtype, n);

            let scale = ds
                .attrs
                .get("scale_factor")
                .and_then(|a| a.as_f64())
                .unwrap_or(1.0);
            let offset = ds
                .attrs
                .get("add_offset")
                .and_then(|a| a.as_f64())
                .unwrap_or(0.0);
            let fill = ds.attrs.get("_FillValue").and_then(|a| a.as_f64());
            let valid = ds
                .attrs
                .get("valid_range")
                .map(|a| a.as_f64_vec())
                .unwrap_or_default();

            // Mask before scaling: the fill value is expressed in stored units.
            let data = ints
                .into_iter()
                .map(|i| {
                    let x = i as f64;
                    if Some(x) == fill {
                        return f32::NAN;
                    }
                    if valid.len() == 2 && (x < valid[0] || x > valid[1]) {
                        return f32::NAN;
                    }
                    (x * scale + offset) as f32
                })
                .collect();

            Field::Continuous {
                data,
                lo: v.lo,
                hi: v.hi,
            }
        }
    };

    Ok(Scene {
        grid,
        field,
        title,
        units: v.units.to_string(),
    })
}

/// Read a `*_pal` companion dataset as 256 RGB entries.
fn read_palette(f: &H5File, links: &[(String, u64)], name: &str) -> Result<Vec<[u8; 3]>> {
    let mut pal = vec![[0u8; 3]; 256];
    let addr = match links.iter().find(|(n, _)| n == name) {
        Some((_, a)) => *a,
        None => return Ok(pal),
    };
    let ds = f.dataset(addr)?;
    let raw = f.read_raw(&ds)?;

    // Palettes are stored either as raw bytes or as scaled floats in 0..1.
    let scaled = ds
        .attrs
        .get("scale_factor")
        .and_then(|a| a.as_f64())
        .unwrap_or(1.0);
    let v = |b: u8| {
        if scaled != 1.0 {
            ((b as f64) * scaled * 255.0).clamp(0.0, 255.0) as u8
        } else {
            b
        }
    };
    for (i, entry) in pal.iter_mut().enumerate() {
        let o = i * 3;
        if o + 2 < raw.len() {
            *entry = [v(raw[o]), v(raw[o + 1]), v(raw[o + 2])];
        }
    }
    Ok(pal)
}

/// Human-readable class names, taken from the file when present.
pub fn class_labels(path: &Path, variable: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(f) = H5File::open(path) else {
        return out;
    };
    let Ok(links) = f.links(f.root_addr) else {
        return out;
    };
    let Some((_, addr)) = links.iter().find(|(n, _)| n == variable) else {
        return out;
    };
    let Ok(attrs) = f.attributes(*addr) else {
        return out;
    };
    if let Some(AttrValue::Text(m)) = attrs.get("flag_meanings") {
        out = m
            .split_whitespace()
            .map(|s| s.replace('_', " "))
            .collect::<Vec<_>>();
    }
    out
}
