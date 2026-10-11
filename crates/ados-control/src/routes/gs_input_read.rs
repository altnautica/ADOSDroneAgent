//! Ground-station input-device read routes (controllers + paired Bluetooth).
//!
//! Two read-only routes the GS setup UI + the GCS Hardware tab poll on a
//! ground-station node, both gated on the node resolving to the ground-station
//! profile. On a drone-profile node each answers `404` with the body
//! `{"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}}` — the same shape the
//! FastAPI `_require_ground_profile` gate raises, so the GCS distinguishes
//! "wrong profile" from "endpoint missing".
//!
//! - **`GET /api/v1/ground-station/gamepads`** — the attached controllers
//!   (`{devices, primary_id}`). `devices` is the live evdev enumeration of
//!   `/dev/input/event*` filtered to gamepads (two analog axes + a healthy
//!   button set), each rendered as the same `{device_id, name, path, vendor,
//!   product, type, connected}` record the Python `list_gamepads` returns;
//!   `primary_id` is the `primary` field of the `ground-station-input.json`
//!   sidecar (the input service's persisted primary selection), null when unset
//!   or unreadable. When evdev is unavailable (a non-Linux dev host, or a host
//!   without the input extra) the list is empty — the exact shape the Python
//!   route returns when its evdev import fails.
//! - **`GET /api/v1/ground-station/bluetooth/paired`** — the paired Bluetooth
//!   devices (`{devices}`). Sourced by running `bluetoothctl paired-devices` and
//!   parsing its `Device <MAC> <Name>` lines into the same `{device_id, mac,
//!   name, type, connected}` records the Python `paired_bluetooth` returns. A
//!   non-zero exit / a missing `bluetoothctl` degrades to an empty list, matching
//!   the Python `if rc != 0: return []` and its spawn-failure path.
//!
//! Both device lists are volatile (they depend on what is physically attached /
//! paired); the contract is the envelope shape + the profile gate, so the
//! conformance harness masks `devices` + `primary_id` and an empty list on a
//! bench ground station compares clean against either transport.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile gate (mirrors the FastAPI `_require_ground_profile`).
// ---------------------------------------------------------------------------

/// True when the node resolves to the ground-station profile. Resolves through
/// `current_profile_and_role` (the same source of truth the node advertises on
/// the wire), so a `profile: auto` node that resolves to a ground station via
/// `profile.conf` passes the gate, matching the Python `_require_ground_profile`.
fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// The `404` profile-mismatch response, byte-identical to the FastAPI
/// `HTTPException(status_code=404, detail={"error": {"code":
/// "E_PROFILE_MISMATCH"}})` (FastAPI wraps the `detail` dict under a top-level
/// `"detail"` key).
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/gamepads
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/gamepads` → the attached controllers + the
/// primary selection.
///
/// `404` `E_PROFILE_MISMATCH` off a ground-station node. Otherwise `{devices,
/// primary_id}`: `devices` is the live evdev enumeration (empty when evdev is
/// unavailable, the same fault-tolerant shape the Python route falls back to),
/// `primary_id` is the persisted primary device id or null.
///
/// The enumeration opens and ioctls every `/dev/input/event*` node and the
/// sidecar read is a blocking file read, so both run on the blocking pool under
/// [`ENUMERATION_TIMEOUT`]; a wedged input node answers `503` instead of parking
/// an async worker.
pub async fn get_gamepads(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let sidecar = gs_input_json_path(&state);
    let work = tokio::task::spawn_blocking(move || {
        (enumerate_gamepad_records(), primary_device_id(&sidecar))
    });
    match tokio::time::timeout(ENUMERATION_TIMEOUT, work).await {
        Ok(Ok((devices, primary_id))) => {
            Json(json!({ "devices": devices, "primary_id": primary_id })).into_response()
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "gamepad enumeration task failed");
            crate::routes::detail(
                StatusCode::SERVICE_UNAVAILABLE,
                "gamepad enumeration failed",
            )
        }
        Err(_) => crate::routes::detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "gamepad enumeration timed out",
        ),
    }
}

/// Upper bound on one evdev enumeration + sidecar read.
const ENUMERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// The live evdev enumeration rendered as the Python `list_gamepads` record set.
///
/// Reuses the shared gamepad enumeration (the same evdev seam + gamepad predicate
/// the input service uses) and maps each device to the seven-field dict the REST
/// layer returns: `device_id`, `name`, `path`, `vendor`, `product`, the static
/// `type: "usb"`, and `connected: true`. On a host where evdev cannot list
/// devices the underlying enumeration is empty, so this returns `[]` — the exact
/// shape the Python route returns when its evdev import / list fails.
fn enumerate_gamepad_records() -> Vec<Value> {
    enumerate_gamepads_snapshot()
        .into_iter()
        .map(|g| {
            json!({
                "device_id": g.device_id,
                "name": g.name,
                "path": g.path,
                "vendor": g.vendor,
                "product": g.product,
                "type": "usb",
                "connected": true,
            })
        })
        .collect()
}

/// The attached-gamepad set on Linux (the live evdev enumeration), in the stable
/// device-id order the shared snapshot keeps.
#[cfg(target_os = "linux")]
fn enumerate_gamepads_snapshot() -> Vec<ados_hid::input::Gamepad> {
    ados_hid::input::enumerate_gamepads()
        .into_values()
        .collect()
}

/// The non-Linux fallback: no evdev, so no controllers. Matches the Python route,
/// whose evdev import fails off a Linux host and yields `[]`.
#[cfg(not(target_os = "linux"))]
fn enumerate_gamepads_snapshot() -> Vec<ados_hid::input::Gamepad> {
    Vec::new()
}

/// The persisted primary device id from the `ground-station-input.json` sidecar,
/// or null when the file is absent / unreadable / carries no `primary`. Mirrors
/// the Python `get_primary()` over the same `{primary}` file.
fn primary_device_id(path: &Path) -> Value {
    match ados_hid::sidecar::GroundStationInput::load(path).and_then(|g| g.primary) {
        Some(id) => Value::String(id),
        None => Value::Null,
    }
}

/// The `ground-station-input.json` sidecar path. Honours `ADOS_GS_INPUT` (a test
/// override), else sits beside the agent config (`<config-dir>/ground-station-input.json`),
/// which on a real install is `/etc/ados/ground-station-input.json` — the same
/// path the Python input service persists the primary selection to.
fn gs_input_json_path(state: &AppState) -> PathBuf {
    if let Ok(p) = std::env::var("ADOS_GS_INPUT") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    state
        .pairing_paths
        .config
        .parent()
        .map(|dir| dir.join("ground-station-input.json"))
        .unwrap_or_else(|| PathBuf::from(ados_hid::sidecar::GS_INPUT_JSON))
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/bluetooth/paired
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/bluetooth/paired` → the paired Bluetooth devices.
///
/// `404` `E_PROFILE_MISMATCH` off a ground-station node. Otherwise `{devices}`: the
/// `bluetoothctl paired-devices` output parsed into one `{device_id, mac, name, type,
/// connected}` record per device. A non-zero exit / a missing `bluetoothctl` degrades
/// to an empty list rather than an error.
pub async fn get_bluetooth_paired(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let devices = paired_bluetooth_records().await;
    Json(json!({ "devices": devices })).into_response()
}

/// Run `bluetoothctl paired-devices` and render its output as the Python
/// `paired_bluetooth` record set. A non-zero exit, a missing `bluetoothctl`, a
/// spawn failure, or a probe timeout all yield the empty list (the Python route
/// returns `[]` on `rc != 0` and treats a spawn failure as `rc=127`).
async fn paired_bluetooth_records() -> Vec<Value> {
    let stdout = match bluetoothctl_paired_devices().await {
        Some(out) => out,
        None => return Vec::new(),
    };
    let mut records = Vec::new();
    for (mac, name) in parse_bt_device_lines(&stdout) {
        // `paired-devices` lists every bonded device whether or not it is in
        // range, so the link state is asked per device; no answer is unknown.
        let info =
            crate::probe::capture("bluetoothctl", &["info", &mac], crate::probe::PROBE_TIMEOUT)
                .await;
        let connected = info
            .is_ok()
            .then(|| bt_info_connected(info.text()))
            .flatten();
        records.push(json!({
            "device_id": device_id_for_bt(&mac),
            "mac": mac,
            "name": name,
            "type": "bluetooth",
            "connected": connected,
        }));
    }
    records
}

/// The `Connected:` line of a `bluetoothctl info <mac>` answer, or `None` when
/// the answer carries none.
fn bt_info_connected(info: &str) -> Option<bool> {
    info.lines().find_map(|l| {
        let v = l.trim().strip_prefix("Connected:")?.trim();
        match v {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        }
    })
}

/// The stdout of `bluetoothctl paired-devices`, or `None` when the command could
/// not be spawned, exited non-zero, or did not answer inside
/// [`crate::probe::SLOW_PROBE_TIMEOUT`]. Lossy-decodes the stdout the same way
/// the Python `_btctl` does (`decode(errors="replace")`).
///
/// Bounded via `probe::capture` rather than spawned inline: `bluetoothctl` talks
/// to `bluetoothd` over D-Bus and never returns when `bluetoothd` is wedged, so
/// a blocking spawn here parked a reactor worker for the life of the process.
/// `routes::gs_bluetooth::btctl` already made this exact call this way.
async fn bluetoothctl_paired_devices() -> Option<String> {
    match crate::probe::capture(
        "bluetoothctl",
        &["paired-devices"],
        crate::probe::SLOW_PROBE_TIMEOUT,
    )
    .await
    {
        crate::probe::ProbeOutput::Ok(out) => Some(out),
        crate::probe::ProbeOutput::Unavailable => None,
    }
}

/// Parse `Device <MAC> <Name>` lines from `bluetoothctl` output into `(mac,
/// name)` pairs. Byte-faithful to the Python `_parse_bt_device_lines`: each line
/// is stripped, only `Device `-prefixed lines are kept, the line is split into at
/// most three whitespace-delimited fields, the MAC is the second field, and the
/// name is the third field (falling back to the MAC when the name is absent).
fn parse_bt_device_lines(text: &str) -> Vec<(String, String)> {
    let mut devices = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if !line.starts_with("Device ") {
            continue;
        }
        // `split(None, 2)` in Python: at most three fields, splitting on runs of
        // whitespace, with the remainder (incl. internal spaces) kept in the last
        // field. `splitn(3, whitespace)` here would not collapse runs, so split
        // the head off explicitly and keep the rest verbatim.
        let parts = split_whitespace_max3(line);
        if parts.len() < 2 {
            continue;
        }
        let mac = parts[1].trim().to_string();
        let name = if parts.len() >= 3 {
            parts[2].trim().to_string()
        } else {
            mac.clone()
        };
        devices.push((mac, name));
    }
    devices
}

/// Split `line` into at most three fields on runs of ASCII whitespace, keeping
/// the remainder (including its own internal whitespace) verbatim in the third
/// field. Reproduces Python `str.split(None, 2)`: leading whitespace is skipped,
/// runs of whitespace collapse, and the third field is the untrimmed rest.
fn split_whitespace_max3(line: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut rest = line.trim_start();
    while out.len() < 2 {
        match rest.find(char::is_whitespace) {
            Some(idx) => {
                out.push(&rest[..idx]);
                // Skip the run of whitespace to the next field.
                rest = rest[idx..].trim_start();
            }
            None => {
                if !rest.is_empty() {
                    out.push(rest);
                }
                rest = "";
                break;
            }
        }
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// The stable Bluetooth device id `bt:<lowercase-mac>`. Byte-identical to the
/// Python `_device_id_for_bt`.
fn device_id_for_bt(mac: &str) -> String {
    format!("bt:{}", mac.to_lowercase())
}

#[cfg(test)]
mod tests;
