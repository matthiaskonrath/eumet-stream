//! Solar geometry.
//!
//! The raw imagery has no illumination flags of its own, so the day/night
//! blend and the reflectance normalisation compute the sun's position from the
//! slot time directly. Accuracy of a hundredth of a degree is far more than a
//! terminator needs.

/// Sun declination and Greenwich hour angle for an instant, both in degrees.
fn sun_position(epoch: i64) -> (f64, f64) {
    // Days since the J2000.0 epoch (2000-01-01 12:00 UTC).
    let n = epoch as f64 / 86400.0 - 10957.5;

    let mean_long = (280.460 + 0.9856474 * n).rem_euclid(360.0);
    let mean_anom = (357.528 + 0.9856003 * n).rem_euclid(360.0).to_radians();

    // Apparent ecliptic longitude, correcting for the orbit's eccentricity.
    let ecliptic =
        (mean_long + 1.915 * mean_anom.sin() + 0.020 * (2.0 * mean_anom).sin()).to_radians();
    let obliquity = (23.439 - 0.0000004 * n).to_radians();

    let declination = (obliquity.sin() * ecliptic.sin()).asin();
    let right_asc = (obliquity.cos() * ecliptic.sin()).atan2(ecliptic.cos());

    // Greenwich mean sidereal time, as an angle.
    let gmst = (280.46061837 + 360.98564736629 * n).rem_euclid(360.0);
    let hour_angle = gmst - right_asc.to_degrees();

    (declination.to_degrees(), hour_angle)
}

/// Cosine of the solar zenith angle. Positive means the sun is above the
/// horizon; the value is also the illumination factor on a flat surface.
pub fn cos_zenith(lat_deg: f64, lon_deg: f64, epoch: i64) -> f64 {
    let (dec, gha) = sun_position(epoch);
    let lat = lat_deg.to_radians();
    let dec = dec.to_radians();
    let h = (gha + lon_deg).to_radians();
    lat.sin() * dec.sin() + lat.cos() * dec.cos() * h.cos()
}

/// A `Sun` caches the solar position for one slot so per-pixel work stays cheap.
pub struct Sun {
    sin_dec: f64,
    cos_dec: f64,
    gha: f64,
}

impl Sun {
    pub fn at(epoch: i64) -> Sun {
        let (dec, gha) = sun_position(epoch);
        let dec = dec.to_radians();
        Sun {
            sin_dec: dec.sin(),
            cos_dec: dec.cos(),
            gha,
        }
    }

    pub fn cos_zenith(&self, lat_deg: f64, lon_deg: f64) -> f64 {
        let lat = lat_deg.to_radians();
        let h = (self.gha + lon_deg).to_radians();
        lat.sin() * self.sin_dec + lat.cos() * self.cos_dec * h.cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an epoch from a civil date, rather than hand-computing a constant.
    fn utc(y: i64, m: u32, d: u32, h: i64) -> i64 {
        crate::catalog::days_from_civil(y, m, d) * 86400 + h * 3600
    }

    /// Northern solstice: the sun stands over the Tropic of Cancer.
    #[test]
    fn solstice_declination_is_near_the_tropic() {
        let (dec, _) = sun_position(utc(2026, 6, 21, 12));
        assert!((dec - 23.44).abs() < 0.3, "declination was {dec}");
    }

    /// Southern solstice is the mirror image.
    #[test]
    fn december_solstice_is_the_mirror() {
        let (dec, _) = sun_position(utc(2026, 12, 21, 12));
        assert!((dec + 23.44).abs() < 0.3, "declination was {dec}");
    }

    /// At the equinox the sun crosses the equator.
    #[test]
    fn equinox_declination_is_near_zero() {
        let (dec, _) = sun_position(utc(2026, 3, 20, 12));
        assert!(dec.abs() < 0.5, "declination was {dec}");
    }

    /// Around noon UTC at the equinox the sub-solar point is near 0 degN 0 degE.
    #[test]
    fn equinox_noon_on_the_equator_is_overhead() {
        let c = cos_zenith(0.0, 0.0, utc(2026, 3, 20, 12));
        assert!(c > 0.99, "cos(zenith) was {c}");
    }

    #[test]
    fn the_night_side_is_negative() {
        // The opposite meridian must be in darkness at the same instant.
        assert!(cos_zenith(0.0, 180.0, utc(2026, 3, 20, 12)) < -0.9);
    }

    /// Northern Europe in midsummer stays lit late into the evening.
    #[test]
    fn midsummer_evening_in_the_north_is_still_daylight() {
        assert!(cos_zenith(65.0, 20.0, utc(2026, 6, 21, 20)) > 0.0);
    }

    #[test]
    fn cached_sun_matches_the_direct_computation() {
        let epoch = 1_786_970_700;
        let sun = Sun::at(epoch);
        for (lat, lon) in [(50.0, 10.0), (0.0, -30.0), (70.0, 25.0)] {
            let a = sun.cos_zenith(lat, lon);
            let b = cos_zenith(lat, lon, epoch);
            assert!((a - b).abs() < 1e-12);
        }
    }
}
