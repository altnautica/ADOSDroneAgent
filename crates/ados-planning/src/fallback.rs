//! What the back end flies when the optimised trajectory fails its checks:
//! the same trajectory slowed down uniformly, or a stop-and-go motion along
//! the front-end polyline; and when nothing can be flown, a hard brake.

use crate::corridor::Corridor;
use crate::traj::{Segment, Trajectory};
use crate::world::{dist, norm};
use crate::{Plan, State};

/// A time stretch may slow the start state by at most this much, m/s …
pub(crate) const RESCALE_V_TOL: f64 = 0.05;
/// … and m/s².
pub(crate) const RESCALE_A_TOL: f64 = 0.1;
/// Stop-and-go ranks after every optimised candidate.
pub(crate) const STOP_AND_GO_COST: f64 = 1.0e9;

/// How the executed trajectory was shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shape {
    /// The optimised trajectory as it came out.
    #[default]
    Optimised,
    /// The optimised trajectory stretched in time to keep the limits.
    Rescaled,
    /// Rest-to-rest pieces along the front-end path (after a stop when the
    /// vehicle was moving).
    StopAndGo,
    /// No safe way on from the current state: a jerk-limited brake to rest
    /// straight ahead, with no goal reached.
    EmergencyStop,
}

/// The factor `k ≥ 1` by which stretching `tr` in time brings sampled
/// speed, acceleration and jerk within `caps` (they scale as 1/k, 1/k², 1/k³).
pub(crate) fn stretch_needed(tr: &Trajectory, caps: [f32; 3]) -> f64 {
    let dur = tr.duration();
    let n = ((dur / 0.01).ceil() as usize).max(1);
    let mut k = 1.0f64;
    for i in 0..=n {
        let t = dur * i as f32 / n as f32;
        let (_, v, a) = tr.sample(t);
        let j = tr.jerk(t);
        k = k
            .max(norm(v) as f64 / caps[0] as f64)
            .max((norm(a) as f64 / caps[1] as f64).sqrt())
            .max((norm(j) as f64 / caps[2] as f64).cbrt());
    }
    k
}

/// `tr` flown `k` times slower: `p'(t) = p(t / k)`.
pub(crate) fn stretch(tr: &Trajectory, k: f64) -> Trajectory {
    let segs = tr
        .segments()
        .iter()
        .map(|s| {
            let mut c = s.c;
            let mut f = 1.0;
            for row in c.iter_mut() {
                for v in row.iter_mut() {
                    *v *= f;
                }
                f /= k;
            }
            Segment { dur: s.dur * k, c }
        })
        .collect();
    Trajectory::new(segs)
}

/// The quintic from `(p0, v0, a0)` to rest at `p1` in time `t`.
fn quintic_to_rest(p0: [f32; 3], v0: [f32; 3], a0: [f32; 3], p1: [f32; 3], t: f64) -> Segment {
    let mut c = [[0.0f64; 3]; 6];
    for k in 0..3 {
        let (x0, v, a) = (p0[k] as f64, v0[k] as f64, a0[k] as f64);
        let h = p1[k] as f64 - x0;
        c[0][k] = x0;
        c[1][k] = v;
        c[2][k] = 0.5 * a;
        c[3][k] = (20.0 * h - 12.0 * v * t - 3.0 * a * t * t) / (2.0 * t.powi(3));
        c[4][k] = (-30.0 * h + 16.0 * v * t + 3.0 * a * t * t) / (2.0 * t.powi(4));
        c[5][k] = (12.0 * h - 6.0 * v * t - a * t * t) / (2.0 * t.powi(5));
    }
    Segment { dur: t, c }
}

fn keeps(seg: &Segment, caps: [f32; 3]) -> bool {
    let n = 40;
    (0..=n).all(|i| {
        let t = seg.dur * i as f64 / n as f64;
        let m = |o| {
            let v = seg.eval(t, o);
            (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
        };
        m(1) <= caps[0] as f64 && m(2) <= caps[1] as f64 && m(3) <= caps[2] as f64
    })
}

/// Rest to rest along a straight leg: the minimum-jerk profile, as short as
/// the limits allow (peak speed 1.875 L/T, acceleration 5.77 L/T², jerk
/// 60 L/T³).
fn leg(a: [f32; 3], b: [f32; 3], caps: [f32; 3]) -> Segment {
    let l = dist(a, b) as f64;
    let t = (1.875 * l / caps[0] as f64)
        .max((5.773_5 * l / caps[1] as f64).sqrt())
        .max((60.0 * l / caps[2] as f64).cbrt())
        .max(1e-3)
        * 1.01;
    quintic_to_rest(a, [0.0; 3], [0.0; 3], b, t)
}

/// Stop (if moving), then stop-and-go along `path` to `end`, every piece
/// within `caps`. `None` when no stop within the limits is found.
pub(crate) fn stop_and_go(
    start: &State,
    path: &[[f32; 3]],
    end: [f32; 3],
    caps: [f32; 3],
) -> Option<Trajectory> {
    let mut segs = Vec::with_capacity(path.len() + 2);
    let mut at = start.p;
    if norm(start.v) > 1e-3 || norm(start.a) > 1e-3 {
        let s = brake_to_rest(start, caps)?;
        at = s.eval(s.dur, 0).map(|v| v as f32);
        segs.push(s);
    }
    let mut last_dir: Option<[f32; 3]> = None;
    let mut corners: Vec<[f32; 3]> = Vec::with_capacity(path.len() + 1);
    for &p in path.iter().chain(std::iter::once(&end)) {
        let from = corners.last().copied().unwrap_or(at);
        let d = dist(from, p);
        if d < 1e-3 {
            continue;
        }
        let dir = [
            (p[0] - from[0]) / d,
            (p[1] - from[1]) / d,
            (p[2] - from[2]) / d,
        ];
        // Merge straight runs: replace the last corner when the direction
        // is unchanged.
        if let (Some(ld), Some(_)) = (last_dir, corners.last()) {
            if ld[0] * dir[0] + ld[1] * dir[1] + ld[2] * dir[2] > 0.9999 {
                corners.pop();
            }
        }
        let from = corners.last().copied().unwrap_or(at);
        let d = dist(from, p).max(1e-6);
        last_dir = Some([
            (p[0] - from[0]) / d,
            (p[1] - from[1]) / d,
            (p[2] - from[2]) / d,
        ]);
        corners.push(p);
    }
    let mut from = at;
    for p in corners {
        segs.push(leg(from, p, caps));
        from = p;
    }
    if segs.is_empty() {
        return Some(Trajectory::stationary(start.p));
    }
    Some(Trajectory::new(segs))
}

/// The shortest straight-ahead stop from `start` found within `caps`: a
/// quintic to rest at the point its own speed and acceleration carry it to.
/// `None` when no stop within the limits is found.
pub(crate) fn brake_to_rest(start: &State, caps: [f32; 3]) -> Option<Segment> {
    let speed = norm(start.v) as f64;
    let mut t = (speed / caps[1] as f64).max(0.1);
    for _ in 0..40 {
        let mut p1 = start.p;
        for k in 0..3 {
            p1[k] += (start.v[k] as f64 * t * 0.5 + start.a[k] as f64 * t * t / 12.0) as f32;
        }
        let s = quintic_to_rest(start.p, start.v, start.a, p1, t);
        if keeps(&s, caps) {
            return Some(s);
        }
        t *= 1.15;
    }
    None
}

/// The plan when no candidate can be flown from `start`: brake to rest
/// straight ahead within `caps`, then hold. Reaches no goal. `None` at rest
/// (nothing to stop) or when no stop within the limits is found.
pub(crate) fn emergency_stop(start: &State, caps: [f32; 3], now_ms: u64) -> Option<Plan> {
    if norm(start.v) <= 0.05 && norm(start.a) <= 0.1 {
        return None;
    }
    let exec = Trajectory::new(vec![brake_to_rest(start, caps)?]);
    let end = exec.end();
    Some(Plan {
        backup: Trajectory::stationary(end),
        t_commit: exec.duration(),
        exec,
        made_ms: now_ms,
        end,
        reaches_goal: false,
        path: vec![start.p, end],
        corridor: Corridor::default(),
        candidates: 0,
        shape: Shape::EmergencyStop,
    })
}
