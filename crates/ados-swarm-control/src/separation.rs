//! The separation safety layer.
//!
//! Two distinct mechanisms, and conflating them is the bug this module's shape
//! exists to prevent:
//!
//! * A SOFT repulsive potential inside [`SEPARATION_RADIUS_M`], summed over the
//!   `k` nearest neighbours and added to whatever the active behaviour layer
//!   commanded. It shapes the lattice; it does not guarantee anything.
//! * A HARD override inside [`SEPARATION_HARD_M`] that discards the horizontal
//!   solution entirely and commands hold plus a slot-indexed climb. This is the
//!   guarantee, and it outranks every other layer including a direct operator
//!   command.
//!
//! The layer runs whenever the vehicle is armed, regardless of the commanded
//! swarm mode. A drone in `hold` with a neighbour closing on it still gets out
//! of the way.

use crate::geo::Ned;
use crate::neighbor::{NearestSet, NeighborState};

/// Below this range the soft repulsive term applies.
pub const SEPARATION_RADIUS_M: f64 = 8.0;

/// Below this range the hard override takes the vehicle: hold horizontally,
/// climb to a slot-indexed offset, raise the emergency status bit.
pub const SEPARATION_HARD_M: f64 = 4.0;

/// Soft repulsion gain, m/s per unit of the `(1/d - 1/R)` potential gradient.
pub const SEPARATION_GAIN: f64 = 1.5;

/// How many nearest neighbours the repulsive sum weights.
pub const SEPARATION_NEIGHBORS: usize = 7;

/// Vertical rate the hard override climbs at, m/s.
pub const HARD_CLIMB_RATE_MPS: f64 = 1.0;

/// Altitude offset per fleet slot the hard override climbs to, metres.
///
/// The deconfliction rule is `slot * HARD_CLIMB_STEP_M`: strictly increasing in
/// slot, so two converging drones separate vertically by a rule both compute
/// identically from data both already have. No negotiation, no round trip, and
/// no dependence on which drone noticed first — which is exactly why it is safe
/// to run on a lossy broadcast medium.
pub const HARD_CLIMB_STEP_M: f64 = 0.5;

/// The separation gains, as configured. Defaults are the plan's constants; the
/// `swarm.separation.*` config block overrides the two radii.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeparationTuning {
    pub radius_m: f64,
    pub hard_m: f64,
    pub gain: f64,
    pub neighbors: usize,
}

impl Default for SeparationTuning {
    fn default() -> Self {
        Self {
            radius_m: SEPARATION_RADIUS_M,
            hard_m: SEPARATION_HARD_M,
            gain: SEPARATION_GAIN,
            neighbors: SEPARATION_NEIGHBORS,
        }
    }
}

impl SeparationTuning {
    /// Clamp an operator-supplied pair into a usable ordering.
    ///
    /// A `hard_m` at or above `radius_m` would mean the hard override engages
    /// before the soft term ever gets a chance to act, so the vehicle would jump
    /// straight from unaware to emergency hold. The UI gates edits to these two
    /// behind a confirm because they are the safety layer; this is the belt to
    /// that braces, since a config file can be edited by hand.
    pub fn sanitised(mut self) -> Self {
        if !self.radius_m.is_finite() || self.radius_m <= 0.0 {
            self.radius_m = SEPARATION_RADIUS_M;
        }
        if !self.hard_m.is_finite() || self.hard_m <= 0.0 {
            self.hard_m = SEPARATION_HARD_M;
        }
        if !self.gain.is_finite() || self.gain < 0.0 {
            self.gain = SEPARATION_GAIN;
        }
        if self.hard_m >= self.radius_m {
            self.radius_m = self.hard_m * 2.0;
        }
        if self.neighbors == 0 {
            self.neighbors = SEPARATION_NEIGHBORS;
        }
        self
    }
}

/// The nearest neighbour inside [`SeparationTuning::hard_m`] — the reason the
/// hard override is engaged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HardBreach {
    pub slot: u8,
    pub distance_m: f64,
    /// The offender's altitude offset from this drone, metres UP-positive. Needed
    /// because the climb datum has to be shared; see [`climb_datum_m`].
    pub relative_alt_m: f64,
}

/// The soft repulsive velocity contribution, m/s in the local NED frame.
///
/// `SEPARATION_GAIN * (1/d - 1/R)` per neighbour, along the unit vector pointing
/// AWAY from it, summed over the `k` nearest inside `R`. Neighbours at or beyond
/// `R` contribute exactly zero, so the term is continuous at the boundary.
pub fn repulsion(neighbors: &[NeighborState], t: &SeparationTuning) -> Ned {
    let set = NearestSet::build(neighbors, t.radius_m, t.neighbors);
    repulsion_over(neighbors, &set, t)
}

/// [`repulsion`] over an already-built nearest set. The flocking law searches
/// once at its own wider radius and then needs the repulsive term over the same
/// candidates, so it calls this rather than paying for a second search.
pub fn repulsion_over(neighbors: &[NeighborState], set: &NearestSet, t: &SeparationTuning) -> Ned {
    let inv_r = 1.0 / t.radius_m;
    let mut acc = Ned::ZERO;
    for (i, d) in set.iter() {
        if d >= t.radius_m || d <= f64::EPSILON {
            // At or beyond the radius the potential is zero; at zero range there
            // is no direction to push along and the hard override owns the case.
            continue;
        }
        let magnitude = t.gain * (1.0 / d - inv_r);
        // `pos` points from this drone TO the neighbour, so away is its negation.
        acc = acc + neighbors[i].pos.scale(-magnitude / d);
    }
    acc
}

/// The hard breach, if any: the nearest neighbour inside
/// [`SeparationTuning::hard_m`].
pub fn hard_breach(neighbors: &[NeighborState], t: &SeparationTuning) -> Option<HardBreach> {
    let set = NearestSet::build(neighbors, t.hard_m, 1);
    set.closest().map(|(i, d)| HardBreach {
        slot: neighbors[i].slot,
        distance_m: d,
        // `pos.d` is down-positive, so a neighbour above us has negative `d`.
        relative_alt_m: -neighbors[i].pos.d,
    })
}

/// Altitude this slot climbs above the offender, metres.
///
/// `HARD_CLIMB_STEP_M · (offender_slot − own_slot)` for the LOWER slot, and zero
/// for the higher one: of a converging pair, exactly one climbs.
///
/// # Why only one of them climbs
///
/// The rule has to produce a STATIONARY target or it does not terminate. Have
/// both vehicles climb `0.5 · slot` above their own altitude and each
/// re-engagement measures from a height the previous one created: measured, a pair
/// held on a collision course for ninety seconds ratchets almost forty metres
/// into the sky, one 0.5 m step per engagement, and never stops. Have both climb
/// above the higher of the two and the same ratchet appears through the maximum.
///
/// Pinning the climber to the HOLDER's altitude fixes it: the holder is not
/// moving vertically, so `offender_alt + 0.5·Δslot` is the same number on every
/// tick and every re-engagement. The climb happens once and then holds. Both
/// vehicles still compute it from data both already have — the beacon carries the
/// neighbour's slot and altitude — so it is still a deterministic rule needing no
/// negotiation, which is the property that makes it safe on a lossy broadcast.
pub fn climb_offset_m(own_slot: u8, offender_slot: u8) -> f64 {
    if own_slot >= offender_slot {
        0.0
    } else {
        HARD_CLIMB_STEP_M * (offender_slot - own_slot) as f64
    }
}

/// The altitude the hard override climbs to.
///
/// Three cases, and each one exists to kill a specific failure:
///
/// * The HIGHER slot holds. Exactly one of a pair manoeuvres, which is what makes
///   the climber's target stationary (see [`climb_offset_m`]).
/// * A lower slot already BELOW its offender holds too. It already has vertical
///   separation; climbing would spend it, and a vehicle that flies UP into the
///   aircraft it is avoiding is worse than one that holds. Measured: without this
///   guard a packed cluster of eight loses a third of its separation floor to
///   climb-throughs.
/// * Otherwise — level with, or above, the offender — climb to
///   [`climb_offset_m`] above the OFFENDER's altitude. Measured from the holder,
///   never from the climber's own rising altitude, so the number is the same on
///   every tick and every re-engagement.
///
/// `offender_relative_alt_m` is the offender's altitude minus this drone's,
/// up-positive, straight off [`HardBreach::relative_alt_m`].
pub fn hard_climb_target_m(
    own_alt_m: f64,
    own_slot: u8,
    offender_slot: u8,
    offender_relative_alt_m: f64,
) -> f64 {
    if own_slot >= offender_slot || offender_relative_alt_m > 0.0 {
        return own_alt_m;
    }
    own_alt_m + offender_relative_alt_m + climb_offset_m(own_slot, offender_slot)
}

/// The hard override's velocity command: no horizontal component at all, and a
/// climb at [`HARD_CLIMB_RATE_MPS`] until `alt_m` reaches `target_alt_m`.
///
/// Down-positive, so a climb is negative. Once the offset is reached the command
/// is a pure zero — the vehicle holds, it does not drift on up.
pub fn hard_override(alt_m: f64, target_alt_m: f64) -> Ned {
    if alt_m >= target_alt_m {
        return Ned::ZERO;
    }
    let remaining = target_alt_m - alt_m;
    // Taper inside the last tick of travel so the vehicle settles on the offset
    // instead of overshooting it at 1 m/s forever.
    let rate = HARD_CLIMB_RATE_MPS.min(remaining);
    Ned::new(0.0, 0.0, -rate)
}

/// Smallest pairwise 3-D distance in a set of positions, metres. `None` for
/// fewer than two positions. Used by the scenario harness to assert the
/// [`SEPARATION_HARD_M`] floor held for a whole run.
pub fn min_pairwise_distance(positions: &[Ned]) -> Option<f64> {
    let mut best: Option<f64> = None;
    for (i, a) in positions.iter().enumerate() {
        for b in &positions[i + 1..] {
            let d = (*a - *b).norm();
            if best.is_none_or(|m| d < m) {
                best = Some(d);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nbr(slot: u8, n: f64, e: f64, d: f64) -> NeighborState {
        NeighborState::new(slot, Ned::new(n, e, d), Ned::ZERO, 0)
    }

    #[test]
    fn repulsion_points_away_and_grows_as_range_closes() {
        let t = SeparationTuning::default();
        let mut last = 0.0;
        for d in [7.5, 6.0, 5.0, 4.5, 3.0, 1.0] {
            // Neighbour due north: the push must be due south.
            let f = repulsion(&[nbr(2, d, 0.0, 0.0)], &t);
            assert!(f.n < 0.0, "at {d} m the push is {f:?}, not southward");
            assert_eq!(f.e, 0.0);
            assert_eq!(f.d, 0.0);
            let m = f.norm();
            assert!(
                m > last,
                "magnitude must grow as range closes: {m} <= {last}"
            );
            last = m;
        }
    }

    #[test]
    fn repulsion_matches_the_plans_closed_form() {
        let t = SeparationTuning::default();
        let d = 5.0;
        let f = repulsion(&[nbr(2, 0.0, d, 0.0)], &t);
        let expect = SEPARATION_GAIN * (1.0 / d - 1.0 / SEPARATION_RADIUS_M);
        assert!(
            (f.norm() - expect).abs() < 1e-12,
            "{} vs {expect}",
            f.norm()
        );
        // Neighbour to the east, push to the west.
        assert!((f.e + expect).abs() < 1e-12);
    }

    #[test]
    fn repulsion_is_zero_at_and_beyond_the_radius() {
        let t = SeparationTuning::default();
        assert_eq!(
            repulsion(&[nbr(2, SEPARATION_RADIUS_M, 0.0, 0.0)], &t),
            Ned::ZERO
        );
        assert_eq!(repulsion(&[nbr(2, 40.0, 0.0, 0.0)], &t), Ned::ZERO);
        // Continuous at the boundary: just inside is a vanishing push, not a step.
        let just_in = repulsion(&[nbr(2, SEPARATION_RADIUS_M - 1e-6, 0.0, 0.0)], &t);
        assert!(just_in.norm() < 1e-6, "{}", just_in.norm());
    }

    #[test]
    fn repulsion_sums_over_neighbours_and_cancels_symmetric_pairs() {
        let t = SeparationTuning::default();
        // One neighbour 5 m north, one 5 m south: the pushes cancel exactly.
        let f = repulsion(&[nbr(2, 5.0, 0.0, 0.0), nbr(3, -5.0, 0.0, 0.0)], &t);
        assert!(f.norm() < 1e-12, "{f:?}");
        // Three on one side sum, they do not average.
        let one = repulsion(&[nbr(2, 5.0, 0.0, 0.0)], &t).norm();
        let three = repulsion(
            &[
                nbr(2, 5.0, 0.0, 0.0),
                nbr(3, 5.0, 0.001, 0.0),
                nbr(4, 5.0, -0.001, 0.0),
            ],
            &t,
        )
        .norm();
        assert!(three > 2.9 * one, "{three} vs {one}");
    }

    #[test]
    fn repulsion_weights_only_k_nearest() {
        let t = SeparationTuning {
            neighbors: 2,
            ..Default::default()
        };
        // Five neighbours all inside the radius, only the two nearest count.
        let ns: Vec<NeighborState> = (1..=5).map(|s| nbr(s, s as f64, 0.0, 0.0)).collect();
        let capped = repulsion(&ns, &t);
        let two_nearest = repulsion(&ns[..2], &t);
        assert!((capped.norm() - two_nearest.norm()).abs() < 1e-12);
    }

    #[test]
    fn a_coincident_neighbour_does_not_produce_a_nan_command() {
        let t = SeparationTuning::default();
        let f = repulsion(&[nbr(2, 0.0, 0.0, 0.0)], &t);
        assert!(f.is_finite(), "{f:?}");
        assert_eq!(
            f,
            Ned::ZERO,
            "no direction exists; the hard layer owns this"
        );
    }

    #[test]
    fn hard_breach_reports_the_nearest_offender_only_inside_the_hard_radius() {
        let t = SeparationTuning::default();
        assert_eq!(hard_breach(&[nbr(2, 4.0, 0.0, 0.0)], &t), None);
        assert_eq!(hard_breach(&[nbr(2, 4.001, 0.0, 0.0)], &t), None);
        let b = hard_breach(&[nbr(7, 3.5, 0.0, 0.0), nbr(9, 2.0, 0.0, 0.0)], &t)
            .expect("two inside the hard radius");
        assert_eq!(b.slot, 9, "the NEAREST offender, not the first seen");
        assert!((b.distance_m - 2.0).abs() < 1e-12);
        // Purely vertical: 3 m directly below is a breach.
        assert!(hard_breach(&[nbr(2, 0.0, 0.0, 3.0)], &t).is_some());
    }

    #[test]
    fn climb_offset_is_deterministic_and_monotone_in_the_slot_gap() {
        // Against a fixed offender, the offset grows monotonically as the gap in
        // slot numbers grows.
        let mut last = f64::NEG_INFINITY;
        for own in (1..=23u8).rev() {
            let o = climb_offset_m(own, 24);
            assert!(
                o > last,
                "own {own} offset {o} did not increase past {last}"
            );
            assert!((o - 0.5 * (24 - own) as f64).abs() < 1e-12);
            last = o;
        }
        // Deterministic: the same pair always yields the same answer, so both
        // drones compute it without exchanging a byte.
        assert_eq!(climb_offset_m(1, 3), climb_offset_m(1, 3));
        assert!(climb_offset_m(1, 2) < climb_offset_m(1, 3));
        // Exactly ONE of a pair climbs, and it is the lower slot.
        assert!(climb_offset_m(1, 2) > 0.0);
        assert_eq!(climb_offset_m(2, 1), 0.0);
        assert_eq!(
            climb_offset_m(5, 5),
            0.0,
            "a slot never deconflicts with itself"
        );
    }

    #[test]
    fn the_climb_target_is_stationary_so_a_pair_cannot_ratchet() {
        // Slots 1 and 4 level at 30 m. Slot 1 climbs 1.5 m above slot 4; slot 4
        // holds.
        let low = hard_climb_target_m(30.0, 1, 4, 0.0);
        let high = hard_climb_target_m(30.0, 4, 1, 0.0);
        assert!((low - 31.5).abs() < 1e-12, "{low}");
        assert_eq!(
            high, 30.0,
            "the higher slot holds, which is what fixes the datum"
        );

        // Re-evaluated once the climber has arrived, the answer is UNCHANGED. This
        // is the property whose absence ratchets a held pair forty metres upward
        // over ninety seconds.
        let again = hard_climb_target_m(31.5, 1, 4, 30.0 - 31.5);
        assert!((again - 31.5).abs() < 1e-12, "{again}");
        assert_eq!(hard_climb_target_m(30.0, 4, 1, 31.5 - 30.0), 30.0);

        // A climber already above its station is not commanded down.
        let above = hard_climb_target_m(40.0, 1, 4, 30.0 - 40.0);
        assert!((above - 31.5).abs() < 1e-12);
        assert_eq!(
            hard_override(40.0, above),
            Ned::ZERO,
            "no descent is commanded"
        );
    }

    #[test]
    fn hard_override_climbs_then_holds() {
        // Well below the target: full rate, negative down, zero horizontal.
        let v = hard_override(100.0, 103.0);
        assert_eq!(v.n, 0.0);
        assert_eq!(v.e, 0.0);
        assert!((v.d + HARD_CLIMB_RATE_MPS).abs() < 1e-12);
        // Inside the last metre: tapered, never overshooting.
        let near = hard_override(100.0, 100.4);
        assert!((near.d + 0.4).abs() < 1e-12);
        // Reached: a pure hold, not a lingering climb.
        assert_eq!(hard_override(103.0, 103.0), Ned::ZERO);
        assert_eq!(hard_override(110.0, 103.0), Ned::ZERO);
    }

    #[test]
    fn tuning_sanitises_an_inverted_or_absurd_pair() {
        // hard >= radius would skip the soft term entirely.
        let t = SeparationTuning {
            radius_m: 3.0,
            hard_m: 6.0,
            ..Default::default()
        }
        .sanitised();
        assert!(t.hard_m < t.radius_m, "{t:?}");
        assert_eq!(
            t.hard_m, 6.0,
            "the SAFETY radius is the one that is honoured"
        );

        let t = SeparationTuning {
            radius_m: f64::NAN,
            hard_m: -1.0,
            gain: f64::INFINITY,
            neighbors: 0,
        }
        .sanitised();
        assert_eq!(t.radius_m, SEPARATION_RADIUS_M);
        assert_eq!(t.hard_m, SEPARATION_HARD_M);
        assert_eq!(t.gain, SEPARATION_GAIN);
        assert_eq!(t.neighbors, SEPARATION_NEIGHBORS);
    }

    #[test]
    fn min_pairwise_distance_finds_the_closest_pair() {
        assert_eq!(min_pairwise_distance(&[]), None);
        assert_eq!(min_pairwise_distance(&[Ned::ZERO]), None);
        let ps = [
            Ned::new(0.0, 0.0, 0.0),
            Ned::new(10.0, 0.0, 0.0),
            Ned::new(10.0, 3.0, 0.0),
        ];
        assert!((min_pairwise_distance(&ps).unwrap() - 3.0).abs() < 1e-12);
    }
}
