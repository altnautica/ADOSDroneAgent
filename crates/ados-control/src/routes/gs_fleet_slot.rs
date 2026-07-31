//! Tell each drone the fleet slot the ground station issued it.
//!
//! The ground station allocates a slot at pair time and returns it in the pair
//! response — to the CALLER. Nothing ever carried it to the aircraft, so a
//! drone learned its own fleet address only if an operator typed it in or the
//! cloud pushed a config. That gap is why the fleet bench has to seed
//! `video.wfb.fleet_slot` by hand, and it blocks anything that wants a stable
//! per-drone identity derived from the slot.
//!
//! ## Why this can be delivered at all
//!
//! The slot names the drone's own `link_id`, so at first glance telling a drone
//! its slot over the radio is circular: reach it on the lane its slot defines,
//! to tell it which lane to use.
//!
//! It is not circular, because the two directions are addressed differently.
//! The drone's DOWNLINK is slot-addressed — that is what keeps N drones off
//! each other's decoder. The UPLINK is not: `ados_radio::process::aux_rx_args`
//! points every drone's aux receiver at the GROUND STATION's `link_id`
//! (`link_id(fleet_id, SLOT_GROUND)`), so one uplink transmission reaches the
//! whole fleet and the addressee travels in the record. That is the same
//! property the link-feedback lane relies on.
//!
//! So a drone on the wrong slot — or on no slot at all — still hears the
//! ground station. It is reachable precisely when it most needs correcting.
//!
//! ## Shape
//!
//! A reconciler, not a one-shot at pair time. A one-shot loses the race with a
//! drone that is rebooting, powered off, or out of range when its slot is
//! issued, and leaves it silently wrong until someone notices. This states the
//! desired slot repeatedly and cheaply, and goes idle the moment the fleet
//! agrees — the same shape as the hero reconciler next door, for the same
//! reason.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ados_groundlink::FleetSlot;
use ados_protocol::aux_rpc::RpcMethod;
use ados_protocol::aux_rpc_proxy::AuxRpcProxy;
use serde_json::json;

/// How often the ground station restates the fleet's slot assignments.
///
/// Slower than the hero reconciler: a slot changes only at pair time, whereas
/// video attention follows the operator. This exists to catch a drone that was
/// unreachable when its slot was issued, which is a per-boot event, not a
/// per-second one.
pub const SLOT_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// The drone-side config write. `PUT /api/config` is already relay-reachable
/// (it is absent from `auth::relay_forbidden`) and already validates its own
/// key/value, so this carries no new drone-side surface.
const DRONE_CONFIG_PATH: &[u8] = b"/api/config";

/// The config key holding a node's fleet slot.
const FLEET_SLOT_KEY: &str = "video.wfb.fleet_slot";

/// One drone that should be told its slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotAssignment {
    pub device_id: String,
    pub slot: u8,
}

/// Which drones still need telling.
///
/// Pure over its inputs so the decision is testable without a registry file or
/// a radio. `confirmed` maps device id to the slot that drone has acknowledged;
/// a drone missing from it, or acknowledging a different slot, is restated.
pub fn decide_tick(slots: &[FleetSlot], confirmed: &BTreeMap<String, u8>) -> Vec<SlotAssignment> {
    slots
        .iter()
        .filter(|s| confirmed.get(&s.device_id) != Some(&s.slot))
        .map(|s| SlotAssignment {
            device_id: s.device_id.clone(),
            slot: s.slot,
        })
        .collect()
}

/// Send one drone its slot. Returns `Ok` only on a 2xx, so a drone that
/// answered with an error is retried rather than recorded as agreeing.
pub async fn deliver(proxy: &Arc<AuxRpcProxy>, assignment: &SlotAssignment) -> Result<(), String> {
    let body = json!({ "key": FLEET_SLOT_KEY, "value": assignment.slot }).to_string();
    match proxy
        .call(
            assignment.device_id.as_bytes(),
            RpcMethod::Put,
            DRONE_CONFIG_PATH,
            body.as_bytes(),
        )
        .await
    {
        Ok(resp) if (200..300).contains(&resp.status) => Ok(()),
        Ok(resp) => Err(format!("drone answered HTTP {}", resp.status)),
        Err(e) => Err(format!("{e}")),
    }
}

/// Run the slot reconciler until the process exits. Ground-station profile
/// only; spawned once at startup beside the hero reconciler that shares its
/// relay proxy.
pub async fn run_slot_reconciler(proxy: Arc<AuxRpcProxy>) {
    let mut tick = tokio::time::interval(SLOT_RECONCILE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // What each drone has acknowledged. Held in memory only: on restart the
    // ground station restates every slot once, which is one cheap tick and
    // strictly safer than trusting a persisted claim about a drone that may
    // have been re-flashed while we were down.
    let mut confirmed: BTreeMap<String, u8> = BTreeMap::new();
    loop {
        tick.tick().await;
        let slots = registered_slots();
        // A drone that left the fleet stops being tracked, so its slot can be
        // reissued to someone else without a stale acknowledgement suppressing
        // the delivery.
        let present: std::collections::BTreeSet<&str> =
            slots.iter().map(|s| s.device_id.as_str()).collect();
        confirmed.retain(|device_id, _| present.contains(device_id.as_str()));

        for assignment in decide_tick(&slots, &confirmed) {
            match deliver(&proxy, &assignment).await {
                Ok(()) => {
                    tracing::info!(
                        device_id = %assignment.device_id,
                        slot = assignment.slot,
                        "fleet_slot_delivered"
                    );
                    confirmed.insert(assignment.device_id, assignment.slot);
                }
                Err(e) => {
                    // Not an error state: a drone that is off, rebooting or out
                    // of range is the ordinary case this reconciler exists for.
                    tracing::debug!(
                        device_id = %assignment.device_id,
                        slot = assignment.slot,
                        error = %e,
                        "fleet_slot_delivery_deferred"
                    );
                }
            }
        }
    }
}

/// The fleet registry's current slots.
fn registered_slots() -> Vec<FleetSlot> {
    ados_groundlink::FleetRegistry::load(std::path::Path::new(ados_groundlink::FLEET_REGISTRY_PATH))
        .slots()
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(device_id: &str, slot: u8) -> FleetSlot {
        FleetSlot {
            slot,
            device_id: device_id.to_string(),
            paired_at_ms: 0,
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
}
