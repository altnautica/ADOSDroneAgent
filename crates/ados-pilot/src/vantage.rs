//! Vantage choice for a lost subject: a reachable point, clear of obstacles
//! and within the search bound, that sees as much of the predicted
//! position's uncertainty ellipse (and the skill's other hypotheses) as
//! possible, around trunks and past corners.

use ados_protocol::pilot::VantageSpec;

use crate::geo::{self, V2};
use crate::obstacles::{Grid, Obstacles, CELL_M, GRID_N};
use crate::planner::astar;

/// Wider than this along the blocking surface is a wall or a corner, else a
/// trunk or a column.
pub const NARROW_OCCLUDER_M: f32 = 1.5;
/// Ring candidates are this many degrees apart.
const RING_STEP_DEG: f32 = 15.0;
/// A prediction inside an obstacle moves at most this far to free space.
const SNAP_MAX_M: f32 = 2.0;
/// Free space a snapped prediction keeps.
const SNAP_MARGIN_M: f32 = 0.3;
/// Seconds of flight one unit of lost coverage is worth.
const COVERAGE_WEIGHT_S: f32 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Occluder {
    pub ne: V2,
    pub width_m: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VantageChoice {
    /// Where to look from; `None` when no reachable point in the bound sees
    /// any sample.
    pub point: Option<V2>,
    /// Fraction of the samples in line of sight from `point` (0..1).
    pub coverage: f32,
    /// What hides the prediction from the vehicle now.
    pub occluder: Option<Occluder>,
    /// The route from the vehicle to `point`.
    pub path: Vec<V2>,
}

/// The ellipse centre and 8 points on the `sigma_k` ellipse of `cov_ne`.
pub fn ellipse_samples(s: &VantageSpec) -> Vec<V2> {
    let [a, b, c] = s.cov_ne;
    let tr = a + c;
    let disc = ((a - c) * (a - c) / 4.0 + b * b).sqrt();
    let (l1, l2) = ((tr / 2.0 + disc).max(0.0), (tr / 2.0 - disc).max(0.0));
    let th = 0.5 * (2.0 * b).atan2(a - c);
    let (r1, r2) = (s.sigma_k * l1.sqrt(), s.sigma_k * l2.sqrt());
    let (st, ct) = th.sin_cos();
    let mut out = Vec::with_capacity(9 + s.hypotheses_ne.len());
    out.push(s.predicted_ne);
    for k in 0..8 {
        let (sp, cp) = (k as f32 * std::f32::consts::FRAC_PI_4).sin_cos();
        let (u, v) = (r1 * cp, r2 * sp);
        out.push([
            s.predicted_ne[0] + u * ct - v * st,
            s.predicted_ne[1] + u * st + v * ct,
        ]);
    }
    out
}

/// Fraction of `samples` in line of sight from `from`.
pub fn coverage(obs: &Obstacles, from: V2, samples: &[V2]) -> f32 {
    let seen = samples
        .iter()
        .filter(|p| obs.line_of_sight(from, **p))
        .count();
    seen as f32 / samples.len().max(1) as f32
}

/// `p`, or the nearest point within `max_m` of it with `margin` clearance
/// (the corridor constraint: a prediction inside an obstacle is moved to the
/// free space next to it). `p` itself when none is found.
pub fn snap_free(grid: &Grid, p: V2, margin: f32, max_m: f32) -> V2 {
    if grid.clearance(p).unwrap_or(f32::INFINITY) >= margin {
        return p;
    }
    let mut best = (p, f32::INFINITY);
    let n = (max_m / CELL_M).ceil() as i32;
    for i in -n..=n {
        for j in -n..=n {
            let q = [p[0] + i as f32 * CELL_M, p[1] + j as f32 * CELL_M];
            let d = geo::dist(p, q);
            if d <= max_m
                && d < best.1
                && grid.clearance(q).unwrap_or(f32::INFINITY) >= margin
            {
                best = (q, d);
            }
        }
    }
    best.0
}

/// The first obstacle on `from → to` and its width across the line of sight.
pub fn occluder_between(obs: &Obstacles, from: V2, to: V2) -> Option<Occluder> {
    let d = geo::dist(from, to);
    if d < 1e-3 {
        return None;
    }
    let dir = geo::scale(geo::sub(to, from), 1.0 / d);
    let hit = obs.grid.march(from, dir, d - 0.3)?;
    let ne = geo::add(from, geo::scale(dir, hit));
    // Width: the occupied run across the line of sight through the hit,
    // 0.1 m steps each side, a little behind the hit's front face.
    let perp = [-dir[1], dir[0]];
    let base = geo::add(ne, geo::scale(dir, 0.15));
    let occupied = |s: f32| {
        obs.grid
            .clearance(geo::add(base, geo::scale(perp, s)))
            .is_some_and(|c| c < 0.15)
    };
    let side = |sgn: f32| {
        (1..=60)
            .take_while(|k| occupied(sgn * *k as f32 * 0.1))
            .count() as f32
            * 0.1
    };
    Some(Occluder {
        ne,
        width_m: side(1.0) + side(-1.0) + 0.1,
    })
}

/// The centre of the candidate ring: the prediction, or, when it lies off
/// the obstacle grid, the point toward it that the grid still covers.
fn ring_centre(grid: &Grid, from: V2, pred: V2, standoff: f32) -> V2 {
    if grid.cell(pred).is_some() {
        return pred;
    }
    let d = geo::dist(from, pred);
    let inside = (GRID_N as f32 * CELL_M / 2.0 - 1.0 - standoff).max(1.0);
    geo::add(from, geo::scale(geo::sub(pred, from), inside.min(d) / d))
}

/// Choose the vantage point for `s` from the vehicle at `from`.
pub fn choose(obs: &Obstacles, s: &VantageSpec, from: V2, margin: f32, max_speed: f32) -> VantageChoice {
    let grid = &obs.grid;
    let pred = snap_free(grid, s.predicted_ne, SNAP_MARGIN_M, SNAP_MAX_M);
    let mut samples = ellipse_samples(&VantageSpec {
        predicted_ne: pred,
        ..s.clone()
    });
    samples.extend(
        s.hypotheses_ne
            .iter()
            .map(|h| snap_free(grid, *h, SNAP_MARGIN_M, SNAP_MAX_M)),
    );
    let occluder = occluder_between(obs, from, pred);
    let here = coverage(obs, from, &samples);
    if here > 0.99 {
        return VantageChoice {
            point: Some(from),
            coverage: here,
            occluder,
            path: vec![from],
        };
    }
    let v = max_speed.max(0.5);
    let centre = ring_centre(grid, from, pred, s.standoff_m);
    // Every free ring point in the bound that sees something, with a lower
    // bound on its cost (the straight line); A* runs only while a candidate
    // could still beat the best found.
    let mut cands: Vec<(f32, V2, f32)> = Vec::new();
    for dr in [0.0f32, 2.0, -2.0] {
        let r = (s.standoff_m + dr).max(2.0);
        let mut deg = 0.0f32;
        while deg < 360.0 {
            let p = geo::add(centre, geo::scale(geo::bearing_unit(deg.to_radians()), r));
            deg += RING_STEP_DEG;
            if geo::dist(p, s.anchor_ne) > s.max_radius_m {
                continue;
            }
            match grid.clearance(p) {
                Some(c) if c >= margin => {}
                _ => continue,
            }
            let cov = coverage(obs, p, &samples);
            if cov <= 0.0 {
                continue;
            }
            let lb = geo::dist(from, p) / v + COVERAGE_WEIGHT_S * (1.0 - cov);
            cands.push((lb, p, cov));
        }
    }
    cands.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut best: Option<(f32, V2, f32, Vec<V2>)> = None;
    for (lb, p, cov) in cands {
        if best.as_ref().is_some_and(|b| lb >= b.0) {
            break;
        }
        let Some(path) = astar(grid, from, p, margin) else {
            continue;
        };
        let len: f32 = path.windows(2).map(|w| geo::dist(w[0], w[1])).sum();
        let cost = len / v + COVERAGE_WEIGHT_S * (1.0 - cov);
        if best.as_ref().is_none_or(|b| cost < b.0) {
            best = Some((cost, p, cov, path));
        }
    }
    match best {
        Some((_, p, cov, path)) => VantageChoice {
            point: Some(p),
            coverage: cov,
            occluder,
            path,
        },
        None => VantageChoice {
            point: None,
            coverage: 0.0,
            occluder,
            path: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests;
