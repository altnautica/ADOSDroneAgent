//! The subject as an obstacle to a route: a person on the ground is kept
//! clear by the subject brake whatever the skill asks, and a route the
//! planner makes keeps the same distance from them, so a detour never
//! takes the vehicle round a trunk and over the person standing beside it.

use super::Narrows;
use crate::geo::{self, V2};
use crate::obstacles::CELL_M;

/// What a route keeps out of beyond the grid's obstacles: the narrows a
/// stall marked, and the subject.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Bars<'a> {
    pub narrows: Option<&'a Narrows>,
    pub subject: Option<Keepout>,
}

impl Bars<'_> {
    /// The clearance at `p`, whose grid clearance is `c`, counting the
    /// subject's disk.
    pub fn clearance(&self, p: V2, c: f32) -> f32 {
        self.subject.map_or(c, |k| c.min(k.clearance(p)))
    }
}

/// The subject as a disk a route keeps its margin from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keepout {
    ne: V2,
    r: f32,
}

impl Keepout {
    /// The subject at `ne`, kept `keep` metres off (centre to the vehicle's
    /// centre), as a disk that a route keeping `margin` clearance stays
    /// `keep` from. A vehicle at `from` already nearer than that keeps the
    /// margin and a cell from a disk shrunk to fit: a route still leaves
    /// from where it is (the subject brake pushes it out), and never
    /// closes in further. `None` for a subject that is not a number.
    pub fn new(ne: V2, keep: f32, margin: f32, from: V2) -> Option<Self> {
        if !(ne.iter().all(|x| x.is_finite()) && keep.is_finite()) {
            return None;
        }
        let fits = geo::dist(from, ne) - margin - CELL_M;
        let r = (keep - margin).min(fits).max(0.0);
        Some(Self { ne, r })
    }

    /// Clearance from `p` to the disk, metres.
    pub fn clearance(&self, p: V2) -> f32 {
        (geo::dist(p, self.ne) - self.r).max(0.0)
    }

    /// True when every point of `a → b` keeps `margin` clearance from the
    /// disk.
    pub fn segment_clear(&self, a: V2, b: V2, margin: f32) -> bool {
        let ab = geo::sub(b, a);
        let l2 = geo::dot(ab, ab);
        let t = if l2 > 1e-9 {
            (geo::dot(geo::sub(self.ne, a), ab) / l2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.clearance(geo::add(a, geo::scale(ab, t))) >= margin
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disk_keeps_the_kept_distance_and_shrinks_round_a_vehicle_inside_it() {
        let k = Keepout::new([0.0, 0.0], 2.5, 0.5, [10.0, 0.0]).unwrap();
        // The margin from the disk is the kept distance from the subject.
        assert!((k.clearance([2.5, 0.0]) - 0.5).abs() < 1e-5);
        assert!(!k.segment_clear([-5.0, 1.0], [5.0, 1.0], 0.5));
        assert!(k.segment_clear([-5.0, 2.6], [5.0, 2.6], 0.5));
        // A vehicle 1.5 m off still has its margin and a cell where it is.
        let k = Keepout::new([0.0, 0.0], 2.5, 0.5, [1.5, 0.0]).unwrap();
        assert!(k.clearance([1.5, 0.0]) >= 0.5 + CELL_M - 1e-5);
        assert!(Keepout::new([f32::NAN, 0.0], 2.5, 0.5, [1.5, 0.0]).is_none());
    }
}
