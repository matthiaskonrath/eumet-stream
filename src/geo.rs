//! Geostationary projection for MSG/SEVIRI grids.
//!
//! Products arrive on the satellite's own view geometry. To draw a familiar map
//! of Europe each output pixel is walked back to a grid cell: latitude and
//! longitude are projected forward to scan angles (the CGMS LRIT/HRIT
//! formulation), converted to projection metres, and then turned into a
//! column/row through the file's GDAL geotransform.

use crate::hdf5::AttrValue;
use std::collections::HashMap;

/// Equatorial radius, km.
const R_EQ: f64 = 6378.137;
/// Polar radius, km.
const R_POL: f64 = 6356.7523;
/// Satellite distance from the centre of the Earth, km.
const H_CENTRE: f64 = 42164.0;
/// Satellite height above the ellipsoid, metres - the PROJ `h` parameter, used
/// to turn scan angles into projection metres.
const H_METRES: f64 = 35785863.0;

const RATIO2: f64 = (R_POL * R_POL) / (R_EQ * R_EQ);
const E2: f64 = (R_EQ * R_EQ - R_POL * R_POL) / (R_EQ * R_EQ);

#[derive(Debug, Clone)]
pub struct GeosGrid {
    pub width: usize,
    pub height: usize,
    /// Geotransform: projection metres of the upper-left grid corner and the
    /// per-pixel step. `dy` is negative because rows run north to south.
    pub x0: f64,
    pub dx: f64,
    pub y0: f64,
    pub dy: f64,
    /// Sub-satellite longitude, radians.
    pub sub_lon: f64,
}

impl GeosGrid {
    pub fn from_attrs(
        attrs: &HashMap<String, AttrValue>,
        width: usize,
        height: usize,
    ) -> Option<GeosGrid> {
        let gt = attrs.get("gdal_geotransform_table")?.as_f64_vec();
        if gt.len() < 6 {
            return None;
        }
        let sub_lon = attrs
            .get("sub-satellite_longitude")
            .or_else(|| attrs.get("centre_projection_longitude"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        Some(GeosGrid {
            width,
            height,
            x0: gt[0],
            dx: gt[1],
            y0: gt[3],
            dy: gt[5],
            sub_lon: sub_lon.to_radians(),
        })
    }

    /// Project geographic coordinates to a fractional grid position.
    /// Returns `None` when the point is over the horizon or off the grid.
    pub fn forward(&self, lat_deg: f64, lon_deg: f64) -> Option<(f64, f64)> {
        let (x, y) = scan_angles(lat_deg, lon_deg, self.sub_lon)?;

        // PROJ measures y positive northward; CGMS measures it southward.
        let x_m = H_METRES * x;
        let y_m = -H_METRES * y;

        let col = (x_m - self.x0) / self.dx - 0.5;
        let row = (y_m - self.y0) / self.dy - 0.5;

        if !col.is_finite() || !row.is_finite() {
            return None;
        }
        Some((col, row))
    }

    /// Nearest grid cell for a geographic position.
    pub fn sample_index(&self, lat: f64, lon: f64) -> Option<usize> {
        let (col, row) = self.forward(lat, lon)?;
        let c = col.round();
        let r = row.round();
        if c < 0.0 || r < 0.0 {
            return None;
        }
        let (c, r) = (c as usize, r as usize);
        if c >= self.width || r >= self.height {
            return None;
        }
        Some(r * self.width + c)
    }
}

/// Viewing angles from the satellite to a point on the ellipsoid, in radians,
/// following the CGMS LRIT/HRIT formulation: `x` grows eastward and `y` grows
/// southward. Returns `None` for points hidden behind the limb.
///
/// This is the shared core of both georeferencing paths - the NWC SAF products
/// scale these angles into projection metres, while raw HRIT scales them with
/// the CFAC/LFAC coefficients carried in the file.
pub fn scan_angles(lat_deg: f64, lon_deg: f64, sub_lon: f64) -> Option<(f64, f64)> {
    let lat = lat_deg.to_radians();
    let lon = lon_deg.to_radians();

    // Geodetic to geocentric latitude.
    let c_lat = (RATIO2 * lat.tan()).atan();
    let (sin_c, cos_c) = c_lat.sin_cos();
    let r_l = R_POL / (1.0 - E2 * cos_c * cos_c).sqrt();

    let dlon = lon - sub_lon;
    let r1 = H_CENTRE - r_l * cos_c * dlon.cos();
    let r2 = -r_l * cos_c * dlon.sin();
    let r3 = r_l * sin_c;

    // Standard CGMS visibility test: reject points hidden by the limb.
    if H_CENTRE * (H_CENTRE - r1) < r2 * r2 + (R_EQ * R_EQ / (R_POL * R_POL)) * r3 * r3 {
        return None;
    }

    let rn = (r1 * r1 + r2 * r2 + r3 * r3).sqrt();
    Some(((-r2 / r1).atan(), (-r3 / rn).asin()))
}

/// Turn scan angles back into geographic coordinates.
///
/// The inverse of [`scan_angles`], following the same CGMS formulation.
/// Returns `None` when the line of sight misses the Earth entirely, which is
/// what puts the black corners around a full-disc image.
pub fn inverse_scan_angles(x: f64, y: f64, sub_lon: f64) -> Option<(f64, f64)> {
    // Squared ratio of the equatorial to the polar radius, and the constant
    // (H^2 - Req^2) that the standard formulation carries.
    const RATIO_SQ: f64 = 1.006803;
    const K: f64 = 1_737_121_856.0;

    let (sin_x, cos_x) = x.sin_cos();
    let (sin_y, cos_y) = y.sin_cos();

    let a = cos_y * cos_y + RATIO_SQ * sin_y * sin_y;
    let b = H_CENTRE * cos_x * cos_y;
    let disc = b * b - a * K;
    if disc < 0.0 {
        return None; // the ray passes beside the planet
    }

    let sn = (b - disc.sqrt()) / a;
    let s1 = H_CENTRE - sn * cos_x * cos_y;
    let s2 = sn * sin_x * cos_y;
    let s3 = -sn * sin_y;
    let sxy = (s1 * s1 + s2 * s2).sqrt();

    let lon = (s2 / s1).atan() + sub_lon;
    let lat = (RATIO_SQ * s3 / sxy).atan();
    Some((lat.to_degrees(), lon.to_degrees()))
}

/// Pixels per radian of scan angle for a 3 km SEVIRI grid.
///
/// Both services here sample at 3 km: the HRIT navigation encodes it as
/// CFAC/65536 per degree, the NWC SAF grid as 3000.4 m per pixel of projection
/// distance. The two agree to within a pixel.
pub const SEVIRI_PX_PER_RAD: f64 = H_METRES / 3000.4033203125;

/// How many distinct source samples a window actually spans.
///
/// Rendering finer than this cannot reveal more of the satellite image - the
/// extra output pixels just repeat neighbours - so it is the natural place to
/// put a "native" resolution option.
pub fn native_span(bb: &BBox, sub_lon: f64, px_per_rad: f64) -> (usize, usize) {
    let (mut x0, mut x1) = (f64::MAX, f64::MIN);
    let (mut y0, mut y1) = (f64::MAX, f64::MIN);

    // The extremes need not sit on the corners: the grid is curved in
    // latitude/longitude, so the whole window is sampled.
    const STEPS: usize = 48;
    for i in 0..=STEPS {
        let lat = bb.lat_min + (bb.lat_max - bb.lat_min) * i as f64 / STEPS as f64;
        for j in 0..=STEPS {
            let lon = bb.lon_min + (bb.lon_max - bb.lon_min) * j as f64 / STEPS as f64;
            if let Some((x, y)) = scan_angles(lat, lon, sub_lon) {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    if x0 > x1 || y0 > y1 {
        return (0, 0);
    }
    (
        ((x1 - x0) * px_per_rad).round() as usize,
        ((y1 - y0) * px_per_rad).round() as usize,
    )
}

/// A geographic window to render, in degrees.
#[derive(Debug, Clone, Copy)]
pub struct BBox {
    pub lat_min: f64,
    pub lat_max: f64,
    pub lon_min: f64,
    pub lon_max: f64,
}

impl BBox {
    /// The default view: Europe from the Atlantic to the Urals.
    pub const EUROPE: BBox = BBox {
        lat_min: 32.0,
        lat_max: 72.0,
        lon_min: -25.0,
        lon_max: 45.0,
    };

    /// Europe with the surrounding Atlantic, Africa and western Asia.
    pub const WIDE: BBox = BBox {
        lat_min: 27.0,
        lat_max: 80.0,
        lon_min: -60.0,
        lon_max: 60.0,
    };

    pub fn named(name: &str) -> BBox {
        match name {
            "wide" => BBox::WIDE,
            _ => BBox::EUROPE,
        }
    }
}

/// What the output image represents.
///
/// Reprojecting a whole disc onto a latitude/longitude grid stretches the limb
/// beyond recognition, so the global view keeps the satellite's own geometry
/// and simply scales the scan angles onto the canvas.
#[derive(Debug, Clone, Copy)]
pub enum Canvas {
    /// A plate carree window.
    LatLon(BBox),
    /// The satellite's view. `half_deg` is the scan angle from the centre of
    /// the canvas to its nearer edge, and `cx_deg`/`cy_deg` place that centre,
    /// so the disc can be panned and zoomed like any other window.
    Disc {
        half_deg: f64,
        cx_deg: f64,
        cy_deg: f64,
    },
}

/// The Earth subtends about 8.7 degrees of scan angle from geostationary orbit.
pub const DISC_HALF_DEG: f64 = 9.0;

/// `a,b,c[,d]` -> four finite numbers, the last defaulting to zero.
/// Centre and radius, in pixels per radian, for drawing the disc on a canvas of
/// any shape.
fn disc_geometry(w: usize, h: usize, half_deg: f64) -> (f64, f64, f64) {
    let half = half_deg.to_radians();
    let radius = (w.min(h) as f64) / 2.0 / half;
    (w as f64 / 2.0, h as f64 / 2.0, radius)
}

impl Canvas {
    pub fn named(name: &str) -> Canvas {
        match name {
            "globe" => Canvas::FULL_DISC,
            other => Canvas::LatLon(BBox::named(other)),
        }
    }

    pub const FULL_DISC: Canvas = Canvas::Disc {
        half_deg: DISC_HALF_DEG,
        cx_deg: 0.0,
        cy_deg: 0.0,
    };

    pub fn is_disc(&self) -> bool {
        matches!(self, Canvas::Disc { .. })
    }

    /// The lat/lon window this canvas covers. The disc has no rectangular
    /// extent, so the widest regional window stands in; only the NWC SAF
    /// renderers ask for this, and they never draw the disc.
    pub fn bbox(&self) -> BBox {
        match self {
            Canvas::LatLon(bb) => *bb,
            Canvas::Disc { .. } => BBox::WIDE,
        }
    }

    /// Scan angles looking through the centre of an output pixel.
    pub fn scan_at(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        sub_lon: f64,
    ) -> Option<(f64, f64)> {
        match self {
            Canvas::LatLon(_) => {
                let (lat, lon) = self.lonlat_at(x, y, w, h)?;
                scan_angles(lat, lon, sub_lon)
            }
            Canvas::Disc {
                half_deg,
                cx_deg,
                cy_deg,
            } => {
                let (cx, cy, r) = disc_geometry(w, h, *half_deg);
                // One angular scale for both axes, so the Earth stays a circle
                // whatever shape the canvas is; rows run north to south, which
                // is the direction CGMS y already grows in.
                Some((
                    cx_deg.to_radians() + (x as f64 + 0.5 - cx) / r,
                    cy_deg.to_radians() + (y as f64 + 0.5 - cy) / r,
                ))
            }
        }
    }

    /// Geographic position of a pixel, where the canvas defines one directly.
    pub fn lonlat_at(&self, x: usize, y: usize, w: usize, h: usize) -> Option<(f64, f64)> {
        match self {
            Canvas::LatLon(bb) => {
                let dlat = (bb.lat_max - bb.lat_min) / h as f64;
                let dlon = (bb.lon_max - bb.lon_min) / w as f64;
                Some((
                    bb.lat_max - (y as f64 + 0.5) * dlat,
                    bb.lon_min + (x as f64 + 0.5) * dlon,
                ))
            }
            Canvas::Disc { .. } => {
                let (sx, sy) = self.scan_at(x, y, w, h, 0.0)?;
                // sub_lon is unknown here, so this yields a position relative to
                // the sub-satellite meridian; callers add it where it matters.
                inverse_scan_angles(sx, sy, 0.0)
            }
        }
    }

    /// Geographic position of a pixel, including the sub-satellite longitude.
    pub fn lonlat_at_sub(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        sub_lon: f64,
    ) -> Option<(f64, f64)> {
        match self {
            Canvas::LatLon(_) => self.lonlat_at(x, y, w, h),
            Canvas::Disc { .. } => {
                let (sx, sy) = self.scan_at(x, y, w, h, sub_lon)?;
                inverse_scan_angles(sx, sy, sub_lon)
            }
        }
    }

    /// Where a geographic position lands on the canvas, in fractional pixels.
    pub fn project(
        &self,
        lat: f64,
        lon: f64,
        w: usize,
        h: usize,
        sub_lon: f64,
    ) -> Option<(f64, f64)> {
        match self {
            Canvas::LatLon(bb) => {
                if !(bb.lat_min..=bb.lat_max).contains(&lat)
                    || !(bb.lon_min..=bb.lon_max).contains(&lon)
                {
                    return None;
                }
                Some((
                    (lon - bb.lon_min) / (bb.lon_max - bb.lon_min) * w as f64,
                    (bb.lat_max - lat) / (bb.lat_max - bb.lat_min) * h as f64,
                ))
            }
            Canvas::Disc {
                half_deg,
                cx_deg,
                cy_deg,
            } => {
                let (sx, sy) = scan_angles(lat, lon, sub_lon)?;
                let (cx, cy, r) = disc_geometry(w, h, *half_deg);
                Some((
                    cx + (sx - cx_deg.to_radians()) * r,
                    cy + (sy - cy_deg.to_radians()) * r,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg_n() -> GeosGrid {
        // The geotransform published in the NWC SAF MSG-N products.
        GeosGrid {
            width: 2295,
            height: 928,
            x0: -3445963.25,
            dx: 3000.4033203125,
            y0: 5570249.0,
            dy: -3000.4033203125,
            sub_lon: 0.0,
        }
    }

    #[test]
    fn centre_of_region_matches_metadata() {
        // region_name advertises "CENTRE=44N 0E".
        let g = msg_n();
        let (col, row) = g.forward(44.0, 0.0).expect("centre must be visible");
        assert!(
            (col - (g.width as f64 - 1.0) / 2.0).abs() < 2.0,
            "col={col}"
        );
        assert!(
            (row - (g.height as f64 - 1.0) / 2.0).abs() < 2.0,
            "row={row}"
        );
    }

    #[test]
    fn far_side_of_the_earth_is_not_visible() {
        let g = msg_n();
        assert!(g.forward(0.0, 180.0).is_none());
    }

    /// The Europe window covers a known number of 3 km samples; this is what
    /// the "Native" resolution option resolves to.
    ///
    /// The span is set by the *southern* edge of the window, which lies nearer
    /// the sub-satellite point and so subtends more columns per degree than the
    /// middle of the window does.
    #[test]
    fn native_span_matches_the_grid_sampling() {
        let (w, h) = native_span(&BBox::EUROPE, 0.0, SEVIRI_PX_PER_RAD);
        assert!((1820..2020).contains(&w), "width {w}");
        assert!((600..820).contains(&h), "height {h}");
    }

    /// The inverse projection must undo the forward one. Getting a sign wrong
    /// here mirrors the globe without any other symptom.
    #[test]
    fn scan_angles_round_trip() {
        let sub = 9.5f64.to_radians();
        for &(lat, lon) in &[
            (0.0, 9.5),
            (48.2, 16.4),
            (-33.9, 18.4),
            (60.0, -20.0),
            (-10.0, 40.0),
        ] {
            let (x, y) = scan_angles(lat, lon, sub).expect("visible");
            let (blat, blon) = inverse_scan_angles(x, y, sub).expect("on the disc");
            assert!((blat - lat).abs() < 0.01, "lat {lat} -> {blat}");
            assert!((blon - lon).abs() < 0.01, "lon {lon} -> {blon}");
        }
    }

    /// The Earth must come out round on any canvas shape, not stretched to fit.
    #[test]
    fn the_disc_stays_circular_on_any_canvas() {
        let c = Canvas::FULL_DISC;
        for (w, h) in [(900usize, 900usize), (1600, 600), (500, 1200)] {
            // Two points the same angular distance from the sub-satellite point,
            // one east and one north, must land the same distance from centre.
            let east = c.project(0.0, 8.0, w, h, 0.0).unwrap();
            let north = c.project(8.0, 0.0, w, h, 0.0).unwrap();
            let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
            let re = ((east.0 - cx).powi(2) + (east.1 - cy).powi(2)).sqrt();
            let rn = ((north.0 - cx).powi(2) + (north.1 - cy).powi(2)).sqrt();
            assert!(
                (re - rn).abs() / re < 0.02,
                "{w}x{h}: east radius {re:.1} vs north {rn:.1}"
            );
        }
    }

    /// A pixel and the scan angle it maps to must agree in both directions.
    #[test]
    fn disc_pixel_mapping_round_trips() {
        let c = Canvas::FULL_DISC;
        let (w, h) = (1200usize, 800usize);
        let (lat, lon) = (30.0, -12.0);
        let (px, py) = c.project(lat, lon, w, h, 0.0).unwrap();
        let (blat, blon) = c
            .lonlat_at_sub(px.round() as usize, py.round() as usize, w, h, 0.0)
            .unwrap();
        assert!((blat - lat).abs() < 0.2, "lat {lat} -> {blat}");
        assert!((blon - lon).abs() < 0.2, "lon {lon} -> {blon}");
    }

    /// Panning and zooming the disc must move the picture, not distort it.
    #[test]
    fn a_zoomed_disc_keeps_its_scale_in_both_axes() {
        let c = Canvas::Disc {
            half_deg: 3.0,
            cx_deg: 1.5,
            cy_deg: -2.0,
        };
        let (w, h) = (1000usize, 700usize);
        // Two points either side of the canvas centre, equal angular steps.
        let mid = c.lonlat_at_sub(w / 2, h / 2, w, h, 0.0).unwrap();
        let (px, py) = c.project(mid.0, mid.1, w, h, 0.0).unwrap();
        assert!((px - w as f64 / 2.0).abs() < 1.0, "centre x {px}");
        assert!((py - h as f64 / 2.0).abs() < 1.0, "centre y {py}");
    }

    #[test]
    fn named_areas_map_to_canvases() {
        match Canvas::named("europe") {
            Canvas::LatLon(bb) => assert_eq!(bb.lat_max, BBox::EUROPE.lat_max),
            _ => panic!("expected a lat/lon window"),
        }
        match Canvas::named("wide") {
            Canvas::LatLon(bb) => assert_eq!(bb.lat_max, BBox::WIDE.lat_max),
            _ => panic!("expected a lat/lon window"),
        }
        match Canvas::named("globe") {
            Canvas::Disc { half_deg, .. } => assert_eq!(half_deg, DISC_HALF_DEG),
            _ => panic!("expected a disc"),
        }
        // An unknown area falls back to the default rather than failing.
        assert!(matches!(Canvas::named("nonsense"), Canvas::LatLon(_)));
    }

    #[test]
    fn rays_missing_the_earth_have_no_position() {
        // Well beyond the limb, which is about 8.7 degrees of scan angle.
        assert!(inverse_scan_angles(0.15, 0.15, 0.0).is_none());
    }

    /// A wider window must span more samples, never fewer.
    #[test]
    fn wider_windows_span_more_samples() {
        let (ew, eh) = native_span(&BBox::EUROPE, 0.0, SEVIRI_PX_PER_RAD);
        let (ww, wh) = native_span(&BBox::WIDE, 0.0, SEVIRI_PX_PER_RAD);
        assert!(ww > ew && wh > eh);
    }
}
