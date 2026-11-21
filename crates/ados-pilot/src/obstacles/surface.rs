//! The nearest surface the current scan returned, and how wide it is.

use super::{Obstacles, SURFACE_GAP_M};
use crate::geo::{self, V2};

impl Obstacles {
    /// The current scan's nearest non-floor return as (horizontal range,
    /// local NE position, extent). The extent is the length of the unbroken
    /// run of returns through it: walking outward by bearing from the
    /// nearest return until neighbours are more than [`SURFACE_GAP_M`]
    /// apart. A wall, a car or a room around the vehicle reads metres long;
    /// a pole, a person, a drone, or several of them apart, read short.
    pub fn nearest_surface(&self) -> Option<(f32, V2, f32)> {
        let n = self.hits.len();
        let (i0, near) = self
            .hits
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.range_m.total_cmp(&b.1.range_m))?;
        // The hits are in bearing order and the scan wraps at the nose:
        // walk `step` (1 or n - 1) at most `max` neighbours, returning the
        // run's length and how many it took.
        let walk = |step: usize, max: usize| {
            let (mut at, mut len, mut taken) = (i0, 0.0, 0);
            while taken < max {
                let next = (at + step) % n;
                let gap = geo::dist(self.hits[at].ne, self.hits[next].ne);
                if gap > SURFACE_GAP_M {
                    break;
                }
                len += gap;
                taken += 1;
                at = next;
            }
            (len, taken)
        };
        let (fwd, taken) = walk(1, n - 1);
        let (back, _) = walk(n - 1, n - 1 - taken);
        Some((near.range_m, near.ne, fwd + back))
    }
}
