//! The trajectory the pilot is committed to. Only a plan with a backup is
//! ever committed (the planner returns none without one): `exec` is flown
//! from when the plan was made, and if no newer plan has replaced it by its
//! commit time, the backup from there, which stops at rest in seen-free
//! space. A replan that fails therefore never leaves the vehicle without a
//! way to stop: it keeps flying the committed plan into its backup.

use ados_planning::{Plan, Shape};

use crate::geo::V2;

/// Where the committed trajectory puts the reference at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrajRef {
    /// Position, velocity and acceleration now.
    pub p: V2,
    pub v: V2,
    pub a: V2,
    /// The point the trajectory reaches [`CARROT_S`] on: what the goal aims
    /// at on the published command, and the heading's aim.
    pub carrot: V2,
    /// Past the commit time on the backup.
    pub on_backup: bool,
}

/// The carrot's time ahead on the trajectory, s.
pub const CARROT_S: f32 = 1.0;

fn ne(p: [f32; 3]) -> V2 {
    [p[0], p[1]]
}

/// The committed plan and how replans since have gone.
#[derive(Debug, Default)]
pub struct Committed {
    plan: Option<Plan>,
    /// Replans in a row that failed since the plan was committed.
    pub fails: u32,
}

impl Committed {
    /// Commit `plan`; it must come from the planner (it has a backup).
    pub fn commit(&mut self, plan: Plan) {
        debug_assert!(plan.backup.duration() >= 0.0 && plan.t_commit >= 0.0);
        self.plan = Some(plan);
        self.fails = 0;
    }

    /// A replan failed: the committed plan flies on toward its backup.
    pub fn failed(&mut self) {
        self.fails = self.fails.saturating_add(1);
    }

    pub fn clear(&mut self) {
        self.plan = None;
        self.fails = 0;
    }

    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    /// Seconds since the plan was made at `now_ms`.
    fn t(plan: &Plan, now_ms: u64) -> f32 {
        now_ms.saturating_sub(plan.made_ms) as f32 / 1000.0
    }

    /// The reference at `now_ms`.
    pub fn sample(&self, now_ms: u64) -> Option<TrajRef> {
        let plan = self.plan.as_ref()?;
        let t = Self::t(plan, now_ms);
        let (p, v, a) = plan.sample(t, true);
        let (c, _, _) = plan.sample(t + CARROT_S, true);
        Some(TrajRef {
            p: ne(p),
            v: ne(v),
            a: ne(a),
            carrot: ne(c),
            on_backup: t > plan.t_commit,
        })
    }

    /// The committed plan's remaining path from `now_ms`: positions evenly
    /// spaced in time to where it comes to rest (exec, or the backup when it
    /// is past the commit time), at most `n`.
    pub fn path(&self, now_ms: u64, n: usize) -> Vec<V2> {
        let Some(plan) = self.plan.as_ref() else {
            return Vec::new();
        };
        let t = Self::t(plan, now_ms);
        let tail = if t > plan.t_commit {
            plan.backup.tail(t - plan.t_commit)
        } else {
            plan.exec.tail(t)
        };
        tail.positions(n).into_iter().map(ne).collect()
    }

    /// Where the committed plan comes to rest, and whether that is its goal
    /// (not a backup's stop or an emergency stop).
    pub fn rest(&self, now_ms: u64) -> Option<(V2, bool)> {
        let plan = self.plan.as_ref()?;
        let on_backup = Self::t(plan, now_ms) > plan.t_commit;
        let stop = on_backup || plan.shape == Shape::EmergencyStop;
        Some(if stop {
            (ne(plan.backup.end()), false)
        } else {
            (ne(plan.end), plan.reaches_goal)
        })
    }
}

#[cfg(test)]
mod tests {
    use ados_planning::{Config, DenseGrid, Limits, Planner, State};

    use super::*;

    /// A plan into unknown space: free for 8 m, unknown beyond.
    fn plan_into_unknown() -> Plan {
        let mut g = DenseGrid::new(
            [-2.0, -5.0, -0.125],
            0.25,
            [120, 40, 1],
            ados_planning::Cell::Unknown,
        );
        for i in 0..40 {
            for j in 0..40 {
                g.set([i, j, 0], ados_planning::Cell::Free);
            }
        }
        g.update_esdf();
        let mut p = Planner::new(Config {
            radius_m: 0.25,
            margin_m: 0.25,
            ..Config::default()
        });
        let lim = Limits {
            v_max: 2.0,
            a_max: 1.0,
            j_max: 2.0,
        };
        let start = State {
            p: [0.0, 0.0, 0.0],
            v: [1.0, 0.0, 0.0],
            ..State::default()
        };
        p.plan(&g, start, [25.0, 0.0, 0.0], &lim, 1_000).unwrap()
    }

    #[test]
    fn a_failed_replan_flies_into_the_backup_and_stops_in_seen_space() {
        let plan = plan_into_unknown();
        let mut c = Committed::default();
        c.commit(plan.clone());
        c.failed();
        let before = c.sample(1_000).unwrap();
        assert!(!before.on_backup);
        let late = 1_000 + ((plan.t_commit + plan.backup.duration() + 1.0) * 1000.0) as u64;
        let end = c.sample(late).unwrap();
        assert!(end.on_backup);
        assert!(end.v[0].hypot(end.v[1]) < 1e-3, "{end:?}");
        assert!(end.p[0] < 8.0, "the backup stops in seen space: {end:?}");
        let (rest, goal) = c.rest(late).unwrap();
        assert!(!goal && (rest[0] - end.p[0]).abs() < 1e-3);
        let path = c.path(1_000, 16);
        assert_eq!(path.len(), 16);
    }
}
