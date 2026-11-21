//! The trajectory planner in the pilot ([`ados_planning`]): a smooth
//! trajectory from the reference the pilot flies to the goal, over the
//! obstacle grid with what the sensors see ([`super::map2d`]), at the goal's
//! height (the grid is the obstacle source's slab at flight height: the
//! planar scans, or the live voxel map's), replanned every
//! [`TRAJ_PERIOD_MS`]. Unknown space may be planned through at a cost; a
//! plan is committed only with a backup that stops in seen-free space, and
//! a failed replan flies the committed plan on into its backup
//! ([`super::backup`]). A goal beyond the grid is planned toward the
//! grid's edge on the straight way; when no way there is found, toward a
//! point along a coarse route ([`super::route_goal`]), and a goal is
//! blocked only when no route reaches it within
//! [`super::route_goal::ROUTE_REACH_M`].

use ados_planning::{Cell, Config, FixedHeight, Limits, PlanError, PlanarQuery, Planner, State};
use ados_protocol::pilot::AvoidState;

use super::backup::{Committed, TrajRef};
use super::map2d::{seen_ranges, PlanMap, Seen, OWN_M};
use super::route_goal::Router;
use super::{decimate, Keepout, Plan};
use crate::geo::{self, V2};
use crate::obstacles::{Obstacles, CELL_M};
use crate::world::VoxWorld;

/// The trajectory planner's replan period while it flies (10 Hz).
pub const TRAJ_PERIOD_MS: u64 = 100;
/// Clearance beyond the margin a plan is first made with, m: room for the
/// grid's quantisation and the reference's tracking error, so the flown
/// path keeps out of the margin.
pub const TRAJ_ROOM_M: f32 = 0.5;
/// The room tried next when nothing is reachable with [`TRAJ_ROOM_M`], m:
/// just past the obstacle brake's keep-out (margin, creep slack and half a
/// cell, 0.21 m beyond the margin), where the brake lets the vehicle fly.
/// Only when nothing is reachable with it either, or the ways it finds go a
/// long way round ([`LONG_WAY`]), is the bare margin planned (a passage the
/// brake holds to a creep).
pub const TRAJ_ROOM_MIN_M: f32 = 0.25;
/// A moving goal is planned to where it will be this long on, at most, s
/// (its stopping time at the plan's acceleration): the plan comes to rest
/// there, not behind the goal.
const LEAD_MAX_S: f32 = 3.0;
/// The window round the start and goal the search may use, m.
const PAD_M: f32 = 8.0;
/// A plan whose path strays this far from the straight way is a detour, m.
const DETOUR_M: f32 = 0.5;
/// A plan whose path is more than this many times the straight way is
/// compared with ones made with each tighter room in turn (down to the bare
/// margin), and the first that is this many times shorter and that the
/// pilot keeps to is flown instead: a trail between trunks rather than a
/// wide loop round them. Without the bare margin among them, a vehicle on a
/// narrow trail turned back round the stand on the first replan a wider
/// room found a way again, then back onto the trail once it found none.
const LONG_WAY: f32 = 1.25;
/// The rooms tried, widest first ([`TRAJ_ROOM_M`], [`TRAJ_ROOM_MIN_M`],
/// the bare margin).
const ROOMS: [f32; 3] = [TRAJ_ROOM_M, TRAJ_ROOM_MIN_M, 0.0];

fn path_len(p: &ados_planning::Plan) -> f32 {
    p.path
        .windows(2)
        .map(|w| ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2)).sqrt())
        .sum()
}

/// A plan's way is looked along this far from its start, m.
const LOOK_M: f32 = 2.0;

/// The bearing from `start` to the first turning point of `p`'s path at
/// least [`LOOK_M`] off (its end when none is).
fn look_along(p: &ados_planning::Plan, start: V2) -> f32 {
    let far = p
        .path
        .iter()
        .map(|q| [q[0], q[1]])
        .find(|q| geo::dist(*q, start) >= LOOK_M)
        .unwrap_or([p.end[0], p.end[1]]);
    geo::bearing_of(geo::sub(far, start))
}

/// The heading to look along after a plan the forward view alone turned
/// down, whose way bears `b`: the one `held` while `b` lies within the
/// view's half-angle of it (the vehicle keeps turning to one heading, never
/// hunting between near ones), else `b` (a plan turned down in another
/// direction is looked at, so the vehicle never holds a heading that no
/// longer sees the way).
fn next_look(held: Option<f32>, b: f32) -> f32 {
    match held {
        Some(h) if geo::wrap_pi(b - h).abs() <= FORWARD_VIEW_HALF_RAD => h,
        _ => b,
    }
}

/// The plan's start when the reference lies within a cell of the vehicle:
/// the space the vehicle itself takes up ([`PlanMap::own`]).
fn own_point(g: &TrajGoal) -> Option<V2> {
    (geo::dist(g.start[0], g.pos) <= CELL_M).then_some(g.start[0])
}

/// One replan's inputs, in the FC's local NED (horizontal).
#[derive(Debug, Clone, Copy)]
pub struct TrajGoal {
    /// The vehicle now (the seen ranges' origin).
    pub pos: V2,
    /// The state the plan starts from: the reference flown.
    pub start: [V2; 3],
    /// The goal's point and velocity.
    pub goal: V2,
    pub goal_vel: V2,
    pub margin: f32,
    /// The vehicle's heading when its sensors see forward only (the depth
    /// and camera tiers): the sure part of a plan keeps inside that view.
    pub heading: Option<f32>,
    /// The subject's disk, kept out of like an obstacle.
    pub subject: Option<Keepout>,
    pub limits: Limits,
    pub now_ms: u64,
}

/// What a replan did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A new plan with its backup is committed.
    Committed,
    /// No new plan: the committed one (if any) flies on into its backup.
    Kept(PlanError),
    /// No route reaches the goal within the reach: hold.
    Blocked,
}

/// The planner, its route to a far goal and the plan committed.
#[derive(Debug, Default)]
pub struct TrajPlanner {
    planner: Planner,
    router: Router,
    seen: Vec<f32>,
    pub committed: Committed,
    /// The last replan's outcome.
    pub last: Option<Outcome>,
    /// The goal the committed plan was made for (led by its velocity).
    aimed: Option<V2>,
    /// Where the last plan wanted to go when only the forward view turned
    /// it down: the heading to look along, so the next plan is seen.
    look: Option<f32>,
}

fn v3(v: V2) -> [f32; 3] {
    [v[0], v[1], 0.0]
}

fn config(margin: f32, room: f32) -> Config {
    Config {
        radius_m: margin,
        margin_m: room,
        pad_m: [PAD_M, 0.0],
        ..Config::default()
    }
}

impl TrajPlanner {
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            ..Default::default()
        }
    }

    /// Drop the committed plan and the route (no goal to plan).
    pub fn clear(&mut self) {
        self.committed.clear();
        self.router.clear();
        self.last = None;
        self.aimed = None;
        self.look = None;
    }

    /// The heading to look along while no plan is committed: set when the
    /// last plan found keeps out of everything but lies outside the
    /// forward view ([`TrajGoal::heading`]).
    pub fn look(&self) -> Option<f32> {
        self.look
    }

    /// Plan `g` over `obs`'s grid (rebuilt round the vehicle just before),
    /// with `vox` the live 3D voxel map and the flight height (local down)
    /// when a 3D tier's map is the obstacle source. A replan that finds no
    /// new plan keeps the committed one only while what it still flies for
    /// sure keeps out of what is seen now ([`keeps_out`]): a walker that
    /// stepped into it since drops it, and the vehicle holds.
    pub fn replan(
        &mut self,
        obs: &Obstacles,
        vox: Option<(&VoxWorld, f32)>,
        g: &TrajGoal,
    ) -> Outcome {
        let out = self.plan_once(obs, vox, g);
        if matches!(out, Outcome::Kept(_)) {
            self.committed.failed();
            let seen = match vox {
                Some((w, down)) => Seen::Voxels(w, down),
                None => Seen::Ranges(&self.seen),
            };
            let map = PlanMap {
                grid: &obs.grid,
                pos: g.pos,
                seen,
                own: own_point(g),
                subject: g.subject,
            };
            let clear = self.committed.plan().is_none_or(|p| {
                let t = g.now_ms.saturating_sub(p.made_ms) as f32 / 1000.0;
                keeps_out(p, &map, g.margin, None, &g.limits, t)
            });
            if !clear {
                self.committed.clear();
            }
        }
        if out == Outcome::Blocked {
            self.committed.clear();
        }
        self.last = Some(out);
        out
    }

    fn plan_once(
        &mut self,
        obs: &Obstacles,
        vox: Option<(&VoxWorld, f32)>,
        g: &TrajGoal,
    ) -> Outcome {
        let seen = match vox {
            Some((w, down)) => Seen::Voxels(w, down),
            None => {
                seen_ranges(obs, g.pos, g.now_ms, &mut self.seen);
                Seen::Ranges(&self.seen)
            }
        };
        let map = PlanMap {
            grid: &obs.grid,
            pos: g.pos,
            seen,
            own: own_point(g),
            subject: g.subject,
        };
        let world = FixedHeight::new(&map, 0.0);
        let speed = geo::norm(g.goal_vel);
        let lead = (speed / g.limits.a_max.max(0.1)).min(LEAD_MAX_S);
        let goal = geo::add(g.goal, geo::scale(g.goal_vel, lead));
        // The planner clips a goal beyond its window to the window's edge
        // on the straight way; only when no way to that is found is the
        // coarse route asked for a point along it.
        let need = g.margin;
        let start = State {
            p: v3(g.start[0]),
            v: v3(g.start[1]),
            a: v3(g.start[2]),
        };
        let mut aim = goal;
        let mut routed = false;
        loop {
            let mut last = PlanError::NoPath;
            for (i, room) in ROOMS.into_iter().enumerate() {
                self.planner.set_config(config(g.margin, room));
                match self
                    .planner
                    .plan(&world, start, v3(aim), &g.limits, g.now_ms)
                {
                    Ok(p) => {
                        let tighter = &ROOMS[i + 1..];
                        let p = shorter_tighter(
                            &mut self.planner,
                            &world,
                            &map,
                            (start, aim),
                            g,
                            p,
                            tighter,
                        );
                        if !keeps_out(&p, &map, g.margin, g.heading, &g.limits, 0.0) {
                            // Out of the forward view only: look that way.
                            if g.heading.is_some()
                                && keeps_out(&p, &map, g.margin, None, &g.limits, 0.0)
                            {
                                let b = look_along(&p, g.start[0]);
                                self.look = Some(next_look(self.look, b));
                            }
                            return Outcome::Kept(PlanError::NoBackup);
                        }
                        self.committed.commit(p);
                        self.aimed = Some(goal);
                        self.look = None;
                        return Outcome::Committed;
                    }
                    Err(e) => last = e,
                }
                if last != PlanError::NoPath {
                    return Outcome::Kept(last);
                }
            }
            // No way to it inside the window: a route round, outside it?
            if routed {
                return Outcome::Kept(last);
            }
            routed = true;
            match self
                .router
                .local_goal(&obs.grid, g.pos, goal, need, g.now_ms)
            {
                Ok(p) if geo::dist(p, aim) > 0.5 => aim = p,
                Ok(_) => return Outcome::Kept(last),
                Err(_) => return Outcome::Blocked,
            }
        }
    }

    /// The committed trajectory's reference at `now_ms`.
    pub fn sample(&self, now_ms: u64) -> Option<TrajRef> {
        self.committed.sample(now_ms)
    }

    /// The committed plan in the grid planner's terms, for the published
    /// state and the flight code: the carrot as the target, where it comes
    /// to rest as the goal, its remaining path, a detour when the path
    /// strays from the straight way to the goal it was made for; blocked
    /// (no target) when the last replan found no route or nothing is
    /// committed.
    pub fn view(&self, obs: &Obstacles, pos: V2, now_ms: u64) -> Plan {
        let (Some(r), Some((rest, _))) = (self.sample(now_ms), self.committed.rest(now_ms)) else {
            return Plan::hold(AvoidState::Blocked);
        };
        let mut path = vec![pos];
        path.extend(self.committed.path(now_ms, 32));
        let path = decimate(&path, 16);
        let aim = self.aimed.unwrap_or(rest);
        let straight = |p: V2| {
            let ab = geo::sub(aim, pos);
            let l2 = geo::dot(ab, ab);
            let t = if l2 > 1e-6 {
                (geo::dot(geo::sub(p, pos), ab) / l2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            geo::dist(p, geo::add(pos, geo::scale(ab, t)))
        };
        let detour = path.iter().any(|p| straight(*p) > DETOUR_M);
        let minc = path
            .iter()
            .filter_map(|p| obs.grid.clearance(*p))
            .fold(f32::INFINITY, f32::min);
        Plan {
            state: if detour {
                AvoidState::Detour
            } else {
                AvoidState::Clear
            },
            target: Some(r.carrot),
            goal: Some(rest),
            path,
            min_clearance: minc.is_finite().then_some(minc),
        }
    }
}

/// `wide`, or when it goes a long way round ([`LONG_WAY`]) the first plan
/// made with one of the `tighter` rooms in turn, from `start` toward `aim`,
/// that is that much shorter and that the pilot keeps to ([`keeps_out`]).
fn shorter_tighter(
    planner: &mut Planner,
    world: &FixedHeight<'_, PlanMap<'_>>,
    map: &PlanMap<'_>,
    (start, aim): (State, V2),
    g: &TrajGoal,
    wide: ados_planning::Plan,
    tighter: &[f32],
) -> ados_planning::Plan {
    let straight = geo::dist(g.start[0], [wide.end[0], wide.end[1]]);
    let long = path_len(&wide);
    if long <= LONG_WAY * straight {
        return wide;
    }
    for &room in tighter {
        planner.set_config(config(g.margin, room));
        if let Ok(tight) = planner.plan(world, start, v3(aim), &g.limits, g.now_ms) {
            if LONG_WAY * path_len(&tight) < long
                && keeps_out(&tight, map, g.margin, g.heading, &g.limits, 0.0)
            {
                return tight;
            }
        }
    }
    wide
}

/// What a plan flies for sure (`exec` to its commit time, then the
/// backup) is sampled this often, s.
const KEEP_DT_S: f32 = 0.05;
/// One step of the grid's clearance field (a chamfer unit), m: a sample may
/// read this much under the start without being nearer.
const CLEAR_STEP_M: f32 = CELL_M / 3.0 + 1e-3;

/// How far off the heading the forward view lets a plan's sure part go,
/// rad: a forward depth camera's half view (40-45°) and the 45° a grid
/// path's first step may take; a step sideways of that, into space seen
/// from elsewhere, brushes what the camera never saw.
const FORWARD_VIEW_HALF_RAD: f32 = 50.0 * std::f32::consts::PI / 180.0;
/// Within this of a plan's start a sample has not moved anywhere yet, m:
/// the rounding allowance the map itself gives the start's own point
/// ([`OWN_M`]). A plan from rest takes its first centimetres along its
/// smoothest start, which may point anywhere (seen 1-2 cm at 65-77° off
/// the way it then goes); judged by bearing, every plan from a hover was
/// turned down. The allowance stays a fraction of a voxel: a plan whose
/// sure part keeps inside it stops inside it, so plan after plan may step
/// the vehicle sideways by no more than that each.
const VIEW_SLACK_M: f32 = OWN_M;

/// How far from a plan's start its samples may lie in any direction before
/// the forward view judges them, m: the way the start's own motion carries
/// the vehicle braking as hard as the limits let it (any acceleration
/// undone at the jerk limit, the braking ramped in at it, then held at the
/// acceleration limit), plus [`VIEW_SLACK_M`]. That much is no plan's
/// choice: a hovering vehicle's drift of a few cm/s goes every way, and
/// judging its first centimetres by bearing turns every plan down. Any more
/// is: a plan that steers sideways leaves it, and one that brakes a
/// sideways drift at the limits stays inside it, so no plan adds sideways
/// speed for the next to be allowed more.
fn carried_m(v0: f32, a0: f32, lim: &Limits) -> f32 {
    let a = lim.a_max.max(0.1);
    let j = lim.j_max.max(0.1);
    let v = v0 + a0 * a0 / (2.0 * j);
    let stop = if v <= a * a / (2.0 * j) {
        // Stopped while the braking still ramps in.
        2.0 / 3.0 * v * (2.0 * v / j).sqrt()
    } else {
        let t1 = a / j;
        let v1 = v - a * t1 / 2.0;
        v * t1 - j * t1 * t1 * t1 / 6.0 + v1 * v1 / (2.0 * a)
    };
    VIEW_SLACK_M + stop
}

/// The pilot commits only to a plan whose sure part (`exec` to the commit
/// time, and the backup it stops on) lies in seen-free space, keeps the
/// margin, or, from a start already inside the margin, comes no nearer
/// than the start is (an escape, never deeper in: the planner lets a start
/// inside the margin end a little nearer still, which replan after replan
/// walks the vehicle onto what it is inside the margin of), and, with a
/// forward view (`heading`), stays inside it: a camera never sees beside
/// the vehicle, and a sideways step into space it saw from elsewhere
/// brushes what it never saw. `from_s` into the plan is where the check
/// starts: 0 for a new plan, now for one kept on.
fn keeps_out(
    plan: &ados_planning::Plan,
    map: &PlanMap<'_>,
    margin: f32,
    heading: Option<f32>,
    lim: &Limits,
    from_s: f32,
) -> bool {
    let at = |p: [f32; 3]| [p[0], p[1]];
    let (p0, v0, a0) = plan.sample(from_s, true);
    let start = at(p0);
    let c0 = map.clearance(start);
    let need = if c0 >= margin {
        margin
    } else {
        c0 - CLEAR_STEP_M
    };
    let carried = carried_m(geo::norm(at(v0)), geo::norm(at(a0)), lim);
    let in_view = |p: V2| {
        let rel = geo::sub(p, start);
        heading.is_none_or(|h| {
            geo::norm(rel) <= carried
                || geo::wrap_pi(geo::bearing_of(rel) - h).abs() <= FORWARD_VIEW_HALF_RAD
        })
    };
    let ok = |p: [f32; 3]| {
        let p = at(p);
        map.state(p) == Cell::Free && map.clearance(p) >= need && in_view(p)
    };
    let sure = |tr: &ados_planning::Trajectory, from: f32, end: f32| {
        let n = (((end - from) / KEEP_DT_S).ceil() as usize).max(1);
        (0..=n).all(|i| ok(tr.sample(from + (end - from) * i as f32 / n as f32).0))
    };
    let t_c = plan.t_commit.min(plan.exec.duration());
    let dur = plan.backup.duration();
    if from_s <= t_c {
        sure(&plan.exec, from_s, t_c) && sure(&plan.backup, 0.0, dur)
    } else {
        sure(&plan.backup, (from_s - t_c).min(dur), dur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_look_moves_to_a_plan_turned_down_in_another_direction() {
        let deg = |d: f32| d.to_radians();
        // None held: the plan's bearing.
        assert_eq!(next_look(None, deg(30.0)), deg(30.0));
        // A bearing inside the view of the one held keeps it.
        assert_eq!(next_look(Some(deg(30.0)), deg(60.0)), deg(30.0));
        assert_eq!(next_look(Some(deg(170.0)), deg(-170.0)), deg(170.0));
        // One beyond the view's half-angle replaces it: the vehicle turned
        // to the first and a new obstacle sent the next plan elsewhere.
        assert_eq!(next_look(Some(deg(30.0)), deg(-60.0)), deg(-60.0));
        assert_eq!(next_look(Some(deg(0.0)), deg(180.0)), deg(180.0));
    }
}
