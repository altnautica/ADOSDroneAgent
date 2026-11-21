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
/// A path passing a point inside the creep slack's band while keeping the
/// margin is slowed to no less than this share of [`Sweep::speed`]. The
/// margin is measured from the near face of the point's grid cell, and the
/// surface may lie up to a cell beyond it: a pass at the margin's edge
/// halves the speed rather than stopping hard, whose braking pitch also
/// tilts a planar lidar's beams out of the band.
const PASS_FLOOR_SHARE: f32 = 0.5;
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
    /// A path passing a point nearer than this enters its margin (measured
    /// from the near face of the point's cell): it stops short of the
    /// keep-out.
    inner: f32,
    /// The speed a path passing a point between [`Sweep::inner`] and
    /// [`Sweep::pass`] is slowed from in proportion ([`Sweep::cap`]).
    speed: f32,
    /// The paths drawn so far: (unit direction, length).
    pub(super) paths: [(V2, f32); SWEEP_PATHS],
    pub(super) n: usize,
}

impl Sweep {
    pub(super) fn new(a: f32, radius: f32, pass: f32, inner: f32, speed: f32) -> Self {
        let pass = pass.min(radius);
        Self {
            a,
            radius,
            pass,
            inner: inner.min(pass),
            speed,
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

    /// The most speed along path `k` the point at `rel` allows while the
    /// vehicle still closes at `closing` over the FC's lag; `None` when the
    /// path passes the point no nearer than [`Sweep::pass`] or ends before
    /// it enters the point's keep-out disk.
    ///
    /// A path coming nearer than [`Sweep::inner`] would bring the vehicle
    /// inside the margin: it stops short of the keep-out disk
    /// ([`super::room_cap`]). One passing between [`Sweep::inner`] and
    /// [`Sweep::pass`] keeps the margin all along and enters only the creep
    /// slack's band: its speed is held to a share of [`Sweep::speed`] that
    /// rises with its passing distance across that band, from
    /// [`PASS_FLOOR_SHARE`] at `inner` to all of it at `pass`, never below
    /// the stop short of the keep-out. A corridor the vehicle barely fits
    /// slows it in proportion instead of stopping it, and no stop along
    /// such a path ends inside the margin.
    pub(super) fn cap(&self, k: usize, rel: V2, closing: f32) -> Option<f32> {
        let (e, len) = self.paths[k];
        let along = geo::dot(rel, e);
        let perp = (rel[0] * e[1] - rel[1] * e[0]).abs();
        if along <= 0.0 || perp >= self.pass {
            return None;
        }
        let room = along - (self.radius * self.radius - perp * perp).sqrt();
        if room >= len {
            return None;
        }
        let stop = super::room_cap(room.max(0.0), closing, self.a);
        let band = self.pass - self.inner;
        if perp < self.inner || band <= 1e-6 {
            return Some(stop);
        }
        let share = (perp - self.inner) / band;
        let floor = PASS_FLOOR_SHARE;
        Some(stop.max(self.speed * (floor + (1.0 - floor) * share)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_passed_inside_the_creep_slack_slows_the_path_in_proportion() {
        // Keep-out 1.705 m, margin from the cell's near face 1.625 m, flying
        // north at 3 m/s with a point just ahead on the left.
        let mut s = Sweep::new(3.0, 1.705, 1.705, 1.625, 3.0);
        assert!(s.add([3.0, 0.0], 3.0));
        let at = |perp: f32| s.cap(0, [0.3, -perp], 3.0);
        // Passed beyond the keep-out: no cap.
        assert_eq!(at(1.71), None);
        // Passed keeping the margin, inside the creep slack: a share of the
        // speed rising across the band from half at the margin's edge.
        let c = at(1.68).expect("caps");
        let share = 0.055 / 0.08;
        assert!((c - 3.0 * (0.5 + 0.5 * share)).abs() < 1e-3, "{c}");
        let edge = at(1.626).expect("caps");
        assert!(edge < c && edge > 1.5, "{edge}");
        // Passed inside the margin: stopped short of the keep-out, which
        // this point's lies across the path at once.
        assert_eq!(at(1.6), Some(0.0));
    }
}
