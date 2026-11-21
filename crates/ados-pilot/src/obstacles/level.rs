//! Levelling one planar return with the vehicle's attitude: whether it is
//! the ground, and how high over the scan plane it lies.

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

/// The body FRD beam at `theta` (clockwise from the nose) rotated by roll
/// (about x) then pitch (about y): `(forward, right, down)` per metre of
/// slant range.
fn levelled(theta: f32, l: Level) -> (f32, f32, f32) {
    let (bx, by) = (theta.cos(), theta.sin());
    let (sr, cr) = l.roll.sin_cos();
    let (sp, cp) = l.pitch.sin_cos();
    // Rx(roll): y' = y·cr, z' = y·sr
    let (y1, z1) = (by * cr, by * sr);
    // Ry(pitch): x'' = x·cp + z'·sp, z'' = −x·sp + z'·cp
    (bx * cp + z1 * sp, y1, -bx * sp + z1 * cp)
}

/// One beam levelled: its horizontal unit direction (local NE), its
/// horizontal metres per metre of slant range, and its rise over the scan
/// plane per horizontal metre (negative when it points down).
pub(super) fn beam(theta: f32, l: Level) -> (V2, f32, f32) {
    let (x, y, z) = levelled(theta, l);
    let hz = x.hypot(y).max(1e-6);
    let dir = geo::body_to_ned(x / hz, y / hz, l.yaw);
    (dir, hz, -z / hz)
}

/// Level one return: its horizontal position, its height over the scan
/// plane ([`Hit::lift_m`]; [`Hit::overhead`] past [`SLICE_HALF_M`]), and
/// whether it is the ground (closer to it than [`GROUND_REJECT_M`] after
/// levelling).
pub(super) fn level_return(range: f32, theta: f32, l: Level, pos_ne: V2) -> (Hit, bool) {
    let (x, y, z) = levelled(theta, l);
    let (x2, y1, z2) = (x * range, y * range, z * range);
    // z2 is down: the ground below, the slice's top at −SLICE_HALF_M.
    let ground = l.agl.is_some_and(|agl| agl - z2 < GROUND_REJECT_M);
    let hit = Hit {
        body_bearing: geo::atan2(y1, x2),
        range_m: x2.hypot(y1),
        ne: geo::add(pos_ne, geo::body_to_ned(x2, y1, l.yaw)),
        overhead: -z2 > SLICE_HALF_M,
        lift_m: -z2,
    };
    (hit, ground)
}
