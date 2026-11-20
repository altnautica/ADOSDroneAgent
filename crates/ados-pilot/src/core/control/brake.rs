//! The stick brake and the geofence: pure velocity limits the control loop
//! applies before a setpoint goes out.

use super::{BRAKE_LAG_S, BRAKE_RANGE_M, BRAKE_SECTORS, HARD_STOP_FRAC};
use crate::core::limit;
use crate::geo::{self, V2};

mod clearance;
mod sweep;

pub use clearance::*;
use sweep::{Sweep, BRAKE_CAPS, SWEEP_DEPTH_SLACK_M, SWEEP_PATHS, SWEEP_ROUNDS};

/// What the brake keeps clear: the obstacle margin and the vehicle's own
/// radius from the lidar (the CG) to its outermost rigid extent
/// ([`crate::config::PilotSection::body_radius_m`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrakeGeom {
    pub margin: f32,
    pub body_m: f32,
}
/// The scans' closing speed toward a direction is trusted up to this many
/// times the EKF's velocity component along it, plus a floor
/// ([`scan_closing_bound`]).
pub const SCAN_CLOSING_VEL_FACTOR: f32 = 2.0;
/// The closing speed the scans may read toward a direction the vehicle is not
/// flying: a creep under what the estimate resolves. A hovering vehicle among
/// trunks reads occlusion steps (one trunk stepping in front of another) and
/// range noise as closing; a full 1 m/s in every direction cut its stop cap
/// to nothing 2 m from every trunk around it.
pub const SCAN_CLOSING_FLOOR_MPS: f32 = 0.3;
/// The floor straight along the estimated flight line once the estimate
/// reads [`SCAN_CLOSING_LAG_FROM_MPS`]: an estimate trailing the real motion
/// (half or a third of it on a full-stick start) must still brake as the true
/// speed toward what lies ahead.
pub const SCAN_CLOSING_LAG_MPS: f32 = 1.0;
/// The estimated speed from which the floor along the flight line is the full
/// [`SCAN_CLOSING_LAG_MPS`]; slower, it ramps from [`SCAN_CLOSING_FLOOR_MPS`],
/// so a hover's velocity noise points it nowhere.
pub const SCAN_CLOSING_LAG_FROM_MPS: f32 = 0.5;

/// The most the brake trusts the scans' closing speed toward `dir` (unit) at
/// the EKF velocity `vel`: a standing obstacle closes no faster than the
/// vehicle flies toward it, so twice that component, plus a floor for an
/// estimate trailing the real motion. The floor is
/// [`SCAN_CLOSING_FLOOR_MPS`] toward directions the vehicle flies away from
/// or across (or while it hovers), rising with the cosine to the flight line
/// to [`SCAN_CLOSING_LAG_MPS`] straight along it.
pub fn scan_closing_bound(vel: V2, dir: V2) -> f32 {
    let along = geo::dot(vel, dir).max(0.0);
    let lag = (SCAN_CLOSING_LAG_MPS - SCAN_CLOSING_FLOOR_MPS) * along
        / geo::norm(vel).max(SCAN_CLOSING_LAG_FROM_MPS);
    SCAN_CLOSING_VEL_FACTOR * along + SCAN_CLOSING_FLOOR_MPS + lag
}

/// The widest of [`scan_closing_bound`] over every direction, at the EKF's
/// horizontal `speed`: the bound straight along the flight line.
pub fn scan_closing_bound_max(speed: f32) -> f32 {
    scan_closing_bound([speed, 0.0], [1.0, 0.0])
}

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
    /// Which terms changed the velocity, as [`limit`] bits: `HARD_STOP`,
    /// `OBSTACLE` (with `OBSTACLE_CLOSING` when the scans' own closing speed
    /// set that cap), `SEEN_SPACE`, `CLEARANCE` and `PUSH`. 0 when nothing
    /// did.
    pub by: u16,
}

/// The keep-out radius the swept check holds around an obstacle point,
/// measured as the radial caps measure it: the margin plus the creep slack
/// ([`CREEP_SLACK_M`]), from the near face of the point's cell.
pub fn keep_out_m(margin: f32) -> f32 {
    margin + CREEP_SLACK_M + crate::obstacles::CELL_M / 2.0
}

/// The 5° bearing sector of a unit direction.
fn sector_of(dir: V2) -> usize {
    (geo::bearing_of(dir).rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU
        * BRAKE_SECTORS as f32) as usize
        % BRAKE_SECTORS
}

/// Hold the velocity so the vehicle stops outside the margin of every
/// obstacle, judged along the motion.
///
/// The brake draws stop paths ([`Sweep`]): straight from the vehicle along
/// the asked velocity and along the flown one, each as long as the stop from
/// its speed (the flown one from the faster of the estimate and the speed
/// the scans see the vehicle close along it). Every obstacle point near the
/// vehicle (occupied grid cell or scan hit) whose keep-out disk
/// ([`keep_out_m`]: the margin, measured as the radial caps measure it) a
/// path enters caps the speed along that path so it still stops short of
/// the disk, allowing for [`BRAKE_LAG_S`], the FC's response, and for the
/// speed it already closes along the path ([`stop_cap`]). Trunks beside a
/// clear corridor cap nothing; a trunk the path meets slows it, to a stop
/// outside the margin when the path ends on it. Every point of the scan's
/// horizon (where each beam stops seeing: the floor a pitched beam meets, or
/// the lidar's range) caps the velocity component toward it ([`stop_cap`]),
/// per 5° bearing sector the most restrictive, what lies beyond it being
/// unknown in any direction: the vehicle never goes faster than it can stop
/// within the space it has seen free. Inside the margin of anything, every
/// point near the vehicle caps that way. The result is the velocity nearest
/// to `v` that meets every cap ([`nearest_within`]), so a cap adds no
/// sideways motion of its own (a path cap keeps the direction; a radial cap
/// slides along the obstacle). The answer's own stop path is drawn and
/// solved again while it breaks its cap; after [`SWEEP_ROUNDS`] every point
/// near the vehicle caps the velocity toward it too. Inside the hard-stop
/// distance of an obstacle nothing but motion away from it is kept.
///
/// Two limits keep the vehicle off walls in confined spaces, whoever flies
/// (sticks, goto, skill goal, swarm): beside a wall the speed is held to the
/// clearance speed ([`CONFINED_SPEED_PER_M`]), so position error and
/// overshoot, which grow with speed, stay small next to walls; and an
/// obstacle inside the margin adds a push away from it ([`PUSH_GAIN`]),
/// weighted per sector by how deep each nearest point is inside, so drift
/// that carried the vehicle inside is undone and opposite walls of a passage
/// narrower than twice the margin centre it. The push uses the obstacles'
/// positions relative to the vehicle, so it holds even when the vehicle's
/// own position estimate drifts.
pub fn brake_limit(
    obs: &crate::obstacles::Obstacles,
    pos: V2,
    vel: V2,
    v: V2,
    a_h: f32,
    geom: BrakeGeom,
    now_ms: u64,
) -> Braked {
    let margin = geom.margin;
    // A standing obstacle closes no faster than the vehicle flies toward it:
    // the scans' range rate is trusted per direction up to
    // [`scan_closing_bound`], so a nearer surface stepping into a sector (a
    // post in front of a wall, a trunk occluding another, a flickering edge)
    // is not read as motion. The reach uses the widest of those bounds.
    let reach_bound = scan_closing_bound_max(geo::norm(vel));
    let speed = geo::norm(v)
        .max(geo::norm(vel))
        .max(obs.max_scan_closing(now_ms).min(reach_bound));
    let reach = (margin + speed * speed / (2.0 * a_h) + speed * BRAKE_LAG_S + 1.0)
        .clamp(margin, BRAKE_RANGE_M);
    let stop_at = HARD_STOP_FRAC * margin;
    let keep_out = keep_out_m(margin);
    // Every point within reach: (relative position, distance, unit direction
    // toward it, distance to its near face, obstacle or horizon).
    let points = || {
        let obstacles = obs.points_near(pos, reach, now_ms).map(|p| (p, true));
        let horizon = obs.horizon_near(pos, reach, now_ms).map(|p| (p, false));
        obstacles.chain(horizon).filter_map(move |(p, obstacle)| {
            let rel = geo::sub(p, pos);
            let r = geo::norm(rel);
            if r < 1e-3 {
                return None;
            }
            let dir = geo::scale(rel, 1.0 / r);
            // Distance to the cell's near face (conservative for a scan point).
            let d = (r - crate::obstacles::CELL_M / 2.0).max(0.0);
            Some((rel, r, dir, d, obstacle))
        })
    };
    // Closing speed along a direction: the EKF's, or the scans' own range
    // rate when that is faster (an estimate lagging the real motion brakes
    // too late). Returns (closing, whether the scans set it).
    let closing_along = |dir: V2| {
        let scan = obs
            .scan_closing(dir, now_ms)
            .min(scan_closing_bound(vel, dir));
        let ekf = geo::dot(vel, dir);
        (ekf.max(scan), scan > ekf.max(0.0) + 1e-3)
    };
    let tag = |obstacle: bool, by_scan: bool| match obstacle {
        true if by_scan => limit::OBSTACLE | limit::OBSTACLE_CLOSING,
        true => limit::OBSTACLE,
        false => limit::SEEN_SPACE,
    };
    // Per sector: (unit direction, distance) of the nearest obstacle point.
    let mut near: [Option<(V2, f32)>; BRAKE_SECTORS] = [None; BRAKE_SECTORS];
    let mut away = [0.0, 0.0];
    let mut close = false;
    let mut r_min = f32::INFINITY;
    for (_, r, dir, d, obstacle) in points() {
        if !obstacle {
            continue;
        }
        r_min = r_min.min(r);
        if d < stop_at {
            close = true;
            away = geo::sub(away, dir);
        }
        let sector = sector_of(dir);
        if near[sector].is_none_or(|(_, n)| d < n) {
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
        out
    };
    let mut by = if close {
        limit::HARD_STOP
    } else {
        // The caps `v` broke that the answer sits on.
        caps.iter()
            .zip(tags)
            .filter_map(|(c, tag)| c.map(|(dir, cap)| (dir, cap, tag)))
            .filter(|&(dir, cap, _)| {
                geo::dot(dir, v) > cap + 1e-4 && geo::dot(dir, out) >= cap - 1e-3
            })
            .fold(0, |by, (_, _, tag)| by | tag)
    };
    let (v_conf, push) = confined(&near, geom);
    if geo::norm(out) > v_conf + 1e-3 {
        by |= limit::CLEARANCE;
    }
    if geo::norm(push) >= PUSH_REPORT_MPS {
        by |= limit::PUSH;
    }
    let out = with_push(out, v_conf, push);
    let cut = geo::norm(out) < geo::norm(v) - 1e-3 || geo::norm(push) >= PUSH_REPORT_MPS;
    Braked {
        v: out,
        cut,
        close,
        by: if cut || close { by } else { 0 },
    }
}


/// Room the brake keeps beyond the margin for a creep it cannot see, metres:
/// a vehicle drifting at under the stopped speed reads as still to the
/// estimate, and its range rate lies inside the lidar's noise, so the scans
/// measure no closing either; over [`BRAKE_LAG_S`] it covers up to this.
pub const CREEP_SLACK_M: f32 = super::STOPPED_MPS * BRAKE_LAG_S;

/// The speed toward a point `d` away (beyond `margin`) that still stops in
/// time when the FC answers a cut after [`BRAKE_LAG_S`]: the vehicle covers
/// `v·T` before braking at `a`, so `v·T + v²/2a ≤ d − margin` less
/// [`CREEP_SLACK_M`]; and while it still closes at `closing` (measured) it
/// covers `closing·T` first. A slow creep therefore ends that much outside
/// the margin, not on it.
pub fn stop_cap(d: f32, closing: f32, a: f32, margin: f32) -> f32 {
    room_cap(d - margin - CREEP_SLACK_M, closing, a)
}

/// [`stop_cap`] with `room` metres left before the vehicle must have
/// stopped.
fn room_cap(room: f32, closing: f32, a: f32) -> f32 {
    let t = BRAKE_LAG_S;
    let room = room.max(0.0);
    let reacting = -a * t + ((a * t).powi(2) + 2.0 * a * room).sqrt();
    let coasting = (2.0 * a * (room - closing * t).max(0.0)).sqrt();
    reacting.min(coasting)
}

/// The point nearest to `v` inside every half-plane `dir·x ≤ cap` (the
/// origin is always inside). In 2-D the answer is `v` itself, its projection
/// onto one constraint line, or a vertex of two; at the answer some
/// constraint `v` violates is active (KKT), so only lines `v` crosses are
/// projected onto and paired with the rest.
fn nearest_within(v: V2, caps: &[Option<(V2, f32)>]) -> V2 {
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

