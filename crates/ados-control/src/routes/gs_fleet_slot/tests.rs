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
    let got = decide_tick(&slots, 7, &BTreeMap::new());
    assert_eq!(got.len(), 2);
    assert_eq!((got[0].fleet_id, got[0].slot), (7, 1));
    assert_eq!((got[1].fleet_id, got[1].slot), (7, 2));
}

#[test]
fn a_fleet_that_already_agrees_costs_nothing() {
    // The reconciler runs forever, so the steady state has to be silent —
    // otherwise it is a standing airtime cost on a shared radio.
    let slots = vec![slot("aaaa", 1), slot("bbbb", 2)];
    let confirmed = BTreeMap::from([("aaaa".into(), (7, 1u8)), ("bbbb".into(), (7, 2u8))]);
    assert!(decide_tick(&slots, 7, &confirmed).is_empty());
}

#[test]
fn a_drone_holding_the_wrong_slot_is_corrected() {
    // The case that matters: two drones sharing one slot thrash each
    // other's FEC decoder, and the symptom is unexplained link loss on
    // both. A stale acknowledgement must not read as agreement.
    let slots = vec![slot("aaaa", 1)];
    let confirmed = BTreeMap::from([("aaaa".into(), (7, 3u8))]);
    let got = decide_tick(&slots, 7, &confirmed);
    assert_eq!(
        got,
        vec![SlotAssignment {
            device_id: "aaaa".into(),
            fleet_id: 7,
            slot: 1
        }]
    );
}

#[test]
fn a_drone_that_acknowledged_another_fleet_is_told_this_one() {
    // The ground's fleet id changed (an operator moved it), or the drone
    // took its slot on the default fleet: the slot alone agreeing is not the
    // address agreeing, and a drone on another fleet hears nothing from here.
    let slots = vec![slot("aaaa", 1)];
    let confirmed = BTreeMap::from([("aaaa".into(), (1, 1u8))]);
    let got = decide_tick(&slots, 7, &confirmed);
    assert_eq!(
        got,
        vec![SlotAssignment {
            device_id: "aaaa".into(),
            fleet_id: 7,
            slot: 1
        }]
    );
}

#[test]
fn the_ground_s_fleet_id_is_read_as_its_radio_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "video:\n  wfb:\n    fleet_id: 7\n").unwrap();
    assert_eq!(ground_fleet_id(&cfg), Some(7));
    // An absent key is the radio's default fleet.
    std::fs::write(&cfg, "agent:\n  profile: ground_station\n").unwrap();
    assert_eq!(
        ground_fleet_id(&cfg),
        Some(ados_radio::config::DEFAULT_FLEET_ID)
    );
    // The reserved 0 parks the ground's radio: there is no address to give.
    std::fs::write(&cfg, "video:\n  wfb:\n    fleet_id: 0\n").unwrap();
    assert_eq!(ground_fleet_id(&cfg), None);
}

#[tokio::test]
async fn a_ground_off_the_default_fleet_offers_on_both_uplinks() {
    let uplinks = Uplinks {
        fleet: test_drone::unheard_lane().await,
        provisional: Some(test_drone::unheard_lane().await),
    };
    assert_eq!(uplinks.lanes(ados_radio::config::DEFAULT_FLEET_ID).len(), 1);
    let lanes = uplinks.lanes(7);
    assert_eq!(lanes.len(), 2);
    assert!(Arc::ptr_eq(&lanes[0], &uplinks.fleet));
    let without = Uplinks {
        provisional: None,
        ..uplinks
    };
    assert_eq!(without.lanes(7).len(), 1);
}

#[tokio::test]
async fn an_address_reaches_a_drone_that_hears_only_the_provisional_uplink() {
    // The drone an auto-pair bind left on the default fleet: the ground's
    // own uplink is never answered, the provisional one is, and the delivery
    // carries the ground's fleet id beside the slot.
    const DRONE: &str = "provisional0a";
    let drone = test_drone::spawn(DRONE, Some(test_drone::SECRET)).await;
    let uplinks = Uplinks {
        fleet: test_drone::unheard_lane().await,
        provisional: Some(Arc::clone(&drone.proxy)),
    };
    let slots = vec![slot_with_secret(DRONE, 2, test_drone::SECRET)];
    let assignment = decide_tick(&slots, 7, &BTreeMap::new()).remove(0);
    deliver(uplinks.lanes(7), &slots, &assignment)
        .await
        .expect("answered on the provisional uplink");
    let seen = drone
        .seen
        .lock()
        .last()
        .cloned()
        .expect("the drone was called");
    assert_eq!(seen.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(body["key"], "video.wfb.fleet_slot");
    assert_eq!(body["value"], 2);
    assert_eq!(body["fleet_id"], 7);

    // The relay secret offer reaches it the same way. (This stand-in drone
    // runs only the relay ticket check, which refuses the ticketless offer:
    // an answer at all is the proof it heard the provisional uplink.)
    let secret = SecretDelivery {
        device_id: DRONE.into(),
        secret: test_drone::SECRET.into(),
    };
    assert_eq!(
        deliver_secret(uplinks.lanes(7), &secret).await,
        Err("drone answered HTTP 401".to_string())
    );
    let seen = drone
        .seen
        .lock()
        .last()
        .cloned()
        .expect("the drone was called");
    assert_eq!(seen.method, RpcMethod::Post);
    assert_eq!(seen.path, DRONE_RELAY_SECRET_PATH);

    // A ground on the default fleet does not use the provisional uplink, so
    // this drone (not on its fleet lane) is not reached and is retried.
    assert!(deliver(
        uplinks.lanes(ados_radio::config::DEFAULT_FLEET_ID),
        &slots,
        &assignment
    )
    .await
    .is_err());
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
    let confirmed = BTreeMap::from([("ghost".into(), (7, 7u8))]);
    assert!(decide_tick(&[], 7, &confirmed).is_empty());
}

#[test]
fn the_cadence_is_slow_enough_to_be_free_and_fast_enough_to_matter() {
    // A slot changes at pair time, so this only has to catch a drone that
    // was unreachable then — a per-boot event.
    assert!(SLOT_RECONCILE_INTERVAL >= Duration::from_secs(10));
    assert!(SLOT_RECONCILE_INTERVAL <= Duration::from_secs(120));
}
