//! The local-tangent-plane geodesy and kinematics the neighbour table runs on.
//!
//! Two operations, both pure: how far apart two geodetic points are, and where a
//! beacon's sender will be after `dt` seconds at its reported velocity. Split out
//! from the table because they are the parts a separation law's correctness actually
//! rests on, and they are testable with no table, no radio and no clock.

use crate::beacon::SwarmBeacon;

/// Earth radius for the local-tangent-plane conversions, matching
/// `ados_atlas::pose_source::geodetic_to_enu` so a position converted by either crate
/// lands in the same place.
pub const R_EARTH: f64 = 6_378_137.0;

/// Straight-line distance in metres between two `(lat_deg, lon_deg, alt_m)` points:
/// the equirectangular east/north projection plus the altitude difference.
///
/// The longitude scale is taken at the **midpoint** latitude, not at `a`'s. Scaling by
/// one endpoint's latitude (as the one-way conversion `geodetic_to_enu` does,
/// correctly, about a fixed session anchor) would make
/// `distance_m(a, b) != distance_m(b, a)`. An asymmetric metric is a real hazard here
/// rather than an aesthetic one: two drones running the same separation law would
/// compute different distances to each other and disagree about which has to give way.
///
/// Accurate to well under a metre across the hundreds of metres a swarm spans, an
/// order of magnitude finer than any separation threshold.
pub fn distance_m(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    let north = (b.0 - a.0).to_radians() * R_EARTH;
    let mid_lat = (a.0 + b.0) / 2.0;
    let east = (b.1 - a.1).to_radians() * mid_lat.to_radians().cos() * R_EARTH;
    let up = b.2 - a.2;
    (north * north + east * east + up * up).sqrt()
}

/// A beacon's reported position as the `(lat_deg, lon_deg, alt_m)` triple.
pub fn position_of(b: &SwarmBeacon) -> (f64, f64, f64) {
    (b.lat_deg(), b.lon_deg(), b.alt_m())
}

/// Dead-reckon a beacon's position forward by `dt` seconds at its reported constant
/// velocity, as `(lat_deg, lon_deg, alt_m)`.
///
/// This predict step is what lets a 10 Hz control loop run against a 2 Hz beacon:
/// between beacons a neighbour's position is extrapolated rather than held, so the
/// separation layer sees a closing aircraft move continuously instead of jumping
/// 500 ms at a time.
pub fn dead_reckon(b: &SwarmBeacon, dt: f64) -> (f64, f64, f64) {
    let lat = b.lat_deg();
    let d_lat = (b.vx_ms() * dt / R_EARTH).to_degrees();
    let cos_lat = lat.to_radians().cos();
    let d_lon = if cos_lat.abs() < 1e-12 {
        // At a pole every longitude is the same place; holding it is the only answer
        // that is not a division blow-up.
        0.0
    } else {
        (b.vy_ms() * dt / (R_EARTH * cos_lat)).to_degrees()
    };
    // `vz` is NED down-positive, so a climb (negative vz) gains altitude.
    (lat + d_lat, b.lon_deg() + d_lon, b.alt_m() - b.vz_ms() * dt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bengaluru, so the longitude scaling is exercised at a real non-zero latitude
    /// rather than on the equator where `cos(lat)` is 1 and a missing scale factor
    /// would pass.
    const LAT: f64 = 12.9716;
    const LON: f64 = 77.5946;

    /// The longitude scale is the easy thing to omit. At this latitude a degree of
    /// longitude is ~0.975 of a degree of latitude in metres, so a scale-free
    /// implementation mis-measures east-west separation by ~2.5%.
    #[test]
    fn distance_scales_longitude_by_the_cosine_of_the_latitude() {
        let one_deg_north = distance_m((LAT, LON, 0.0), (LAT + 1.0, LON, 0.0));
        let one_deg_east = distance_m((LAT, LON, 0.0), (LAT, LON + 1.0, 0.0));
        assert!(one_deg_east < one_deg_north);
        let ratio = one_deg_east / one_deg_north;
        assert!(
            (ratio - LAT.to_radians().cos()).abs() < 1e-9,
            "ratio {ratio} must be cos(lat)"
        );
        // Altitude contributes to the norm.
        assert!((distance_m((LAT, LON, 0.0), (LAT, LON, 12.0)) - 12.0).abs() < 1e-9);
        assert_eq!(distance_m((LAT, LON, 0.0), (LAT, LON, 0.0)), 0.0);
    }

    /// Symmetry is a safety property, not an aesthetic one: two drones running the
    /// same separation law must compute the same distance to each other, or they
    /// disagree about which has to give way. Scaling longitude by one endpoint's
    /// latitude instead of the midpoint breaks this — the tolerance is EXACT
    /// equality, because the midpoint construction makes it exact.
    #[test]
    fn the_distance_metric_is_exactly_symmetric() {
        let cases = [
            ((LAT, LON, 0.0), (LAT + 0.001, LON + 0.001, 30.0)),
            ((LAT, LON, 100.0), (LAT - 0.05, LON + 0.05, -20.0)),
            // A large latitude span, where an endpoint-scaled metric diverges most.
            ((0.0, 0.0, 0.0), (60.0, 10.0, 0.0)),
            ((-33.9, 18.4, 5.0), (55.7, 12.6, 500.0)),
        ];
        for (a, b) in cases {
            assert_eq!(
                distance_m(a, b),
                distance_m(b, a),
                "asymmetric between {a:?} and {b:?}"
            );
        }
    }

    /// Dead reckoning against an arithmetically known answer: 10 m/s north for 2 s is
    /// 20 m north, which at this Earth radius is a specific number of degrees. A sign
    /// flip or a missing `cos(lat)` fails this.
    #[test]
    fn dead_reckoning_moves_a_known_velocity_to_the_right_place() {
        let b = SwarmBeacon {
            lat: (LAT * 1e7) as i32,
            lon: (LON * 1e7) as i32,
            alt_dm: 500,  // 50.0 m
            vx_cms: 1000, // 10 m/s north
            vy_cms: 500,  // 5 m/s east
            vz_cms: -200, // 2 m/s CLIMB (vz is down-positive)
            ..SwarmBeacon::default()
        };
        let (lat, lon, alt) = dead_reckon(&b, 2.0);

        // 20 m north and 10 m east, in degrees, at this latitude.
        let want_lat = LAT + (20.0f64 / R_EARTH).to_degrees();
        let want_lon = LON + (10.0f64 / (R_EARTH * LAT.to_radians().cos())).to_degrees();
        assert!((lat - want_lat).abs() < 1e-9, "lat {lat} want {want_lat}");
        assert!((lon - want_lon).abs() < 1e-9, "lon {lon} want {want_lon}");
        // Climbing at 2 m/s for 2 s: 50 m becomes 54 m. A sign error gives 46.
        assert!((alt - 54.0).abs() < 1e-9, "alt {alt} want 54.0");

        // Zero elapsed time returns the beacon's own position untouched, and the
        // displacement is linear in dt.
        let (lat0, lon0, alt0) = dead_reckon(&b, 0.0);
        assert_eq!((lat0, lon0, alt0), position_of(&b));
        let (lat1, _, _) = dead_reckon(&b, 1.0);
        assert!(
            ((lat - LAT) / (lat1 - LAT) - 2.0).abs() < 1e-6,
            "linear in dt"
        );
    }

    /// A descending neighbour must lose altitude. The `vz` sign convention is the
    /// single easiest thing to invert, and inverting it makes the separation layer
    /// climb into a descending aircraft.
    #[test]
    fn a_descending_neighbour_loses_altitude() {
        let sinking = SwarmBeacon {
            alt_dm: 500,
            vz_cms: 300, // 3 m/s DOWN
            ..SwarmBeacon::default()
        };
        let (_, _, alt) = dead_reckon(&sinking, 2.0);
        assert!((alt - 44.0).abs() < 1e-9, "alt {alt} want 44.0");
    }

    /// At a pole `cos(lat)` is zero and the longitude term would divide by it. Holding
    /// longitude is the only answer that is not an infinity propagating into a
    /// separation distance.
    #[test]
    fn dead_reckoning_at_a_pole_holds_longitude_rather_than_dividing_by_zero() {
        let polar = SwarmBeacon {
            lat: 900_000_000, // 90.0 degrees
            lon: 0,
            vy_cms: 1000, // 10 m/s east
            ..SwarmBeacon::default()
        };
        let (lat, lon, _) = dead_reckon(&polar, 1.0);
        assert!(lat.is_finite() && lon.is_finite());
        assert_eq!(lon, 0.0, "longitude is held at the pole");
    }
}
