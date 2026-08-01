//! Decide what MAVLink system id an aircraft should carry, and whether it is
//! safe to change it right now.
//!
//! ## The problem this answers
//!
//! Two flight controllers on the shipped default system id are ONE vehicle to
//! everything downstream, and a command addressed to that id is accepted by
//! both. Making the collision observable was the first half of closing it; this
//! is the decision half of the second.
//!
//! ## Why the decision is separated from the act
//!
//! Changing a vehicle's system id means writing a parameter to the flight
//! controller, and on the firmware we target that does not take effect until
//! the autopilot reboots. Rebooting an autopilot is not something to do on a
//! guess. So the question "what should this aircraft be, and may it change
//! now" is answered here -- purely, from values, with no I/O and no clock --
//! and the actuation is a separate, gated step that consumes this answer.
//!
//! That split is what lets the rule be tested exhaustively. A branch buried
//! inside a routine that also talks to an autopilot gets exercised on a bench,
//! once, in whatever state the bench happened to be in.
//!
//! ## The rules, and why each exists
//!
//! An ARMED aircraft is never re-identified. The change needs a reboot to take
//! effect, and rebooting the autopilot of an aircraft whose motors are live is
//! not a trade worth making against a numbering problem that has waited this
//! long already.
//!
//! An aircraft with no slot is left alone rather than given a guess. A slot is
//! issued centrally, and an aircraft that has not been issued one is exactly
//! the aircraft whose identity nobody has authority to assert.
//!
//! An aircraft already carrying the right id is left alone, so the steady state
//! costs nothing and no aircraft is rebooted to arrive where it already is.

/// The MAVLink system id an aircraft on `slot` should carry.
///
/// The slot is already the fleet's addressing primitive and is already unique
/// by construction, so deriving from it needs no second allocator and cannot
/// disagree with the one that exists. Slots are issued from 1, and MAVLink
/// reserves 0 as a broadcast address, so the two ranges line up without an
/// offset.
///
/// `None` for a slot that cannot be a valid system id. Slot 0 is not issued and
/// 0 is not addressable; 255 is conventionally the ground station's own id, and
/// handing it to an aircraft would make that aircraft indistinguishable from
/// the station commanding it.
pub fn system_id_for_slot(slot: u8) -> Option<u8> {
    match slot {
        0 => None,
        255 => None,
        s => Some(s),
    }
}

/// Why an aircraft's identity is or is not being changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityDecision {
    /// Already correct. The steady state, and silent.
    AlreadyCorrect,
    /// Should become `target`, and it is safe to do so now.
    Adopt { target: u8 },
    /// Armed. Deferred until disarmed, because the change needs an autopilot
    /// reboot to take effect.
    DeferredArmed { target: u8 },
    /// No slot has been issued, so there is nothing to derive an identity from.
    NoSlot,
    /// The slot cannot map to a usable system id.
    UnusableSlot { slot: u8 },
}

/// Decide what to do about this aircraft's identity.
///
/// `slot` is the fleet slot it has been issued, `current` the system id its
/// flight controller reports today, and `armed` whether its motors are live.
pub fn decide_identity(slot: Option<u8>, current: u8, armed: bool) -> IdentityDecision {
    let Some(slot) = slot else {
        return IdentityDecision::NoSlot;
    };
    let Some(target) = system_id_for_slot(slot) else {
        return IdentityDecision::UnusableSlot { slot };
    };
    if current == target {
        return IdentityDecision::AlreadyCorrect;
    }
    if armed {
        return IdentityDecision::DeferredArmed { target };
    }
    IdentityDecision::Adopt { target }
}

/// Whether a fleet's members would be distinguishable from one another.
///
/// Answered over the identities the aircraft actually present, not over the
/// slots they were issued: a slot table is what the ground station intended,
/// and the question here is what is true on the air. Returns the system ids
/// carried by more than one aircraft, ascending, empty when the fleet is
/// unambiguous.
pub fn colliding_identities(observed: &[(u8, u8)]) -> Vec<u8> {
    let mut seen: std::collections::BTreeMap<u8, usize> = Default::default();
    for (_slot, system_id) in observed {
        *seen.entry(*system_id).or_insert(0) += 1;
    }
    seen.into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(id, _)| id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_maps_to_the_same_number_as_its_system_id() {
        // No offset and no second allocator: the slot is already unique by
        // construction, so deriving from it cannot disagree with the authority
        // that issued it.
        assert_eq!(system_id_for_slot(1), Some(1));
        assert_eq!(system_id_for_slot(24), Some(24));
    }

    #[test]
    fn the_unusable_slots_are_refused_rather_than_wrapped() {
        // 0 is the MAVLink broadcast address; an aircraft answering to it would
        // answer to every command addressed at the whole fleet. 255 is
        // conventionally the ground station, and an aircraft carrying it would
        // be indistinguishable from the station commanding it.
        assert_eq!(system_id_for_slot(0), None);
        assert_eq!(system_id_for_slot(255), None);
    }

    #[test]
    fn an_aircraft_already_carrying_its_identity_is_left_alone() {
        // The steady state has to be silent, or every reconcile reboots an
        // autopilot to arrive where it already is.
        assert_eq!(
            decide_identity(Some(3), 3, false),
            IdentityDecision::AlreadyCorrect
        );
    }

    #[test]
    fn an_aircraft_on_the_default_identity_adopts_its_slot() {
        // The case the whole thing exists for: two aircraft shipped on id 1.
        assert_eq!(
            decide_identity(Some(2), 1, false),
            IdentityDecision::Adopt { target: 2 }
        );
    }

    #[test]
    fn an_armed_aircraft_is_never_re_identified() {
        // The change does not take effect until the autopilot reboots, and
        // rebooting the autopilot of an aircraft whose motors are live is not a
        // trade worth making against a numbering problem.
        assert_eq!(
            decide_identity(Some(2), 1, true),
            IdentityDecision::DeferredArmed { target: 2 }
        );
    }

    #[test]
    fn an_armed_aircraft_that_is_already_correct_reports_correct_not_deferred() {
        // Otherwise a healthy armed fleet reads as though it were waiting to be
        // changed, and an operator learns to ignore the state that matters.
        assert_eq!(
            decide_identity(Some(2), 2, true),
            IdentityDecision::AlreadyCorrect
        );
    }

    #[test]
    fn an_aircraft_with_no_slot_is_left_alone_rather_than_guessed_at() {
        // A slot is issued centrally. An aircraft without one is precisely the
        // aircraft whose identity nobody has the authority to assert.
        assert_eq!(decide_identity(None, 1, false), IdentityDecision::NoSlot);
    }

    #[test]
    fn an_unusable_slot_is_reported_rather_than_silently_skipped() {
        assert_eq!(
            decide_identity(Some(0), 1, false),
            IdentityDecision::UnusableSlot { slot: 0 }
        );
    }

    #[test]
    fn a_fleet_on_distinct_identities_is_unambiguous() {
        assert!(colliding_identities(&[(1, 1), (2, 2), (3, 3)]).is_empty());
    }

    #[test]
    fn a_fleet_sharing_an_identity_names_the_shared_one() {
        // Two aircraft on id 1 are one vehicle to a ground station, and a
        // command to that id reaches both.
        assert_eq!(colliding_identities(&[(1, 1), (2, 1), (3, 3)]), vec![1]);
    }

    #[test]
    fn every_shared_identity_is_reported_not_just_the_first() {
        // A fleet can be wrong in more than one place, and stopping at the
        // first would let an operator fix one collision and believe they were
        // done.
        assert_eq!(
            colliding_identities(&[(1, 1), (2, 1), (3, 7), (4, 7), (5, 5)]),
            vec![1, 7]
        );
    }

    #[test]
    fn an_empty_fleet_collides_with_nothing() {
        assert!(colliding_identities(&[]).is_empty());
    }

    #[test]
    fn a_full_fleet_of_slot_derived_identities_never_collides() {
        // The property the derivation is for: if every aircraft adopts its
        // slot, the fleet is distinguishable by construction rather than by
        // luck.
        let fleet: Vec<(u8, u8)> = (1..=crate::FLEET_MAX_SLOTS)
            .map(|s| (s, system_id_for_slot(s).expect("issued slots are usable")))
            .collect();
        assert!(colliding_identities(&fleet).is_empty());
        assert_eq!(fleet.len(), crate::FLEET_MAX_SLOTS as usize);
    }
}
