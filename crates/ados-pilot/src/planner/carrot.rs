//! The point a detour is flown toward: [`super::LOOKAHEAD_M`] along its
//! route, nearer where the straight line to it would cut a bend closer to
//! an obstacle than the route itself passes. The vehicle flies that line,
//! not the route: cutting the inside of a bend round a trunk carries it
//! into the band between the margin and the obstacle brake's keep-out
//! that the route kept out of, and the brake holds it to a stop-go creep
//! there (the route reads clear, the line to its carrot does not).

use super::{along, clearance, LOOKAHEAD_M, ROUTE_ROOM_M};
use crate::geo::{self, V2};
use crate::obstacles::{Grid, CELL_M};

/// The carrot moves back along the route this far at a time, m.
const STEP_M: f32 = CELL_M;
/// Clearance is sampled this often along a line, m.
const SAMPLE_M: f32 = CELL_M / 2.0;

/// The least clearance along `a → b`, sampled every [`SAMPLE_M`].
fn least(grid: &Grid, a: V2, b: V2) -> f32 {
    let n = (geo::dist(a, b) / SAMPLE_M).ceil().max(1.0) as usize;
    (0..=n)
        .map(|k| {
            let p = geo::add(a, geo::scale(geo::sub(b, a), k as f32 / n as f32));
            clearance(grid, p)
        })
        .fold(f32::INFINITY, f32::min)
}

/// The least clearance along `path` over its first `dist` metres.
fn least_along(grid: &Grid, path: &[V2], dist: f32) -> f32 {
    let mut left = dist;
    let mut out = f32::INFINITY;
    for w in path.windows(2) {
        if left <= 0.0 {
            break;
        }
        let seg = geo::dist(w[0], w[1]);
        let end = if seg > left {
            geo::add(w[0], geo::scale(geo::sub(w[1], w[0]), left / seg))
        } else {
            w[1]
        };
        out = out.min(least(grid, w[0], end));
        left -= seg;
    }
    out
}

/// The carrot on `path` (from the vehicle): the farthest point up to
/// [`LOOKAHEAD_M`] along it, in [`STEP_M`] steps, that the straight line
/// from the path's start reaches as clear as the path does up to it (held
/// to the room the brake flies freely, `margin` and [`ROUTE_ROOM_M`]); the
/// first step's point when none is.
pub fn carrot(grid: &Grid, path: &[V2], margin: f32) -> V2 {
    let Some(&from) = path.first() else {
        return along(path, LOOKAHEAD_M);
    };
    let room = margin + ROUTE_ROOM_M;
    let mut d = LOOKAHEAD_M;
    loop {
        let wp = along(path, d);
        let need = least_along(grid, path, d).min(room);
        if d <= STEP_M || least(grid, from, wp) >= need - 1e-3 {
            return wp;
        }
        d -= STEP_M;
    }
}
