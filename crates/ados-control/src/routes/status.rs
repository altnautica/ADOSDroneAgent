//! Status + telemetry routes.
//!
//! Both read from the same vehicle-state snapshot the MAVLink service publishes
//! on `/run/ados/state.sock` (held by the [`StateIpcClient`](crate::ipc)), plus a
//! couple of process-level reads that have no IPC seam:
//!
//! - **`/api/status`** reports agent version, uptime, the FC connection triple,
//!   the external-binary dependency map, and the board + health blocks. The FC
//!   fields and the uptime come from the state snapshot's runtime extras; the
//!   dependency map is a `PATH` scan; the board + health blocks are live
//!   HAL/process reads with no native source wired into this surface yet, so they
//!   emit as empty dicts (the same shape the FastAPI route emits when its own HAL
//!   detect raises). See the module note on those two below.
//! - **`/api/telemetry`** is the vehicle-state dict alone: the snapshot with the
//!   four runtime-only extras (`fc_connected`, `fc_port`, `fc_baud`,
//!   `service_uptime`) stripped, mirroring the Python `vehicle_state_dict`.
//!
//! With no agent running (an empty snapshot), both routes return a valid,
//! GCS-parseable body rather than failing: status reports `fc_connected:false`
//! with empty board/health, and telemetry returns `{}`.

use std::time::Instant;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Map, Value};

use crate::state::AppState;

/// The four runtime-only keys the state snapshot carries alongside the vehicle
/// state. `/api/telemetry` strips them so it surfaces only the vehicle fields the
/// GCS expects; `/api/status` reads them as the FC connection triple + the
/// service uptime. Mirrors the Python `_ipc_only_keys` set.
const IPC_ONLY_KEYS: [&str; 4] = ["fc_connected", "fc_port", "fc_baud", "service_uptime"];

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
/// dependency map is a `PATH` scan. The board + health blocks are live
/// HAL/process reads this surface does not source natively yet, so they emit as
/// empty dicts of the right shape; the FastAPI route's own `board` is likewise
/// `{}` when its HAL detect raises. Guaranteed-200, never 500.
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

    Json(json!({
        "version": state.agent_version,
        "uptime_seconds": uptime,
        // Live HAL / process reads with no native source on this surface yet; the
        // FastAPI route emits {} for board when its detect raises, and the health
        // shape is a dict either way. Reported as a gap for the cutover review.
        "board": json!({}),
        "health": json!({}),
        "fc_connected": fc_connected,
        "fc_port": fc_port,
        "fc_baud": fc_baud,
        "dependencies": Value::Object(dependencies),
    }))
}

/// `GET /api/telemetry` → the vehicle-state dict from the live snapshot, with the
/// four runtime-only extras stripped. An absent snapshot returns `{}`, matching
/// the Python fallback when no state has arrived. Mirrors `vehicle_state_dict`.
pub async fn get_telemetry(State(state): State<AppState>) -> Json<Value> {
    let snapshot = state.state.snapshot();
    Json(project_telemetry(snapshot))
}

/// Strip the four runtime-only extras from a snapshot, leaving the vehicle-state
/// dict. An absent or non-object snapshot projects to `{}`.
fn project_telemetry(snapshot: Option<Value>) -> Value {
    match snapshot {
        Some(Value::Object(map)) => {
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
fn fc_from_snapshot(snapshot: Option<&Value>) -> (Value, Value, Value, Option<Value>) {
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
}
