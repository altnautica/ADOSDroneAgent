//! The mode-precedence ladder.
//!
//! # Where the type lives
//!
//! [`ModePrecedence`] is WIRE FORMAT: it rides beacon `status` bits 5..7, so it
//! belongs to the transport (`ados_swarmbus::precedence`) and is merely
//! re-exported here. This crate already depends on `ados-swarmbus` for the
//! neighbour table, so owning the enum here would be a cargo cycle — and more to
//! the point, the bit layout is what one aircraft tells another about itself, so
//! the crate that puts bytes on the air is the one that gets to define it.
//!
//! What this module owns is the LADDER — [`precedence_rank`] and [`arbitrate`] —
//! because priority is a control-law property, not a wire one.
//!
//! # Why rank is a function and not the discriminant
//!
//! The discriminants are chosen so `Hold = 0`: a drone with no autonomy running
//! leaves the precedence field at `000`, which is exactly what a pre-Phase-5 node
//! emits, so `mode_precedence` reads `"hold"` fleet-wide and truthfully until this
//! runtime starts setting it. That makes the discriminant order (`Hold`,
//! `HardSeparation`, `Operator`, `Formation`, `Flocking`) unrelated to the
//! priority order — `Flocking = 4` must never outrank `HardSeparation = 1`.
//! `ModePrecedence` therefore deliberately implements no `Ord`, and nothing in
//! this crate compares its discriminants.

pub use ados_swarmbus::precedence::ModePrecedence;

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
/// [`ModePrecedence::Hold`] — nothing active means nothing is commanding.
pub fn arbitrate(active: impl IntoIterator<Item = ModePrecedence>) -> ModePrecedence {
    active
        .into_iter()
        .min_by_key(|l| precedence_rank(*l))
        .unwrap_or(ModePrecedence::Hold)
}

#[cfg(test)]
mod tests {
    use super::*;

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
