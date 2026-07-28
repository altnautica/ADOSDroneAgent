//! The mode-precedence level a drone is actually being flown by, and its
//! three-bit encoding inside the beacon's `status` byte.
//!
//! This lives with the transport rather than with the autonomy layer because the
//! bit layout is **wire format**: it is what one aircraft tells another about
//! itself, and the onboard control crate consumes it rather than defining it.
//!
//! Why it is transmitted at all: mode-transition ambiguity — an operator
//! believing one mode governs a vehicle while another actually does — is
//! implicated in a long series of supervisory-control losses. A drone whose
//! separation layer has taken over MUST read `hard-separation` on the operator's
//! screen, not the `formation` it was commanded into. That is only truthful if
//! the *active* level rides on the aircraft's own broadcast, so every reader
//! (peer drone and ground station alike) derives it from the same byte.

/// The precedence level currently governing a vehicle, highest authority first
/// in the arbitration ladder but **NOT** in discriminant order.
///
/// The discriminants are wire values chosen so [`ModePrecedence::Hold`] is zero:
/// a node with no autonomy layer running leaves the beacon's precedence field at
/// `000` and every reader honestly decodes `hold`.
///
/// [`Ord`] is deliberately NOT derived. The arbitration order is
/// `HardSeparation > Operator > Formation > Flocking > Hold`, which is the
/// reverse of nothing in particular and certainly not the discriminant order —
/// `Flocking as u8 == 4` must never outrank `HardSeparation as u8 == 1`. Code
/// that needs the ladder ranks it explicitly; comparing these values numerically
/// is a bug, so the type refuses to make it expressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ModePrecedence {
    /// No swarm layer is commanding the vehicle: the flight controller holds.
    /// Also what an unknown encoding decodes to, and the pre-Phase-5 steady
    /// state of every drone in the fleet.
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
        ((self as u8) << crate::beacon::STATUS_PRECEDENCE_SHIFT)
            & crate::beacon::STATUS_PRECEDENCE_MASK
    }

    /// Decode the level out of a full `status` byte.
    ///
    /// The three unassigned encodings (5, 6, 7) decode to [`Self::Hold`] rather
    /// than panicking or erroring: a beacon from a newer agent that has learned a
    /// sixth level must still be read for its position and its condition bits,
    /// and `hold` is the honest reading of "this node is running a mode I do not
    /// know about" — it claims no authority this reader can act on.
    pub const fn from_status_bits(status: u8) -> Self {
        match (status & crate::beacon::STATUS_PRECEDENCE_MASK)
            >> crate::beacon::STATUS_PRECEDENCE_SHIFT
        {
            1 => Self::HardSeparation,
            2 => Self::Operator,
            3 => Self::Formation,
            4 => Self::Flocking,
            _ => Self::Hold,
        }
    }

    /// The GCS wire string. Stable: Mission Control's beacon store types this as
    /// a closed union, so these five spellings are a published contract.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::HardSeparation => "hard-separation",
            Self::Operator => "operator",
            Self::Formation => "formation",
            Self::Flocking => "flocking",
            Self::Hold => "hold",
        }
    }

    /// Parse the wire string back to a level — the exact inverse of
    /// [`Self::as_wire`], kept beside it so the five spellings live in ONE place. A
    /// consumer that matched on the strings from outside would be a second copy of
    /// the table, and the two would eventually disagree.
    ///
    /// An unrecognised or empty string decodes to [`Self::Hold`], for the same reason
    /// [`Self::from_status_bits`] does: a level this build does not know about claims
    /// no authority this build can act on, and `hold` is the honest reading of that.
    pub fn from_wire(s: &str) -> Self {
        match s {
            "hard-separation" => Self::HardSeparation,
            "operator" => Self::Operator,
            "formation" => Self::Formation,
            "flocking" => Self::Flocking,
            _ => Self::Hold,
        }
    }

    /// Every level, in arbitration order highest-authority first. The autonomy
    /// layer's ladder and the operator surface's legend both read this so the two
    /// can never disagree about the order.
    pub const ARBITRATION_ORDER: [Self; 5] = [
        Self::HardSeparation,
        Self::Operator,
        Self::Formation,
        Self::Flocking,
        Self::Hold,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beacon::{STATUS_ARMED, STATUS_HERO, STATUS_PRECEDENCE_MASK};

    /// Every level survives the three-bit round trip, and each occupies a
    /// distinct encoding — a duplicated discriminant would silently merge two
    /// levels on the wire.
    #[test]
    fn every_level_round_trips_through_the_status_field() {
        let mut seen = std::collections::BTreeSet::new();
        for level in ModePrecedence::ARBITRATION_ORDER {
            let bits = level.as_status_bits();
            assert_eq!(bits & !STATUS_PRECEDENCE_MASK, 0, "{level:?} leaked a bit");
            assert!(seen.insert(bits), "{level:?} duplicates another encoding");
            assert_eq!(ModePrecedence::from_status_bits(bits), level);
            // And it decodes the same with unrelated condition bits set.
            assert_eq!(
                ModePrecedence::from_status_bits(bits | STATUS_ARMED | STATUS_HERO),
                level
            );
        }
        assert_eq!(seen.len(), 5);
    }

    #[test]
    fn hold_is_the_zero_encoding_and_the_default() {
        assert_eq!(ModePrecedence::Hold as u8, 0);
        assert_eq!(ModePrecedence::Hold.as_status_bits(), 0);
        assert_eq!(ModePrecedence::default(), ModePrecedence::Hold);
        assert_eq!(ModePrecedence::from_status_bits(0), ModePrecedence::Hold);
    }

    /// A newer agent using encoding 5/6/7 must be readable, not fatal, and must
    /// not be mistaken for an authority level this reader would act on.
    #[test]
    fn unassigned_encodings_degrade_to_hold() {
        for raw in [5u8, 6, 7] {
            let status = raw << 5;
            assert_eq!(
                ModePrecedence::from_status_bits(status),
                ModePrecedence::Hold,
                "encoding {raw} must degrade to hold"
            );
        }
    }

    /// The five spellings are a published GCS contract; a typo here silently
    /// breaks the operator surface's closed union.
    #[test]
    fn the_wire_strings_are_the_published_union() {
        assert_eq!(ModePrecedence::HardSeparation.as_wire(), "hard-separation");
        assert_eq!(ModePrecedence::Operator.as_wire(), "operator");
        assert_eq!(ModePrecedence::Formation.as_wire(), "formation");
        assert_eq!(ModePrecedence::Flocking.as_wire(), "flocking");
        assert_eq!(ModePrecedence::Hold.as_wire(), "hold");
    }

    /// The ladder is hard-separation first and hold last. Pinning it here is what
    /// makes the "never compare discriminants" rule safe to rely on: the order is
    /// data, not a numeric accident.
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
        // The discriminant order is NOT the ladder order; that is the whole
        // reason Ord is not derived.
        assert!(
            (ModePrecedence::Flocking as u8) > (ModePrecedence::HardSeparation as u8),
            "discriminants must not be mistaken for authority"
        );
    }
}
