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

/// The drone-side route that accepts the per-pair relay secret.
///
/// A dedicated route rather than a config key: a credential written into the
/// config file is a credential displayed by every surface that renders config
/// and written by every path that logs it.
const DRONE_RELAY_SECRET_PATH: &[u8] = b"/api/relay/peer-secret";

/// One drone that should be told the secret its ground station issued it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretDelivery {
    pub device_id: String,
    pub secret: String,
}

/// Which drones still need their relay secret.
///
/// Pure, like [`decide_tick`] beside it. `acknowledged` holds the device ids
/// that have already confirmed the secret this ground station holds for them, so
/// the steady state is silent -- a reconciler that restates a credential on
/// every tick forever is a standing airtime cost and a standing exposure.
///
/// A registry entry with no secret is skipped rather than treated as an empty
/// one. Those exist: the field is optional so an older registry loads, and such
/// an entry is issued a secret the next time it is allocated, not here.
pub fn decide_secret_tick(
    slots: &[FleetSlot],
    acknowledged: &std::collections::BTreeSet<String>,
) -> Vec<SecretDelivery> {
    slots
        .iter()
        .filter(|s| !acknowledged.contains(&s.device_id))
        .filter_map(|s| {
            s.relay_secret.as_ref().map(|secret| SecretDelivery {
                device_id: s.device_id.clone(),
                secret: secret.clone(),
            })
        })
        .collect()
}

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
pub async fn deliver(
    proxy: &Arc<AuxRpcProxy>,
    slots: &[FleetSlot],
    assignment: &SlotAssignment,
) -> Result<(), String> {
    let body = json!({ "key": FLEET_SLOT_KEY, "value": assignment.slot }).to_string();
    let ticket = mint_ticket(slots, &assignment.device_id);
    match proxy
        .call_with_ticket(
            assignment.device_id.as_bytes(),
            RpcMethod::Put,
            DRONE_CONFIG_PATH,
            body.as_bytes(),
            ticket.as_bytes(),
        )
        .await
    {
        Ok(resp) if (200..300).contains(&resp.status) => Ok(()),
        Ok(resp) => Err(format!("drone answered HTTP {}", resp.status)),
        Err(e) => Err(format!("{e}")),
    }
}

/// What a delivered secret offer settled.
#[derive(Debug, PartialEq, Eq)]
pub enum SecretOutcome {
    /// The drone took it, or already held this one.
    Held,
    /// The drone holds a DIFFERENT secret: it is paired to another ground
    /// station, or was and never unpaired. Retrying cannot fix it, so it is
    /// settled (reported once) rather than re-offered every tick.
    Conflict,
}

/// Map the drone's answer to an offer. Any other status is a transient refusal
/// the next tick retries.
fn secret_outcome(status: u16) -> Result<SecretOutcome, String> {
    match status {
        200..=299 => Ok(SecretOutcome::Held),
        409 => Ok(SecretOutcome::Conflict),
        other => Err(format!("drone answered HTTP {other}")),
    }
}

/// Mint a relay ticket for `device_id` from the secret `slots` holds for it.
///
/// Every ground-station-to-drone call except the secret delivery itself goes
/// out with one: the drone refuses any relayed request that does not carry a
/// ticket it can verify, so a caller that skips this is refused outright.
/// `slots` is the registry snapshot the caller is already working from, so a
/// fan-out across the fleet reads the registry once rather than per call.
///
/// Empty when the snapshot holds no secret for that drone, which encodes
/// byte-identically to the request it always sent. That is the compatibility
/// hinge: a drone running a build that predates the ticket field would refuse a
/// frame carrying one, and this is what guarantees it never receives one.
pub fn mint_ticket(slots: &[FleetSlot], device_id: &str) -> String {
    let Some(secret) = slots
        .iter()
        .find(|s| s.device_id == device_id)
        .and_then(|s| s.relay_secret.as_deref())
    else {
        return String::new();
    };
    ados_protocol::relay_ticket::RelayTicketIssuer::from_secret(secret.as_bytes()).mint_at(
        device_id,
        ados_protocol::relay_ticket::DEFAULT_TTL_SECONDS,
        now_unix_secs(),
    )
}

/// Wall-clock unix seconds. The drone checks the ticket's expiry against its own
/// wall clock, so the stamp has to come from the same kind of clock.
fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Offer one drone the relay secret its ground station issued it.
///
/// Sent WITHOUT a ticket, necessarily: the drone cannot verify one until it
/// holds the secret this call is delivering. That is the trust-on-first-use
/// window the scheme states plainly, and it is bounded by the drone's own
/// first-write-wins rule -- a drone that already holds a secret answers 409 and
/// keeps what it has.
pub async fn deliver_secret(
    proxy: &Arc<AuxRpcProxy>,
    delivery: &SecretDelivery,
) -> Result<SecretOutcome, String> {
    let body = json!({ "secret": delivery.secret }).to_string();
    match proxy
        .call(
            delivery.device_id.as_bytes(),
            RpcMethod::Post,
            DRONE_RELAY_SECRET_PATH,
            body.as_bytes(),
        )
        .await
    {
        // 200 covers both "accepted" and "already held" -- the drone treats a
        // restatement as a no-op, and so does this; 409 is a settled conflict.
        Ok(resp) => secret_outcome(resp.status),
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
    // Which drones have taken the relay secret. In memory for the same reason
    // the slot acknowledgements are: on restart the ground station re-offers
    // once, which the drone answers as already-held at no cost, and that is
    // strictly safer than trusting a persisted claim about a drone that may have
    // been re-flashed while we were down.
    let mut secret_acked: std::collections::BTreeSet<String> = Default::default();
    loop {
        tick.tick().await;
        let slots = registered_slots();

        // The secret goes first. Slot delivery now carries a ticket, and the
        // drone cannot verify one until it holds the secret -- so offering the
        // credential before the call that depends on it is what keeps the very
        // first reconcile from being refused by the gate it just armed.
        let present_ids: std::collections::BTreeSet<&str> =
            slots.iter().map(|s| s.device_id.as_str()).collect();
        secret_acked.retain(|device_id| present_ids.contains(device_id.as_str()));
        for delivery in decide_secret_tick(&slots, &secret_acked) {
            match deliver_secret(&proxy, &delivery).await {
                Ok(SecretOutcome::Held) => {
                    tracing::info!(device_id = %delivery.device_id, "relay_secret_delivered");
                    secret_acked.insert(delivery.device_id);
                }
                Ok(SecretOutcome::Conflict) => {
                    tracing::warn!(
                        device_id = %delivery.device_id,
                        "relay_secret_conflict: the drone holds a different relay secret; unpair it to re-key"
                    );
                    // Settled: no further offers until it leaves the fleet.
                    secret_acked.insert(delivery.device_id);
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
        // A drone that left the fleet stops being tracked, so its slot can be
        // reissued to someone else without a stale acknowledgement suppressing
        // the delivery.
        let present: std::collections::BTreeSet<&str> =
            slots.iter().map(|s| s.device_id.as_str()).collect();
        confirmed.retain(|device_id, _| present.contains(device_id.as_str()));

        for assignment in decide_tick(&slots, &confirmed) {
            match deliver(&proxy, &slots, &assignment).await {
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

/// The fleet registry's current slots, read fresh. Never cached:
/// `ados-groundlink` owns the writes and a drone can pair at any moment, so a
/// cached copy is stale the instant it matters.
pub(crate) fn registered_slots() -> Vec<FleetSlot> {
    ados_groundlink::FleetRegistry::load(std::path::Path::new(ados_groundlink::FLEET_REGISTRY_PATH))
        .slots()
        .cloned()
        .collect()
}

/// A ground station's relay lane with one drone on the far end that runs the
/// drone's REAL relay authorization (`ados_mavlink_router::aux_rpc_handler::
/// authorize`) before answering. Shared by every test of a relayed caller, so
/// a caller that forgets its ticket fails the way it would against an
/// aircraft instead of passing against a stub that answers anything.
#[cfg(test)]
pub(crate) mod test_drone {
    use std::sync::Arc;
    use std::time::Duration;

    use ados_protocol::aux_egress::AuxEgress;
    use ados_protocol::aux_mux::{self, AuxChannel};
    use ados_protocol::aux_rpc::{self, RpcMethod};
    use ados_protocol::aux_rpc_proxy::AuxRpcProxy;
    use tokio::net::UdpSocket;

    /// A 32-byte relay secret, hex, as the ground station issues one.
    pub const SECRET: &str = "5f1c0e2a9b7d4c3e8a6f0b1d2c3e4f5a6b7c8d9e0f1a2b3c4d5e6f708192a3b4";

    /// One request the drone received and how it answered it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Seen {
        pub method: RpcMethod,
        pub path: Vec<u8>,
        pub body: Vec<u8>,
        pub status: u16,
    }

    /// The ground station's end of the lane, and what the drone saw.
    pub struct TestDrone {
        pub proxy: Arc<AuxRpcProxy>,
        pub seen: Arc<parking_lot::Mutex<Vec<Seen>>>,
    }

    /// Start a drone called `device_id` holding `held` as its relay secret.
    /// It answers 200 to every request `authorize` admits and 401 to the rest.
    pub async fn spawn(device_id: &'static str, held: Option<&'static str>) -> TestDrone {
        let radio = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = radio.local_addr().unwrap().port();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", port)).await.unwrap();
        let proxy = Arc::new(AuxRpcProxy::with_timeout(
            AuxEgress::connected_for_test(sock),
            Duration::from_secs(2),
        ));
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));

        let ground = Arc::clone(&proxy);
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, _)) = radio.recv_from(&mut buf).await {
                let Ok((AuxChannel::Request, payload)) = aux_mux::decode(&buf[..n]) else {
                    continue;
                };
                let Ok(request) = aux_rpc::decode_request(payload) else {
                    continue;
                };
                if request.target != device_id.as_bytes() {
                    continue;
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64;
                let status = match ados_mavlink_router::aux_rpc_handler::authorize(
                    held,
                    request.ticket,
                    request.method,
                    request.path,
                    device_id,
                    now,
                ) {
                    Ok(()) => 200,
                    Err(_) => 401,
                };
                log.lock().push(Seen {
                    method: request.method,
                    path: request.path.to_vec(),
                    body: request.body.to_vec(),
                    status,
                });
                let symbols = aux_rpc::split_response(&[]).unwrap();
                let total = symbols.symbols.len() as u16;
                for (index, symbol) in symbols.symbols.iter().enumerate() {
                    let fragment = aux_rpc::encode_response_fragment(
                        device_id.as_bytes(),
                        request.id,
                        status,
                        index as u16,
                        total,
                        symbols.oti,
                        symbol,
                    )
                    .unwrap();
                    ground
                        .dispatch_response(&aux_rpc::decode_response(&fragment).unwrap())
                        .await;
                }
            }
        });
        TestDrone { proxy, seen }
    }
}

#[cfg(test)]
mod tests {
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
}
