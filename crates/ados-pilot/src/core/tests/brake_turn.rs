//! A turn past a post, closed loop: the vehicle flies one way, the ask
//! jumps to another, and an FC answering the brake after a lag carries it
//! round a curve between the two. The brake must stop it outside the margin
//! of a post inside that bend whenever a plain stop from the start would
//! have, and never come deeper into it than the radial brake it replaced
//! ([`a_post_inside_a_turn_stays_outside_the_margin`] for the bars per lag).

use super::safety::G;
use crate::core::control::{
    brake_limit, confined, nearest_within, scan_closing_bound, scan_closing_bound_max, stop_cap,
    with_push, BRAKE_LAG_S, CONFINED_MIN_MPS, CONFINED_SPEED_PER_M,
};
use crate::geo::{self, V2};
use crate::obstacles::{Obstacles, CELL_M};

const A_H: f32 = 3.0;
const MARGIN: f32 = 1.5;
/// The post's radius, m: thinner than a grid cell.
const POST_M: f32 = 0.15;
/// Simulation step, s.
const DT: f32 = 0.05;
/// FC lags flown: none, half and the full lag the brake plans for.
const LAGS: [f32; 3] = [0.0, 0.4, BRAKE_LAG_S];
/// Turns flown, degrees from the starting heading (north) toward east.
const TURNS: [f32; 2] = [90.0, 60.0];
const SPEEDS: [f32; 4] = [1.0, 2.0, 3.0, 4.0];
/// Post centres: north 0..5.5 m, east 2..5.5 m, every 0.5 m (the bend of a
/// right turn; twice the grid cell, so the post sits differently in its
/// cells from one position to the next).
const POSTS_N: usize = 12;
const POSTS_E: usize = 8;
const POST_STEP_M: f32 = 0.5;
/// A flight entering the margin by more than this counts as an entry, m: the
/// grid places a thin post's surface to within about this.
const ENTRY_TOL_M: f32 = 0.05;

/// One post, every cell its surface touches marked.
fn post(c: V2) -> Obstacles {
    let mut o = Obstacles::new();
    o.grid.reset([0.0, 0.0]);
    for k in 0..72 {
        let b = (k as f32 * 5.0).to_radians();
        o.grid
            .mark(geo::add(c, geo::scale(geo::bearing_unit(b), POST_M)));
    }
    o.grid.mark(c);
    o.grid.transform();
    o
}

type Brake = fn(&Obstacles, V2, V2, V2) -> V2;

fn brake_now(o: &Obstacles, pos: V2, vel: V2, v: V2) -> V2 {
    brake_limit(o, pos, vel, v, A_H, G, 0).v
}

/// The brake as it was before obstacles were judged along the motion: every
/// obstacle point within reach caps the velocity toward it, and the
/// clearance speed counts every point. The reference the brake may not fall
/// behind.
fn brake_radial(o: &Obstacles, pos: V2, vel: V2, v: V2) -> V2 {
    let speed = geo::norm(v).max(geo::norm(vel)).max(
        o.max_scan_closing(0)
            .min(scan_closing_bound_max(geo::norm(vel))),
    );
    let reach =
        (MARGIN + speed * speed / (2.0 * A_H) + speed * BRAKE_LAG_S + 1.0).clamp(MARGIN, 20.0);
    let mut caps: [Option<(V2, f32)>; 72] = [None; 72];
    let mut near: [Option<(V2, f32)>; 72] = [None; 72];
    let sector = |dir: V2| {
        (geo::bearing_of(dir).rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU * 72.0)
            as usize
            % 72
    };
    for p in o.points_near(pos, reach, 0) {
        let rel = geo::sub(p, pos);
        let r = geo::norm(rel);
        if r < 1e-3 {
            continue;
        }
        let dir = geo::scale(rel, 1.0 / r);
        let d = (r - CELL_M / 2.0).max(0.0);
        let closing =
            geo::dot(vel, dir).max(o.scan_closing(dir, 0).min(scan_closing_bound(vel, dir)));
        let cap = stop_cap(d, closing, A_H, MARGIN);
        let s = sector(dir);
        if caps[s].is_none_or(|(_, c)| cap < c) {
            caps[s] = Some((dir, cap));
        }
        if near[s].is_none_or(|(_, n)| d < n) {
            near[s] = Some((dir, d));
        }
    }
    let out = nearest_within(v, &caps);
    let d_min = near
        .iter()
        .flatten()
        .map(|&(_, d)| d)
        .fold(f32::INFINITY, f32::min);
    let v_conf = (CONFINED_SPEED_PER_M * (d_min - G.body_m)).max(CONFINED_MIN_MPS);
    with_push(out, v_conf, confined(&near, G).1)
}

/// Distance from the post's surface.
fn clearance(c: V2, p: V2) -> f32 {
    geo::dist(c, p) - POST_M
}

/// The nearest a plain stop from the start comes to the post: on at `s`
/// north for the lag, then braking at [`A_H`].
fn plain_stop(c: V2, s: f32, lag: f32) -> f32 {
    let len = s * lag + s * s / (2.0 * A_H);
    clearance(c, [c[0].clamp(0.0, len), 0.0])
}

/// Fly from the origin north at `s` with the ask jumped to `turn` degrees
/// east of north at the same speed, the FC following each braked setpoint
/// `lag` seconds late at [`A_H`]. Returns the nearest the vehicle came to
/// the post's surface.
fn fly(brake: Brake, o: &Obstacles, c: V2, s: f32, turn: f32, lag: f32) -> f32 {
    let ask = geo::scale(geo::bearing_unit(turn.to_radians()), s);
    let mut pos = [0.0f32, 0.0];
    let mut vel = [s, 0.0];
    let mut queue: std::collections::VecDeque<V2> =
        std::iter::repeat_n(vel, (lag / DT).round() as usize).collect();
    let mut nearest = clearance(c, pos);
    for _ in 0..(8.0 / DT) as usize {
        queue.push_back(brake(o, pos, vel, ask));
        let to = queue.pop_front().expect("queued");
        let dv = geo::sub(to, vel);
        let n = geo::norm(dv);
        if n > 1e-6 {
            vel = geo::add(vel, geo::scale(dv, (A_H * DT).min(n) / n));
        }
        pos = geo::add(pos, geo::scale(vel, DT));
        nearest = nearest.min(clearance(c, pos));
        // Settled on the ask and leaving the post behind: no nearer from here.
        let rel = geo::sub(c, pos);
        let settled = queue.iter().all(|&q| geo::dist(q, ask) < 1e-3);
        if settled && geo::dist(vel, ask) < 1e-3 && geo::dot(rel, ask) < 0.0 {
            break;
        }
    }
    nearest
}

/// Per lag and turn: the cells a plain stop keeps outside the margin, and
/// for each brake the entries past [`ENTRY_TOL_M`] and the nearest approach;
/// and the most the second brake came further inside the margin than the
/// first in any cell (outside it, nearer is the speed the first brake gave
/// away beside a post, not a loss of safety), with that cell.
struct Row {
    lag: f32,
    turn: f32,
    cells: usize,
    entries: [usize; 2],
    nearest: [f32; 2],
    deeper: (f32, V2, f32),
}

fn rows(brakes: [Brake; 2]) -> Vec<Row> {
    let mut rows = Vec::new();
    for lag in LAGS {
        for turn in TURNS {
            let mut row = Row {
                lag,
                turn,
                cells: 0,
                entries: [0; 2],
                nearest: [f32::INFINITY; 2],
                deeper: (0.0, [0.0; 2], 0.0),
            };
            for i in 0..POSTS_N {
                for j in 0..POSTS_E {
                    let c = [POST_STEP_M * i as f32, 2.0 + POST_STEP_M * j as f32];
                    let o = post(c);
                    for s in SPEEDS {
                        if plain_stop(c, s, lag) < MARGIN {
                            continue;
                        }
                        row.cells += 1;
                        let m = brakes.map(|b| fly(b, &o, c, s, turn, lag));
                        for ((n, near), m) in row.entries.iter_mut().zip(&mut row.nearest).zip(m) {
                            *n += usize::from(m < MARGIN - ENTRY_TOL_M);
                            *near = near.min(m);
                        }
                        let deeper = m[0].min(MARGIN) - m[1].min(MARGIN);
                        if deeper > row.deeper.0 {
                            row.deeper = (deeper, c, s);
                        }
                    }
                }
            }
            rows.push(row);
        }
    }
    rows
}

/// Below the lag the brake plans for, the brake never enters the margin
/// where a plain stop would not, and never comes deeper than the radial
/// brake (to a centimetre). At a pure delay of the whole planned lag the FC
/// still flies setpoints the brake gave before it could see the turn bring
/// the post near, and the radial brake entered too: there the brake must
/// enter fewer cells, never as near, and no cell more than a few
/// centimetres deeper than the radial brake.
#[test]
fn a_post_inside_a_turn_stays_outside_the_margin() {
    let rows = rows([brake_radial, brake_now]);
    let mut failed = Vec::new();
    for r in &rows {
        let (deeper, c, s) = r.deeper;
        println!(
            "lag {:.1} s, turn {:>2}°: {} cells; radial {} entries, nearest {:.3} m; now {} entries, nearest {:.3} m; at most {:.3} m deeper (post {c:?}, {s} m/s)",
            r.lag, r.turn, r.cells, r.entries[0], r.nearest[0], r.entries[1], r.nearest[1], deeper
        );
        let ok = if r.lag < BRAKE_LAG_S {
            r.entries[1] == 0 && deeper <= 0.01
        } else {
            r.entries[1] < r.entries[0] && r.nearest[1] > r.nearest[0] && deeper <= 0.08
        };
        if !ok {
            failed.push(format!("lag {} turn {}", r.lag, r.turn));
        }
    }
    assert!(failed.is_empty(), "{failed:?}");
}
