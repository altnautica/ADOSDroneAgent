use super::*;

#[test]
fn a_conflicting_secret_is_settled_not_retried() {
    assert_eq!(secret_outcome(409), Ok(SecretOutcome::Conflict));
    assert_eq!(secret_outcome(200), Ok(SecretOutcome::Held));
    assert!(secret_outcome(503).is_err());
}

fn slot(device_id: &str, slot: u8) -> FleetSlot {
    FleetSlot {
        slot,
        device_id: device_id.to_string(),
        paired_at_ms: 0,
        relay_secret: None,
    }
}

#[test]
fn a_drone_that_has_never_acknowledged_is_told_its_slot() {
    let slots = vec![slot("aaaa", 1), slot("bbbb", 2)];
    let got = decide_tick(&slots, &BTreeMap::new());
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].slot, 1);
    assert_eq!(got[1].slot, 2);
}

#[test]
fn a_fleet_that_already_agrees_costs_nothing() {
    // The reconciler runs forever, so the steady state has to be silent —
    // otherwise it is a standing airtime cost on a shared radio.
    let slots = vec![slot("aaaa", 1), slot("bbbb", 2)];
    let confirmed = BTreeMap::from([("aaaa".into(), 1u8), ("bbbb".into(), 2u8)]);
    assert!(decide_tick(&slots, &confirmed).is_empty());
}

#[test]
fn a_drone_holding_the_wrong_slot_is_corrected() {
    // The case that matters: two drones sharing one slot thrash each
    // other's FEC decoder, and the symptom is unexplained link loss on
    // both. A stale acknowledgement must not read as agreement.
    let slots = vec![slot("aaaa", 1)];
    let confirmed = BTreeMap::from([("aaaa".into(), 3u8)]);
    let got = decide_tick(&slots, &confirmed);
    assert_eq!(
        got,
        vec![SlotAssignment {
            device_id: "aaaa".into(),
            slot: 1
        }]
    );
}

fn slot_with_secret(device_id: &str, slot: u8, secret: &str) -> FleetSlot {
    FleetSlot {
        slot,
        device_id: device_id.to_string(),
        paired_at_ms: 0,
        relay_secret: Some(secret.to_string()),
    }
}

#[test]
fn a_drone_that_has_not_taken_the_secret_is_offered_it() {
    let slots = vec![slot_with_secret("aaaa", 1, "ff")];
    let got = decide_secret_tick(&slots, &Default::default());
    assert_eq!(
        got,
        vec![SecretDelivery {
            device_id: "aaaa".into(),
            secret: "ff".into()
        }]
    );
}

#[test]
fn a_credential_is_not_restated_forever() {
    // A reconciler that re-offers a secret on every tick is both a standing
    // airtime cost and a standing exposure, since the offer necessarily
    // travels unauthenticated.
    let slots = vec![slot_with_secret("aaaa", 1, "ff")];
    let acked = std::collections::BTreeSet::from(["aaaa".to_string()]);
    assert!(decide_secret_tick(&slots, &acked).is_empty());
}

#[test]
fn a_registration_with_no_secret_is_skipped_not_sent_an_empty_one() {
    // The field is optional so an older registry loads; such an entry is
    // issued a secret when it is next allocated, not handed an empty string.
    let slots = vec![slot("aaaa", 1)];
    assert!(decide_secret_tick(&slots, &Default::default()).is_empty());
}

#[test]
fn a_departed_drone_is_not_offered_a_credential() {
    let acked = std::collections::BTreeSet::from(["ghost".to_string()]);
    assert!(decide_secret_tick(&[], &acked).is_empty());
}

#[test]
fn a_drone_that_left_the_fleet_is_not_chased() {
    // Only the registry decides who is in the fleet; a leftover
    // acknowledgement must not resurrect a departed drone.
    let confirmed = BTreeMap::from([("ghost".into(), 7u8)]);
    assert!(decide_tick(&[], &confirmed).is_empty());
}

#[test]
fn the_cadence_is_slow_enough_to_be_free_and_fast_enough_to_matter() {
    // A slot changes at pair time, so this only has to catch a drone that
    // was unreachable then — a per-boot event.
    assert!(SLOT_RECONCILE_INTERVAL >= Duration::from_secs(10));
    assert!(SLOT_RECONCILE_INTERVAL <= Duration::from_secs(120));
}
