//! The skill and goto planner (C2): keep the goal when it is clear, otherwise
//! search detour candidates on the skill's ring and route to the best
//! reachable one with A* over the clearance grid, preferring wide gaps; a
//! chosen detour is held from one replan to the next ([`commit`]).

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use ados_protocol::pilot::{AvoidState, Goal, GoalKind};

use crate::geo::{self, V2};
use crate::obstacles::{Grid, Obstacles, CELL_M, GRID_N};

mod commit;
pub use commit::{plan_committed, Commit, RECHOOSE_GOAL_M, RELEASE_PLANS, RELEASE_ROOM_M};

/// Expansion budget per search.
pub const MAX_EXPANSIONS: usize = 4000;
/// The waypoint sits this far along the found path.
pub const LOOKAHEAD_M: f32 = 2.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub state: AvoidState,
    /// Where to fly now (goal or detour waypoint); `None` = hold.
    pub target: Option<V2>,
    /// The goal finally aimed at (candidate when detouring).
    pub goal: Option<V2>,
    /// Path polyline, at most 16 points.
    pub path: Vec<V2>,
    pub min_clearance: Option<f32>,
}

impl Plan {
    pub fn hold(state: AvoidState) -> Self {
        Self {
            state,
            target: None,
            goal: None,
            path: Vec::new(),
            min_clearance: None,
        }
    }
}

fn clearance(grid: &Grid, p: V2) -> f32 {
    grid.clearance(p).unwrap_or(f32::INFINITY)
}

/// True when every point on `a → b` keeps `margin` clearance (the first
/// `margin` metres from `a` are exempt, so a vehicle already inside the margin
/// can still back out).
pub fn segment_clear(grid: &Grid, a: V2, b: V2, margin: f32) -> bool {
    let d = geo::dist(a, b);
    let n = (d / (CELL_M / 2.0)).ceil().max(1.0) as usize;
    (0..=n).all(|k| {
        let p = geo::add(a, geo::scale(geo::sub(b, a), k as f32 / n as f32));
        geo::dist(p, a) < margin || clearance(grid, p) >= margin
    })
}

/// Where a detour candidate sits on the subject's ring, relative to the
/// goal's own point: (bearing offset, degrees; radius offset, metres). A
/// committed detour keeps its spot while the ring moves with the subject.
pub type Spot = (f32, f32);

/// Detour candidates around the subject, cheapest first: `(point, cost)`.
pub fn candidates(goal: &Goal, from: V2) -> Vec<(V2, f32)> {
    ring(goal, from, 0.0)
        .into_iter()
        .map(|(p, cost, _)| (p, cost))
        .collect()
}

/// What choosing a detour on the other side of the ring than the one held
/// costs, in the ring's cost units (one is 90° off the goal's bearing): a
/// detour chosen again keeps round the side it went, unless the other side
/// is clearly better or the only way.
pub const SIDE_SWITCH_COST: f32 = 0.5;

/// [`candidates`] with each one's [`Spot`]; those on the other side of the
/// ring than `side` (the sign of a held spot's bearing offset; 0 for none)
/// cost [`SIDE_SWITCH_COST`] more.
fn ring(goal: &Goal, from: V2, side: f32) -> Vec<(V2, f32, Spot)> {
    let Some(c) = &goal.candidates else {
        return Vec::new();
    };
    let center = c.ring_center_ne;
    let mut out = Vec::new();
    match goal.kind {
        GoalKind::Orbit => {
            for dphi in [
                10.0f32, -10.0, 20.0, -20.0, 30.0, -30.0, 40.0, -40.0, 50.0, -50.0, 60.0, -60.0,
            ] {
                for dr in [0.0f32, 2.0, -2.0] {
                    let r = c.radius_m + dr;
                    if r < 1.0 {
                        continue;
                    }
                    let b = (c.angle_deg + dphi).to_radians();
                    let p = geo::add(center, geo::scale(geo::bearing_unit(b), r));
                    out.push((p, dphi.abs() / 60.0 + dr.abs() / 2.0, (dphi, dr)));
                }
            }
        }
        _ => {
            for delta in [0.0f32, 20.0, -20.0, 40.0, -40.0, 60.0, -60.0, 90.0, -90.0] {
                for dd in [0.0f32, 2.0, 4.0] {
                    if delta == 0.0 && dd == 0.0 {
                        continue;
                    }
                    let b = (c.angle_deg + delta).to_radians();
                    let p = geo::add(center, geo::scale(geo::bearing_unit(b), c.radius_m + dd));
                    out.push((p, delta.abs() / 90.0 + dd / 4.0, (delta, dd)));
                }
            }
        }
    }
    for (_, cost, (delta, _)) in &mut out {
        if *delta * side < 0.0 {
            *cost += SIDE_SWITCH_COST;
        }
    }
    // Ties broken by distance from the vehicle.
    out.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                geo::dist(a.0, from)
                    .partial_cmp(&geo::dist(b.0, from))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    out
}

/// Plan one goal from `from`, with no route held from before
/// ([`plan_committed`] with a fresh [`Commit`]).
pub fn plan(obs: &Obstacles, goal: &Goal, from: V2, margin: f32) -> Plan {
    let motion = Motion {
        speed_mps: f32::INFINITY,
        now_ms: 0,
    };
    plan_committed(obs, goal, from, margin, motion, &mut Commit::default())
}

/// The straight plan to the goal's own point `target` (clearance `gc`).
fn direct(grid: &Grid, from: V2, target: V2, gc: f32) -> Plan {
    Plan {
        state: AvoidState::Clear,
        target: Some(target),
        goal: Some(target),
        path: vec![from, target],
        min_clearance: Some(gc.min(clearance(grid, from))).filter(|c| c.is_finite()),
    }
}

/// Clearance beyond the margin a detour is first routed with, metres: the
/// obstacle brake keeps its creep slack and a cell's quantisation beyond
/// the margin (0.205 m) clear of every stop path, so a route through a gap
/// it fits only at the bare margin is flown at a crawl. Only when no
/// candidate is reachable with it is the bare margin routed.
pub const ROUTE_ROOM_M: f32 = 0.25;

/// Which detour spots a route may go to.
#[derive(Debug, Clone, Copy)]
enum Choose {
    /// Only this one (a held detour, re-routed).
    Only(Spot),
    /// Any, keeping to `side` of the ring ([`ring`]).
    Any { side: f32 },
}

/// The cheapest reachable detour candidate of those `choose` allows, routed
/// from `from` with the search staying near `prefer` (the route flown
/// before; empty for none) and out of `narrows` (a stall's, for none),
/// with [`ROUTE_ROOM_M`] beyond the margin when any candidate is reachable
/// with it. A candidate off the grid (a subject more than half the grid
/// away) is routed toward through the grid's edge ([`Flood::exit_toward`]):
/// the grid knows nothing beyond it, so the way out that leaves the least
/// straight distance still to go is flown. When `narrows` leaves no
/// candidate reachable, the route ignores them: they never shut the only
/// way out.
fn detour(
    obs: &Obstacles,
    goal: &Goal,
    from: V2,
    margin: f32,
    choose: Choose,
    prefer: &[V2],
    narrows: Option<&Narrows>,
) -> Option<(Plan, Spot)> {
    route(obs, goal, from, margin + ROUTE_ROOM_M, choose, prefer)
        .or_else(|| route(obs, goal, from, margin, choose, prefer))
}

/// [`detour`] at one routing margin.
fn route(
    obs: &Obstacles,
    goal: &Goal,
    from: V2,
    margin: f32,
    choose: Choose,
    prefer: &[V2],
    narrows: Option<&Narrows>,
) -> Option<(Plan, Spot)> {
    let grid = &obs.grid;
    let subject = goal.subject_ne;
    // One flood serves every candidate off the grid.
    let mut flooded: Option<Option<Flood>> = None;
    let side = match choose {
        Choose::Only(_) => 0.0,
        Choose::Any { side } => side,
    };
    for (cand, _, spot) in ring(goal, from, side) {
        let other = matches!(choose, Choose::Only(o) if o != spot);
        if other || clearance(grid, cand) < margin {
            continue;
        }
        if let Some(s) = subject {
            if !obs.line_of_sight(cand, s) {
                continue;
            }
        }
        let path = match grid.cell(cand) {
            Some(cell) => search(grid, from, Some(cell), margin, MAX_EXPANSIONS, prefer)
                .and_then(|f| f.path(grid, cand)),
            None => flooded
                .get_or_insert_with(|| {
                    search(grid, from, None, margin, GRID_N * GRID_N, prefer, narrows)
                })
                .as_ref()
                .and_then(|f| f.exit_toward(grid, cand)),
        };
        if let Some(path) = path {
            let wp = along(&path, LOOKAHEAD_M);
            let minc = path
                .iter()
                .map(|p| clearance(grid, *p))
                .fold(f32::INFINITY, f32::min);
            let plan = Plan {
                state: AvoidState::Detour,
                target: Some(wp),
                goal: Some(cand),
                path: decimate(&path, 16),
                min_clearance: minc.is_finite().then_some(minc),
            };
            return Some((plan, spot));
        }
    }
    None
}

/// Costs and parents of a search over the grid from one point.
pub struct Flood {
    from: V2,
    g: Vec<f32>,
    parent: Vec<u32>,
}

impl Flood {
    /// The route from the search's start to `to`, if the search reached the
    /// cell holding it.
    pub fn path(&self, grid: &Grid, to: V2) -> Option<Vec<V2>> {
        let (i, j) = grid.cell(to)?;
        let mut k = i * GRID_N + j;
        if !self.g[k].is_finite() {
            return None;
        }
        let mut path = vec![to];
        while self.parent[k] != u32::MAX {
            k = self.parent[k] as usize;
            path.push(grid.center_of(k / GRID_N, k % GRID_N));
        }
        path.pop();
        path.push(self.from);
        path.reverse();
        Some(path)
    }

    /// The route to `to`, off the grid, through the edge cell the search
    /// reached where the route's cost plus the straight distance on to `to`
    /// is least, with `to` as its last point. `None` when no edge cell was
    /// reached nearer `to` than the start.
    pub fn exit_toward(&self, grid: &Grid, to: V2) -> Option<Vec<V2>> {
        let last = GRID_N - 1;
        let start = geo::dist(self.from, to);
        let edge = (0..GRID_N).flat_map(|k| [(0, k), (last, k), (k, 0), (k, last)]);
        let (_, exit) = edge
            .filter_map(|(i, j)| {
                let g = self.g[i * GRID_N + j];
                let c = grid.center_of(i, j);
                let rest = geo::dist(c, to);
                (g.is_finite() && rest < start).then_some((g + rest, c))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))?;
        let mut path = self.path(grid, exit)?;
        path.push(to);
        Some(path)
    }
}

/// Within this much clearance beyond the margin a route's length costs up to
/// [`GAP_COST`] times more, metres: of two gaps the vehicle fits, the wider
/// one is preferred, and through a gap the route keeps to its middle.
pub const GAP_ROOM_M: f32 = 0.6;
/// The extra cost per metre at the margin itself ([`GAP_ROOM_M`]).
pub const GAP_COST: f32 = 1.0;
/// The extra cost per metre per metre a route strays from the one flown
/// before (up to [`PREFER_CAP_M`]): a held detour stays on the side of a
/// trunk it chose unless the other side is clearly shorter.
pub const PREFER_COST: f32 = 0.5;
pub const PREFER_CAP_M: f32 = 1.5;

/// Distance from `p` to the polyline `path` (infinite for an empty one).
fn dist_to_path(path: &[V2], p: V2) -> f32 {
    match path {
        [] => f32::INFINITY,
        [only] => geo::dist(*only, p),
        _ => path
            .windows(2)
            .map(|w| {
                let ab = geo::sub(w[1], w[0]);
                let l2 = geo::dot(ab, ab);
                let t = if l2 > 1e-9 {
                    (geo::dot(geo::sub(p, w[0]), ab) / l2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                geo::dist(geo::add(w[0], geo::scale(ab, t)), p)
            })
            .fold(f32::INFINITY, f32::min),
    }
}

/// Best-first search over 8 neighbours from `from`; step cost
/// `len·(1 + gap) + 0.5 / max(clearance, 0.25)`, where `gap` grows to
/// [`GAP_COST`] as the clearance falls to the margin ([`GAP_ROOM_M`]), plus
/// [`PREFER_COST`] per metre of distance from `prefer` (a route flown
/// before; empty for none); cells under `margin` clearance are blocked
/// except near the start, and so are the cells `narrows` bars
/// ([`Narrows::bars`]). With a `goal` it is A* and stops there; without
/// one it floods every reachable cell. `None` when `from` is off the grid,
/// or a goal was given and not reached within `max_expansions`.
fn search(
    grid: &Grid,
    from: V2,
    goal: Option<(usize, usize)>,
    margin: f32,
    max_expansions: usize,
    prefer: &[V2],
    narrows: Option<&Narrows>,
) -> Option<Flood> {
    let (si, sj) = grid.cell(from)?;
    let idx = |i: usize, j: usize| i * GRID_N + j;
    let h = |i: usize, j: usize| {
        goal.map_or(0.0, |(gi, gj)| {
            let di = i.abs_diff(gi) as f32;
            let dj = j.abs_diff(gj) as f32;
            (di.max(dj) + (std::f32::consts::SQRT_2 - 1.0) * di.min(dj)) * CELL_M
        })
    };
    let mut f = Flood {
        from,
        g: vec![f32::INFINITY; GRID_N * GRID_N],
        parent: vec![u32::MAX; GRID_N * GRID_N],
    };
    let mut open = BinaryHeap::new();
    f.g[idx(si, sj)] = 0.0;
    open.push(Reverse((ordered(h(si, sj)), si as u16, sj as u16)));
    let mut expansions = 0;
    while let Some(Reverse((key, i, j))) = open.pop() {
        let (i, j) = (i as usize, j as usize);
        let gi0 = f.g[idx(i, j)];
        // A cell pushed again at a lower cost was already expanded.
        if key > ordered(gi0 + h(i, j)) {
            continue;
        }
        if Some((i, j)) == goal {
            return Some(f);
        }
        expansions += 1;
        if expansions > max_expansions {
            break;
        }
        for (di, dj) in [
            (-1i32, 0i32),
            (1, 0),
            (0, -1),
            (0, 1),
            (-1, -1),
            (-1, 1),
            (1, -1),
            (1, 1),
        ] {
            let (ni, nj) = (i as i32 + di, j as i32 + dj);
            if ni < 0 || nj < 0 || ni >= GRID_N as i32 || nj >= GRID_N as i32 {
                continue;
            }
            let (ni, nj) = (ni as usize, nj as usize);
            let c = grid.clearance_cell(ni, nj).unwrap_or(f32::INFINITY);
            let p = grid.center_of(ni, nj);
            if c < margin && geo::dist(p, from) >= margin {
                continue;
            }
            if narrows.is_some_and(|n| n.bars(p, c)) {
                continue;
            }
            let len = if di != 0 && dj != 0 {
                CELL_M * std::f32::consts::SQRT_2
            } else {
                CELL_M
            };
            let gap = GAP_COST * ((margin + GAP_ROOM_M - c) / GAP_ROOM_M).clamp(0.0, 1.0);
            let stray = if prefer.is_empty() {
                0.0
            } else {
                PREFER_COST * dist_to_path(prefer, p).min(PREFER_CAP_M)
            };
            let cost = gi0 + len * (1.0 + gap + stray) + 0.5 / c.max(0.25);
            let k = idx(ni, nj);
            if cost < f.g[k] {
                f.g[k] = cost;
                f.parent[k] = idx(i, j) as u32;
                open.push(Reverse((ordered(cost + h(ni, nj)), ni as u16, nj as u16)));
            }
        }
    }
    goal.is_none().then_some(f)
}

/// Every cell reachable from `from` at `margin`, in one search: routes to
/// many candidate points cost one flood instead of one A* each.
pub fn flood(grid: &Grid, from: V2, margin: f32) -> Option<Flood> {
    search(grid, from, None, margin, GRID_N * GRID_N, &[], None)
}

fn ordered(v: f32) -> u32 {
    // Non-negative floats order like their bit patterns.
    v.max(0.0).to_bits()
}

/// The point `dist` metres along a polyline.
pub fn along(path: &[V2], dist: f32) -> V2 {
    let mut left = dist;
    for w in path.windows(2) {
        let seg = geo::dist(w[0], w[1]);
        if seg >= left && seg > 0.0 {
            return geo::add(w[0], geo::scale(geo::sub(w[1], w[0]), left / seg));
        }
        left -= seg;
    }
    *path.last().unwrap_or(&[0.0, 0.0])
}

pub(crate) fn decimate(path: &[V2], max: usize) -> Vec<V2> {
    if path.len() <= max {
        return path.to_vec();
    }
    let step = (path.len() - 1) as f32 / (max - 1) as f32;
    (0..max)
        .map(|k| path[((k as f32 * step).round() as usize).min(path.len() - 1)])
        .collect()
}

#[cfg(test)]
mod tests;
