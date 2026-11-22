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

use ados_planning::{Config, FixedHeight, Limits, PlanError, Planner, State};
use ados_protocol::pilot::AvoidState;

use super::backup::{Committed, TrajRef};
use super::keep::{keeps_out, FORWARD_VIEW_HALF_RAD};
use super::map2d::{seen_ranges, PlanMap, Seen};
use super::route_goal::Router;
use super::{decimate, Keepout, Plan};
use crate::geo::{self, V2};
use crate::obstacles::{Mover, Obstacles, CELL_M};
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
    /// The movers each replan keeps clear of (reused).
    movers: Vec<Mover>,
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

    /// How far the route held to `goal` still runs from `from`, m.
    pub fn route_remaining_m(&self, from: V2, goal: V2) -> Option<f32> {
        self.router.remaining_m(from, goal)
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
        self.movers.clear();
        self.movers.extend(obs.movers.guarded(g.now_ms).copied());
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
                movers: &self.movers,
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
            movers: &self.movers,
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
