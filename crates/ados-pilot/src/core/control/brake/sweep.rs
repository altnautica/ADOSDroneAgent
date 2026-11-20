//! The stop paths the brake judges obstacle points along.

use super::{BRAKE_LAG_S, BRAKE_SECTORS};
use crate::geo::{self, V2};

/// Rounds of stop paths drawn ([`Sweep`]) before every point within reach
/// also caps the velocity toward it, as a radial cap.
pub(super) const SWEEP_ROUNDS: usize = 4;
/// Stop paths at most: the asked velocity, the flown one and one per round.
pub(super) const SWEEP_PATHS: usize = SWEEP_ROUNDS + 2;
/// A velocity under this has no direction a stop path is drawn along.
const SWEEP_MIN_MPS: f32 = 0.05;
/// Path drawn beyond the stopping distance, metres (the reach's own extra
/// metre, so a point straight ahead is judged as far out as the reach).
const SWEEP_EXTRA_M: f32 = 1.0;
/// Between the margin and the keep-out, a stop path may come this much
/// nearer than the vehicle is now to its nearest obstacle point, metres:
/// the nearest point of a surface steps by up to this as the vehicle moves
/// along it (scan noise, a trunk's curve).
pub(super) const SWEEP_DEPTH_SLACK_M: f32 = 0.05;
/// Constraints the brake solves at most: one radial cap per sector, one cap
/// per stop path.
pub(super) const BRAKE_CAPS: usize = BRAKE_SECTORS + SWEEP_PATHS;

/// The stop paths the brake judges obstacle points along: straight from the
/// vehicle along a velocity it asks, flies or would be given, as long as the
/// stop from that speed after the FC's lag ([`BRAKE_LAG_S`]) plus
/// [`SWEEP_EXTRA_M`].
pub(super) struct Sweep {
    /// Braking acceleration the stop is planned on, m/s².
    a: f32,
    /// The keep-out radius ([`super::keep_out_m`]): a path stops short of it.
    radius: f32,
    /// A path passing a point nearer than this caps (the keep-out, or less
    /// when the vehicle is already that near something).
    pass: f32,
    /// The paths drawn so far: (unit direction, length).
    pub(super) paths: [(V2, f32); SWEEP_PATHS],
    pub(super) n: usize,
}

impl Sweep {
    pub(super) fn new(a: f32, radius: f32, pass: f32) -> Self {
        Self {
            a,
            radius,
            pass: pass.min(radius),
            paths: [([0.0; 2], 0.0); SWEEP_PATHS],
            n: 0,
        }
    }

    /// Draw the stop path of `v`, from at least `speed` (the scans may see
    /// the vehicle closing faster along it than the estimate says); false
    /// when `v` has no direction or a path at least as long is drawn along
    /// it already.
    pub(super) fn add(&mut self, v: V2, speed: f32) -> bool {
        let n = geo::norm(v);
        if n < SWEEP_MIN_MPS || self.n == self.paths.len() {
            return false;
        }
        let e = geo::scale(v, 1.0 / n);
        let s = n.max(speed);
        let len = s * BRAKE_LAG_S + s * s / (2.0 * self.a) + SWEEP_EXTRA_M;
        if self.paths[..self.n]
            .iter()
            .any(|&(d, l)| geo::dot(d, e) > 1.0 - 1e-6 && l >= len)
        {
            return false;
        }
        self.paths[self.n] = (e, len);
        self.n += 1;
        true
    }

    /// How far path `k` runs before it enters the keep-out disk of the point
    /// at `rel`; `None` when it passes the point no nearer than
    /// [`Sweep::pass`] or ends before.
    pub(super) fn room(&self, k: usize, rel: V2) -> Option<f32> {
        let (e, len) = self.paths[k];
        let along = geo::dot(rel, e);
        let perp = (rel[0] * e[1] - rel[1] * e[0]).abs();
        if along <= 0.0 || perp >= self.pass {
            return None;
        }
        let room = along - (self.radius * self.radius - perp * perp).sqrt();
        (room < len).then_some(room.max(0.0))
    }
}
