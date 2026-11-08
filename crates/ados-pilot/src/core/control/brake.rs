//! The stick brake and the geofence: pure velocity limits the control loop
//! applies before a setpoint goes out.

use super::{BRAKE_LAG_S, BRAKE_RANGE_M, BRAKE_SECTORS, HARD_STOP_FRAC};
use crate::geo::{self, V2};

/// What the stick brake did to one velocity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Braked {
    /// The velocity to fly.
    pub v: V2,
    /// The brake reduced the speed.
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
pub fn brake_limit(
    obs: &crate::obstacles::Obstacles,
    pos: V2,
    vel: V2,
    v: V2,
    a_h: f32,
    margin: f32,
    now_ms: u64,
) -> Braked {
    let speed = geo::norm(v).max(geo::norm(vel));
    let reach = (margin + speed * speed / (2.0 * a_h) + speed * BRAKE_LAG_S + 1.0)
        .clamp(margin, BRAKE_RANGE_M);
    let stop_at = HARD_STOP_FRAC * margin;
    // Per sector: (unit direction toward the point, cap on speed along it).
    let mut caps: [Option<(V2, f32)>; BRAKE_SECTORS] = [None; BRAKE_SECTORS];
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
        let cap = stop_cap(d, geo::dot(vel, dir).max(0.0), a_h, margin);
        let sector = (geo::bearing_of(dir).rem_euclid(std::f32::consts::TAU)
            / std::f32::consts::TAU
            * BRAKE_SECTORS as f32) as usize
            % BRAKE_SECTORS;
        if caps[sector].is_none_or(|(_, c)| cap < c) {
            caps[sector] = Some((dir, cap));
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
    Braked {
        v: out,
        cut: geo::norm(out) < geo::norm(v) - 1e-3,
        close,
    }
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
