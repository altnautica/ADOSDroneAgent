//! The runtime extras the state snapshot carries on top of the vehicle fields.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use ados_swarm::ModePrecedence;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use super::startup::ProxyPosture;
use crate::aux_tee::TeeCounters;
use crate::aux_uplink;
use crate::aux_uplink_consumer::AuxUplinkConsumerCounters;
use crate::connection::swarm_setpoint::SwarmSetpointStatus;
use crate::connection::FcConnection;
use crate::frame_ingest::IngestCounters;
use crate::param_cache::ParamCache;
use crate::relayed::RelayedVehicle;
use crate::state::{firmware_family, VehicleState};

/// Build the runtime extras the state snapshot carries on top of the vehicle
/// fields.
///
/// `mavlink_drops` / `state_drops` are the monotonic slow-consumer eviction
/// counts from the two IPC servers, carried on the snapshot so a silently
/// pruned client is observable to Mission Control (it never sees an error of
/// its own when it is dropped for falling behind).
/// The honest connected-or-reachable verdict, from the same four inputs
/// `/api/status` and `/api/status/full` already gate on. True for a live
/// MAVLink heartbeat, and also true for a healthy MSP flight controller
/// (Betaflight/iNav) with its transport open — an MSP board never emits a
/// MAVLink heartbeat, so it would otherwise read as absent despite being
/// reachable and drivable over the byte-transparent proxy.
fn compute_fc_reachable(
    mavlink_alive: bool,
    transport_open: bool,
    has_fc_variant: bool,
    fc_link_hint: &str,
) -> bool {
    mavlink_alive || (transport_open && (has_fc_variant || fc_link_hint == "msp_detected"))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn build_extras(
    fc: &Arc<FcConnection>,
    state: &Arc<Mutex<VehicleState>>,
    params: &Arc<Mutex<ParamCache>>,
    started: Instant,
    mavlink_drops: u64,
    state_drops: u64,
    aux_tee_counters: Option<&Arc<TeeCounters>>,
    frame_ingest_counters: Option<&Arc<IngestCounters>>,
    aux_rpc_counters: Option<&AuxUplinkConsumerCounters>,
    swarm: Option<&Arc<SwarmSetpointStatus>>,
    uplink: Option<&aux_uplink::AuxUplinkSender>,
    relayed_vehicle: Option<&Arc<StdMutex<RelayedVehicle>>>,
    proxy_posture: &ProxyPosture,
) -> Map<String, Value> {
    // The cached param count and the map's change counter, read under one lock.
    let (cached, param_generation) = {
        let pc = params.lock().await;
        (pc.count(), pc.generation())
    };
    // The expected param count and the decoded autopilot code, read under one
    // lock. `autopilot` is the already-decoded HEARTBEAT discriminator (3 =
    // ArduPilot, 12 = PX4) used to name the MAVLink firmware family below.
    let (expected, autopilot) = {
        let s = state.lock().await;
        (s.param_count, s.autopilot)
    };
    let mut extras = Map::new();
    // The gated truth: fc_connected = transport_open && mavlink_alive. A port
    // that opens but never hears a HEARTBEAT reads transport_open:true but
    // fc_connected:false, so a broken link never shows "connected". Surface the
    // two halves + the heartbeat age + the configured source so a consumer can
    // render "port open · no MAVLink" distinctly and validate the link is live.
    let transport_open = fc.transport_open();
    let mavlink_alive = fc.mavlink_alive().await;
    extras.insert(
        "fc_connected".into(),
        json!(transport_open && mavlink_alive),
    );
    extras.insert("transport_open".into(), json!(transport_open));
    // Cumulative frames decoded off the FC link: the supervisor's proof that
    // the reader is working while the transport is open.
    extras.insert("fc_frames_decoded".into(), json!(fc.frames_decoded()));
    // Client flight-action frames dropped because this node is a visor.
    extras.insert(
        "visor_flight_frames_dropped".into(),
        json!(fc.visor_flight_frames_dropped()),
    );
    extras.insert("mavlink_alive".into(), json!(mavlink_alive));
    extras.insert(
        "heartbeat_age_s".into(),
        fc.heartbeat_age_s()
            .await
            .map(|v| json!(v))
            .unwrap_or(Value::Null),
    );
    extras.insert("fc_source".into(), json!(fc.source()));
    // The canonical device of the open serial FC link (symlinks resolved), so
    // the plugin sandbox can mask the port the router actually latched. Null
    // for a network source or no link.
    extras.insert("fc_device".into(), json!(fc.fc_device().await));
    // Whether declared autonomous injectors are gated here: `active`,
    // `inactive_no_local_arbiter` (the flag is on but this profile runs no
    // PIC arbiter) or `off`.
    extras.insert(
        "injector_arbitration".into(),
        json!(fc.injector_arbitration_state()),
    );
    // The FC answers signed on a link where it enforces signing, so the
    // router's own unsigned frames (stream rates, the parameter sweep, swarm
    // setpoints) are likely dropped. Null with no serial link to judge; false
    // while the agent signs its own frames.
    extras.insert(
        "agent_commands_unsigned_rejected_risk".into(),
        json!(fc.agent_commands_unsigned_rejected_risk().await),
    );
    // Agent-held signing: `off` (flag off), `active` (signing with the stored
    // key) or `no_key` (flag on, no key stored).
    extras.insert("agent_signing".into(), json!(fc.agent_signing().state()));
    // The flight controller's own (system, component) id, latched from its
    // HEARTBEAT. Absent until latched so a consumer never addresses a guess.
    if let Some((system_id, component_id)) = fc.learned_fc_identity() {
        extras.insert("fc_system_id".into(), json!(system_id));
        extras.insert("fc_component_id".into(), json!(component_id));
    }
    // A human-actionable hint for the not-alive case: msp_detected (the FC speaks
    // MSP, not MAVLink, on this port), no_heartbeat (open but silent), or none.
    let fc_link_hint = fc.link_hint().await;
    extras.insert("fc_link_hint".into(), json!(fc_link_hint));
    // Whether the open FC source is the MAVLink-over-ELRS ingest running
    // telemetry-only, its host->FC command-down direction gated closed. When
    // true the link can read alive (telemetry flows) yet a GCS command is
    // silently dropped — surface it so a consumer never renders a bare
    // "connected" as "commands are getting through". False for every ordinary
    // FC source (serial / UDP / TCP / discovery), whose command path is open.
    extras.insert(
        "fc_command_down_gated".into(),
        json!(fc.command_down_gated()),
    );
    // The FC firmware family identified from the port's USB descriptor
    // (betaflight/inav), or null for a MAVLink/unknown FC — lets a consumer
    // badge "Betaflight (MSP)" instead of a misleading "not connected".
    let fc_variant = fc.fc_variant().await;
    extras.insert(
        "fc_variant".into(),
        fc_variant.as_ref().map(|v| json!(v)).unwrap_or(Value::Null),
    );
    // The honest connected-or-reachable verdict, computed once here (the
    // router owns every input) so every consumer reads the SAME answer
    // instead of re-deriving it from the parts. `/api/status` and
    // `/api/status/full` recompute it from these identical fields today; this
    // is additionally the field the compact aux-lane node-status snapshot
    // reads, so a relayed MSP flight controller (Betaflight/iNav, which never
    // emits a MAVLink heartbeat) is reachable-and-drivable over the radio the
    // same way it already is over a direct LAN connection.
    let fc_reachable = compute_fc_reachable(
        mavlink_alive,
        transport_open,
        fc_variant.is_some(),
        fc_link_hint,
    );
    extras.insert("fc_reachable".into(), json!(fc_reachable));
    // The canonical firmware family (ardupilot/px4/betaflight/inav/unknown):
    // the MSP variant above, or — for a MAVLink FC — the live-heartbeat
    // autopilot code that names ArduPilot vs PX4 (which fc_variant cannot).
    // Read-only classification of already-decoded signals; no payload parsed.
    extras.insert(
        "fc_firmware".into(),
        json!(firmware_family(
            fc_variant.as_deref(),
            mavlink_alive,
            autopilot
        )),
    );
    extras.insert("fc_port".into(), json!(fc.port().await));
    extras.insert("fc_baud".into(), json!(fc.baud()));
    extras.insert(
        "service_uptime".into(),
        json!(started.elapsed().as_secs_f64()),
    );
    extras.insert("param_priming".into(), json!(fc.param_priming()));
    extras.insert(
        "param_sweep_timed_out".into(),
        json!(fc.param_sweep_timed_out()),
    );
    extras.insert(
        "param_sweep_send_failed".into(),
        json!(fc.param_sweep_send_failed()),
    );
    extras.insert("param_cached_count".into(), json!(cached));
    extras.insert("param_expected_count".into(), json!(expected));
    // The parameter map's change counter, NOT the map. The map used to ride here
    // in full — ~24 KB of JSON, no delta, no cap, republished ten times a second
    // — which alone made a relayed `/api/telemetry` need ~21 aux fragments and
    // fail 15% of the time. A consumer compares this counter against its own
    // last-seen value and refetches `GET /api/params` (served from the router's
    // on-disk cache) once on a mismatch. Restart-safe by being wrong in the
    // harmless direction: the counter restarts at 0, which reads as a mismatch
    // and costs exactly one refetch.
    extras.insert("param_generation".into(), json!(param_generation));
    extras.insert("ipc_mavlink_drops".into(), json!(mavlink_drops));
    extras.insert("ipc_state_drops".into(), json!(state_drops));
    // The MAVLink-over-radio tee's full tally: what it forwarded and, just as
    // importantly, everything it dropped and why. Present only where the tee
    // runs, so its absence reads as "not running on this profile" rather than
    // as a lane that carried nothing. Forwarded counts datagrams handed to the
    // kernel, which is not by itself proof the radio radiated them.
    if let Some(counters) = aux_tee_counters {
        extras.insert(
            "aux_mavlink_tee".into(),
            serde_json::to_value(counters.snapshot()).unwrap_or(Value::Null),
        );
    }
    // The receiving half's tally: what the republish seam accepted from
    // off-board, what it rejected, and what went nowhere for want of a
    // listener. Present only where the seam runs (a ground station), so its
    // absence reads as "not running on this profile" rather than as a seam that
    // received nothing. Published counts frames handed to the fan-out, which is
    // not by itself proof a ground control station rendered them.
    if let Some(counters) = frame_ingest_counters {
        extras.insert(
            "mavlink_frame_ingest".into(),
            serde_json::to_value(counters.snapshot()).unwrap_or(Value::Null),
        );
    }
    // The decoded reading of the relayed vehicle, when this node has ever seen
    // one. Carries its own provenance and freshness (see `crate::relayed`), and
    // is nested rather than merged so it can never be mistaken for the
    // attached-FC fields alongside it. Absent on a node that has never relayed
    // a vehicle, which is a different signal from one whose vehicle has gone
    // quiet (that case is present with `fresh: false`).
    if let Some(relayed) = relayed_vehicle {
        let snapshot = match relayed.lock() {
            Ok(r) => r.to_wire(Instant::now()),
            // Read through a poisoned lock rather than dropping the surface:
            // a blank reading is exactly the failure this projection exists to
            // fix, so it must not be reintroduced by a lock error.
            Err(poisoned) => poisoned.into_inner().to_wire(Instant::now()),
        };
        if let Some(value) = snapshot {
            extras.insert("relayed_vehicle".into(), value);
        }
    }
    // The relay-proxy uplink's tally: how many HTTP requests crossed the radio,
    // how many were dropped as another node's, and — the pair that says whether
    // the ground's retransmission is working — how many arrived as duplicates
    // or were answered from the dedupe cache. Present only on the drone
    // profile, which is the only one that consumes the uplink.
    if let Some(counters) = aux_rpc_counters {
        extras.insert(
            "aux_rpc".into(),
            serde_json::to_value(counters.snapshot()).unwrap_or(Value::Null),
        );
    }
    // Who the direct-GCS proxies serve (see `proxy_posture`), with each edge's
    // live listening flag.
    extras.insert("mavlink_proxy_posture".into(), proxy_posture.to_value());
    // The drone's video attention profile, so the swarm beacon's hero bit and the
    // fleet view read one source of truth. Owned by `ados-video`, which stamps
    // the sidecar on every encoder profile apply; this republishes it.
    extras.insert("video_profile".into(), json!(video_profile()));
    // The onboard-autonomy layer's two outputs. `ados-swarmbus` reads both out of
    // this snapshot and folds them into the outgoing beacon, which is how a
    // neighbour and the operator screen learn which layer is ACTUALLY flying this
    // aircraft rather than which one it was commanded into. Absent (the swarm
    // disabled, or no drone slot assigned) reads as the honest pre-Phase-5 state.
    let (precedence, emergency) = match swarm {
        Some(s) => (s.precedence_wire(), s.emergency()),
        None => (ModePrecedence::Hold.as_wire(), false),
    };
    extras.insert(
        crate::connection::swarm_setpoint::payload::EXTRA_PRECEDENCE.into(),
        json!(precedence),
    );
    // A real JSON bool, not 0/1: the beacon builder reads a non-bool as false, and
    // for a safety flag that is the wrong direction to fail in.
    extras.insert(
        crate::connection::swarm_setpoint::payload::EXTRA_EMERGENCY.into(),
        json!(emergency),
    );
    // Operator frames the ground station held because two linked aircraft
    // share the system id they were addressed to. Ground station only.
    if let Some(u) = uplink {
        extras.insert(
            "uplink_blocked_sysid_conflict".into(),
            json!(u.blocked_sysid_conflict()),
        );
        // Client frames the uplink dropped: queue full (the radio lane is
        // falling behind), or a trailing partial frame too large to carry.
        extras.insert(
            "aux_uplink_queue_full_dropped".into(),
            json!(u.queue_full_dropped()),
        );
        extras.insert(
            "aux_uplink_partial_dropped".into(),
            json!(u.partial_dropped()),
        );
    }
    extras
}

/// The sidecar `ados-video` stamps on every encoder profile apply. Read here
/// rather than tracked, so a video restart cannot desync the two services.
const VIDEO_PROFILE_SIDECAR: &str = "/run/ados/video-profile.json";

/// The drone's current video attention profile: `"hero"` (full-rate stream) or
/// `"thumbnail"` (1 fps). Anything other than a readable sidecar naming `hero`
/// reads as `thumbnail`, which is the correct boot default — a fleet powering up
/// together must not put 24 drones on a hero's worth of airtime each.
///
/// A tmpfs read at the 10 Hz publish cadence, so the value lags an encoder apply
/// by up to one tick. That is deliberate: the alternative is another IPC hop for
/// a single string.
fn video_profile() -> &'static str {
    let named_hero = std::fs::read_to_string(VIDEO_PROFILE_SIDECAR)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| {
            v.get("profile")
                .and_then(Value::as_str)
                .map(|p| p == "hero")
        })
        .unwrap_or(false);
    if named_hero {
        "hero"
    } else {
        "thumbnail"
    }
}

#[cfg(test)]
mod extras_key_set_tests;
#[cfg(test)]
mod fc_reachable_tests;
