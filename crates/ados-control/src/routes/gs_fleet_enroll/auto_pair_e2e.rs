//! An auto-paired drone, from the bind that gave it no slot to flying on the
//! slot the ground's registry issued, through the production code at every
//! hop: the bind's pair-state write, the ground's receive slots, enrolment,
//! the slot reconciler's decision and its relayed call (authorized by the
//! drone's real relay check), the drone's config write, and the radio restart
//! the address owes. Each hop's input is the previous hop's output.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ados_groundlink::{wfb_rx, FleetRegistry, FleetSlot};
use ados_protocol::aux_rpc::RpcMethod;
use ados_supervisor::bind::keys::apply_pair_fields;
use ados_supervisor::bind::BindRole;
use axum::http::StatusCode;
use serde_norway::{Mapping, Value};

use super::enrol_into;
use crate::routes::config_rw::{put_config_at, ConfigUpdate};
use crate::routes::gs_fleet_slot::test_drone::{self, SECRET};
use crate::routes::gs_fleet_slot::{decide_tick, deliver};
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

/// The ground enrols `drone` from its beacon, its reconciler delivers the
/// registry's slot over the relay, and the drone writes what it was sent.
/// Returns the issued slot.
async fn enrol_and_deliver(registry: &Path, drone: &'static str, run_dir: &Path, cfg: &Path) -> u8 {
    let issued = enrol_into(registry, &[(drone.to_string(), "drone".to_string())]);
    let slot = issued
        .iter()
        .find(|(d, _)| d == drone)
        .map(|(_, s)| *s)
        .or_else(|| FleetRegistry::load(registry).slot_of(drone))
        .expect("enrolled");

    let slots = slots_with_secret(registry, drone);
    let assignment = decide_tick(&slots, &BTreeMap::new())
        .into_iter()
        .find(|a| a.device_id == drone)
        .expect("the reconciler tells the drone its slot");
    assert_eq!(assignment.slot, slot);

    let link = test_drone::spawn(drone, Some(SECRET)).await;
    deliver(&link.proxy, &slots, &assignment)
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
    assert_eq!(provisional, (1, ados_radio::config::FIRST_DRONE_SLOT));

    let registry = dir.path().join("fleet.json");
    assert!(
        wfb_rx::fleet_slots(&FleetRegistry::load(&registry)).contains(&provisional.1),
        "the ground hears it"
    );
    let slot = enrol_and_deliver(&registry, DRONE, dir.path(), &cfg).await;

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
    let ground_hears = wfb_rx::fleet_slots(&FleetRegistry::load(&registry));
    assert!(ground_hears.contains(&ados_radio::config::FIRST_DRONE_SLOT));

    let slot = enrol_and_deliver(&registry, DRONE, dir.path(), &cfg).await;

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

    let slot_a = enrol_and_deliver(&registry, FIRST, a.path(), &cfg_a).await;
    let slot_b = enrol_and_deliver(&registry, SECOND, b.path(), &cfg_b).await;

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
