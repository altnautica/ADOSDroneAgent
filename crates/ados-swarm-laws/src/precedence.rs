//! The mode-precedence ladder, and the three-bit encoding of the active level
//! inside a beacon `status` byte.
//!
//! # Why the active level is broadcast
//!
//! Mode-transition ambiguity (an operator believing one mode governs a vehicle
//! while another actually does) is implicated in a long series of
//! supervisory-control losses. A drone whose separation layer has taken over
//! must read `hard-separation` on the operator's screen, not the `formation` it
//! was commanded into. That is only truthful if the ACTIVE level rides on the
//! aircraft's own broadcast, so every reader (peer drone and ground station
//! alike) derives it from the same byte. [`ModePrecedence::as_status_bits`]
//! packs it into bits 5..7 (see [`crate::status`]).
//!
//! The ladder itself, [`precedence_rank`] and [`arbitrate`], is a control-law
//! property rather than a wire one.
//!
//! # Why rank is a function and not the discriminant
//!
//! The discriminants are chosen so `Hold = 0`: a drone with no autonomy running
//! leaves the precedence field at `000`, so every reader decodes `hold`
//! truthfully until a control layer starts setting it. That makes the
//! discriminant order (`Hold`, `HardSeparation`, `Operator`, `Formation`,
//! `Flocking`) unrelated to the priority order: `Flocking = 4` must never
//! outrank `HardSeparation = 1`. `ModePrecedence` therefore deliberately
//! implements no `Ord`, and nothing in this crate compares its discriminants.

use crate::status::{STATUS_PRECEDENCE_MASK, STATUS_PRECEDENCE_SHIFT};

/// The precedence level currently governing a vehicle, highest authority first
/// in the arbitration ladder but **NOT** in discriminant order.
///
/// The discriminants are wire values chosen so [`ModePrecedence::Hold`] is zero:
/// a node with no autonomy layer running leaves the beacon's precedence field at
/// `000` and every reader honestly decodes `hold`.
///
/// [`Ord`] is deliberately NOT derived. The arbitration order is
/// `HardSeparation > Operator > Formation > Flocking > Hold`, which is not the
/// discriminant order. Code that needs the ladder ranks it explicitly with
/// [`precedence_rank`]; comparing these values numerically is a bug, so the type
/// refuses to make it expressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ModePrecedence {
    /// No swarm layer is commanding the vehicle: the flight controller holds.
    /// Also what an unknown encoding decodes to, and the state of every drone
    /// with no autonomy layer running.
    #[default]
    Hold = 0,
    /// The separation layer has overridden everything else (a neighbour inside
    /// the hard-separation radius).
    HardSeparation = 1,
    /// An operator direct command is in force.
    Operator = 2,
    /// A named formation is driving the vehicle to its per-slot offset.
    Formation = 3,
    /// Cohesion / alignment / separation flocking is driving the vehicle.
    Flocking = 4,
}

impl ModePrecedence {
    /// The level shifted into its `status` bit field, masked so it can be OR'd
    /// over a cleared field without touching the five condition bits.
    pub const fn as_status_bits(self) -> u8 {
        ((self as u8) << STATUS_PRECEDENCE_SHIFT) & STATUS_PRECEDENCE_MASK
    }

    /// Decode the level out of a full `status` byte.
    ///
    /// The three unassigned encodings (5, 6, 7) decode to [`Self::Hold`] rather
    /// than panicking or erroring: a beacon from a newer sender that has learned
    /// a sixth level must still be read for its position and its condition bits,
    /// and `hold` claims no authority this reader can act on.
    pub const fn from_status_bits(status: u8) -> Self {
        match (status & STATUS_PRECEDENCE_MASK) >> STATUS_PRECEDENCE_SHIFT {
            1 => Self::HardSeparation,
            2 => Self::Operator,
            3 => Self::Formation,
            4 => Self::Flocking,
            _ => Self::Hold,
        }
    }

    /// The stable wire string. Operator surfaces can type this as a closed
    /// union, so these five spellings are a contract.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::HardSeparation => "hard-separation",
            Self::Operator => "operator",
            Self::Formation => "formation",
            Self::Flocking => "flocking",
            Self::Hold => "hold",
        }
    }

    /// Parse the wire string back to a level: the exact inverse of
    /// [`Self::as_wire`], kept beside it so the five spellings live in one place.
    ///
    /// An unrecognised or empty string decodes to [`Self::Hold`], for the same
    /// reason [`Self::from_status_bits`] does.
    pub fn from_wire(s: &str) -> Self {
        match s {
            "hard-separation" => Self::HardSeparation,
            "operator" => Self::Operator,
            "formation" => Self::Formation,
            "flocking" => Self::Flocking,
            _ => Self::Hold,
        }
    }

    /// Every level, in arbitration order highest-authority first. The control
    /// ladder and any operator-facing legend both read this so the two can never
    /// disagree about the order.
    pub const ARBITRATION_ORDER: [Self; 5] = [
        Self::HardSeparation,
        Self::Operator,
        Self::Formation,
        Self::Flocking,
        Self::Hold,
    ];
}

/// Priority of a level, 0 = highest. See the module docs for why this is not the
/// discriminant.
pub fn precedence_rank(level: ModePrecedence) -> usize {
    ModePrecedence::ARBITRATION_ORDER
        .iter()
        .position(|c| *c == level)
        // Unreachable: `ARBITRATION_ORDER` is exhaustive over the enum, and the
        // `arbitration_order_enumerates_every_variant_once` test pins that.
        // Falling back to the lowest rank rather than panicking keeps a future
        // variant from aborting the control loop mid-flight.
        .unwrap_or(ModePrecedence::ARBITRATION_ORDER.len())
}

/// The winner among the levels a tick found active. Empty input is
/// [`ModePrecedence::Hold`] - nothing active means nothing is commanding.
pub fn arbitrate(active: impl IntoIterator<Item = ModePrecedence>) -> ModePrecedence {
    active
        .into_iter()
        .min_by_key(|l| precedence_rank(*l))
        .unwrap_or(ModePrecedence::Hold)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{STATUS_ARMED, STATUS_HERO};

    /// `from_wire` is the exact inverse of `as_wire`, and a level this build does
    /// not know (a newer peer's sixth level, or an empty string) reads as `hold`.
    #[test]
    fn from_wire_inverts_as_wire_and_degrades_unknown_to_hold() {
        for level in ModePrecedence::ARBITRATION_ORDER {
            assert_eq!(ModePrecedence::from_wire(level.as_wire()), level);
        }
        assert_eq!(
            ModePrecedence::from_wire("murmuration"),
            ModePrecedence::Hold
        );
        assert_eq!(ModePrecedence::from_wire(""), ModePrecedence::Hold);
    }

    /// Every level survives the three-bit round trip, and each occupies a
    /// distinct encoding: a duplicated discriminant would silently merge two
    /// levels on the wire.
    #[test]
    fn every_level_round_trips_through_the_status_field() {
        let mut seen = std::collections::BTreeSet::new();
        for level in ModePrecedence::ARBITRATION_ORDER {
            let bits = level.as_status_bits();
            assert_eq!(bits & !STATUS_PRECEDENCE_MASK, 0, "{level:?} leaked a bit");
            assert!(seen.insert(bits), "{level:?} duplicates another encoding");
            assert_eq!(ModePrecedence::from_status_bits(bits), level);
            assert_eq!(
                ModePrecedence::from_status_bits(bits | STATUS_ARMED | STATUS_HERO),
                level
            );
        }
        assert_eq!(seen.len(), 5);
    }

    /// A newer sender using encoding 5/6/7 must be readable, not fatal, and must
    /// not be mistaken for an authority level this reader would act on.
    #[test]
    fn unassigned_encodings_degrade_to_hold() {
        for raw in [5u8, 6, 7] {
            let status = raw << STATUS_PRECEDENCE_SHIFT;
            assert_eq!(
                ModePrecedence::from_status_bits(status),
                ModePrecedence::Hold,
                "encoding {raw} must degrade to hold"
            );
        }
    }

    /// The ladder is hard-separation first and hold last.
    #[test]
    fn the_arbitration_ladder_is_highest_authority_first() {
        assert_eq!(
            ModePrecedence::ARBITRATION_ORDER.map(|l| l.as_wire()),
            [
                "hard-separation",
                "operator",
                "formation",
                "flocking",
                "hold"
            ]
        );
    }

    const ALL: [ModePrecedence; 5] = [
        ModePrecedence::Hold,
        ModePrecedence::HardSeparation,
        ModePrecedence::Operator,
        ModePrecedence::Formation,
        ModePrecedence::Flocking,
    ];

    #[test]
    fn hold_is_the_zero_value_so_a_silent_node_is_truthful() {
        assert_eq!(ModePrecedence::default(), ModePrecedence::Hold);
        assert_eq!(ModePrecedence::Hold as u8, 0);
        // The load-bearing property: a node that never touches the field emits
        // all-zero precedence bits and still reads back as "hold".
        assert_eq!(ModePrecedence::Hold.as_status_bits(), 0);
        assert_eq!(ModePrecedence::from_status_bits(0), ModePrecedence::Hold);
    }

    #[test]
    fn wire_strings_are_exactly_the_contract() {
        assert_eq!(ModePrecedence::HardSeparation.as_wire(), "hard-separation");
        assert_eq!(ModePrecedence::Operator.as_wire(), "operator");
        assert_eq!(ModePrecedence::Formation.as_wire(), "formation");
        assert_eq!(ModePrecedence::Flocking.as_wire(), "flocking");
        assert_eq!(ModePrecedence::Hold.as_wire(), "hold");
    }

    #[test]
    fn status_bits_round_trip_without_touching_the_condition_bits() {
        // bits 0..4: armed | guided | emergency | gps_ok | hero, all set.
        let flags: u8 = 0b0001_1111;
        for level in ALL {
            let packed = flags | level.as_status_bits();
            assert_eq!(packed & 0b0001_1111, flags, "{level:?} clobbered a flag");
            assert_eq!(ModePrecedence::from_status_bits(packed), level);
        }
    }

    #[test]
    fn arbitration_order_enumerates_every_variant_once() {
        let mut seen = ModePrecedence::ARBITRATION_ORDER.to_vec();
        seen.sort_by_key(|l| *l as u8);
        seen.dedup();
        assert_eq!(seen.len(), 5, "ARBITRATION_ORDER must be a permutation");
        for level in ALL {
            assert!(ModePrecedence::ARBITRATION_ORDER.contains(&level));
        }
    }

    #[test]
    fn rank_is_the_ladder_and_not_the_discriminant() {
        assert_eq!(precedence_rank(ModePrecedence::HardSeparation), 0);
        assert_eq!(precedence_rank(ModePrecedence::Operator), 1);
        assert_eq!(precedence_rank(ModePrecedence::Formation), 2);
        assert_eq!(precedence_rank(ModePrecedence::Flocking), 3);
        assert_eq!(precedence_rank(ModePrecedence::Hold), 4);
        // The bug this guards: Flocking's discriminant (4) is the largest, so a
        // discriminant comparison would rank it above HardSeparation (1).
        assert!(
            precedence_rank(ModePrecedence::HardSeparation)
                < precedence_rank(ModePrecedence::Flocking)
        );
        assert!(
            (ModePrecedence::HardSeparation as u8) < (ModePrecedence::Flocking as u8),
            "discriminant order really is the trap this test describes"
        );
    }

    #[test]
    fn arbitrate_picks_the_correct_level_for_every_combination() {
        // Every non-empty subset of the five levels: the winner is always the one
        // with the lowest ladder rank, regardless of insertion order.
        for mask in 1u32..32 {
            let mut subset = Vec::new();
            for (bit, level) in ALL.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    subset.push(*level);
                }
            }
            let expect = *subset
                .iter()
                .min_by_key(|l| precedence_rank(**l))
                .expect("non-empty");
            assert_eq!(arbitrate(subset.clone()), expect, "subset {subset:?}");
            subset.reverse();
            assert_eq!(
                arbitrate(subset.clone()),
                expect,
                "order must not matter: {subset:?}"
            );
        }
    }

    #[test]
    fn arbitrate_of_nothing_is_hold() {
        assert_eq!(arbitrate(std::iter::empty()), ModePrecedence::Hold);
    }
}
