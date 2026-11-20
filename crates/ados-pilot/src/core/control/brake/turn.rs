//! The stop curves of a turn: where the vehicle goes when the velocity it
//! flies and the answer differ in direction.

use super::BRAKE_LAG_S;
use crate::geo::{self, V2};

/// Points along the lag at which the FC may start turning toward the answer
/// (the setpoints it still has queued turn at some point within the lag).
const TURN_STARTS: usize = 4;
/// Straight pieces each curve's turn is drawn with.
const TURN_PIECES: usize = 4;
/// Halvings of the answer's speed searched for the fastest clear one.
const TURN_HALVINGS: usize = 8;
/// An answer differing from the flown velocity by less than this turns
/// within the straight paths' reach, m/s.
const TURN_MIN_MPS: f32 = 0.05;
/// Pieces per curve: the run on, the turn, the stop.
const CURVE_PIECES: usize = TURN_PIECES + 2;
/// An answer's curves may come this much nearer a point than the stop asked
/// now does before they count as turning into it, m: an answer that turns
/// away from a point the vehicle is already flying at spends part of the
/// FC's acceleration on the turn and stops a few centimetres later, which
/// the flown path's cap answers for.
const TURN_NEARER_M: f32 = 0.05;
/// A turn narrower than this between the flown and the asked direction has
/// no bend the straight paths miss, radians (about 5°).
const BEND_MIN_RAD: f32 = 0.09;

/// The bend between the flown and the asked direction, both unit, turning
/// through less than a half turn.
#[derive(Clone, Copy)]
pub(super) struct Bend {
    from: V2,
    to: V2,
}

impl Bend {
    /// The bend from flying `vel` to asking `v`; `None` when either has no
    /// direction or they turn through less than [`BEND_MIN_RAD`] (or
    /// reverse along one line, with no side to bend toward).
    pub(super) fn of(vel: V2, v: V2) -> Option<Self> {
        let (a, b) = (geo::norm(vel), geo::norm(v));
        if a < TURN_MIN_MPS || b < TURN_MIN_MPS {
            return None;
        }
        let (from, to) = (geo::scale(vel, 1.0 / a), geo::scale(v, 1.0 / b));
        let cross = from[0] * to[1] - from[1] * to[0];
        (cross.abs() >= BEND_MIN_RAD.sin()).then_some(Self { from, to })
    }

    /// Whether the point at `rel` (relative to the vehicle) lies inside the
    /// bend.
    pub(super) fn holds(self, rel: V2) -> bool {
        let side = (self.from[0] * self.to[1] - self.from[1] * self.to[0]).signum();
        let a = (self.from[0] * rel[1] - self.from[1] * rel[0]) * side;
        let b = (rel[0] * self.to[1] - rel[1] * self.to[0]) * side;
        a > 0.0 && b > 0.0 && geo::dot(rel, geo::add(self.from, self.to)) > 0.0
    }
}

/// Distance from `p` to the segment `a`-`b`.
fn to_segment(p: V2, a: V2, b: V2) -> f32 {
    let ab = geo::sub(b, a);
    let l2 = geo::dot(ab, ab);
    let t = if l2 > 1e-9 {
        (geo::dot(geo::sub(p, a), ab) / l2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    geo::dist(p, geo::add(a, geo::scale(ab, t)))
}

/// The stop curves from flying `vel` while answering `w`: the FC flies on at
/// `vel` for part of its lag ([`BRAKE_LAG_S`]), turns toward `w` at `a` for
/// the rest (the answers it has queued), and then stops at `a` along the
/// way it is flying, the stop asked now taking effect only after the lag.
/// One curve per point along the lag the turn may start at; the curve that
/// flies on for the whole lag is the straight flown path, drawn already.
fn curves(vel: V2, w: V2, a: f32) -> [[V2; CURVE_PIECES + 1]; TURN_STARTS] {
    std::array::from_fn(|s| {
        let on = BRAKE_LAG_S * s as f32 / TURN_STARTS as f32;
        let turn = BRAKE_LAG_S - on;
        let dv = geo::sub(w, vel);
        let n = geo::norm(dv);
        let reach = n / a;
        let dir = geo::scale(dv, 1.0 / n.max(1e-6));
        // Position and velocity `t` into the turn.
        let at = |t: f32| {
            let ta = t.min(reach);
            let p = geo::add(geo::scale(vel, t), geo::scale(dir, a * ta * (t - ta / 2.0)));
            (p, geo::add(vel, geo::scale(dir, a * ta)))
        };
        let mut pts = [[0.0; 2]; CURVE_PIECES + 1];
        pts[1] = geo::scale(vel, on);
        let mut u = vel;
        for k in 1..=TURN_PIECES {
            let (p, v) = at(turn * k as f32 / TURN_PIECES as f32);
            pts[k + 1] = geo::add(pts[1], p);
            u = v;
        }
        let stop = geo::norm(u) / (2.0 * a);
        pts[CURVE_PIECES] = geo::add(pts[TURN_PIECES + 1], geo::scale(u, stop));
        pts
    })
}

/// The nearest the curves come to `p`.
fn nearest(curves: &[[V2; CURVE_PIECES + 1]; TURN_STARTS], p: V2) -> f32 {
    curves
        .iter()
        .flat_map(|c| c.windows(2).map(move |w| to_segment(p, w[0], w[1])))
        .fold(f32::INFINITY, f32::min)
}

/// The share of the answer `out` (0 to 1) whose stop curves keep every
/// obstacle point (relative to the vehicle, from `points`) at least `pass`
/// away (the margin to the point's cell centre), the vehicle flying `vel`
/// and braking at `a`. Where even the stop asked now (the curves of a zero
/// answer) comes nearer than `pass`, the answer may bring the vehicle no
/// nearer than that stop does: no answer changes where the FC flies before
/// it turns. A point inside the `bend` is judged on the curves toward the
/// same share of `queued` as well (the setpoints the FC still has queued,
/// answers to the same ask given before this one cut them).
pub(super) fn turn_share<I: Iterator<Item = V2>>(
    vel: V2,
    out: V2,
    queued: V2,
    bend: Option<Bend>,
    a: f32,
    pass: f32,
    points: impl Fn() -> I,
) -> f32 {
    if geo::dist(out, vel) < TURN_MIN_MPS && bend.is_none() {
        return 1.0;
    }
    let stop = curves(vel, [0.0; 2], a);
    let hits = |k: f32| {
        let turn = curves(vel, geo::scale(out, k), a);
        let late = bend.map(|b| (b, curves(vel, geo::scale(queued, k), a)));
        points().any(|p| {
            let mut d = nearest(&turn, p);
            if let Some((b, late)) = &late {
                if b.holds(p) {
                    d = d.min(nearest(late, p));
                }
            }
            d < pass && d < nearest(&stop, p) - TURN_NEARER_M
        })
    };
    if !hits(1.0) {
        return 1.0;
    }
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..TURN_HALVINGS {
        let mid = 0.5 * (lo + hi);
        if hits(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    lo
}
