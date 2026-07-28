//! The control laws' view of a neighbour, and the allocation-free nearest-set
//! search every law shares.
//!
//! [`NeighborState`] is deliberately NOT `ados_swarmbus::Neighbor`: it is that
//! type projected into the local NED frame and stripped of transport concerns
//! (receive time, RSSI, the wire's fixed-point scaling). Keeping the laws on
//! this type is what makes every one of them a pure function testable on a
//! laptop with no radio, no socket and no clock.

use crate::geo::Ned;

// The beacon `status` condition bits, re-exported from the transport that defines
// them. Restating the bit positions here would be a second copy of a wire format
// this crate does not own — and the first time the two disagreed, a drone would
// read a neighbour's `armed` flag as its `guided` one.
pub use ados_swarmbus::beacon::{
    STATUS_ARMED, STATUS_EMERGENCY, STATUS_GPS_OK, STATUS_GUIDED, STATUS_HERO,
};

/// A neighbour as the control laws see it: a slot, a position and velocity in
/// the local NED frame, and the raw beacon status byte.
///
/// `pos` is dead-reckoned forward from the last beacon by the caller
/// (`NeighborTable::predicted`), which is why a 2 Hz beacon feeds a 10 Hz
/// control loop without the loop ever seeing a staircase.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NeighborState {
    /// Fleet slot, `1..=FLEET_MAX_SLOTS`. Unique within a fleet.
    pub slot: u8,
    /// Offset from this drone, metres NED.
    pub pos: Ned,
    /// Velocity, metres/second NED (down-positive, MAVLink convention).
    pub vel: Ned,
    /// The beacon status byte verbatim. Flag bits 0..4 are read through the
    /// helpers below; bits 5..7 are the mode-precedence field
    /// ([`crate::ModePrecedence::from_status_bits`]).
    pub status: u8,
}

impl NeighborState {
    pub const fn new(slot: u8, pos: Ned, vel: Ned, status: u8) -> Self {
        Self {
            slot,
            pos,
            vel,
            status,
        }
    }

    pub const fn armed(&self) -> bool {
        self.status & STATUS_ARMED != 0
    }

    pub const fn guided(&self) -> bool {
        self.status & STATUS_GUIDED != 0
    }

    pub const fn emergency(&self) -> bool {
        self.status & STATUS_EMERGENCY != 0
    }

    pub const fn gps_ok(&self) -> bool {
        self.status & STATUS_GPS_OK != 0
    }

    pub const fn hero(&self) -> bool {
        self.status & STATUS_HERO != 0
    }

    /// 3-D distance from this drone, metres.
    pub fn distance(&self) -> f64 {
        self.pos.norm()
    }
}

/// Upper bound on the neighbour count any single law weights.
///
/// Both `k` values the plan sets are 7 (`SEPARATION_NEIGHBORS`,
/// `FLOCK_NEIGHBORS`), and Dronisos flew N=10 outdoor flocking weighting just
/// two, so 16 is already double the generous setting. The cap exists so
/// [`NearestSet`] is a fixed-size stack value and the 10 Hz control loop never
/// allocates: a heap allocation per tick per law, times 24 drones, is pure
/// waste on an SBC that also runs the video encoder.
pub const MAX_NEAREST: usize = 16;

/// The `k` nearest neighbours within a radius, nearest first, held inline.
///
/// Stores INDICES into the caller's slice rather than references or copies, so
/// one search feeds several laws (cohesion, alignment and repulsion all want the
/// same set) without re-searching and without a borrow that pins the slice.
#[derive(Debug, Clone, Copy)]
pub struct NearestSet {
    idx: [u8; MAX_NEAREST],
    dist: [f64; MAX_NEAREST],
    len: u8,
}

impl Default for NearestSet {
    fn default() -> Self {
        Self {
            idx: [0; MAX_NEAREST],
            dist: [0.0; MAX_NEAREST],
            len: 0,
        }
    }
}

impl NearestSet {
    /// The `k` nearest of `neighbors` strictly inside `radius`, ordered nearest
    /// first.
    ///
    /// `k` is clamped to [`MAX_NEAREST`]; a `radius` that is not finite or is
    /// non-positive yields an empty set. Insertion sort over at most `k` slots
    /// beats a full sort here: `k` is 7 and the candidate list is at most 64, so
    /// this is ~64 comparisons and no allocation, against a 64-element sort plus
    /// a `Vec`.
    ///
    /// Ties break on the lower slot number, so two neighbours at exactly equal
    /// range produce the same set on every drone — a swarm where two aircraft
    /// disagree about who their neighbours are is a swarm with no lattice.
    pub fn build(neighbors: &[NeighborState], radius: f64, k: usize) -> Self {
        let mut out = Self::default();
        let k = k.min(MAX_NEAREST).min(u8::MAX as usize);
        if k == 0 || !radius.is_finite() || radius <= 0.0 {
            return out;
        }
        let r_sq = radius * radius;
        for (i, n) in neighbors.iter().enumerate().take(u8::MAX as usize) {
            let d_sq = n.pos.norm_sq();
            if !d_sq.is_finite() || d_sq >= r_sq {
                continue;
            }
            let len = out.len as usize;
            // Rank against the entries already held. A candidate is better than an
            // entry when it is nearer, or exactly as near with a lower slot — the
            // tie-break has to be part of the RANKING, not a separate check, or a
            // full set rejects an equidistant lower slot that belongs in it.
            let mut at = len;
            for p in 0..len {
                let better = d_sq < out.dist[p]
                    || (d_sq == out.dist[p] && n.slot < neighbors[out.idx[p] as usize].slot);
                if better {
                    at = p;
                    break;
                }
            }
            if at >= k {
                continue;
            }
            // Shift the tail down, dropping the last entry when already full.
            let end = len.min(k - 1);
            for j in (at..end).rev() {
                out.idx[j + 1] = out.idx[j];
                out.dist[j + 1] = out.dist[j];
            }
            out.idx[at] = i as u8;
            out.dist[at] = d_sq;
            if len < k {
                out.len += 1;
            }
        }
        // Squared distances were carried through the search to avoid a sqrt per
        // candidate; the laws want metres.
        for j in 0..out.len as usize {
            out.dist[j] = out.dist[j].sqrt();
        }
        out
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `(index into the caller's slice, distance in metres)`, nearest first.
    pub fn iter(&self) -> impl Iterator<Item = (usize, f64)> + '_ {
        (0..self.len as usize).map(move |j| (self.idx[j] as usize, self.dist[j]))
    }

    /// The nearest entry, if any.
    pub fn closest(&self) -> Option<(usize, f64)> {
        (self.len > 0).then(|| (self.idx[0] as usize, self.dist[0]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(slot: u8, n: f64, e: f64, d: f64) -> NeighborState {
        NeighborState::new(slot, Ned::new(n, e, d), Ned::ZERO, 0)
    }

    #[test]
    fn status_helpers_read_the_contract_bits() {
        let all = at(1, 0.0, 0.0, 0.0);
        assert!(!all.armed() && !all.guided() && !all.emergency());
        let n = NeighborState::new(1, Ned::ZERO, Ned::ZERO, 0b0001_1111);
        assert!(n.armed() && n.guided() && n.emergency() && n.gps_ok() && n.hero());
        // Each bit is independent: an armed drone with no GPS must not read as
        // gps_ok just because armed is set.
        let armed_only = NeighborState::new(1, Ned::ZERO, Ned::ZERO, STATUS_ARMED);
        assert!(armed_only.armed());
        assert!(!armed_only.gps_ok() && !armed_only.hero() && !armed_only.emergency());
        // The precedence field must not leak into a flag.
        let hi = NeighborState::new(1, Ned::ZERO, Ned::ZERO, 0b1110_0000);
        assert!(!hi.armed() && !hi.guided() && !hi.emergency() && !hi.gps_ok() && !hi.hero());
    }

    #[test]
    fn nearest_orders_by_distance_and_honours_k() {
        let ns = vec![
            at(3, 30.0, 0.0, 0.0),
            at(1, 10.0, 0.0, 0.0),
            at(2, 20.0, 0.0, 0.0),
            at(4, 5.0, 0.0, 0.0),
        ];
        let set = NearestSet::build(&ns, 100.0, 3);
        let got: Vec<u8> = set.iter().map(|(i, _)| ns[i].slot).collect();
        assert_eq!(got, vec![4, 1, 2]);
        let dists: Vec<f64> = set.iter().map(|(_, d)| d).collect();
        assert!((dists[0] - 5.0).abs() < 1e-12);
        assert!((dists[2] - 20.0).abs() < 1e-12);
        assert_eq!(set.closest().map(|(i, _)| ns[i].slot), Some(4));
    }

    #[test]
    fn radius_is_exclusive_and_excludes_the_far_field() {
        let ns = vec![
            at(1, 7.999, 0.0, 0.0),
            at(2, 8.0, 0.0, 0.0),
            at(3, 8.001, 0.0, 0.0),
        ];
        let set = NearestSet::build(&ns, 8.0, 7);
        let got: Vec<u8> = set.iter().map(|(i, _)| ns[i].slot).collect();
        assert_eq!(got, vec![1], "a neighbour exactly at the radius is outside");
    }

    #[test]
    fn distance_is_three_dimensional() {
        let ns = vec![at(1, 3.0, 4.0, 12.0)];
        let set = NearestSet::build(&ns, 14.0, 1);
        let (_, d) = set.closest().expect("inside 14 m");
        assert!((d - 13.0).abs() < 1e-12, "3-4-12 is 13, not 5");
        // Purely vertical separation still counts: a drone 5 m directly below is
        // 5 m away, and a horizontal-only metric would call it a collision.
        let below = vec![at(1, 0.0, 0.0, 5.0)];
        assert!(NearestSet::build(&below, 8.0, 1).closest().is_some());
    }

    #[test]
    fn ties_break_on_the_lower_slot_so_every_drone_agrees() {
        let ns = vec![
            at(9, 10.0, 0.0, 0.0),
            at(2, 0.0, 10.0, 0.0),
            at(5, 0.0, 0.0, 10.0),
        ];
        let set = NearestSet::build(&ns, 50.0, 2);
        let got: Vec<u8> = set.iter().map(|(i, _)| ns[i].slot).collect();
        assert_eq!(got, vec![2, 5]);
        // Same three neighbours presented in a different order: same answer.
        let shuffled = vec![ns[2], ns[0], ns[1]];
        let set2 = NearestSet::build(&shuffled, 50.0, 2);
        let got2: Vec<u8> = set2.iter().map(|(i, _)| shuffled[i].slot).collect();
        assert_eq!(got2, vec![2, 5]);
    }

    #[test]
    fn k_is_clamped_and_degenerate_inputs_are_empty() {
        let ns: Vec<NeighborState> = (1..=40).map(|s| at(s, s as f64, 0.0, 0.0)).collect();
        let set = NearestSet::build(&ns, 1000.0, 999);
        assert_eq!(set.len(), MAX_NEAREST);
        let got: Vec<u8> = set.iter().map(|(i, _)| ns[i].slot).collect();
        assert_eq!(got, (1..=MAX_NEAREST as u8).collect::<Vec<_>>());

        assert!(NearestSet::build(&ns, 1000.0, 0).is_empty());
        assert!(NearestSet::build(&ns, 0.0, 7).is_empty());
        assert!(NearestSet::build(&ns, -1.0, 7).is_empty());
        assert!(NearestSet::build(&ns, f64::NAN, 7).is_empty());
        assert!(NearestSet::build(&[], 10.0, 7).is_empty());
    }

    #[test]
    fn a_non_finite_neighbour_position_is_skipped_not_propagated() {
        let ns = vec![at(1, f64::NAN, 0.0, 0.0), at(2, 3.0, 0.0, 0.0)];
        let set = NearestSet::build(&ns, 10.0, 7);
        let got: Vec<u8> = set.iter().map(|(i, _)| ns[i].slot).collect();
        assert_eq!(got, vec![2]);
    }

    #[test]
    fn full_set_rejects_a_farther_candidate_without_disturbing_order() {
        let ns = vec![
            at(1, 1.0, 0.0, 0.0),
            at(2, 2.0, 0.0, 0.0),
            at(3, 9.0, 0.0, 0.0),
            at(4, 3.0, 0.0, 0.0),
        ];
        let set = NearestSet::build(&ns, 100.0, 3);
        let got: Vec<u8> = set.iter().map(|(i, _)| ns[i].slot).collect();
        assert_eq!(
            got,
            vec![1, 2, 4],
            "slot 3 at 9 m displaced by slot 4 at 3 m"
        );
    }
}
