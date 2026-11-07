//! Mission geometry: waypoint latitude/longitude/altitude to the FC's local
//! NED frame, anchored on the home position the FC reports in both frames.

/// WGS84 equatorial radius, metres. A flat-earth projection around home is
/// exact to well under a metre over a mission's few kilometres.
const EARTH_RADIUS_M: f64 = 6_378_137.0;

/// The home position in both frames (HOME_POSITION).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Home {
    pub lat_e7: i32,
    pub lon_e7: i32,
    pub alt_msl_m: f64,
    /// Home in the FC's local NED frame.
    pub ned: [f64; 3],
}

/// How a mission item's altitude is referenced (MAV_FRAME).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AltRef {
    /// Above mean sea level (GLOBAL 0, GLOBAL_INT 5).
    Msl,
    /// Above home (GLOBAL_RELATIVE_ALT 3, GLOBAL_RELATIVE_ALT_INT 6).
    Relative,
    /// Above terrain (GLOBAL_TERRAIN_ALT 10, _INT 11): drawn as relative, with
    /// the frame marked approximate, since the agent holds no terrain model.
    Terrain,
}

impl AltRef {
    pub fn from_frame(frame: u8) -> Self {
        match frame {
            0 | 5 => AltRef::Msl,
            10 | 11 => AltRef::Terrain,
            _ => AltRef::Relative,
        }
    }
}

/// North/east offset in metres of `(lat_e7, lon_e7)` from home.
pub fn ne_from_home(home: &Home, lat_e7: i32, lon_e7: i32) -> (f64, f64) {
    let lat0 = (home.lat_e7 as f64 / 1e7).to_radians();
    let dlat = ((lat_e7 as i64 - home.lat_e7 as i64) as f64 / 1e7).to_radians();
    let dlon = ((lon_e7 as i64 - home.lon_e7 as i64) as f64 / 1e7).to_radians();
    (dlat * EARTH_RADIUS_M, dlon * EARTH_RADIUS_M * lat0.cos())
}

/// A waypoint in local NED, and whether its height is approximate.
pub fn waypoint_ned(home: &Home, lat_e7: i32, lon_e7: i32, alt_m: f64, frame: u8) -> ([f64; 3], bool) {
    let (dn, de) = ne_from_home(home, lat_e7, lon_e7);
    let alt_ref = AltRef::from_frame(frame);
    let d = match alt_ref {
        AltRef::Msl => home.ned[2] - (alt_m - home.alt_msl_m),
        AltRef::Relative | AltRef::Terrain => home.ned[2] - alt_m,
    };
    ([home.ned[0] + dn, home.ned[1] + de, d], alt_ref == AltRef::Terrain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Home {
        Home { lat_e7: 470_000_000, lon_e7: 85_000_000, alt_msl_m: 400.0, ned: [2.0, -1.0, 0.5] }
    }

    #[test]
    fn north_east_offsets() {
        let (n, e) = ne_from_home(&home(), 470_000_000 + 899, 85_000_000);
        assert!((n - 10.0).abs() < 0.05, "{n}");
        assert!(e.abs() < 1e-9);
        let (_, e) = ne_from_home(&home(), 470_000_000, 85_000_000 + 1000);
        // 1e-4 deg of longitude at 47 deg north.
        assert!((e - 7.59).abs() < 0.02, "{e}");
    }

    #[test]
    fn altitude_frames() {
        let h = home();
        let (rel, approx) = waypoint_ned(&h, h.lat_e7, h.lon_e7, 30.0, 3);
        assert_eq!(rel, [2.0, -1.0, 0.5 - 30.0]);
        assert!(!approx);
        let (msl, _) = waypoint_ned(&h, h.lat_e7, h.lon_e7, 430.0, 0);
        assert!((msl[2] - (0.5 - 30.0)).abs() < 1e-9);
        let (_, approx) = waypoint_ned(&h, h.lat_e7, h.lon_e7, 30.0, 10);
        assert!(approx);
    }
}
