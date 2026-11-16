//! The flown path, as the FC's SmartRTL keeps it: where a held return goes
//! next. A return retraces the path flown out, so the way it resumes along
//! is back down that path, not the straight line home (in a bore that runs
//! into the rock at the first bend).

use crate::geo::{self, V2};

/// A point is kept once the vehicle is this far from the last one, metres.
pub const TRAIL_STEP_M: f32 = 0.25;
/// The point a held return heads for lies at least this far back along the
/// path from the vehicle, metres: past the cell-size noise of a probe.
pub const TRAIL_AHEAD_M: f32 = 1.0;
/// Points kept: 1 km of flight; the oldest go first.
const TRAIL_MAX: usize = 4000;

#[derive(Debug, Default, Clone)]
pub struct Trail {
    pts: Vec<V2>,
}

impl Trail {
    pub fn clear(&mut self) {
        self.pts.clear();
    }

    /// Keep `pos` once it is [`TRAIL_STEP_M`] from the last point kept.
    pub fn record(&mut self, pos: V2) {
        if self
            .pts
            .last()
            .is_some_and(|&p| geo::dist(p, pos) < TRAIL_STEP_M)
        {
            return;
        }
        if self.pts.len() == TRAIL_MAX {
            self.pts.drain(..TRAIL_MAX / 4);
        }
        self.pts.push(pos);
    }

    /// A return retraces the path: drop what lies past the point nearest
    /// `pos`, as SmartRTL drops the points it has flown back over, so a later
    /// flight out continues the path from where the return left it.
    pub fn retrace(&mut self, pos: V2) {
        if let Some(k) = self.nearest(pos) {
            self.pts.truncate(k + 1);
        }
    }

    fn nearest(&self, pos: V2) -> Option<usize> {
        (0..self.pts.len())
            .min_by(|&a, &b| geo::dist(self.pts[a], pos).total_cmp(&geo::dist(self.pts[b], pos)))
    }

    /// Where a return from `pos` heads next: from the path's point nearest
    /// `pos`, the first point back along it at least [`TRAIL_AHEAD_M`] away.
    /// `None` with no such point (no path kept, or `pos` near its start).
    pub fn back_from(&self, pos: V2) -> Option<V2> {
        let k = self.nearest(pos)?;
        self.pts[..=k]
            .iter()
            .rev()
            .find(|&&p| geo::dist(p, pos) >= TRAIL_AHEAD_M)
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// North 10 m, then east 10 m: an L, as a bore with one bend.
    fn ell() -> Trail {
        let mut t = Trail::default();
        for k in 0..=100 {
            t.record([k as f32 * 0.1, 0.0]);
        }
        for k in 1..=100 {
            t.record([10.0, k as f32 * 0.1]);
        }
        t
    }

    #[test]
    fn a_point_every_step() {
        let t = ell();
        for w in t.pts.windows(2) {
            let d = geo::dist(w[0], w[1]);
            assert!((TRAIL_STEP_M..0.36).contains(&d), "{d}");
        }
    }

    #[test]
    fn the_way_back_follows_the_path_round_the_bend_not_the_line_home() {
        let t = ell();
        // At the end of the east leg: back west along it, not south-west
        // toward home.
        let p = t.back_from([10.0, 10.0]).expect("a point back");
        assert!(
            (p[0] - 10.0).abs() < 1e-3 && (p[1] - 9.0).abs() < 0.31,
            "{p:?}"
        );
        // Just past the bend: back round it, south along the north leg.
        let p = t.back_from([10.0, 0.5]).expect("a point back");
        assert!(p[1].abs() < 1e-3 && p[0] < 9.2, "{p:?}");
        // Beside the start: nothing further back.
        assert_eq!(t.back_from([0.3, 0.0]), None);
        assert_eq!(Trail::default().back_from([1.0, 1.0]), None);
    }

    #[test]
    fn a_return_cut_short_leaves_the_path_ending_where_it_stopped() {
        // Out along the L, back to (10, 4), then out again from there and a
        // return held beside the restart: back west toward the bend, not to
        // the far end of the first flight out.
        let mut t = ell();
        for k in 0..=60 {
            t.retrace([10.0, 10.0 - k as f32 * 0.1]);
        }
        t.record([10.0, 4.3]);
        let p = t.back_from([10.0, 4.3]).expect("a point back");
        assert!(p[1] < 3.4 && p[1] > 2.9, "{p:?}");
    }
}
