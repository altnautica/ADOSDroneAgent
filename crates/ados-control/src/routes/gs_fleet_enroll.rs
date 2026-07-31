//! Register a drone in the fleet registry however it was paired.
//!
//! ## The gap this closes
//!
//! A slot is issued in exactly one place today: the pair route
//! (`gs_wfb_pair`), which calls [`FleetRegistry::allocate`] and persists. That
//! route is what an operator drives from the ground station's own surface.
//!
//! It is not how the rigs in the field are paired. The supervisor's auto-bind
//! runs unattended on first boot and calls `start_local_bind(role, None, ...)`
//! — no peer device id, because there is nobody to name one and the bind is a
//! two-party radio rendezvous rather than an addressed request. `allocate` is
//! keyed on a device id, so auto-bind cannot call it and never has.
//!
//! The result is a fleet that works and a registry that is empty. Everything
//! keyed on the registry is then inert without appearing broken: the slot
//! reconciler has nothing to deliver, and the per-pair relay secret — which is
//! stored on the registry entry — is never minted, so the credential that
//! distinguishes a drone's own ground station from anything else on the air
//! does not exist for any auto-bound pair.
//!
//! ## Why the beacon is the right source
//!
//! The ground station learns a peer's device id from the presence beacon, and
//! that is the only place it appears for an auto-bound pair. Decoding a beacon
//! requires the shared fleet keypair: wfb-ng authenticates and decrypts before
//! a frame is ever handed up, so a peer that shows up here has already proved
//! possession of the key.
//!
//! That is the SAME gate the pair route applies. `gs_wfb_pair` accepts a
//! byte-identical keypair blob and issues a slot on the strength of it; a fleet
//! is one trust domain and the key is what defines membership. Enrolling on a
//! decoded beacon is therefore not a weaker admission than the pair route — it
//! is the same evidence, arriving over the radio instead of over HTTP.
//!
//! ## Shape
//!
//! A reconciler rather than a hook in the bind path, for the reason the bind
//! path cannot serve: the device id does not exist at bind time. It appears
//! when the peer starts beaconing, which may be well after the bind completed,
//! and again on every reboot. Restating the question cheaply on a tick covers
//! all of those without the bind FSM having to know anything about registries.
//!
//! Reading the sidecar rather than the presence cache is what keeps the
//! registry single-writer: the cache lives in-process in `ados-groundlink`,
//! while `ados-control` owns every write to `fleet.json`. Two processes
//! allocating into one file is the race the slot registry exists to prevent.

use std::path::Path;

use ados_groundlink::{FleetRegistry, FLEET_MAX_SLOTS, FLEET_REGISTRY_PATH};

use crate::routes::status_full::{fresh_linked_peer_rows_in, now_unix_secs, run_dir};

/// How often the ground station checks whether an audible peer is registered.
///
/// Deliberately unhurried. Enrolment is a per-boot event, not a per-second one,
/// and the cost of being late is one extra tick before a slot is issued — the
/// slot reconciler next door is what actually carries it to the aircraft, and it
/// restates until acknowledged. Matching the registry's own reconcile interval
/// keeps the two loops on one cadence.
pub const ENROLL_INTERVAL: std::time::Duration = ados_groundlink::FLEET_RECONCILE_INTERVAL;

/// The beacon role a fleet slot is issued for.
///
/// Slots address drones. The ground station has its own reserved link id and is
/// never allocated one, and a second ground station audible on the same key
/// must not consume a drone's slot — the table is only [`FLEET_MAX_SLOTS`]
/// deep, so admitting a non-drone would cost a real aircraft its address.
const DRONE_ROLE: &str = "drone";

/// Which audible peers are not yet registered.
///
/// Pure over its inputs, so the decision is testable without a radio, a sidecar
/// or a registry file — the same reason `gs_fleet_slot::decide_tick` is pure.
/// Returns device ids in the order given; `allocate` assigns the lowest free
/// slot, so ordering here decides only who gets the lower number when several
/// arrive together.
pub fn decide_enrollments(peers: &[(String, String)], registry: &FleetRegistry) -> Vec<String> {
    peers
        .iter()
        .filter(|(_, role)| role == DRONE_ROLE)
        .map(|(device_id, _)| device_id)
        .filter(|device_id| registry.slot_of(device_id).is_none())
        .cloned()
        .collect()
}

/// Enrol every unregistered audible drone, returning the slots issued.
///
/// Persists once per tick rather than once per drone: the registry write is
/// atomic (temp file plus rename) and a tick that enrols three drones should
/// leave one complete generation on disk, not three.
///
/// A full fleet is not an error here. `allocate` returns `None` when all
/// [`FLEET_MAX_SLOTS`] are taken, and the honest response is to say so and
/// leave the registered drones alone — evicting one to make room would retune a
/// transmitter that may be airborne.
fn enrol_into(registry_path: &Path, peers: &[(String, String)]) -> Vec<(String, u8)> {
    let pending = {
        let registry = FleetRegistry::load(registry_path);
        decide_enrollments(peers, &registry)
    };
    if pending.is_empty() {
        return Vec::new();
    }

    let mut registry = FleetRegistry::load(registry_path);
    let mut issued = Vec::new();
    for device_id in pending {
        match registry.allocate(&device_id) {
            Some(slot) => issued.push((device_id, slot)),
            None => {
                tracing::warn!(
                    device_id = %device_id,
                    capacity = FLEET_MAX_SLOTS,
                    "fleet_enroll_declined_fleet_full"
                );
            }
        }
    }
    if issued.is_empty() {
        return Vec::new();
    }
    if let Err(e) = registry.persist(registry_path) {
        // The slots are lost, not half-written — `persist` is atomic, so a
        // failure leaves the previous complete generation. Reported rather than
        // retried here because the next tick re-derives the same work from the
        // same beacons.
        tracing::error!(error = %e, "fleet_enroll_persist_failed");
        return Vec::new();
    }
    issued
}

/// Run the enrolment reconciler until the process exits. Ground-station profile
/// only; spawned once at startup beside the slot and hero reconcilers.
pub async fn run_enroll_reconciler() {
    let sidecar = run_dir().join("linked-peers.json");
    let registry_path = Path::new(FLEET_REGISTRY_PATH).to_path_buf();
    let mut tick = tokio::time::interval(ENROLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let peers: Vec<(String, String)> = fresh_linked_peer_rows_in(&sidecar, now_unix_secs())
            .into_iter()
            .map(|p| (p.device_id, p.role))
            .collect();
        if peers.is_empty() {
            continue;
        }
        // Blocking disk I/O (`FleetRegistry::persist` says so on the tin), so it
        // does not run directly on the reactor.
        let path = registry_path.clone();
        let issued = tokio::task::spawn_blocking(move || enrol_into(&path, &peers))
            .await
            .unwrap_or_default();
        for (device_id, slot) in issued {
            tracing::info!(device_id = %device_id, slot, "fleet_enrolled_from_beacon");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(device_id: &str, role: &str) -> (String, String) {
        (device_id.to_string(), role.to_string())
    }

    #[test]
    fn an_audible_unregistered_drone_is_enrolled() {
        let registry = FleetRegistry::default();
        assert_eq!(
            decide_enrollments(&[peer("drone-a", "drone")], &registry),
            vec!["drone-a".to_string()]
        );
    }

    #[test]
    fn a_registered_drone_is_left_alone() {
        // The idempotence that keeps a re-heard beacon from renumbering an
        // aircraft that may be flying.
        let mut registry = FleetRegistry::default();
        registry.allocate("drone-a");
        assert!(decide_enrollments(&[peer("drone-a", "drone")], &registry).is_empty());
    }

    #[test]
    fn a_ground_station_peer_is_never_issued_a_slot() {
        // A second ground station audible on the same key would otherwise
        // consume a slot out of a table only FLEET_MAX_SLOTS deep, costing a
        // real aircraft its address.
        let registry = FleetRegistry::default();
        assert!(decide_enrollments(&[peer("gs-b", "gs")], &registry).is_empty());
    }

    #[test]
    fn enrolment_writes_a_registry_an_auto_bound_pair_would_never_have_had() {
        // The whole point: no pair route was called, and a slot exists anyway.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        assert!(!path.exists(), "precondition: no registry yet");

        let issued = enrol_into(&path, &[peer("drone-a", "drone")]);

        assert_eq!(issued, vec![("drone-a".to_string(), 1)]);
        assert!(path.exists(), "the registry is now on disk");
        assert_eq!(FleetRegistry::load(&path).slot_of("drone-a"), Some(1));
    }

    #[test]
    fn enrolment_mints_the_per_pair_relay_secret() {
        // The credential lives on the registry entry, so a fleet with no
        // registry has no secrets. This is the reason enrolment blocks the
        // relay-credential work rather than merely tidying a table.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        enrol_into(&path, &[peer("drone-a", "drone")]);

        let registry = FleetRegistry::load(&path);
        let entry = registry
            .slots()
            .find(|s| s.device_id == "drone-a")
            .expect("registered");
        assert!(
            entry.relay_secret.is_some(),
            "an enrolled drone carries a per-pair relay secret"
        );
    }

    #[test]
    fn a_second_tick_over_the_same_beacon_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        let peers = [peer("drone-a", "drone")];

        enrol_into(&path, &peers);
        let after_first = std::fs::read(&path).unwrap();
        let issued_again = enrol_into(&path, &peers);

        assert!(issued_again.is_empty(), "no second issue");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            after_first,
            "and no rewrite, so the paired-at timestamp cannot drift"
        );
    }

    #[test]
    fn two_drones_take_distinct_slots() {
        // A shared slot is a shared channel_id, which thrashes both drones' FEC
        // decoders at about 1 Hz and presents as unexplained link loss.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");

        let issued = enrol_into(&path, &[peer("drone-a", "drone"), peer("drone-b", "drone")]);

        let slots: Vec<u8> = issued.iter().map(|(_, s)| *s).collect();
        assert_eq!(slots, vec![1, 2]);
    }

    #[test]
    fn a_full_fleet_declines_rather_than_evicting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        let existing: Vec<(String, String)> = (0..FLEET_MAX_SLOTS)
            .map(|i| peer(&format!("drone-{i}"), "drone"))
            .collect();
        enrol_into(&path, &existing);

        let issued = enrol_into(&path, &[peer("one-too-many", "drone")]);

        assert!(issued.is_empty(), "the newcomer is refused");
        let registry = FleetRegistry::load(&path);
        assert_eq!(registry.len(), FLEET_MAX_SLOTS as usize);
        assert_eq!(
            registry.slot_of("drone-0"),
            Some(1),
            "and no registered drone was evicted to make room"
        );
    }
}
