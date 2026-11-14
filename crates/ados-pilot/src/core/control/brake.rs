//! The stick brake and the geofence: pure velocity limits the control loop
//! applies before a setpoint goes out.

use super::{BRAKE_LAG_S, BRAKE_RANGE_M, BRAKE_SECTORS, HARD_STOP_FRAC};
use crate::geo::{self, V2};

/// Horizontal distance from the lidar (the vehicle centre) to the prop tips,
/// metres: the x500 class (0.5 m motor diagonal, 10" props) and smaller.
pub const BODY_RADIUS_M: f32 = 0.4;
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
/// The scans' closing speed is trusted up to this many times the EKF's
/// horizontal speed plus [`SCAN_CLOSING_FLOOR_MPS`].
pub const SCAN_CLOSING_VEL_FACTOR: f32 = 2.0;
pub const SCAN_CLOSING_FLOOR_MPS: f32 = 1.0;

/// What the stick brake did to one velocity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Braked {
    /// The velocity to fly.
    pub v: V2,
    /// The brake changed the velocity: it reduced the speed, or added the
    /// push back out of the margin.
    pub cut: bool,
    /// An obstacle is inside the hard-stop distance (`HARD_STOP_FRAC`·margin):
    /// only motion away from it is left.
    pub close: bool,
}

/// Cap speed toward obstacles at `sqrt(2·a·max(0, d − margin))`, keeping the
/// component along them so the vehicle slides.
///
/// Every obstacle point near the vehicle (occupied grid cell or scan hit)
/// caps the velocity component toward it, so a narrow column cannot slip
/// between sample rays; per 5° bearing sector the most restrictive point is
/// kept. So does the scan's horizon (where each beam stops seeing: the floor
/// a pitched beam meets, or the lidar's range): the vehicle never goes
/// faster than it can stop within the space it has seen free. Each cap
/// allows for [`BRAKE_LAG_S`], the FC's response ([`stop_cap`]), so the
/// commanded stop still ends outside the margin. The result is the velocity
/// nearest to `v` that meets every cap ([`nearest_within`]), so a cap adds
/// no sideways motion of its own. Inside the
/// hard-stop distance of an obstacle nothing but motion away from it is kept.
///
/// Two limits keep the vehicle off walls in confined spaces, whoever flies
/// (sticks, goto, skill goal, swarm): the speed is held to the clearance
/// speed ([`CONFINED_SPEED_PER_M`]), so position error and overshoot, which
/// grow with speed, stay small next to walls; and an obstacle inside the
/// margin adds a push away from it ([`PUSH_GAIN`]), weighted per sector by
/// how deep each nearest point is inside, so drift that carried the vehicle
/// inside is undone and opposite walls of a passage narrower than twice the
/// margin centre it. The push uses the obstacles' positions relative to the
/// vehicle, so it holds even when the vehicle's own position estimate drifts.
pub fn brake_limit(
    obs: &crate::obstacles::Obstacles,
    pos: V2,
    vel: V2,
    v: V2,
    a_h: f32,
    margin: f32,
    now_ms: u64,
) -> Braked {
    // A standing obstacle closes no faster than the vehicle flies: the scans'
    // range rate is trusted up to twice the EKF speed plus 1 m/s (an estimate
    // trailing the real motion), so a nearer surface stepping into a sector
    // (a post in front of a wall, a flickering edge) is not read as motion.
    let scan_bound = SCAN_CLOSING_VEL_FACTOR * geo::norm(vel) + SCAN_CLOSING_FLOOR_MPS;
    let speed = geo::norm(v)
        .max(geo::norm(vel))
        .max(obs.max_scan_closing(now_ms).min(scan_bound));
    let reach = (margin + speed * speed / (2.0 * a_h) + speed * BRAKE_LAG_S + 1.0)
        .clamp(margin, BRAKE_RANGE_M);
    let stop_at = HARD_STOP_FRAC * margin;
    // Per sector: (unit direction toward the point, cap on speed along it).
    let mut caps: [Option<(V2, f32)>; BRAKE_SECTORS] = [None; BRAKE_SECTORS];
    // Per sector: (unit direction, distance) of the nearest obstacle point.
    let mut near: [Option<(V2, f32)>; BRAKE_SECTORS] = [None; BRAKE_SECTORS];
    let mut away = [0.0, 0.0];
    let mut close = false;
    let obstacles = obs.points_near(pos, reach, now_ms).map(|p| (p, true));
    let horizon = obs.horizon_near(pos, reach, now_ms).map(|p| (p, false));
    for (p, obstacle) in obstacles.chain(horizon) {
        let rel = geo::sub(p, pos);
        let r = geo::norm(rel);
        if r < 1e-3 {
            continue;
        }
        let dir = geo::scale(rel, 1.0 / r);
        // Distance to the cell's near face (conservative for a scan point).
        let d = (r - crate::obstacles::CELL_M / 2.0).max(0.0);
        if obstacle && d < stop_at {
            close = true;
            away = geo::sub(away, dir);
        }
        // Closing speed: the EKF's, or the scans' own range rate when that
        // is faster (an estimate lagging the real motion brakes too late).
        let scan = obs.scan_closing(dir, now_ms).min(scan_bound);
        let closing = geo::dot(vel, dir).max(scan);
        let cap = stop_cap(d, closing, a_h, margin);
        let sector = (geo::bearing_of(dir).rem_euclid(std::f32::consts::TAU)
            / std::f32::consts::TAU
            * BRAKE_SECTORS as f32) as usize
            % BRAKE_SECTORS;
        if caps[sector].is_none_or(|(_, c)| cap < c) {
            caps[sector] = Some((dir, cap));
        }
        if obstacle && near[sector].is_none_or(|(_, n)| d < n) {
            near[sector] = Some((dir, d));
        }
    }
    let out = if close {
        let n = geo::norm(away);
        if n > 1e-3 {
            let e = geo::scale(away, 1.0 / n);
            geo::scale(e, geo::dot(v, e).max(0.0))
        } else {
            [0.0, 0.0]
        }
    } else {
        nearest_within(v, &caps)
    };
    let (v_conf, push) = confined(&near, margin);
    let out = with_push(out, v_conf, push);
    Braked {
        v: out,
        cut: geo::norm(out) < geo::norm(v) - 1e-3 || geo::norm(push) >= PUSH_REPORT_MPS,
        close,
    }
}

/// `out` held to the clearance speed `v_conf`, with the push back out of the
/// margin. Motion along the push (away from the walls) is not held to the
/// clearance speed, and the push sets its least speed rather than adding to
/// it: the caps' slide and the push both lead away from the wall, so summed
/// they would leave it faster than either asks. The result is never faster
/// than the larger of `out` and the push, so a speed cap applied before the
/// brake still holds (the push is at most `PUSH_MAX_MPS`, inside every cap).
fn with_push(out: V2, v_conf: f32, push: V2) -> V2 {
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

/// The clearance speed and the push back out of the margin, from the nearest
/// obstacle point per sector (`(direction toward it, distance)`).
pub fn confined(near: &[Option<(V2, f32)>], margin: f32) -> (f32, V2) {
    let d_min = near
        .iter()
        .flatten()
        .map(|&(_, d)| d)
        .fold(f32::INFINITY, f32::min);
    if !d_min.is_finite() {
        return (f32::INFINITY, [0.0, 0.0]);
    }
    let v_conf = (CONFINED_SPEED_PER_M * (d_min - BODY_RADIUS_M)).max(CONFINED_MIN_MPS);
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

/// The speed toward a point `d` away (beyond `margin`) that still stops in
/// time when the FC answers a cut after [`BRAKE_LAG_S`]: the vehicle covers
/// `v·T` before braking at `a`, so `v·T + v²/2a ≤ d − margin`; and while it
/// still closes at `closing` (measured) it covers `closing·T` first.
pub fn stop_cap(d: f32, closing: f32, a: f32, margin: f32) -> f32 {
    let t = BRAKE_LAG_S;
    let room = (d - margin).max(0.0);
    let reacting = -a * t + ((a * t).powi(2) + 2.0 * a * room).sqrt();
    let coasting = (2.0 * a * (room - closing * t).max(0.0)).sqrt();
    reacting.min(coasting)
}

/// The point nearest to `v` inside every half-plane `dir·x ≤ cap` (the
/// origin is always inside). In 2-D the answer is `v` itself, its projection
/// onto one constraint line, or a vertex of two; at the answer some
/// constraint `v` violates is active (KKT), so only lines `v` crosses are
/// projected onto and paired with the rest.
fn nearest_within(v: V2, caps: &[Option<(V2, f32)>; BRAKE_SECTORS]) -> V2 {
    const TOL: f32 = 1e-4;
    let feasible = |x: V2| {
        caps.iter()
            .flatten()
            .all(|&(dir, cap)| geo::dot(dir, x) <= cap + TOL)
    };
    if feasible(v) {
        return v;
    }
    let mut best = [0.0, 0.0];
    let mut best_d = geo::norm(v);
    let mut offer = |x: V2| {
        let d = geo::dist(x, v);
        if d < best_d && feasible(x) {
            best = x;
            best_d = d;
        }
    };
    for &(di, ci) in caps.iter().flatten() {
        let over = geo::dot(di, v) - ci;
        if over <= 0.0 {
            continue;
        }
        offer(geo::sub(v, geo::scale(di, over)));
        for &(dj, cj) in caps.iter().flatten() {
            let det = di[0] * dj[1] - di[1] * dj[0];
            if det.abs() < 1e-6 {
                continue;
            }
            offer([
                (ci * dj[1] - cj * di[1]) / det,
                (di[0] * cj - dj[0] * ci) / det,
            ]);
        }
    }
    best
}

/// Clamp the outward component at the `max_dist` boundary from home.
pub fn geofence(pos: V2, home: V2, max_dist: f32, v: V2) -> V2 {
    let rel = geo::sub(pos, home);
    let d = geo::norm(rel);
    if d < max_dist || d < 1e-3 {
        return v;
    }
    let out_dir = geo::scale(rel, 1.0 / d);
    let outward = geo::dot(v, out_dir);
    if outward > 0.0 {
        geo::sub(v, geo::scale(out_dir, outward))
    } else {
        v
    }
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
