//! Levelling one planar return with the vehicle's attitude: whether it is
//! the ground, and whether it lies over the flight slice.

use super::{Hit, Level, GROUND_REJECT_M, SLICE_HALF_M};
use crate::geo::{self, V2};

/// Level one return and drop it when it is the ground. A return over the
/// flight slice is kept, marked [`Hit::overhead`].
pub fn level_hit(range: f32, theta: f32, l: Level, pos_ne: V2) -> Option<Hit> {
    match level_return(range, theta, l, pos_ne) {
        (hit, false) => Some(hit),
        (_, true) => None,
    }
}

/// Level one return: its horizontal position, whether it lies more than
/// [`SLICE_HALF_M`] above the scan plane ([`Hit::overhead`]), and whether
/// it is the ground (closer to it than [`GROUND_REJECT_M`] after
/// levelling).
pub(super) fn level_return(range: f32, theta: f32, l: Level, pos_ne: V2) -> (Hit, bool) {
    // Body FRD point, then rotate by roll (about x) and pitch (about y).
    let (bx, by) = (range * theta.cos(), range * theta.sin());
    let (sr, cr) = l.roll.sin_cos();
    let (sp, cp) = l.pitch.sin_cos();
    // Rx(roll): y' = y·cr, z' = y·sr
    let (y1, z1) = (by * cr, by * sr);
    // Ry(pitch): x'' = x·cp + z'·sp, z'' = −x·sp + z'·cp
    let x2 = bx * cp + z1 * sp;
    let z2 = -bx * sp + z1 * cp;
    // z2 is down: the ground below, the slice's top at −SLICE_HALF_M.
    let ground = l.agl.is_some_and(|agl| agl - z2 < GROUND_REJECT_M);
    let hit = Hit {
        body_bearing: geo::atan2(y1, x2),
        range_m: x2.hypot(y1),
        ne: geo::add(pos_ne, geo::body_to_ned(x2, y1, l.yaw)),
        overhead: -z2 > SLICE_HALF_M,
    };
    (hit, ground)
}
