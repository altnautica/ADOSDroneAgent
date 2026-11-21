//! Backup trajectories and the flyability check every trajectory passes.
//!
//! The executed trajectory may run into unknown space. Its backup branches
//! off at the commit time `t_c`, before that happens, and stops at rest
//! inside observed free space along the same way: the latest `t_c` whose
//! remaining observed-free length still covers a comfortable stop is tried
//! first, then earlier ones.

use crate::corridor;
use crate::lattice::{Space, FREE};
use crate::minco::Problem;
use crate::planner::{pva, Planner};
use crate::traj::Trajectory;
use crate::world::{dist, norm, OccupancyQuery};
use crate::{Limits, State};

/// Sample spacing along trajectories, seconds.
const DT: f32 = 0.02;
/// Spacing of the commit-time candidates along `exec`, seconds.
const COMMIT_DT: f32 = 0.05;
/// Fraction of the acceleration limit a backup's stop is sized for.
const STOP_DECEL: f32 = 0.6;
/// Extra length beyond the stopping distance, metres.
const STOP_SLACK_M: f32 = 0.3;
/// Commit times tried per plan.
const TRIES: usize = 5;

/// True when every sample of `tr` keeps the clearance the plan needs
/// (observed free space only with `known_only`) and the speed,
/// acceleration and jerk `caps`.
pub(crate) fn flyable<W: OccupancyQuery + ?Sized>(
    w: &W,
    sp: &Space,
    tr: &Trajectory,
    caps: [f32; 3],
    known_only: bool,
) -> bool {
    let dur = tr.duration();
    let n = ((dur / DT).ceil() as usize).max(1);
    (0..=n).all(|i| {
        let t = dur * i as f32 / n as f32;
        let (p, v, a) = tr.sample(t);
        norm(v) <= caps[0] + 1e-3
            && norm(a) <= caps[1] + 1e-3
            && norm(tr.jerk(t)) <= caps[2] + 1e-3
            && sp.point_ok(w, p, known_only)
    })
}

impl Planner {
    /// A backup for `exec` and its commit time.
    pub(crate) fn backup<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        exec: &Trajectory,
        start: &State,
        limits: &Limits,
    ) -> Option<(Trajectory, f32)> {
        let dur = exec.duration();
        let n = ((dur / COMMIT_DT).ceil() as usize).max(1);
        let step = dur / n as f32;
        self.samples.clear();
        self.arclen.clear();
        let mut first_out = None;
        for i in 0..=n {
            let p = exec.sample(step * i as f32).0;
            let s = match self.samples.last() {
                Some(q) => self.arclen[i - 1] + dist(*q, p),
                None => 0.0,
            };
            self.samples.push(p);
            self.arclen.push(s);
            if first_out.is_none() && !self.space.point_ok(w, p, true) {
                first_out = Some(i);
            }
        }
        let Some(out) = first_out else {
            return Some((Trajectory::stationary(exec.end()), dur));
        };
        if out == 0 {
            return None;
        }
        let last = out - 1;
        let caps = Self::caps(start, limits);
        let a_stop = (STOP_DECEL * limits.a_max).max(0.1);
        let mut ic = last;
        let mut tries = 0;
        while tries < TRIES {
            let (p, v, a) = exec.sample(step * ic as f32);
            let speed = norm(v);
            let room = self.arclen[last] - self.arclen[ic];
            if room < speed * speed / (2.0 * a_stop) + STOP_SLACK_M && speed > 0.05 {
                if ic == 0 {
                    return None;
                }
                ic -= 1;
                continue;
            }
            tries += 1;
            let t_c = step * ic as f32;
            if speed < 0.05 && norm(a) < 0.1 {
                return Some((Trajectory::stationary(p), t_c));
            }
            if let Some(tr) = self.stop_along(w, ic, last, State { p, v, a }, caps) {
                return Some((tr, t_c));
            }
            let back = ((0.25 / step).ceil() as usize).max(1);
            if ic < back {
                return None;
            }
            ic -= back;
        }
        None
    }

    /// A trajectory from `s` (at sample `from`) that follows the samples up
    /// to `to` through observed free voxels and stops at the last of them.
    fn stop_along<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        from: usize,
        to: usize,
        s: State,
        caps: [f32; 3],
    ) -> Option<Trajectory> {
        let lat = self.space.lat;
        self.cells.clear();
        let mut end = self.samples[from];
        for i in from..to {
            let before = self.cells.len();
            lat.trace(self.samples[i], self.samples[i + 1], &mut self.cells);
            let bad = self.cells[before..]
                .iter()
                .position(|c| self.space.base(w, *c) != FREE);
            if let Some(k) = bad {
                self.cells.truncate(before + k);
                if let Some(c) = self.cells.last() {
                    end = lat.centre(*c);
                }
                break;
            }
            end = self.samples[i + 1];
        }
        if self.cells.is_empty() || lat.cell_of(s.p) != self.cells.first().copied() {
            return None;
        }
        let max = self.box_cells();
        if !corridor::build(
            w,
            &mut self.space,
            &self.cells,
            true,
            max,
            &mut self.corridor,
        ) {
            return None;
        }
        let aim = self.cfg.limit_aim as f64;
        let e = [end[0] as f64, end[1] as f64, end[2] as f64];
        let prob = Problem {
            start: pva(&s),
            end: [e, [0.0; 3], [0.0; 3]],
            boxes: &self.corridor.boxes,
            seeds: &self.corridor.seeds,
            lim: caps.map(|c| c as f64 * aim),
            samples: self.cfg.samples,
        };
        let prm = self.lbfgs_params();
        let (tr, _) = self.minco.optimize(&prob, &self.cfg.weights, prm)?;
        flyable(w, &self.space, &tr, caps, true).then_some(tr)
    }
}
