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
    // samples CPU/memory/disk/temperature continuously); an unreachable store
    // degrades to the zero-valued SystemHealth default. Board from the sidecar the
    // detector persists; an absent file degrades to `{}`.
    let signals = state.logd.latest_hw_signals().await;
    let health = derive_health(signals.as_ref());
    let board = read_board(&state.board_path);

    Json(json!({
        "version": state.agent_version,
        "uptime_seconds": uptime,
        "board": board,
        "health": health,
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
