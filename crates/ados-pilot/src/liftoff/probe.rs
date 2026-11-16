//! Finding an airframe's trim on the ground (first flight, or an airframe
//! that changed): each time the airframe starts to tip during the thrust
//! ramp, the live rate-loop integrator on that axis bounds the trim on the
//! side it tipped, and the next try moves past it.
//!
//! The step scales with how far below hover thrust the tip began (a large
//! imbalance tips early, a small one only as the skids unload). The hover
//! thrust is an estimate that can be far off on a first flight, so a tip
//! the same way again at least doubles the last step. Once the trim is
//! bracketed from both sides the next try is the midpoint, so the search
//! converges instead of stepping back and forth across it.

/// The step per unit of thrust missing to hover at the tip onset.
pub const STEP_PER_MISSING: f32 = 1.5;
/// The smallest and largest step, in rate-loop output units.
pub const STEP_MIN: f32 = 0.02;
pub const STEP_MAX: f32 = 0.2;

/// The bracket on one axis.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Bracket {
    /// The trim is above this (the airframe tipped with this much, short).
    pub lo: Option<f32>,
    /// The trim is below this (the airframe tipped with this much, over).
    pub hi: Option<f32>,
    /// The last step, signed.
    last: f32,
}

impl Bracket {
    /// The next integrator target after a tip on this axis: `i_now` the
    /// live integrator at the onset, `tilt` the tilt from rest on this axis
    /// (rad, the sign of the tip), `thrust_frac` the thrust at the onset as a
    /// fraction of hover.
    pub fn next(&mut self, i_now: f32, tilt: f32, thrust_frac: f32) -> f32 {
        // A nose (or wing) dropping needs more integrator on that axis.
        let need = -tilt.signum();
        if need > 0.0 {
            self.lo = Some(self.lo.map_or(i_now, |v| v.max(i_now)));
        } else {
            self.hi = Some(self.hi.map_or(i_now, |v| v.min(i_now)));
        }
        let mut step = (STEP_PER_MISSING * (1.0 - thrust_frac)).clamp(STEP_MIN, STEP_MAX);
        if self.last * need > 0.0 {
            step = step.max((2.0 * self.last.abs()).min(STEP_MAX));
        }
        self.last = need * step;
        let target = i_now + need * step;
        match (self.lo, self.hi) {
            (Some(lo), Some(hi)) if !(lo < target && target < hi) => 0.5 * (lo + hi),
            _ => target,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_by_how_early_it_tipped_then_bisects() {
        let mut b = Bracket::default();
        // Nose drops at 86 % of hover with no trim: the largest step up.
        let t1 = b.next(0.0, -0.03, 0.86);
        assert!((t1 - STEP_MAX).abs() < 1e-4, "{t1}");
        // Nose rises at lift-off thrust with 0.30: bracketed, and the
        // smallest step down stays inside the bracket.
        let t2 = b.next(0.30, 0.03, 1.1);
        assert!((t2 - 0.28).abs() < 1e-4, "{t2}");
        assert_eq!(b.lo, Some(0.0));
        assert_eq!(b.hi, Some(0.30));
        // A drop at 0.28: a step up would leave the bracket, so the midpoint.
        let t3 = b.next(0.28, -0.03, 1.1);
        assert!((t3 - 0.29).abs() < 1e-4, "{t3}");
    }

    #[test]
    fn tipping_the_same_way_again_doubles_the_step() {
        // A hover estimate far too low reads every tip as at hover.
        let mut b = Bracket::default();
        let mut i = 0.0;
        let mut steps = Vec::new();
        for _ in 0..4 {
            let t = b.next(i, -0.03, 1.2);
            steps.push(((t - i) * 100.0).round() / 100.0);
            i = t;
        }
        assert_eq!(steps, vec![0.02, 0.04, 0.08, 0.16]);
    }
}
