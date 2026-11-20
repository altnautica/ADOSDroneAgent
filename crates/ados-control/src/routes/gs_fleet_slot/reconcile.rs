//! The slot reconciler's loop and its per-tick work.

use std::collections::{BTreeMap, BTreeSet};

use ados_groundlink::FleetSlot;

use super::{
    decide_secret_tick, decide_tick, deliver, deliver_secret, forget_departed_relay_secret_states,
    ground_fleet_id, record_relay_secret_state, registered_slots, uplinks, RelaySecretState,
    SecretOutcome, Uplinks, SLOT_RECONCILE_INTERVAL,
};

/// What the reconciler has learned from the drones, kept across ticks.
#[derive(Debug, Default)]
pub struct SlotReconciler {
    /// What each drone has acknowledged, as `(fleet_id, slot)`. Held in memory
    /// only: on restart the ground station restates every address once, which
    /// is one cheap tick and strictly safer than trusting a persisted claim
    /// about a drone that may have been re-flashed while we were down.
    pub confirmed: BTreeMap<String, (u16, u8)>,
    /// Which drones have taken the relay secret. In memory for the same reason
    /// the slot acknowledgements are: on restart the ground station re-offers
    /// once, which the drone answers as already-held at no cost.
    pub secret_acked: BTreeSet<String>,
}

impl SlotReconciler {
    /// One tick: offer every registered drone that has not taken it the
    /// relay secret, then the address (`fleet_id`, its slot) to every drone
    /// that has not acknowledged it. `fleet_id` is this ground's own, `None`
    /// when its radio refuses it (then no address is delivered). `waiting`
    /// holds the drones heard on the provisional lane: only an offer to one of
    /// them goes out on the provisional uplink as well (see [`uplinks`]).
    pub async fn tick(
        &mut self,
        uplinks: &Uplinks,
        slots: &[FleetSlot],
        fleet_id: Option<u16>,
        waiting: &BTreeSet<String>,
    ) {
        // A ground whose own fleet id its radio refuses offers on its fleet
        // lane alone.
        let lanes = |device_id: &str| {
            uplinks.lanes(
                fleet_id.unwrap_or(ados_radio::config::DEFAULT_FLEET_ID),
                waiting.contains(device_id),
            )
        };
        // A drone that left the fleet stops being tracked, so its slot can be
        // reissued to someone else without a stale acknowledgement suppressing
        // the delivery.
        let present: BTreeSet<&str> = slots.iter().map(|s| s.device_id.as_str()).collect();
        self.secret_acked
            .retain(|device_id| present.contains(device_id.as_str()));
        self.confirmed
            .retain(|device_id, _| present.contains(device_id.as_str()));
        forget_departed_relay_secret_states(&present);

        // The secret goes first. Slot delivery carries a ticket, and the drone
        // cannot verify one until it holds the secret -- so offering the
        // credential before the call that depends on it is what keeps the very
        // first reconcile from being refused by the gate it just armed.
        for delivery in decide_secret_tick(slots, &self.secret_acked) {
            match deliver_secret(lanes(&delivery.device_id), &delivery).await {
                Ok(SecretOutcome::Held) => {
                    tracing::info!(device_id = %delivery.device_id, "relay_secret_delivered");
                    record_relay_secret_state(&delivery.device_id, RelaySecretState::Held);
                    self.secret_acked.insert(delivery.device_id);
                }
                Ok(SecretOutcome::Conflict) => {
                    tracing::warn!(
                        device_id = %delivery.device_id,
                        "relay_secret_conflict: the drone holds a different relay secret; unpair it to re-key"
                    );
                    record_relay_secret_state(&delivery.device_id, RelaySecretState::Conflict);
                    // Settled: no further offers until it leaves the fleet.
                    self.secret_acked.insert(delivery.device_id);
                }
                Err(e) => {
                    // A drone that is off, rebooting or out of range is the
                    // ordinary case this reconciler exists for.
                    tracing::debug!(
                        device_id = %delivery.device_id,
                        error = %e,
                        "relay_secret_delivery_deferred"
                    );
                }
            }
        }

        let Some(fleet_id) = fleet_id else {
            if !slots.is_empty() {
                tracing::warn!(
                    "fleet_address_delivery_skipped: this ground's own fleet id is unusable"
                );
            }
            return;
        };
        for assignment in decide_tick(slots, fleet_id, &self.confirmed) {
            match deliver(lanes(&assignment.device_id), slots, &assignment).await {
                Ok(()) => {
                    tracing::info!(
                        device_id = %assignment.device_id,
                        fleet_id = assignment.fleet_id,
                        slot = assignment.slot,
                        "fleet_slot_delivered"
                    );
                    self.confirmed
                        .insert(assignment.device_id, (assignment.fleet_id, assignment.slot));
                }
                Err(e) => {
                    // Not an error state: a drone that is off, rebooting or out
                    // of range is the ordinary case this reconciler exists for.
                    tracing::debug!(
                        device_id = %assignment.device_id,
                        fleet_id = assignment.fleet_id,
                        slot = assignment.slot,
                        error = %e,
                        "fleet_slot_delivery_deferred"
                    );
                }
            }
        }
    }
}

/// Run the slot reconciler until the process exits. Ground-station profile
/// only; spawned once at startup beside the hero reconciler that shares its
/// relay proxy. The ground's fleet id is read from `config_path` every tick,
/// so an operator's change of it is restated to the whole fleet, and the
/// drones waiting on the provisional lane from the linked-peers sidecar.
pub async fn run_slot_reconciler(uplinks: Uplinks, config_path: std::path::PathBuf) {
    let mut tick = tokio::time::interval(SLOT_RECONCILE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let sidecar = ados_protocol::paths::run_dir().join("linked-peers.json");
    let mut reconciler = SlotReconciler::default();
    loop {
        tick.tick().await;
        let waiting =
            uplinks::waiting_drones(&sidecar, crate::routes::status_full::now_unix_secs());
        reconciler
            .tick(
                &uplinks,
                &registered_slots(),
                ground_fleet_id(&config_path),
                &waiting,
            )
            .await;
    }
}
