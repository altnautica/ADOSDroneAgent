//! Status + telemetry routes.
//!
//! Both read from the same vehicle-state snapshot the MAVLink service publishes
//! on `/run/ados/state.sock` (held by the [`StateIpcClient`](crate::ipc)), plus a
//! couple of process-level reads that have no IPC seam:
//!
//! - **`/api/status`** reports agent version, uptime, the FC connection triple,
//!   the external-binary dependency map, and the board + health blocks. The FC
//!   fields and the uptime come from the state snapshot's runtime extras; the
//!   dependency map is a `PATH` scan; **health** (CPU / memory / disk /
//!   temperature) is read from the most-recent hardware snapshots in the logging
//!   store (the continuous collector samples them, so the surface never probes
//!   the host itself), degrading to the zero-valued default when the store is
//!   unreachable; **board** (the full HAL dict) is read from the board sidecar the
//!   detector persists, degrading to an empty object when that file is absent (a
//!   fresh boot, or a host with no detector running).
//! - **`/api/telemetry`** is the vehicle-state dict with the runtime-only extras
//!   in `IPC_ONLY_KEYS` stripped. When the snapshot reports `mavlink_alive:
//!   false` the vehicle fields are withheld — they would be stale defaults — and
//!   only the agent's own diagnostics in `AGENT_DIAGNOSTIC_KEYS` are reported.
//!   A ground station has no flight controller, so that is its steady state.
//!
//!   That gate is correct for an ABSENT vehicle and was wrong for a RELAYED one.
//!   A ground station decoding another node's MAVLink off the radio holds a real
//!   reading of a real aircraft, and blanking it reported a healthy link as no
//!   link — the defect behind a dead horizon on the on-box cockpit. So when the
//!   snapshot carries a fresh `relayed_vehicle` block, its vehicle fields are
//!   projected in place of the withheld local ones, stamped
//!   `telemetry_source: "relayed"` with a `relayed_link` provenance summary.
//!   `fc_connected` and `mavlink_alive` are untouched and stay false: the
//!   vehicle is visible, not attached. A relayed reading that has gone stale is
//!   NOT projected — the lane is stamped and the fields stay blank, so a quiet
//!   aircraft never renders as a flying one.
//!
//! With no agent running (an empty snapshot, no store, no board sidecar), both
//! routes return a valid, GCS-parseable body rather than failing: status reports
//! `fc_connected:false` with a zero-valued health block and an empty board, and
//! telemetry returns `{}`.

use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::Json;
use serde_json::{json, Map, Value};

use crate::ipc::VisionIpcClient;
use crate::state::AppState;

mod helpers;
mod telemetry;
pub use helpers::*;
pub(crate) use telemetry::*;

/// The runtime-only keys the state snapshot carries alongside the vehicle state. `/api/telemetry`
/// strips them so it surfaces only the vehicle fields the GCS expects; `/api/status` reads them as
/// the FC connection triple + the service uptime + the FC-liveness detail (transport_open /
/// mavlink_alive / heartbeat_age_s / fc_source / fc_link_hint). set. `batteries` is the per-pack
/// BATTERY_STATUS list the battery engine reads and `/api/v1/battery` serves; the telemetry shape
/// keeps its single-pack `battery` block. `fc_frames_decoded` is the cumulative FC frame counter
/// the supervisor judges the router's work on. `fc_device` is the canonical
/// serial path of the FC link, read alongside `fc_port`.
const IPC_ONLY_KEYS: [&str; 15] = [
    "fc_connected",
    "fc_port",
    "fc_device",
    "fc_baud",
    "service_uptime",
    "transport_open",
    "mavlink_alive",
    "heartbeat_age_s",
    "fc_source",
    "fc_link_hint",
    "fc_command_down_gated",
    "fc_variant",
    "fc_firmware",
    "fc_frames_decoded",
    "batteries",
];

/// Runtime extras that describe the AGENT's own operation rather than the
/// vehicle's state.
///
/// The honesty gate in [`project_telemetry`] blanks vehicle fields when no
/// HEARTBEAT is decoding, because a stale reading rendered as live is a lie.
/// These keys are not vehicle readings — they are counters and flags the agent
/// maintains about itself, true whether or not a flight controller is attached
/// — so blanking them is backwards. A ground station never decodes a HEARTBEAT
/// at all, which left `mavlink_frame_ingest` unreadable through any HTTP
/// surface from the day it was added.
///
/// This is an ALLOWLIST on purpose. A key added to the producer's snapshot and
/// not listed here stays gated, which is the safe direction: a new vehicle
/// field must never leak past the gate, while a new counter merely stays
/// invisible until someone classifies it.
///
/// `param_generation` is here because it is the agent's own count of how many
/// times its parameter cache changed — the signal a consumer compares against its
/// last-seen value to decide whether to refetch `GET /api/params`. Withholding it
/// while the FC is down would strand every consumer on a stale map. The parameter
/// VALUES are deliberately not on the snapshot at all any more (they were ~24 KB
/// republished at 10 Hz); `GET /api/params` serves them ungated from the router's
/// on-disk cache.
///
/// `mavlink_proxy_posture` is the router's own configuration — which address
/// each direct-GCS proxy is bound to and whether a paired node's raw TCP/UDP
/// edges serve the LAN — so it answers "can a desktop ground station reach this
/// node" whether or not a flight controller is attached.
///
/// `swarm` is here for the same reason and is the strongest case of it: the swarm
/// bus's counters (`beacons_tx` / `beacons_rx` / `beacons_bad_magic` /
/// `beacons_bad_tag` / `beacons_stale_dropped` / `beacons_replayed` /
/// `beacons_slot_conflict` / `neighbors_now`) describe the
/// agent's own radio plane, and the node most likely to be diagnosed with them is a
/// ground station — which never decodes a HEARTBEAT, so the gate would blank them
/// permanently on exactly the node that needs them. The live table itself is served
/// from `/run/ados/swarm.sock` via `GET /api/swarm/neighbors` rather than from this
/// snapshot; the classification exists so folding the counters in alongside the aux
/// counters is a producer-side change with nothing left to remember here.
///
/// `swarm_precedence` and `swarm_emergency` are here for a sharper version of the
/// same reason. They are what the onboard-autonomy layer says about ITSELF — which
/// control layer is actually flying the aircraft, and whether its separation layer
/// has taken over — not readings off the vehicle. Gating them would blank the
/// operator's only truthful view of mode precedence on a ground station, and
/// mode-transition ambiguity is precisely the failure that field exists to prevent:
/// a blank is indistinguishable from `hold`, which would read as "nothing has
/// intervened" at exactly the moment something has.
///
/// `uplink_blocked_sysid_conflict` is the ground station's count of operator
/// frames it held because two linked aircraft share the system id they were
/// addressed to. A ground station never decodes a HEARTBEAT of its own, so the
/// gate would otherwise blank the one number that explains why a command did
/// not go out.
///
/// `agent_commands_unsigned_rejected_risk`, `agent_signing` and
/// `injector_arbitration` are the router's verdicts about its own command path
/// (whether the FC will drop the agent's unsigned frames, whether the agent
/// signs its own frames, and whether the FC-write arbiter is active), so they
/// stay visible while the FC link is down.
///
/// `visor_flight_frames_dropped` is the router's count of client frames it
/// refused to forward because this node is a visor; it describes the agent's
/// own command path.
///
/// Disjoint from [`IPC_ONLY_KEYS`] by construction; a test pins that.
const AGENT_DIAGNOSTIC_KEYS: [&str; 24] = [
    "agent_commands_unsigned_rejected_risk",
    "agent_signing",
    "aux_mavlink_tee",
    "aux_rpc",
    "aux_uplink_partial_dropped",
    "aux_uplink_queue_full_dropped",
    "fc_reachable",
    "injector_arbitration",
    "ipc_mavlink_drops",
    "ipc_state_drops",
    "mavlink_frame_ingest",
    "mavlink_proxy_posture",
    "param_cached_count",
    "param_expected_count",
    "param_generation",
    "param_priming",
    "param_sweep_send_failed",
    "param_sweep_timed_out",
    "swarm",
    "swarm_emergency",
    "swarm_precedence",
    "uplink_blocked_sysid_conflict",
    "video_profile",
    "visor_flight_frames_dropped",
];

/// External binaries the video pipeline may use, checked by presence on `PATH`:
/// the media server, then the encoder and capture tools. `/api/status` reports
/// `{name: found}`; `/api/video` reports `{name: {found, path}}` over the same
/// list.
pub(crate) const VIDEO_DEPENDENCIES: [&str; 5] = [
    "mediamtx",
    "ffmpeg",
    "rpicam-vid",
    "v4l2-ctl",
    "gst-launch-1.0",
];

/// `GET /api/status` → agent status: version, uptime, board, health, the FC
/// connection triple, and the dependency map.
///
/// The FC triple + uptime are read from the live state snapshot's runtime extras
/// (the MAVLink service publishes them alongside the vehicle state). The
/// dependency map is a `PATH` scan. `health` is derived from the most-recent
/// hardware snapshots in the logging store; `board` is read from the board
/// sidecar the detector persists. Both degrade in place (health to its
/// zero-valued default, board to `{}`) rather than failing — guaranteed-200,
/// never 500.
pub async fn get_status(State(state): State<AppState>) -> Json<Value> {
    let snapshot = state.state.snapshot();

    // FC connection triple + uptime from the snapshot extras. Absent snapshot →
    // disconnected with the defaults (port "", baud 0).
    let (fc_connected, fc_port, fc_baud, snapshot_uptime) = fc_from_snapshot(snapshot.as_ref());
    let uptime = snapshot_uptime.unwrap_or_else(|| json!(state.process_uptime_seconds()));

    let dependencies: Map<String, Value> = VIDEO_DEPENDENCIES
        .iter()
        .map(|name| (name.to_string(), json!(binary_on_path(name))))
        .collect();

    // Health from the store's most-recent hardware snapshots (the collector
    // samples CPU/memory/disk/temperature continuously). On a workstation / macOS
    // host the store's collector is not running, so the snapshot is absent — fall
    // back to a direct host read (`hw_local`) so the health block carries real
    // CPU/memory/disk instead of the zero-valued SystemHealth default. Board from
    // the sidecar the detector persists; when that file is absent (no detector on
    // a workstation host) the board is derived from the host instead of `{}`.
    let signals = match state.logd.latest_hw_signals().await {
        Some(s) => Some(s),
        None => {
            let local = crate::hw_local::collect_signals();
            (!local.is_empty()).then_some(local)
        }
    };
    let health = derive_health(signals.as_ref());
    let board = read_board(&state.board_path);
    let board = if board.as_object().map(Map::is_empty).unwrap_or(false) {
        crate::hw_local::host_board()
    } else {
        board
    };

    // The FC-liveness detail the GCS lane reads (camelCase, like the heartbeat):
    // transportOpen + mavlinkAlive split the truth so a broken-but-open link
    // renders "port open · no MAVLink", heartbeatAgeS validates the link is live,
    // fcSource reflects the picker choice. Present alongside the legacy snake
    // fc_connected (which is now the gated truth, not transport-open).
    let backend_inference_capable = live_backend_inference_capable().await;
    let (npu_tops, has_accelerator, perception_tier, offload_target) =
        perception_fields(&board, backend_inference_capable);

    let liveness = fc_liveness_from_snapshot(snapshot.as_ref());
    // Honest reachability: true for a live MAVLink FC OR a detected MSP FC on an
    // open transport (an MSP FC is present + reachable but never heartbeats, so
    // fc_connected is correctly false for it). Read before the liveness fields
    // are moved into the body below.
    let fc_reachable = derive_fc_reachable(&liveness);

    let mut body = json!({
        "version": state.agent_version(),
        "uptime_seconds": uptime,
        "board": board,
        "health": health,
        "fc_connected": fc_connected,
        "fc_port": fc_port,
        "sim": ados_config::simulation::simulation_enabled(),
        "fc_baud": fc_baud,
        "transportOpen": liveness.transport_open,
        "mavlinkAlive": liveness.mavlink_alive,
        "heartbeatAgeS": liveness.heartbeat_age_s,
        "fcSource": liveness.fc_source,
        "fcLinkHint": liveness.fc_link_hint,
        "fcCommandDownGated": liveness.fc_command_down_gated,
        "fcVariant": liveness.fc_variant,
        "fcFirmware": liveness.fc_firmware,
        "fcReachable": fc_reachable,
        "npuTops": npu_tops,
        "hasAccelerator": has_accelerator,
        "perceptionTier": perception_tier,
        "perceptionOffloadTarget": match &offload_target {
            Some(t) => Value::String(t.clone()),
            None => Value::Null,
        },
        "dependencies": Value::Object(dependencies),
    });

    // Fold in the camera presence + USB-recovery keys (the same `cameraState` /
    // `cameraUsbRecovery` the cloud heartbeat and the consolidated status carry),
    // present only when their sidecars are fresh + valid.
    if let Some(obj) = body.as_object_mut() {
        for (key, value) in crate::routes::status_full::read_camera_status(&state.run_dir) {
            obj.insert(key, value);
        }
        // The reconciler verdicts ride the same seam: `managementLink`,
        // `mgmtLinkMode`, `usbRehomeState`, `wifiPowersave`. Both status routes
        // fold them so a GCS on either endpoint sees the same reading.
        for (key, value) in crate::routes::status_full::read_reconciler_status(&state.run_dir) {
            obj.insert(key, value);
        }
    }

    Json(body)
}

/// `GET /api/telemetry` → the vehicle-state dict from the live snapshot, with the
/// four runtime-only extras stripped. An absent snapshot (no state has arrived
/// yet) returns `{}`.
pub async fn get_telemetry(State(state): State<AppState>) -> Json<Value> {
    let snapshot = state.state.snapshot();
    Json(project_telemetry(snapshot))
}

/// Build the `health` block from merged hardware signals: `{cpu_percent, memory_percent, disk_percent,
/// temperature, timestamp}`.
///
/// `cpu_percent` is the aggregate CPU utilization, `memory_percent` and
/// `disk_percent` are derived used/total ratios (the same arithmetic `psutil`
/// reports), `temperature` is the primary thermal zone. The timestamp is stamped
/// at request time.
///
/// ## An unmeasured field is `null`, never `0.0`
///
/// A `None` signal map (no store AND no local read) — or an individual missing
/// signal — carries that field as an explicit `null`. It used to carry `0.0`,
/// which is a legal reading: 0% CPU is an idle node and 0% disk is an empty one,
/// so a consumer had no way to tell "nothing is reporting" from "everything is
/// fine". `/api/status/full` reported exactly that on every stock node, because
/// the durable store ships off there and nothing fell back to a local read.
///
/// The key is always present and always serializes (an `Option` skipped from the
/// object would put the same ambiguity back one layer up, where a consumer reads
/// a missing key as "not reported" and coerces it). The sibling
/// `diagnostics::collect_system` and `system_resources` blocks already degrade
/// this way.
///
/// Shared with the consolidated `/api/status/full` route, which carries the same
/// `health` block sourced from the same store signals.
pub(crate) fn derive_health(signals: Option<&Map<String, Value>>) -> Value {
    let cpu = signals.and_then(|s| signal_num(s, "cpu.util.all"));
    let mem = signals.and_then(memory_percent);
    let disk = signals.and_then(disk_percent);
    let temp = signals.and_then(|s| signal_num(s, "thermal.primary_c"));

    json!({
        "cpu_percent": cpu.map(round1).map(Value::from).unwrap_or(Value::Null),
        "memory_percent": mem.map(round1).map(Value::from).unwrap_or(Value::Null),
        "disk_percent": disk.map(round1).map(Value::from).unwrap_or(Value::Null),
        "temperature": temp.map(Value::from).unwrap_or(Value::Null),
        "timestamp": iso8601_utc_now(),
    })
}

/// Read the full HAL board dict from the board sidecar the detector persists. A
/// present, well-formed JSON object is returned verbatim; an absent file, a read
/// error, or a non-object body degrades to `{}`.
///
/// Shared with the consolidated `/api/status/full` route, which serves the same
/// board block from the same sidecar.
/// Derive the perception capability + tier from the board block (the same board
/// sidecar [`read_board`] returns) plus the vision engine's live-reported
/// backend capability. `npu_tops` is the board's declared NPU throughput; the
/// tier is the canonical `ados_offload::pick_tier` decision (NOT a second
/// implementation), so `/api/status` reports the same tier the drone runs on.
///
/// A declared capability alone is NOT enough to claim `local`: a board can
/// declare an NPU (`npu_tops > 0`) with no in-tree backend for that silicon
/// (e.g. the A733's VIP9000 has none), or a CPU-ONNX profile
/// (`has_local_inference`) on a binary that was not built with the `onnx`
/// feature — `select_backend` then resolves to `MockBackend`, which loads
/// every model but returns zero detections. Both branches
/// are therefore gated on `backend_inference_capable`, the engine's own
/// `is_inference_capable()` read live over `/run/ados/vision.sock`
/// (`live_backend_inference_capable`) — an NPU board or an ONNX-declared board
/// reads `local` only when the backend actually running says so; an
/// unreachable engine (vision disabled, or not up yet) is `false`, never a
/// fabricated `true`. An NPU-less, non-ONNX board with no offload path reads
/// `none`; the offload-link the reconciler writes (a paired reachable
/// workstation) folds in when present. The offload target stays null until a
/// workstation is paired — never a fabricated reach. Returns
/// `(npu_tops, has_accelerator, tier, offload_target)`. `has_accelerator`
/// itself stays the raw hardware declaration (unaffected by
/// `backend_inference_capable`) — it answers "does this board have NPU
/// hardware", a distinct question from the tier.
///
/// Shared with the consolidated `/api/status/full` route so both report the
/// same perception fields from the same live backend read.
pub(crate) fn perception_fields(
    board: &Value,
    backend_inference_capable: bool,
) -> (f64, bool, &'static str, Option<String>) {
    let npu_tops = board
        .get("npu_tops")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let has_accelerator = npu_tops > 0.0;
    // The board's declared CPU-ONNX local-inference capability (an NPU-less but
    // CPU-strong board runs the detector on-board). Absent on an older board
    // sidecar ⇒ false, so the tier is unchanged there (a local path is
    // reported only when the board really declares one).
    let local_inference_capable = board
        .get("has_local_inference")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // The live offload-link the reconciler writes: a paired, reachable workstation
    // flips compute_node_paired + bearer_acceptable true and names the target.
    // Absent / stale ⇒ no link ⇒ an NPU-less board reports `none` (
    // never a fabricated paired node). Fed identically here and on the cloud
    // heartbeat via `TierInputs::for_drone`.
    let link = ados_protocol::offload_link::read_offload_link(now_epoch_ms());
    let (paired, bearer_ok) = link
        .as_ref()
        .map(|l| (l.paired, l.bearer_acceptable))
        .unwrap_or((false, false));
    let tier = ados_offload::pick_tier(&ados_offload::TierInputs::for_drone(
        has_accelerator && backend_inference_capable,
        local_inference_capable && backend_inference_capable,
        paired,
        bearer_ok,
    ));
    let tier_str = match tier {
        Some(ados_offload::PerceptionTier::Local) => "local",
        Some(ados_offload::PerceptionTier::Offload) => "offload",
        Some(ados_offload::PerceptionTier::Hybrid) => "hybrid",
        None => "none",
    };
    // Surface the target only on an actual offload path (never a
    // reach we are not really using).
    let offload_target = link.filter(|l| l.is_offload_path()).and_then(|l| l.target);
    (npu_tops, has_accelerator, tier_str, offload_target)
}

/// Whether the vision engine's actually-loaded backend runs real inference,
/// read LIVE over `/run/ados/vision.sock`'s `vision.list_models` reply — the
/// engine folds `backend_inference_capable` into that reply unconditionally
/// (see `VisionEngine::is_inference_capable`), so this is answered even when
/// no model is registered. An unreachable engine (vision disabled, the
/// service not up yet, or mid-restart) degrades to `false`: with nothing
/// loaded there is certainly no live inference to report, and a stale `true`
/// would be exactly the fabricated capability an honest surface forbids.
///
/// Shared by `/api/status` and `/api/status/full` so both feed
/// [`perception_fields`] the same live signal.
pub(crate) async fn live_backend_inference_capable() -> bool {
    match VisionIpcClient::default_socket().list_models().await {
        Ok(resp) => resp
            .as_map()
            .and_then(|m| {
                m.iter()
                    .find(|(k, _)| k.as_str() == Some("backend_inference_capable"))
            })
            .and_then(|(_, v)| v.as_bool())
            .unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests;
