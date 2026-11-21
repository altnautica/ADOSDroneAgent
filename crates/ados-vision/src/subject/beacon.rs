//! The subject's own GNSS position (the operator's phone shared as a
//! beacon), in the local NED frame the estimate lives in.

/// A geodetic position: WGS-84 latitude and longitude, degrees, and altitude,
/// metres (above mean sea level or the ellipsoid, the same for both ends of a
/// conversion).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geodetic {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f64,
}

const WGS84_A: f64 = 6_378_137.0;
const WGS84_E2: f64 = 6.694_379_990_14e-3;

/// `p` in the local NED frame whose origin is `origin`: a tangent plane at
/// the origin with the ellipsoid's radii of curvature there. Within a few
/// kilometres of the origin the error is well under a GNSS fix's own.
pub fn ned_from_geodetic(origin: &Geodetic, p: &Geodetic) -> [f64; 3] {
    let lat = origin.lat_deg.to_radians();
    let w = 1.0 - WGS84_E2 * lat.sin().powi(2);
    let r_east = WGS84_A / w.sqrt();
    let r_north = WGS84_A * (1.0 - WGS84_E2) / w.powf(1.5);
    let dlon = (p.lon_deg - origin.lon_deg + 540.0).rem_euclid(360.0) - 180.0;
    [
        (p.lat_deg - origin.lat_deg).to_radians() * r_north,
        dlon.to_radians() * r_east * lat.cos(),
        origin.alt_m - p.alt_m,
    ]
}

/// One beacon fix, already in local NED.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeaconFix {
    /// The fix's time, ns, on the clock the sightings use.
    pub stamp_ns: u64,
    pub ned: [f32; 3],
    /// The receiver's reported horizontal accuracy, metres: the radius
    /// holding the true position with 68 % confidence (Android's
    /// `Location.getAccuracy()`, MAVLink `FOLLOW_TARGET` carries it the same
    /// way).
    pub horiz_acc_m: f32,
}

/// The ratio of a 68 % circular radius to the per-axis sigma of a circular
/// Gaussian: `sqrt(-2 ln(1 - 0.68))`.
const RADIUS68_PER_SIGMA: f32 = 1.5096;

/// The smallest per-axis sigma a beacon fix is trusted with, metres: a phone
/// reporting better is not that good.
const MIN_SIGMA_M: f32 = 0.5;

impl BeaconFix {
    /// The per-axis horizontal sigma, metres.
    pub fn sigma_m(&self) -> f32 {
        (self.horiz_acc_m / RADIUS68_PER_SIGMA).max(MIN_SIGMA_M)
    }
}

/// What a beacon fix did to the estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeaconOutcome {
    /// Fused into the vision track.
    Fused,
    /// Started a track on the beacon alone (only when the caller opted in).
    Started,
    /// Not fused: no vision fix within the window, and the caller has not
    /// opted in to beacon-only tracking.
    NoVision,
    /// Not fused: too far from the vision track to be the same person.
    Gated,
    /// Not fused: older than the estimate, or not finite.
    Stale,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tangent_plane_offset_matches_the_metres_it_should() {
        let o = Geodetic {
            lat_deg: 52.0,
            lon_deg: 4.5,
            alt_m: 10.0,
        };
        // 0.001 degrees of latitude at 52 N is about 111.3 m.
        let p = Geodetic {
            lat_deg: 52.001,
            lon_deg: 4.5,
            alt_m: 12.0,
        };
        let ned = ned_from_geodetic(&o, &p);
        assert!((ned[0] - 111.3).abs() < 0.3, "{ned:?}");
        assert!(ned[1].abs() < 1e-9 && (ned[2] + 2.0).abs() < 1e-9);
        // Across the antimeridian is a short hop, not a world away.
        let a = Geodetic {
            lat_deg: 0.0,
            lon_deg: 179.9999,
            alt_m: 0.0,
        };
        let b = Geodetic {
            lon_deg: -179.9999,
            ..a
        };
        assert!((ned_from_geodetic(&a, &b)[1] - 22.3).abs() < 0.1);
    }

    #[test]
    fn the_sigma_follows_the_68_percent_radius() {
        let f = BeaconFix {
            stamp_ns: 0,
            ned: [0.0; 3],
            horiz_acc_m: 4.5,
        };
        assert!((f.sigma_m() - 2.98).abs() < 0.01);
        assert_eq!(
            BeaconFix {
                horiz_acc_m: 0.1,
                ..f
            }
            .sigma_m(),
            MIN_SIGMA_M
        );
    }
}
