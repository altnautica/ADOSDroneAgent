//! The brake's two limits near walls: the clearance speed beside a wall and
//! the push back out of the margin.

use super::{BrakeGeom, BRAKE_SECTORS};
use crate::geo::{self, V2};

/// Clearance-based speed: within the brake's reach the horizontal speed is
/// held to this many m/s per metre of free space between the prop tips and
/// the nearest obstacle. A vehicle 2.25 m from both walls of a 4.5 m drift
/// flies at most 1.5 m/s; 1 m from a wall, 0.5 m/s.
pub const CONFINED_SPEED_PER_M: f32 = 0.8;
/// The clearance speed never holds the vehicle slower than this, so it can
/// always creep through a passage it fits.
pub const CONFINED_MIN_MPS: f32 = 0.5;
/// Inside the margin the vehicle is moved back out at this many m/s per
/// metre it is inside, up to [`PUSH_MAX_MPS`].
pub const PUSH_GAIN: f32 = 1.0;
pub const PUSH_MAX_MPS: f32 = 0.5;
/// A push at least this fast counts as the brake acting (`Braked::cut`);
/// below it, grid quantisation between two walls the vehicle is centred in.
pub const PUSH_REPORT_MPS: f32 = 0.1;
/// `out` held to the clearance speed `v_conf`, with the push back out of the
/// margin. Motion along the push (away from the walls) is not held to the
/// clearance speed, and the push sets its least speed rather than adding to
/// it: the caps' slide and the push both lead away from the wall, so summed
/// they would leave it faster than either asks. The result is never faster
/// than the larger of `out` and the push, so a speed cap applied before the
/// brake still holds (the push is at most `PUSH_MAX_MPS`, inside every cap).
pub(crate) fn with_push(out: V2, v_conf: f32, push: V2) -> V2 {
    let p = geo::norm(push);
    if p < 1e-6 {
        return geo::clamp_len(out, v_conf);
    }
    let e = geo::scale(push, 1.0 / p);
    let along = geo::dot(out, e);
    let push_v = along.max(p);
    let room = (geo::norm(out).max(p).powi(2) - push_v * push_v)
        .max(0.0)
        .sqrt();
    let rest = geo::clamp_len(geo::sub(out, geo::scale(e, along)), v_conf.min(room));
    geo::add(rest, geo::scale(e, push_v))
}

/// A surface at least this long (its nearest points per sector joined while
/// neighbours are within [`crate::obstacles::SURFACE_GAP_M`]) is a wall the
/// clearance speed counts, metres. A trunk, post or pole reads shorter: the
/// stop caps and the push keep the margin to it, and passing beside it at
/// speed brings the vehicle no nearer, where position error along a wall
/// runs the whole way beside it.
pub const WALL_MIN_M: f32 = 1.5;

/// The nearest of the per-sector points (`(direction toward it, distance)`)
/// that belong to a wall ([`WALL_MIN_M`]); infinite when none does. The
/// points are walked in bearing order (sectors with no point skipped: close
/// in, a 5° sector can fall between two cells of one wall).
fn wall_nearest(near: &[Option<(V2, f32)>]) -> f32 {
    let mut pts = [([0.0f32; 2], 0.0f32); BRAKE_SECTORS];
    let mut m = 0;
    for &(dir, d) in near.iter().flatten().take(BRAKE_SECTORS) {
        pts[m] = (geo::scale(dir, d), d);
        m += 1;
    }
    if m < 2 {
        return f32::INFINITY;
    }
    // The gap from point `i` to the next when the two are one surface.
    let gap = |i: usize| {
        let g = geo::dist(pts[i].0, pts[(i + 1) % m].0);
        (g <= crate::obstacles::SURFACE_GAP_M).then_some(g)
    };
    // A run starts where the surface from the point before it breaks.
    let Some(start) = (0..m).find(|&i| gap((i + m - 1) % m).is_none()) else {
        // One unbroken surface all round (or one short object seen from
        // both sides of a sector): a wall when long enough.
        let len: f32 = (0..m).filter_map(gap).sum();
        return if len >= WALL_MIN_M {
            pts[..m].iter().map(|p| p.1).fold(f32::INFINITY, f32::min)
        } else {
            f32::INFINITY
        };
    };
    let mut best = f32::INFINITY;
    let mut i = start;
    let mut seen = 0;
    while seen < m {
        let (mut len, mut low) = (0.0, f32::INFINITY);
        loop {
            low = low.min(pts[i].1);
            seen += 1;
            match gap(i) {
                Some(g) if seen < m => {
                    len += g;
                    i = (i + 1) % m;
                }
                _ => break,
            }
        }
        if len >= WALL_MIN_M {
            best = best.min(low);
        }
        i = (i + 1) % m;
    }
    best
}

/// The clearance speed and the push back out of the margin, from the nearest
/// obstacle point per sector (`(direction toward it, distance)`). The
/// clearance speed counts walls only ([`WALL_MIN_M`]); the push, every point.
pub fn confined(near: &[Option<(V2, f32)>], geom: BrakeGeom) -> (f32, V2) {
    let BrakeGeom { margin, body_m } = geom;
    let d_min = near
        .iter()
        .flatten()
        .map(|&(_, d)| d)
        .fold(f32::INFINITY, f32::min);
    if !d_min.is_finite() {
        return (f32::INFINITY, [0.0, 0.0]);
    }
    let d_wall = wall_nearest(near);
    let v_conf = if d_wall.is_finite() {
        (CONFINED_SPEED_PER_M * (d_wall - body_m)).max(CONFINED_MIN_MPS)
    } else {
        f32::INFINITY
    };
    let mut sum = [0.0, 0.0];
    let mut weight = 0.0;
    for &(dir, d) in near.iter().flatten() {
        let w = margin - d;
        if w > 0.0 {
            sum = geo::sub(sum, geo::scale(dir, w));
            weight += w;
        }
    }
    if weight <= 0.0 {
        return (v_conf, [0.0, 0.0]);
    }
    let depth = margin - d_min;
    let mut push = geo::scale(sum, PUSH_GAIN * depth / weight);
    // A long wall fills many sectors and a post one or two, so the sum can
    // point away from the wall and at a nearer post. The push may not close
    // on an in-margin point nearer than the nearest one it leaves (so two
    // walls of a narrow passage still centre the vehicle): drop its
    // component toward each such point, the nearest last so that one holds
    // exactly.
    let nearest = near
        .iter()
        .flatten()
        .find(|&&(_, d)| d == d_min)
        .map(|&(dir, _)| dir);
    for _ in 0..2 {
        let behind = near
            .iter()
            .flatten()
            .filter(|&&(dir, d)| d < margin && geo::dot(push, dir) < 0.0)
            .map(|&(_, d)| d)
            .fold(f32::INFINITY, f32::min);
        for &(dir, d) in near.iter().flatten() {
            if d < margin && d < behind {
                push = geo::sub(push, geo::scale(dir, geo::dot(push, dir).max(0.0)));
            }
        }
    }
    if let Some(dir) = nearest {
        push = geo::sub(push, geo::scale(dir, geo::dot(push, dir).max(0.0)));
    }
    (v_conf, geo::clamp_len(push, PUSH_MAX_MPS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_push_never_makes_the_setpoint_faster_than_asked() {
        // 0.5 m/s along a wall (the lowest GNSS-denied cap), pushed 0.4 m/s off it.
        let out = with_push([0.5, 0.0], 0.5, [0.0, -0.4]);
        assert!(geo::norm(out) <= 0.5 + 1e-5, "{out:?}");
        assert!(out[1] <= -0.4 + 1e-5, "the push keeps its speed: {out:?}");
        // A push faster than the ask sets the speed.
        let out = with_push([0.1, 0.0], 0.5, [0.0, -0.4]);
        assert!(geo::norm(out) <= 0.4 + 1e-5, "{out:?}");
    }
}
