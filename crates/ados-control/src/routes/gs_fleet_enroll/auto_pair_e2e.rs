//! An auto-paired drone, from the bind that gave it no address to flying on
//! the fleet id and slot its ground's registry holds, through the production
//! code at every hop: the bind's pair-state write, the ground's receive slots
//! (and, off the default fleet, its provisional links and the provisional
//! presence record its beacon lands in), enrolment, the slot reconciler's
//! decision and its relayed call (authorized by the drone's real relay
//! check), the drone's config write, and the radio restart the address owes.
//! Each hop's input is the previous hop's output.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ados_groundlink::{wfb_rx, FleetRegistry, FleetSlot};
use ados_protocol::aux_rpc::RpcMethod;
use ados_radio::config::{link_id, DEFAULT_FLEET_ID, FIRST_DRONE_SLOT, SLOT_GROUND};
use ados_supervisor::bind::keys::apply_pair_fields;
use ados_supervisor::bind::BindRole;
use axum::http::StatusCode;
use serde_norway::{Mapping, Value};

use super::enrol_into;
use crate::routes::config_rw::{put_config_at, ConfigUpdate};
use crate::routes::gs_fleet_slot::test_drone::{self, SECRET};
use crate::routes::gs_fleet_slot::{decide_tick, deliver, Uplinks};
use crate::routes::radio_fleet_apply;

/// The drone's config after an auto-pair bind: the bind's own pair-state
/// write (no fleet assignment) over a fresh install.
fn auto_paired_drone(dir: &Path) -> PathBuf {
    let mut root: Mapping = serde_norway::from_str("agent:\n  profile: drone\n").unwrap();
    apply_pair_fields(
        &mut root,
        BindRole::Drone,
        None,
        Some("2026-11-20T00:00:00+00:00"),
        Some(false),
        None,
    );
    let cfg = dir.join("config.yaml");
    std::fs::write(&cfg, serde_norway::to_string(&root).unwrap()).unwrap();
    cfg
}

/// The address the radio service would come up on, or the reason it refuses.
fn radio_address(cfg: &Path) -> Result<(u16, u8), String> {
    let root: Value = serde_norway::from_str(&std::fs::read_to_string(cfg).unwrap()).unwrap();
    let wfb = root
        .get("video")
        .and_then(|v| v.get("wfb"))
        .cloned()
        .unwrap_or(Value::Mapping(Mapping::new()));
    let radio: ados_radio::config::WfbConfig = serde_norway::from_value(wfb).unwrap();
    match ados_radio::config::fleet_identity_error(radio.fleet_id, radio.fleet_slot, false) {
        None => Ok((radio.fleet_id, radio.fleet_slot)),
        Some(e) => Err(e.to_string()),
    }
}

/// The registry's slots with the relay secret the test drone holds, the way
/// the reconciler reads them.
fn slots_with_secret(registry: &Path, device_id: &str) -> Vec<FleetSlot> {
    FleetRegistry::load(registry)
        .slots()
        .cloned()
        .map(|mut s| {
            if s.device_id == device_id {
                s.relay_secret = Some(SECRET.to_string());
            }
            s
        })
        .collect()
}

/// Whether a ground on `ground_fleet` decodes what a drone at `address`
/// transmits: one of its own fleet's receive slots, or its provisional link.
fn ground_hears(registry: &Path, ground_fleet: u16, address: (u16, u8)) -> bool {
    let drone = link_id(address.0, address.1);
    wfb_rx::fleet_slots(&FleetRegistry::load(registry))
        .into_iter()
        .any(|slot| link_id(ground_fleet, slot) == drone)
        || wfb_rx::provisional_links(ground_fleet).is_some_and(|l| l.drone == drone)
}

/// The ground on `ground_fleet` enrols `drone` from its beacon, its
/// reconciler delivers the registry's address over the uplinks (the drone,
/// still on `heard_on`'s fleet, answers on the one keyed to that fleet's
/// ground link id), and the drone writes what it was sent. Returns the issued
/// slot.
async fn enrol_and_deliver(
    registry: &Path,
    ground_fleet: u16,
    drone: &'static str,
    heard_on: (u16, u8),
    run_dir: &Path,
    cfg: &Path,
) -> u8 {
    let ground_dir = registry.parent().unwrap();
    let (peers, waiting) = heard_by_ground(ground_fleet, drone, heard_on, ground_dir);
    let issued = enrol_into(registry, &peers);
    let slot = issued
        .iter()
        .find(|(d, _)| d == drone)
        .map(|(_, s)| *s)
        .or_else(|| FleetRegistry::load(registry).slot_of(drone))
        .expect("enrolled");

    let slots = slots_with_secret(registry, drone);
    let assignment = decide_tick(&slots, ground_fleet, &BTreeMap::new())
        .into_iter()
        .find(|a| a.device_id == drone)
        .expect("the reconciler tells the drone its address");
    assert_eq!((assignment.fleet_id, assignment.slot), (ground_fleet, slot));

    // The drone's uplink receivers key to its own fleet's ground link id:
    // the lane on that link id is the one it answers on.
    let drone_uplink = link_id(heard_on.0, SLOT_GROUND);
    let link = test_drone::spawn(drone, Some(SECRET)).await;
    let (fleet, provisional) = if drone_uplink == link_id(ground_fleet, SLOT_GROUND) {
        (Arc::clone(&link.proxy), test_drone::unheard_lane().await)
    } else {
        assert_eq!(
            wfb_rx::provisional_links(ground_fleet).map(|l| l.ground),
            Some(drone_uplink),
            "the ground transmits where the drone listens"
        );
        (test_drone::unheard_lane().await, Arc::clone(&link.proxy))
    };
    let uplinks = Uplinks {
        fleet,
        provisional: Some(provisional),
    };
    deliver(
        uplinks.lanes(ground_fleet, waiting.contains(drone)),
        &slots,
        &assignment,
    )
        .await
        .expect("the drone admits the delivery");
    let seen = link
        .seen
        .lock()
        .last()
        .cloned()
        .expect("the drone was called");
    assert_eq!(seen.method, RpcMethod::Put);
    assert_eq!(seen.path, b"/api/config");
    assert_eq!(seen.status, 200);

    let update: ConfigUpdate = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(
        update.fleet_id,
        Some(ground_fleet),
        "the fleet id rides along"
    );
    let resp = put_config_at(cfg, run_dir, &update, &|_, _| true);
    assert_eq!(resp.status(), StatusCode::OK);
    slot
}

#[tokio::test]
async fn the_lone_drone_of_a_fresh_ground_keeps_the_first_slot() {
    const DRONE: &str = "auto0pair0aa";
    let dir = tempfile::tempdir().unwrap();
    let cfg = auto_paired_drone(dir.path());
    // The radio comes up: no longer parked on slot 0.
    let provisional = radio_address(&cfg).expect("the radio accepts the provisional slot");
    assert_eq!(provisional, (DEFAULT_FLEET_ID, FIRST_DRONE_SLOT));

    let registry = dir.path().join("fleet.json");
    assert!(
        ground_hears(&registry, DEFAULT_FLEET_ID, provisional),
        "the ground hears it"
    );
    let slot = enrol_and_deliver(
        &registry,
        DEFAULT_FLEET_ID,
        DRONE,
        provisional,
        dir.path(),
        &cfg,
    )
    .await;

    assert_eq!(slot, 1, "a fresh registry issues the first slot");
    assert_eq!(radio_address(&cfg), Ok((1, 1)));
    // Already there: no restart owed, the link is never dropped.
    assert!(radio_fleet_apply::read_status(dir.path()).is_null());
}

#[tokio::test]
async fn a_drone_whose_provisional_slot_is_taken_moves_to_its_issued_one() {
    const DRONE: &str = "auto0pair0bb";
    let dir = tempfile::tempdir().unwrap();
    let cfg = auto_paired_drone(dir.path());
    let registry = dir.path().join("fleet.json");
    // Slots 1 and 2 already belong to other aircraft, so the provisional slot
    // this drone took is someone else's address.
    enrol_into(
        &registry,
        &[
            ("earlier0drone".to_string(), "drone".to_string()),
            ("another0drone".to_string(), "drone".to_string()),
        ],
    );
    let provisional = radio_address(&cfg).unwrap();
    assert!(ground_hears(&registry, DEFAULT_FLEET_ID, provisional));

    let slot = enrol_and_deliver(
        &registry,
        DEFAULT_FLEET_ID,
        DRONE,
        provisional,
        dir.path(),
        &cfg,
    )
    .await;

    assert_eq!(slot, 3, "the registry issues the lowest free slot");
    assert_eq!(
        radio_address(&cfg),
        Ok((1, 3)),
        "the drone holds the issued slot"
    );
    let owed = radio_fleet_apply::read_status(dir.path());
    assert_eq!(owed["outcome"], "pending");
    assert_eq!(owed["fleet_slot"], 3);

    // The radio moves onto it only once the vehicle is disarmed.
    let restarted = std::sync::atomic::AtomicU32::new(0);
    let restart = |_unit: String| {
        restarted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(true)
    };
    let deferred = radio_fleet_apply::apply_tick(dir.path(), true, &restart)
        .await
        .unwrap();
    assert_eq!(deferred["outcome"], "deferred_armed");
    assert_eq!(restarted.load(std::sync::atomic::Ordering::SeqCst), 0);
    let applied = radio_fleet_apply::apply_tick(dir.path(), false, &restart)
        .await
        .unwrap();
    assert_eq!(applied["outcome"], "applied");
    assert_eq!(restarted.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_ground_on_another_fleet_moves_its_auto_paired_drone_onto_that_fleet() {
    // The bind carries no fleet id, so the drone comes up on the default
    // fleet whatever fleet its ground is on. A ground on fleet 7 hears it only
    // on its provisional link, reaches it only on its provisional uplink, and
    // delivers fleet id and slot as one address the drone restarts onto once.
    const GROUND_FLEET: u16 = 7;
    const DRONE: &str = "auto0pair0dd";
    let dir = tempfile::tempdir().unwrap();
    let cfg = auto_paired_drone(dir.path());
    let provisional = radio_address(&cfg).unwrap();
    assert_eq!(provisional, (DEFAULT_FLEET_ID, FIRST_DRONE_SLOT));

    let registry = dir.path().join("fleet.json");
    // One aircraft already flies slot 1 of fleet 7.
    enrol_into(
        &registry,
        &[("earlier0drone".to_string(), "drone".to_string())],
    );
    let own_fleet_only = wfb_rx::fleet_slots(&FleetRegistry::load(&registry))
        .into_iter()
        .any(|slot| link_id(GROUND_FLEET, slot) == link_id(provisional.0, provisional.1));
    assert!(!own_fleet_only, "its own fleet's receivers never decode it");
    assert!(
        ground_hears(&registry, GROUND_FLEET, provisional),
        "the provisional link does"
    );

    let slot = enrol_and_deliver(
        &registry,
        GROUND_FLEET,
        DRONE,
        provisional,
        dir.path(),
        &cfg,
    )
    .await;

    assert_eq!(slot, 2);
    assert_eq!(
        radio_address(&cfg),
        Ok((GROUND_FLEET, 2)),
        "fleet id and slot landed together"
    );
    let owed = radio_fleet_apply::read_status(dir.path());
    assert_eq!(owed["outcome"], "pending");
    assert_eq!(owed["fleet_id"], GROUND_FLEET);
    assert_eq!(owed["fleet_slot"], 2);

    // One restart, once disarmed, onto the whole address.
    let restarted = std::sync::atomic::AtomicU32::new(0);
    let restart = |_unit: String| {
        restarted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(true)
    };
    let deferred = radio_fleet_apply::apply_tick(dir.path(), true, &restart)
        .await
        .unwrap();
    assert_eq!(deferred["outcome"], "deferred_armed");
    let applied = radio_fleet_apply::apply_tick(dir.path(), false, &restart)
        .await
        .unwrap();
    assert_eq!(applied["outcome"], "applied");
    assert_eq!(restarted.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Now on its own registered slot of the ground's fleet: heard there, and
    // its uplink receivers key to the ground's own uplink.
    let moved = radio_address(&cfg).unwrap();
    assert!(wfb_rx::fleet_slots(&FleetRegistry::load(&registry))
        .into_iter()
        .any(|s| link_id(GROUND_FLEET, s) == link_id(moved.0, moved.1)));
    assert_eq!(
        link_id(moved.0, SLOT_GROUND),
        link_id(GROUND_FLEET, SLOT_GROUND)
    );
    // Restating the address it now holds owes nothing more.
    std::fs::remove_file(dir.path().join(radio_fleet_apply::SIDECAR_FILE)).unwrap();
    enrol_and_deliver(&registry, GROUND_FLEET, DRONE, moved, dir.path(), &cfg).await;
    assert!(radio_fleet_apply::read_status(dir.path()).is_null());
}

#[tokio::test]
async fn two_unissued_drones_on_the_first_slot_are_separated_by_enrolment() {
    // Both auto-paired drones transmit on slot 1 until the registry issues
    // their own, so until then the ground decodes both on one slot's lanes
    // (their beacons carry their own device ids, so enrolment tells them
    // apart). Enrolment issues each a distinct slot; the one that keeps slot 1
    // never moves, the other is moved off it, and from then their links are
    // separate. Nothing is merged: each registration is keyed by its own id.
    const FIRST: &str = "auto0pair0c1";
    const SECOND: &str = "auto0pair0c2";
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let cfg_a = auto_paired_drone(a.path());
    let cfg_b = auto_paired_drone(b.path());
    assert_eq!(
        radio_address(&cfg_a),
        radio_address(&cfg_b),
        "both on slot 1"
    );
    let heard_on = radio_address(&cfg_a).unwrap();

    let ground = tempfile::tempdir().unwrap();
    let registry = ground.path().join("fleet.json");
    // One tick hears both beacons.
    let issued = enrol_into(
        &registry,
        &[
            (FIRST.to_string(), "drone".to_string()),
            (SECOND.to_string(), "drone".to_string()),
        ],
    );
    assert_eq!(
        issued,
        vec![(FIRST.to_string(), 1), (SECOND.to_string(), 2)],
        "two aircraft, two slots"
    );

    let slot_a = enrol_and_deliver(
        &registry,
        DEFAULT_FLEET_ID,
        FIRST,
        heard_on,
        a.path(),
        &cfg_a,
    )
    .await;
    let slot_b = enrol_and_deliver(
        &registry,
        DEFAULT_FLEET_ID,
        SECOND,
        heard_on,
        b.path(),
        &cfg_b,
    )
    .await;

    assert_eq!(radio_address(&cfg_a), Ok((1, slot_a)));
    assert_eq!(radio_address(&cfg_b), Ok((1, slot_b)));
    assert_ne!(slot_a, slot_b, "their links no longer share a channel id");
    assert!(
        radio_fleet_apply::read_status(a.path()).is_null(),
        "the drone that keeps slot 1 is never restarted"
    );
    assert_eq!(
        radio_fleet_apply::read_status(b.path())["outcome"],
        "pending"
    );
}
