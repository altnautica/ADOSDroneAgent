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
//!   fresh boot, or a host with no detector running — the same shape the FastAPI
//!   route emits when its own HAL detect raises).
//! - **`/api/telemetry`** is the vehicle-state dict alone: the snapshot with the
//!   four runtime-only extras (`fc_connected`, `fc_port`, `fc_baud`,
//!   `service_uptime`) stripped, mirroring the Python `vehicle_state_dict`.
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

use crate::state::AppState;

/// The runtime-only keys the state snapshot carries alongside the vehicle state.
/// `/api/telemetry` strips them so it surfaces only the vehicle fields the GCS
/// expects; `/api/status` reads them as the FC connection triple + the service
/// uptime + the FC-liveness detail (transport_open / mavlink_alive /
/// heartbeat_age_s / fc_source / fc_link_hint). Mirrors the Python
/// `_ipc_only_keys` set.
const IPC_ONLY_KEYS: [&str; 11] = [
    "fc_connected",
    "fc_port",
    "fc_baud",
    "service_uptime",
    "transport_open",
    "mavlink_alive",
    "heartbeat_age_s",
    "fc_source",
    "fc_link_hint",
    "fc_variant",
    "fc_firmware",
];

/// External binaries the video pipeline may use, checked by presence on `PATH`.
/// Name + whether it is required, mirroring `check_video_dependencies`. The
/// status surface emits only `{name: found}`; the required flag is carried for
/// parity with the Python check list and is otherwise unused on this route.
const VIDEO_DEPENDENCIES: [&str; 5] = [
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
    // disconnected with the FastAPI defaults (port "", baud 0).
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
    let (npu_tops, has_accelerator, perception_tier, offload_target) = perception_fields(&board);

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
        "fc_baud": fc_baud,
        "transportOpen": liveness.transport_open,
        "mavlinkAlive": liveness.mavlink_alive,
        "heartbeatAgeS": liveness.heartbeat_age_s,
        "fcSource": liveness.fc_source,
        "fcLinkHint": liveness.fc_link_hint,
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
    // present only when their sidecars are fresh + valid. Mirrors the FastAPI
    // status route folding `_read_camera_status()` into the body.
    if let Some(obj) = body.as_object_mut() {
        for (key, value) in crate::routes::status_full::read_camera_status() {
            obj.insert(key, value);
        }
    }

    Json(body)
}

/// `GET /api/telemetry` → the vehicle-state dict from the live snapshot, with the
/// four runtime-only extras stripped. An absent snapshot returns `{}`, matching
/// the Python fallback when no state has arrived. Mirrors `vehicle_state_dict`.
pub async fn get_telemetry(State(state): State<AppState>) -> Json<Value> {
    let snapshot = state.state.snapshot();
    Json(project_telemetry(snapshot))
}

/// Build the `health` block from merged hardware signals, mirroring the FastAPI
/// `SystemHealth.to_dict()` shape: `{cpu_percent, memory_percent, disk_percent,
/// temperature, timestamp}`.
///
/// `cpu_percent` is the aggregate CPU utilization, `memory_percent` and
/// `disk_percent` are derived used/total ratios (the same arithmetic `psutil`
/// reports), `temperature` is the primary thermal zone. A `None` signal map (the
/// store unreachable) — or an individual missing signal — degrades that field to
/// the `SystemHealth` default: `0.0` for the percentages, `null` for temperature.
/// The timestamp is stamped at request time, as the Python `check_system()` does.
///
/// Shared with the consolidated `/api/status/full` route, which carries the same
/// `health` block sourced from the same store signals.
pub(crate) fn derive_health(signals: Option<&Map<String, Value>>) -> Value {
    let cpu = signals.and_then(|s| signal_num(s, "cpu.util.all"));
    let mem = signals.and_then(memory_percent);
    let disk = signals.and_then(disk_percent);
    let temp = signals.and_then(|s| signal_num(s, "thermal.primary_c"));

    json!({
        "cpu_percent": cpu.map(round1).unwrap_or(0.0),
        "memory_percent": mem.map(round1).unwrap_or(0.0),
        "disk_percent": disk.map(round1).unwrap_or(0.0),
        "temperature": temp.map(Value::from).unwrap_or(Value::Null),
        "timestamp": iso8601_utc_now(),
    })
}

/// Read the full HAL board dict from the board sidecar the detector persists. A
/// present, well-formed JSON object is returned verbatim; an absent file, a read
/// error, or a non-object body degrades to `{}` — the same shape the FastAPI
/// route emits when its own `detect_board()` raises.
///
/// Shared with the consolidated `/api/status/full` route, which serves the same
/// board block from the same sidecar.
/// Derive the perception capability + tier from the board block (the same board
/// sidecar [`read_board`] returns). `npu_tops` is the board's declared NPU
/// throughput; the tier is the canonical `ados_offload::pick_tier` decision (NOT
/// a second implementation), so `/api/status` reports the same tier the drone
/// runs on. An NPU board — or a board whose profile declares CPU-ONNX local
/// inference (`has_local_inference`) — reads `local`; an NPU-less board with no
/// such declaration and no offload path reads `none`; the offload-link the
/// reconciler writes (a paired reachable workstation) folds in when present. The
/// offload target stays null until a workstation is paired — never a fabricated
/// reach (Rule 44). Returns `(npu_tops, has_accelerator, tier)`.
///
/// Shared with the consolidated `/api/status/full` route so both report the same
/// perception fields.
pub(crate) fn perception_fields(board: &Value) -> (f64, bool, &'static str, Option<String>) {
    let npu_tops = board
        .get("npu_tops")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let has_accelerator = npu_tops > 0.0;
    // The board's declared CPU-ONNX local-inference capability (an NPU-less but
    // CPU-strong board runs the detector on-board). Absent on an older board
    // sidecar ⇒ false, so the tier is unchanged there (rule 44 — a local path is
    // reported only when the board really declares one).
    let local_inference_capable = board
        .get("has_local_inference")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // The live offload-link the reconciler writes: a paired, reachable workstation
    // flips compute_node_paired + bearer_acceptable true and names the target.
    // Absent / stale ⇒ no link ⇒ an NPU-less board reports `none` (rule 44 —
    // never a fabricated paired node). Fed identically here and on the cloud
    // heartbeat via `TierInputs::for_drone`.
    let link = ados_protocol::offload_link::read_offload_link(now_epoch_ms());
    let (paired, bearer_ok) = link
        .as_ref()
        .map(|l| (l.paired, l.bearer_acceptable))
        .unwrap_or((false, false));
    let tier = ados_offload::pick_tier(&ados_offload::TierInputs::for_drone(
        has_accelerator,
        local_inference_capable,
        paired,
        bearer_ok,
    ));
    let tier_str = match tier {
        Some(ados_offload::PerceptionTier::Local) => "local",
        Some(ados_offload::PerceptionTier::Offload) => "offload",
        Some(ados_offload::PerceptionTier::Hybrid) => "hybrid",
        None => "none",
    };
    // Surface the target only on an actual offload path (rule 44 — never a
    // reach we are not really using).
    let offload_target = link.filter(|l| l.is_offload_path()).and_then(|l| l.target);
    (npu_tops, has_accelerator, tier_str, offload_target)
}

/// Local epoch ms for the offload-link staleness gate.
fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn read_board(path: &Path) -> Value {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if value.is_object() => value,
            _ => json!({}),
        },
        Err(_) => json!({}),
    }
}

/// A numeric signal value, or `None` if absent / non-numeric. A JSON `bool` is
/// not a `Number`, so it is excluded naturally.
fn signal_num(signals: &Map<String, Value>, key: &str) -> Option<f64> {
    match signals.get(key) {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

/// Memory used percentage from the total + available byte signals: `(total -
/// available) / total * 100`, the same value `psutil.virtual_memory().percent`
/// reports. `None` when either signal is absent; `0.0` when total is non-positive.
fn memory_percent(signals: &Map<String, Value>) -> Option<f64> {
    let total = signal_num(signals, "mem.total_bytes")?;
    let avail = signal_num(signals, "mem.avail_bytes")?;
    if total <= 0.0 {
        return Some(0.0);
    }
    let used = (total - avail).max(0.0);
    Some(used / total * 100.0)
}

/// Filesystem used percentage from the total + used byte signals: `used / total *
/// 100`, the same value `psutil.disk_usage("/").percent` reports. `None` when
/// either signal is absent; `0.0` when total is non-positive.
fn disk_percent(signals: &Map<String, Value>) -> Option<f64> {
    let total = signal_num(signals, "disk.fs_total_bytes")?;
    let used = signal_num(signals, "disk.fs_used_bytes")?;
    if total <= 0.0 {
        return Some(0.0);
    }
    Some(used / total * 100.0)
}

/// Round to one decimal place, matching the canonical resource-field rounding.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// The current UTC time as an ISO-8601 string (`YYYY-MM-DDTHH:MM:SS+00:00`),
/// type-matching the Python `datetime.now(timezone.utc).isoformat()` the health
/// block carries. Second precision (the value is not byte-compared; it is a
/// freshness marker), `+00:00` offset.
fn iso8601_utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    iso8601_from_unix_secs(secs)
}

/// Format a Unix-epoch second count as `YYYY-MM-DDTHH:MM:SS+00:00` (UTC). Uses
/// the civil-from-days conversion so it is correct across month/year/leap
/// boundaries without a date-time dependency.
fn iso8601_from_unix_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Howard Hinnant's civil_from_days: days since the Unix epoch → (y, m, d).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}+00:00")
}

/// Strip the four runtime-only extras from a snapshot, leaving the vehicle-state
/// dict. An absent or non-object snapshot projects to `{}`.
///
/// Shared with the consolidated `/api/status/full` route, whose `telemetry` block
/// is the same vehicle-state projection.
pub(crate) fn project_telemetry(snapshot: Option<Value>) -> Value {
    match snapshot {
        Some(Value::Object(map)) => {
            // Honesty gate: when the FC link is explicitly not alive, the vehicle
            // fields are stale or default (no fresh HEARTBEAT decoded), so surface
            // nothing rather than zeros-as-live (0 alt, 0 battery, 360 heading,
            // HDOP 655). Absence of the flag (an older snapshot) keeps the prior
            // behavior — staleness cannot be proven, so the fields pass through.
            if map.get("mavlink_alive").and_then(Value::as_bool) == Some(false) {
                return json!({});
            }
            let projected: Map<String, Value> = map
                .into_iter()
                .filter(|(k, _)| !IPC_ONLY_KEYS.contains(&k.as_str()))
                .collect();
            Value::Object(projected)
        }
        _ => json!({}),
    }
}

/// Read the FC connection triple + the service uptime out of a snapshot's runtime
/// extras. Returns `(fc_connected, fc_port, fc_baud, uptime?)`. An absent snapshot
/// or absent fields fall back to the FastAPI defaults: `fc_connected:false`,
/// `fc_port:""`, `fc_baud:0`, and no uptime (the caller substitutes the process
/// uptime).
///
/// Shared with the consolidated `/api/status/full` route, which reads the same FC
/// triple + uptime from the same snapshot extras.
pub(crate) fn fc_from_snapshot(snapshot: Option<&Value>) -> (Value, Value, Value, Option<Value>) {
    let obj = snapshot.and_then(Value::as_object);
    let connected = obj
        .and_then(|m| m.get("fc_connected"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let port = obj
        .and_then(|m| m.get("fc_port"))
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!(""));
    let baud = obj
        .and_then(|m| m.get("fc_baud"))
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!(0));
    let uptime = obj
        .and_then(|m| m.get("service_uptime"))
        .cloned()
        .filter(|v| !v.is_null());
    (json!(connected), port, baud, uptime)
}

/// The FC-liveness detail the GCS lane reads, split out of the snapshot extras
/// the MAVLink router publishes. `transport_open` is whether the FC transport is
/// open; `mavlink_alive` is whether a fresh HEARTBEAT decoded within the router's
/// timeout; `heartbeat_age_s` is the seconds since the last HEARTBEAT (`null`
/// when none yet); `fc_source` is the configured transport class (`auto` /
/// `serial` / `udp` / `tcp`). Together they let the GCS show "port open · no
/// MAVLink" distinctly from connected, and validate the link is actually live.
pub(crate) struct FcLiveness {
    pub transport_open: bool,
    pub mavlink_alive: bool,
    pub heartbeat_age_s: Value,
    pub fc_source: Value,
    /// The not-alive diagnostic hint: `msp_detected` (the FC speaks MSP, not
    /// MAVLink, on this port), `no_heartbeat` (open but silent), or `none`.
    pub fc_link_hint: Value,
    /// The FC firmware family from the USB descriptor (`betaflight`/`inav`), or
    /// null for a MAVLink/unknown FC. Lets the GCS badge an MSP FC honestly.
    pub fc_variant: Value,
    /// The canonical FC firmware family: `ardupilot`/`px4` (MAVLink, from the
    /// decoded HEARTBEAT.autopilot discriminator) or `betaflight`/`inav` (MSP,
    /// from the USB descriptor), or `unknown` when no verified FC identity is
    /// available. Unlike `fc_variant` it distinguishes the MAVLink pair
    /// ArduPilot vs PX4, so a consumer can badge all four families.
    pub fc_firmware: Value,
}

/// Read the FC-liveness extras out of a snapshot. An absent snapshot or absent
/// fields fall back to the safe defaults: transport closed, not alive, no
/// heartbeat age, source `"auto"` (the config default). Shared with
/// `/api/status/full`.
pub(crate) fn fc_liveness_from_snapshot(snapshot: Option<&Value>) -> FcLiveness {
    let obj = snapshot.and_then(Value::as_object);
    FcLiveness {
        transport_open: obj
            .and_then(|m| m.get("transport_open"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        mavlink_alive: obj
            .and_then(|m| m.get("mavlink_alive"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        heartbeat_age_s: obj
            .and_then(|m| m.get("heartbeat_age_s"))
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or(Value::Null),
        fc_source: obj
            .and_then(|m| m.get("fc_source"))
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| json!("auto")),
        fc_link_hint: obj
            .and_then(|m| m.get("fc_link_hint"))
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| json!("none")),
        fc_variant: obj
            .and_then(|m| m.get("fc_variant"))
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or(Value::Null),
        // Always a concrete family string; an absent snapshot / field is
        // honestly `unknown`, never guessed.
        fc_firmware: obj
            .and_then(|m| m.get("fc_firmware"))
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| json!("unknown")),
    }
}

/// A truthful "the FC is reachable" signal. Unlike the heartbeat-gated
/// `fc_connected`, this is also true for a detected MSP FC on an open transport:
/// a Betaflight/iNav board never emits a MAVLink heartbeat, yet it is present
/// and reachable over the byte-pipe (identified by its USB descriptor or a
/// recent MSP frame sniff). It is false when the port is open but no FC is
/// evidenced (no heartbeat and no MSP), and when there is no transport at all —
/// it never claims a link that is not there.
pub(crate) fn derive_fc_reachable(liveness: &FcLiveness) -> bool {
    liveness.mavlink_alive
        || (liveness.transport_open
            && (!liveness.fc_variant.is_null() || liveness.fc_link_hint == json!("msp_detected")))
}

/// True when `name` is found as an executable on `PATH`. Mirrors
/// `shutil.which(name)` for the no-explicit-path case (the dependency names are
/// bare command names, never absolute paths): walk each `PATH` entry, join the
/// name, and accept the first regular file that is executable. Off Unix the
/// executable-bit check is skipped (existence is the signal).
fn binary_on_path(name: &str) -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| {
        let candidate = dir.join(name);
        is_executable_file(&candidate)
    })
}

/// True when `path` is a file that is executable by the current user. On Unix the
/// owner/group/other execute bits are checked; elsewhere existence as a file is
/// the signal.
#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Process uptime: seconds since this daemon started, the fallback the status
/// route uses when the snapshot carries no `service_uptime`. The Python route
/// falls back to the runtime's process uptime in the same way.
pub fn process_uptime_seconds(started: Instant) -> f64 {
    started.elapsed().as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn perception_fields_maps_board_caps_to_a_tier() {
        // An NPU board (npu_tops > 0) runs detection locally.
        let (npu, accel, tier, _target) = perception_fields(&json!({ "npu_tops": 6.0 }));
        assert_eq!(npu, 6.0);
        assert!(accel);
        assert_eq!(tier, "local");
        // A CPU board with no NPU + no paired node has no perception tier (no
        // offload-link sidecar in the test env ⇒ compute_node_paired false).
        let (npu, accel, tier, _target) = perception_fields(&json!({ "npu_tops": 0.0 }));
        assert_eq!(npu, 0.0);
        assert!(!accel);
        assert_eq!(tier, "none");
        // A board block with no npu_tops key degrades to no-accelerator/none.
        let (_npu, accel, tier, _target) = perception_fields(&json!({}));
        assert!(!accel);
        assert_eq!(tier, "none");
        // A CPU-strong board with no NPU but the profile-declared ONNX local
        // inference reads `local` (runs the detector on-board), with the
        // accelerator flag still false (it has no NPU).
        let (npu, accel, tier, _target) =
            perception_fields(&json!({ "npu_tops": 0.0, "has_local_inference": true }));
        assert_eq!(npu, 0.0);
        assert!(!accel);
        assert_eq!(tier, "local");
    }

    #[test]
    fn telemetry_strips_the_four_runtime_extras() {
        let snapshot = json!({
            "armed": true,
            "mode": "GUIDED",
            "battery": {"voltage": 16.4},
            "fc_connected": true,
            "fc_port": "/dev/ttyACM0",
            "fc_baud": 115200,
            "service_uptime": 99.0,
        });
        let tel = project_telemetry(Some(snapshot));
        let obj = tel.as_object().unwrap();
        // The vehicle keys survive.
        assert!(obj.contains_key("armed"));
        assert!(obj.contains_key("mode"));
        assert!(obj.contains_key("battery"));
        // The four extras are gone.
        for k in IPC_ONLY_KEYS {
            assert!(!obj.contains_key(k), "{k} must be stripped from telemetry");
        }
    }

    #[test]
    fn telemetry_of_an_absent_snapshot_is_an_empty_object() {
        assert_eq!(project_telemetry(None), json!({}));
    }

    #[test]
    fn fc_triple_defaults_to_disconnected_when_the_snapshot_is_absent() {
        let (connected, port, baud, uptime) = fc_from_snapshot(None);
        assert_eq!(connected, json!(false));
        assert_eq!(port, json!(""));
        assert_eq!(baud, json!(0));
        assert!(uptime.is_none());
    }

    #[test]
    fn fc_triple_reads_the_snapshot_extras() {
        let snap = json!({
            "fc_connected": true,
            "fc_port": "/dev/ttyACM0",
            "fc_baud": 115200,
            "service_uptime": 42.0,
        });
        let (connected, port, baud, uptime) = fc_from_snapshot(Some(&snap));
        assert_eq!(connected, json!(true));
        assert_eq!(port, json!("/dev/ttyACM0"));
        assert_eq!(baud, json!(115200));
        assert_eq!(uptime, Some(json!(42.0)));
    }

    #[test]
    fn fc_liveness_defaults_when_the_snapshot_is_absent() {
        let l = fc_liveness_from_snapshot(None);
        assert!(!l.transport_open);
        assert!(!l.mavlink_alive);
        assert_eq!(l.heartbeat_age_s, Value::Null);
        assert_eq!(l.fc_source, json!("auto"));
        assert_eq!(l.fc_link_hint, json!("none"));
        assert_eq!(l.fc_variant, Value::Null);
        // The firmware family is always a concrete string, defaulting to unknown.
        assert_eq!(l.fc_firmware, json!("unknown"));
    }

    #[test]
    fn fc_liveness_reads_the_snapshot_extras() {
        let snap = json!({
            "transport_open": true,
            "mavlink_alive": false,
            "heartbeat_age_s": 7.5,
            "fc_source": "serial",
            "fc_link_hint": "msp_detected",
            "fc_variant": "betaflight",
            "fc_firmware": "betaflight",
        });
        let l = fc_liveness_from_snapshot(Some(&snap));
        // The exact bug it guards: transport open but MAVLink not alive.
        assert!(l.transport_open);
        assert!(!l.mavlink_alive);
        assert_eq!(l.heartbeat_age_s, json!(7.5));
        assert_eq!(l.fc_source, json!("serial"));
        assert_eq!(l.fc_link_hint, json!("msp_detected"));
        assert_eq!(l.fc_variant, json!("betaflight"));
        assert_eq!(l.fc_firmware, json!("betaflight"));
    }

    #[test]
    fn fc_reachable_is_true_for_a_live_mavlink_fc() {
        let l = fc_liveness_from_snapshot(Some(&json!({
            "transport_open": true,
            "mavlink_alive": true,
            "fc_firmware": "ardupilot",
        })));
        assert!(derive_fc_reachable(&l));
    }

    #[test]
    fn fc_reachable_is_true_for_a_detected_msp_fc_on_an_open_port() {
        // Betaflight/iNav never heartbeats, but it is present + reachable: an
        // open transport plus the USB-descriptor variant is positive evidence.
        let by_variant = fc_liveness_from_snapshot(Some(&json!({
            "transport_open": true,
            "mavlink_alive": false,
            "fc_link_hint": "msp_detected",
            "fc_variant": "betaflight",
            "fc_firmware": "betaflight",
        })));
        assert!(derive_fc_reachable(&by_variant));

        // Or an unrecognised-descriptor MSP board evidenced by the frame sniff
        // alone (fc_variant null, hint msp_detected).
        let by_sniff = fc_liveness_from_snapshot(Some(&json!({
            "transport_open": true,
            "mavlink_alive": false,
            "fc_link_hint": "msp_detected",
        })));
        assert!(derive_fc_reachable(&by_sniff));
    }

    #[test]
    fn fc_reachable_is_false_for_a_silent_open_port_and_for_no_transport() {
        // Port open, no heartbeat, no MSP evidence: nothing proves an FC is
        // there, so reachability must not be claimed.
        let silent = fc_liveness_from_snapshot(Some(&json!({
            "transport_open": true,
            "mavlink_alive": false,
            "fc_link_hint": "no_heartbeat",
        })));
        assert!(!derive_fc_reachable(&silent));

        // No transport at all.
        assert!(!derive_fc_reachable(&fc_liveness_from_snapshot(None)));
    }

    #[test]
    fn telemetry_blanks_when_mavlink_not_alive() {
        // Transport open but the link is not alive: the vehicle fields are stale
        // defaults, so telemetry must surface nothing rather than zeros-as-live.
        let snapshot = json!({
            "armed": false,
            "mode": "STABILIZE",
            "battery": {"voltage": 0.0},
            "transport_open": true,
            "mavlink_alive": false,
            "fc_link_hint": "no_heartbeat",
        });
        assert_eq!(project_telemetry(Some(snapshot)), json!({}));
    }

    #[test]
    fn telemetry_passes_through_when_mavlink_alive() {
        // A live link surfaces the vehicle fields (minus the runtime-only extras).
        let snapshot = json!({
            "armed": true,
            "mode": "GUIDED",
            "mavlink_alive": true,
        });
        let tel = project_telemetry(Some(snapshot));
        let obj = tel.as_object().unwrap();
        assert_eq!(obj["armed"], json!(true));
        assert_eq!(obj["mode"], json!("GUIDED"));
        assert!(!obj.contains_key("mavlink_alive"));
    }

    #[test]
    fn telemetry_strips_the_fc_liveness_extras() {
        let snapshot = json!({
            "armed": true,
            "transport_open": true,
            "mavlink_alive": true,
            "heartbeat_age_s": 0.5,
            "fc_source": "serial",
        });
        let tel = project_telemetry(Some(snapshot));
        let obj = tel.as_object().unwrap();
        assert!(obj.contains_key("armed"));
        for k in [
            "transport_open",
            "mavlink_alive",
            "heartbeat_age_s",
            "fc_source",
        ] {
            assert!(!obj.contains_key(k), "{k} must be stripped from telemetry");
        }
    }

    #[test]
    fn fc_triple_treats_null_fields_as_the_defaults() {
        let snap = json!({
            "fc_connected": false,
            "fc_port": null,
            "fc_baud": null,
            "service_uptime": null,
        });
        let (connected, port, baud, uptime) = fc_from_snapshot(Some(&snap));
        assert_eq!(connected, json!(false));
        assert_eq!(port, json!(""));
        assert_eq!(baud, json!(0));
        assert!(uptime.is_none());
    }

    #[test]
    fn binary_on_path_finds_a_known_command() {
        // `sh` is on PATH on every unix test host; a nonsense name is not.
        #[cfg(unix)]
        {
            assert!(binary_on_path("sh"), "sh should be on PATH");
        }
        assert!(!binary_on_path("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn process_uptime_is_non_negative() {
        let t = Instant::now();
        assert!(process_uptime_seconds(t) >= 0.0);
    }

    fn signals(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn health_derives_the_five_fields_from_signals() {
        let s = signals(&[
            ("cpu.util.all", json!(12.34)),
            ("mem.total_bytes", json!(4000.0)),
            ("mem.avail_bytes", json!(1000.0)),
            ("disk.fs_total_bytes", json!(1000.0)),
            ("disk.fs_used_bytes", json!(250.0)),
            ("thermal.primary_c", json!(47.5)),
        ]);
        let h = derive_health(Some(&s));
        assert_eq!(h["cpu_percent"], json!(12.3)); // rounded to 1 decimal
        assert_eq!(h["memory_percent"], json!(75.0)); // (4000-1000)/4000*100
        assert_eq!(h["disk_percent"], json!(25.0)); // 250/1000*100
        assert_eq!(h["temperature"], json!(47.5));
        // The timestamp is a non-empty ISO string ending in the UTC offset.
        let ts = h["timestamp"].as_str().unwrap();
        assert!(ts.ends_with("+00:00") && ts.contains('T'), "ts: {ts}");
    }

    #[test]
    fn health_of_an_absent_store_is_the_zero_default() {
        let h = derive_health(None);
        assert_eq!(h["cpu_percent"], json!(0.0));
        assert_eq!(h["memory_percent"], json!(0.0));
        assert_eq!(h["disk_percent"], json!(0.0));
        assert_eq!(h["temperature"], Value::Null);
        // Even with no store the shape carries all five keys.
        assert!(h["timestamp"].is_string());
    }

    #[test]
    fn health_missing_temperature_is_null_others_default() {
        // Signals present but the thermal zone has not been sampled yet.
        let s = signals(&[("cpu.util.all", json!(5.0))]);
        let h = derive_health(Some(&s));
        assert_eq!(h["cpu_percent"], json!(5.0));
        assert_eq!(h["temperature"], Value::Null);
        assert_eq!(h["memory_percent"], json!(0.0));
    }

    #[test]
    fn read_board_returns_the_object_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        std::fs::write(&path, r#"{"name":"rpi4b","soc":"BCM2711","ram_mb":4096}"#).unwrap();
        let board = read_board(&path);
        assert_eq!(board["name"], json!("rpi4b"));
        assert_eq!(board["ram_mb"], json!(4096));
    }

    #[test]
    fn read_board_of_an_absent_file_is_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_board(&dir.path().join("nope.json")), json!({}));
    }

    #[test]
    fn read_board_of_a_non_object_body_is_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        std::fs::write(&path, "[1,2,3]").unwrap();
        assert_eq!(read_board(&path), json!({}));
    }

    #[test]
    fn iso8601_formats_known_epochs() {
        // Epoch zero.
        assert_eq!(iso8601_from_unix_secs(0), "1970-01-01T00:00:00+00:00");
        // 2021-01-01T00:00:00Z = 1609459200.
        assert_eq!(
            iso8601_from_unix_secs(1_609_459_200),
            "2021-01-01T00:00:00+00:00"
        );
        // Noon on a leap day exercises the Feb-29 path: 2020-02-29T12:00:00Z =
        // 1582977600.
        assert_eq!(
            iso8601_from_unix_secs(1_582_977_600),
            "2020-02-29T12:00:00+00:00"
        );
        // One second before midnight, 2024-12-31T23:59:59Z = 1735689599.
        assert_eq!(
            iso8601_from_unix_secs(1_735_689_599),
            "2024-12-31T23:59:59+00:00"
        );
    }
}
