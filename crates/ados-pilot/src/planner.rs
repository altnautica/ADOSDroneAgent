//! The skill and goto planner (C2): keep the goal when it is clear, otherwise
//! search detour candidates on the skill's ring and route to the best
//! reachable one with A* over the clearance grid.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use ados_protocol::pilot::{AvoidState, Goal, GoalKind};

use crate::geo::{self, V2};
use crate::obstacles::{Grid, Obstacles, CELL_M, GRID_N};

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

/// Detour candidates around the subject, cheapest first: `(point, cost)`.
pub fn candidates(goal: &Goal, from: V2) -> Vec<(V2, f32)> {
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
                    out.push((p, dphi.abs() / 60.0 + dr.abs() / 2.0));
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
                    out.push((p, delta.abs() / 90.0 + dd / 4.0));
                }
            }
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

/// Plan one goal from `from`.
pub fn plan(obs: &Obstacles, goal: &Goal, from: V2, margin: f32) -> Plan {
    let grid = &obs.grid;
    let target = [goal.n, goal.e];
    let gc = clearance(grid, target);
    if gc >= margin && segment_clear(grid, from, target, margin) {
        return Plan {
            state: AvoidState::Clear,
            target: Some(target),
            goal: Some(target),
            path: vec![from, target],
            min_clearance: Some(gc.min(clearance(grid, from))).filter(|c| c.is_finite()),
        };
    }
    let subject = goal.subject_ne;
    for (cand, _) in candidates(goal, from) {
        if clearance(grid, cand) < margin {
            continue;
        }
        if let Some(s) = subject {
            if !obs.line_of_sight(cand, s) {
                continue;
            }
        }
        if let Some(path) = astar(grid, from, cand, margin) {
            let wp = along(&path, LOOKAHEAD_M);
            let minc = path
                .iter()
                .map(|p| clearance(grid, *p))
                .fold(f32::INFINITY, f32::min);
            return Plan {
                state: AvoidState::Detour,
                target: Some(wp),
                goal: Some(cand),
                path: decimate(&path, 16),
                min_clearance: minc.is_finite().then_some(minc),
            };
        }
    }
    Plan::hold(AvoidState::Blocked)
}

/// A* over 8 neighbours; step cost `len + 0.5 / max(clearance, 0.25)`; cells
/// under `margin` clearance are blocked except near the start.
pub fn astar(grid: &Grid, from: V2, to: V2, margin: f32) -> Option<Vec<V2>> {
    let (si, sj) = grid.cell(from)?;
    let (gi, gj) = grid.cell(to)?;
    let idx = |i: usize, j: usize| i * GRID_N + j;
    let h = |i: usize, j: usize| {
        let di = i.abs_diff(gi) as f32;
        let dj = j.abs_diff(gj) as f32;
        (di.max(dj) + (std::f32::consts::SQRT_2 - 1.0) * di.min(dj)) * CELL_M
    };
    let mut g = vec![f32::INFINITY; GRID_N * GRID_N];
    let mut parent = vec![u32::MAX; GRID_N * GRID_N];
    let mut open = BinaryHeap::new();
    g[idx(si, sj)] = 0.0;
    open.push(Reverse((ordered(h(si, sj)), si as u16, sj as u16)));
    let mut expansions = 0;
    while let Some(Reverse((_, i, j))) = open.pop() {
        let (i, j) = (i as usize, j as usize);
        if (i, j) == (gi, gj) {
            let mut path = vec![to];
            let mut k = idx(i, j);
            while parent[k] != u32::MAX {
                k = parent[k] as usize;
                path.push(grid.center_of(k / GRID_N, k % GRID_N));
            }
            path.pop();
            path.push(from);
            path.reverse();
            return Some(path);
        }
        expansions += 1;
        if expansions > MAX_EXPANSIONS {
            return None;
        }
        let gi0 = g[idx(i, j)];
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
            let len = if di != 0 && dj != 0 {
                CELL_M * std::f32::consts::SQRT_2
            } else {
                CELL_M
            };
            let cost = gi0 + len + 0.5 / c.max(0.25);
            let k = idx(ni, nj);
            if cost < g[k] {
                g[k] = cost;
                parent[k] = idx(i, j) as u32;
                open.push(Reverse((ordered(cost + h(ni, nj)), ni as u16, nj as u16)));
            }
        }
    }
    None
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

fn decimate(path: &[V2], max: usize) -> Vec<V2> {
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
