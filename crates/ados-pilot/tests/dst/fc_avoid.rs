//! The simulated FC's proximity avoidance, as ArduCopter's simple avoidance
//! runs it with `AVOID_ENABLE` bit 2 (proximity) and `AVOID_BEHAVE` 1 (stop)
//! on the distances Navigation streams as `OBSTACLE_DISTANCE`: in GUIDED and
//! LOITER the velocity the position controller asks toward each obstacle in
//! range is cut to what stops `AVOID_MARGIN` short of it at
//! `AVOID_ACCEL_MAX` (`AC_Avoid::limit_velocity`: `sqrt(2·a·(d − margin))`).
//! AUTO and the returns are not limited: ArduCopter applies simple
//! avoidance to its pilot-input and GUIDED velocity paths only.
//!
//! Every cut is counted ([`Proximity::trips`]): in normal flight the pilot
//! keeps its larger margin and the FC never cuts anything.

/// `AVOID_ACCEL_MAX` default, m/s².
const ACCEL_MAX: f32 = 3.0;
/// The proximity sensor's range (the lidar's), m.
const RANGE_M: f32 = 12.0;
/// A cut smaller than this is not counted, m/s.
const TRIP_MPS: f32 = 0.05;

/// The obstacles the FC's proximity library sees and what it did.
#[derive(Debug, Default)]
pub struct Proximity {
    /// Vertical cylinders `(centre NE, radius)`, in truth.
    pub obstacles: Vec<([f32; 2], f32)>,
    /// Ticks on which the FC cut the asked velocity.
    pub trips: u32,
}

impl Proximity {
    /// `want` (NED m/s) from `pos` (truth NE) with the FC's parameters:
    /// cut toward each obstacle whose stop distance it would pass.
    pub fn limit(&mut self, pos: [f32; 2], want: [f32; 3], enable: f32, margin: f32) -> [f32; 3] {
        if (enable as u32) & 2 == 0 {
            return want;
        }
        let mut v = [want[0], want[1]];
        let mut cut = false;
        for &(c, r) in &self.obstacles {
            let rel = [c[0] - pos[0], c[1] - pos[1]];
            let centre = rel[0].hypot(rel[1]);
            let d = centre - r;
            if centre < 1e-3 || d > RANGE_M {
                continue;
            }
            let u = [rel[0] / centre, rel[1] / centre];
            let toward = v[0] * u[0] + v[1] * u[1];
            let max = (2.0 * ACCEL_MAX * (d - margin).max(0.0)).sqrt();
            if toward > max {
                cut |= toward - max > TRIP_MPS;
                let k = toward - max;
                v = [v[0] - k * u[0], v[1] - k * u[1]];
            }
        }
        if cut {
            self.trips += 1;
        }
        [v[0], v[1], want[2]]
    }
}
