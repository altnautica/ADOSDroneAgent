//! Local tangent-plane geodesy and the 3-vector the control laws work in.
//!
//! Every control law in this crate operates on metres in a local NED frame
//! (north / east / down), never on degrees. Beacons carry WGS84 degrees, so a
//! frame conversion sits at the crate boundary and nowhere else: mixing the two
//! inside a law is how a gain silently becomes 111 000× too strong.
//!
//! The projection is the flat-earth equirectangular one ArduPilot itself uses
//! (`AP_Common/Location.cpp`'s `LOCATION_SCALING_FACTOR`), with the same earth
//! radius, so a position this crate computes and a position the flight
//! controller computes from the same fix agree to well under the GPS noise
//! floor. Over the tens-of-metres spans a swarm lattice occupies the projection
//! error is nanometres; it is not valid for hundreds of kilometres and is not
//! used that way.

use std::f64::consts::PI;

/// Earth radius ArduPilot's location maths uses (`AP_Common/Location.h`).
/// Matching it exactly is deliberate: the FC and this crate must not disagree
/// about how far apart two fixes are.
pub const EARTH_RADIUS_M: f64 = 6_378_100.0;

/// Metres per degree of latitude under [`EARTH_RADIUS_M`]. 111 318.845…, which
/// is `LOCATION_SCALING_FACTOR * 1e7`.
pub const DEG_TO_M: f64 = EARTH_RADIUS_M * PI / 180.0;

/// Degrees per metre of latitude — the inverse of [`DEG_TO_M`].
pub const M_TO_DEG: f64 = 1.0 / DEG_TO_M;

/// A displacement or velocity in the local NED frame. Metres, or metres per
/// second: the type carries no unit because every law in this crate uses the
/// two interchangeably (a velocity command IS a scaled displacement).
///
/// `d` is DOWN-positive, matching MAVLink `GLOBAL_POSITION_INT.vz` and
/// `SET_POSITION_TARGET_GLOBAL_INT.vz`. A climb is therefore NEGATIVE `d`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Ned {
    pub n: f64,
    pub e: f64,
    pub d: f64,
}

impl Ned {
    pub const ZERO: Ned = Ned {
        n: 0.0,
        e: 0.0,
        d: 0.0,
    };

    pub const fn new(n: f64, e: f64, d: f64) -> Self {
        Self { n, e, d }
    }

    /// Horizontal-only vector (the `d` component zeroed). The hard-separation
    /// layer needs this: it abandons the horizontal solution and keeps only a
    /// vertical one.
    pub const fn horizontal(self) -> Self {
        Self {
            n: self.n,
            e: self.e,
            d: 0.0,
        }
    }

    pub fn scale(self, k: f64) -> Self {
        Self::new(self.n * k, self.e * k, self.d * k)
    }

    /// Squared 3-D magnitude. Preferred inside comparison loops: the nearest-set
    /// search orders by distance and never needs the square root.
    pub fn norm_sq(self) -> f64 {
        self.n * self.n + self.e * self.e + self.d * self.d
    }

    pub fn norm(self) -> f64 {
        self.norm_sq().sqrt()
    }

    /// Unit vector, or [`Ned::ZERO`] when the magnitude is too small to define a
    /// direction. Returning zero rather than a NaN-laden vector is load-bearing:
    /// two coincident drones must degrade to "no horizontal opinion" and let the
    /// hard-separation layer's deterministic vertical rule resolve them, not
    /// poison the whole command with NaN.
    pub fn unit(self) -> Self {
        let m = self.norm();
        if m <= f64::EPSILON {
            Self::ZERO
        } else {
            self.scale(1.0 / m)
        }
    }

    /// Clamp the magnitude to `max` while preserving direction. `max <= 0`
    /// yields zero.
    pub fn clamp_norm(self, max: f64) -> Self {
        if max <= 0.0 || !self.is_finite() {
            // A non-finite command is scrubbed to zero rather than propagated:
            // this is the last gate before a velocity reaches a flight
            // controller, and a NaN setpoint is a flyaway.
            return Self::ZERO;
        }
        let m = self.norm();
        if m <= max {
            self
        } else {
            self.scale(max / m)
        }
    }

    pub fn is_finite(self) -> bool {
        self.n.is_finite() && self.e.is_finite() && self.d.is_finite()
    }
}

impl std::ops::Add for Ned {
    type Output = Self;

    fn add(self, o: Self) -> Self {
        Self::new(self.n + o.n, self.e + o.e, self.d + o.d)
    }
}

impl std::ops::Sub for Ned {
    type Output = Self;

    fn sub(self, o: Self) -> Self {
        Self::new(self.n - o.n, self.e - o.e, self.d - o.d)
    }
}

/// The local-frame origin a set of geodetic fixes is projected about.
///
/// Built once per control tick from the drone's own fix, so the drone always
/// sits at [`Ned::ZERO`] and every neighbour offset is relative to it. Caching
/// `cos(lat)` here rather than recomputing it per neighbour is the only reason
/// this is a struct and not a pair of free functions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoOrigin {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f64,
    cos_lat: f64,
}

impl GeoOrigin {
    pub fn new(lat_deg: f64, lon_deg: f64, alt_m: f64) -> Self {
        Self {
            lat_deg,
            lon_deg,
            alt_m,
            cos_lat: lat_deg.to_radians().cos(),
        }
    }

    /// Project a geodetic fix into the local NED frame.
    pub fn to_ned(&self, lat_deg: f64, lon_deg: f64, alt_m: f64) -> Ned {
        Ned::new(
            (lat_deg - self.lat_deg) * DEG_TO_M,
            (lon_deg - self.lon_deg) * DEG_TO_M * self.cos_lat,
            -(alt_m - self.alt_m),
        )
    }

    /// Inverse of [`Self::to_ned`]: `(lat_deg, lon_deg, alt_m)`.
    ///
    /// Guards a pole-adjacent origin (`cos(lat) -> 0`) by leaving longitude at
    /// the origin's rather than dividing by ~zero and emitting an absurd
    /// setpoint. A swarm at the pole is not a supported case; a NaN setpoint
    /// radiated at the FC is a crash.
    pub fn to_geo(&self, p: Ned) -> (f64, f64, f64) {
        let lat = self.lat_deg + p.n * M_TO_DEG;
        let lon = if self.cos_lat.abs() < 1e-9 {
            self.lon_deg
        } else {
            self.lon_deg + p.e * M_TO_DEG / self.cos_lat
        };
        (lat, lon, self.alt_m - p.d)
    }
}

/// Degrees to the MAVLink `1e7` fixed-point integer form, saturating instead of
/// wrapping. A wrapped latitude is a setpoint on the other side of the planet.
pub fn deg_to_e7(deg: f64) -> i32 {
    if !deg.is_finite() {
        return 0;
    }
    (deg * 1e7).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

/// MAVLink `1e7` fixed-point degrees back to degrees.
pub fn e7_to_deg(e7: i32) -> f64 {
    e7 as f64 / 1e7
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIRKENES: (f64, f64, f64) = (69.7269, 30.0453, 120.0);
    const BENGALURU: (f64, f64, f64) = (12.9716, 77.5946, 920.0);

    #[test]
    fn one_degree_of_latitude_is_the_standard_arc() {
        let o = GeoOrigin::new(0.0, 0.0, 0.0);
        let p = o.to_ned(1.0, 0.0, 0.0);
        // 111.3 km, the textbook figure for one degree of latitude.
        assert!((p.n - 111_318.845).abs() < 0.01, "north {}", p.n);
        assert_eq!(p.e, 0.0);
    }

    #[test]
    fn east_scales_with_the_cosine_of_latitude() {
        // The whole point of caching cos(lat): a degree of longitude is 111 km at
        // the equator and 39 km at Kirkenes. Scaling it as if it were constant
        // would put a formation offset 3x too far east up there.
        let equator = GeoOrigin::new(0.0, 0.0, 0.0).to_ned(0.0, 1.0, 0.0);
        let north =
            GeoOrigin::new(KIRKENES.0, KIRKENES.1, 0.0).to_ned(KIRKENES.0, KIRKENES.1 + 1.0, 0.0);
        assert!((equator.e - 111_318.845).abs() < 0.01);
        let expect = 111_318.845 * KIRKENES.0.to_radians().cos();
        assert!((north.e - expect).abs() < 0.01, "east {}", north.e);
        assert!(north.e < equator.e * 0.36, "cos(69.7) is about 0.347");
    }

    #[test]
    fn down_is_positive_down() {
        let o = GeoOrigin::new(BENGALURU.0, BENGALURU.1, BENGALURU.2);
        // 10 m HIGHER than the origin is 10 m of NEGATIVE down.
        let up = o.to_ned(BENGALURU.0, BENGALURU.1, BENGALURU.2 + 10.0);
        assert!((up.d + 10.0).abs() < 1e-9, "d {}", up.d);
    }

    #[test]
    fn geodetic_round_trip_is_exact_at_lattice_scale() {
        for origin in [KIRKENES, BENGALURU, (-33.8688, 151.2093, 40.0)] {
            let o = GeoOrigin::new(origin.0, origin.1, origin.2);
            for offset in [
                Ned::new(0.0, 0.0, 0.0),
                Ned::new(8.0, -8.0, 2.5),
                Ned::new(-120.0, 240.0, -35.0),
                Ned::new(500.0, 500.0, 0.0),
            ] {
                let (lat, lon, alt) = o.to_geo(offset);
                let back = o.to_ned(lat, lon, alt);
                assert!(
                    (back - offset).norm() < 1e-6,
                    "origin {origin:?} offset {offset:?} -> {back:?}"
                );
            }
        }
    }

    #[test]
    fn pole_origin_does_not_emit_a_nan_longitude() {
        let o = GeoOrigin::new(90.0, 0.0, 100.0);
        let (lat, lon, alt) = o.to_geo(Ned::new(10.0, 10.0, 0.0));
        assert!(lat.is_finite() && lon.is_finite() && alt.is_finite());
        assert_eq!(lon, 0.0, "longitude is pinned rather than divided by ~0");
    }

    #[test]
    fn unit_of_a_degenerate_vector_is_zero_not_nan() {
        assert_eq!(Ned::ZERO.unit(), Ned::ZERO);
        assert!(Ned::ZERO.unit().is_finite());
        let u = Ned::new(3.0, 4.0, 0.0).unit();
        assert!((u.norm() - 1.0).abs() < 1e-12);
        assert!((u.n - 0.6).abs() < 1e-12 && (u.e - 0.8).abs() < 1e-12);
    }

    #[test]
    fn clamp_norm_preserves_direction_and_caps_magnitude() {
        let v = Ned::new(30.0, 40.0, 0.0); // magnitude 50
        let c = v.clamp_norm(10.0);
        assert!((c.norm() - 10.0).abs() < 1e-12);
        assert!((c.n - 6.0).abs() < 1e-12 && (c.e - 8.0).abs() < 1e-12);
        // Already inside the cap: untouched, not renormalised up.
        let small = Ned::new(1.0, 0.0, 0.0);
        assert_eq!(small.clamp_norm(10.0), small);
        assert_eq!(v.clamp_norm(0.0), Ned::ZERO);
    }

    #[test]
    fn horizontal_drops_only_the_vertical_component() {
        let v = Ned::new(1.0, 2.0, 3.0);
        assert_eq!(v.horizontal(), Ned::new(1.0, 2.0, 0.0));
    }

    #[test]
    fn e7_conversion_saturates_instead_of_wrapping() {
        assert_eq!(deg_to_e7(12.9716), 129_716_000);
        assert!((e7_to_deg(129_716_000) - 12.9716).abs() < 1e-9);
        // A garbage input must not wrap into a valid-looking antipodal fix.
        assert_eq!(deg_to_e7(f64::NAN), 0);
        assert_eq!(deg_to_e7(1e9), i32::MAX);
        assert_eq!(deg_to_e7(-1e9), i32::MIN);
    }
}
