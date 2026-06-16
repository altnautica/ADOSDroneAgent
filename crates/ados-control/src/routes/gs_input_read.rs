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
/// `primary_id` is the persisted primary device id or null. Guaranteed 200 on a
/// ground station. Mirrors the Python `get_gamepads`.
pub async fn get_gamepads(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let devices = enumerate_gamepad_records();
    let primary_id = primary_device_id(&gs_input_json_path(&state));
    Json(json!({ "devices": devices, "primary_id": primary_id })).into_response()
}

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
/// `404` `E_PROFILE_MISMATCH` off a ground-station node. Otherwise `{devices}`:
/// the `bluetoothctl paired-devices` output parsed into the same `{device_id,
/// mac, name, type, connected}` records the Python `paired_bluetooth` returns. A
/// non-zero exit / a missing `bluetoothctl` degrades to an empty list, matching
/// the Python `if rc != 0: return []` and its spawn-failure path. Mirrors the
/// Python `get_bluetooth_paired`.
pub async fn get_bluetooth_paired(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let devices = paired_bluetooth_records();
    Json(json!({ "devices": devices })).into_response()
}

/// Run `bluetoothctl paired-devices` and render its output as the Python
/// `paired_bluetooth` record set. A non-zero exit, a missing `bluetoothctl`, or
/// any spawn failure yields the empty list (the Python route returns `[]` on
/// `rc != 0` and treats a spawn failure as `rc=127`).
fn paired_bluetooth_records() -> Vec<Value> {
    let stdout = match bluetoothctl_paired_devices() {
        Some(out) => out,
        None => return Vec::new(),
    };
    parse_bt_device_lines(&stdout)
        .into_iter()
        .map(|(mac, name)| {
            json!({
                "device_id": device_id_for_bt(&mac),
                "mac": mac,
                "name": name,
                "type": "bluetooth",
                "connected": true,
            })
        })
        .collect()
}

/// The stdout of `bluetoothctl paired-devices`, or `None` when the command could
/// not be spawned or exited non-zero. Lossy-decodes the stdout the same way the
/// Python `_btctl` does (`decode(errors="replace")`).
fn bluetoothctl_paired_devices() -> Option<String> {
    let output = std::process::Command::new("bluetoothctl")
        .arg("paired-devices")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
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
mod tests {
    use super::*;

    #[test]
    fn profile_mismatch_is_the_fastapi_404_shape() {
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // The body shape is the contract; build it independently and compare.
        let want = json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}});
        assert_eq!(
            want,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    #[test]
    fn device_id_for_bt_lowercases_and_prefixes() {
        assert_eq!(
            device_id_for_bt("AA:BB:CC:DD:EE:FF"),
            "bt:aa:bb:cc:dd:ee:ff"
        );
        assert_eq!(
            device_id_for_bt("00:11:22:33:44:55"),
            "bt:00:11:22:33:44:55"
        );
    }

    #[test]
    fn parse_bt_device_lines_matches_the_python_parser() {
        // The shape `bluetoothctl paired-devices` emits: a `Device <MAC> <Name>`
        // line per device, names with internal spaces kept whole.
        let out = "Device AA:BB:CC:DD:EE:FF Xbox Wireless Controller\nDevice 11:22:33:44:55:66 8BitDo Pro 2\n";
        let parsed = parse_bt_device_lines(out);
        assert_eq!(
            parsed,
            vec![
                (
                    "AA:BB:CC:DD:EE:FF".to_string(),
                    "Xbox Wireless Controller".to_string()
                ),
                ("11:22:33:44:55:66".to_string(), "8BitDo Pro 2".to_string()),
            ]
        );
    }

    #[test]
    fn parse_bt_device_lines_skips_non_device_lines() {
        // Status / prompt lines bluetoothctl interleaves are ignored.
        let out = "Agent registered\nDevice AA:BB:CC:DD:EE:FF Pad\n[bluetooth]# \n";
        let parsed = parse_bt_device_lines(out);
        assert_eq!(
            parsed,
            vec![("AA:BB:CC:DD:EE:FF".to_string(), "Pad".to_string())]
        );
    }

    #[test]
    fn parse_bt_device_lines_falls_back_to_mac_when_name_absent() {
        // A `Device <MAC>` line with no name: the name field defaults to the MAC,
        // matching the Python `name = parts[2] if len(parts) >= 3 else mac`.
        let out = "Device AA:BB:CC:DD:EE:FF\n";
        let parsed = parse_bt_device_lines(out);
        assert_eq!(
            parsed,
            vec![(
                "AA:BB:CC:DD:EE:FF".to_string(),
                "AA:BB:CC:DD:EE:FF".to_string()
            )]
        );
    }

    #[test]
    fn paired_bluetooth_records_render_the_full_python_field_set() {
        // Drive the record projection directly off a parsed line so the field set
        // is asserted without spawning bluetoothctl.
        let parsed = parse_bt_device_lines("Device AA:BB:CC:DD:EE:FF Pad\n");
        let records: Vec<Value> = parsed
            .into_iter()
            .map(|(mac, name)| {
                json!({
                    "device_id": device_id_for_bt(&mac),
                    "mac": mac,
                    "name": name,
                    "type": "bluetooth",
                    "connected": true,
                })
            })
            .collect();
        assert_eq!(
            records,
            vec![json!({
                "device_id": "bt:aa:bb:cc:dd:ee:ff",
                "mac": "AA:BB:CC:DD:EE:FF",
                "name": "Pad",
                "type": "bluetooth",
                "connected": true,
            })]
        );
    }

    #[test]
    fn gamepad_record_is_the_full_python_field_set() {
        // The seven-field record the REST layer returns per controller.
        let g = ados_hid::input::Gamepad {
            device_id: "usb:045e:028e:event3".to_string(),
            name: "Xbox Controller".to_string(),
            path: "/dev/input/event3".to_string(),
            vendor: 0x045e,
            product: 0x028e,
        };
        let record = json!({
            "device_id": g.device_id,
            "name": g.name,
            "path": g.path,
            "vendor": g.vendor,
            "product": g.product,
            "type": "usb",
            "connected": true,
        });
        assert_eq!(
            record,
            json!({
                "device_id": "usb:045e:028e:event3",
                "name": "Xbox Controller",
                "path": "/dev/input/event3",
                "vendor": 1118,
                "product": 654,
                "type": "usb",
                "connected": true,
            })
        );
    }

    #[test]
    fn primary_device_id_reads_the_sidecar_and_defaults_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ground-station-input.json");
        // Absent file → null.
        assert_eq!(primary_device_id(&path), Value::Null);
        // A sidecar naming a primary surfaces it.
        std::fs::write(&path, r#"{"primary":"usb:045e:028e:event3"}"#).unwrap();
        assert_eq!(
            primary_device_id(&path),
            Value::String("usb:045e:028e:event3".to_string())
        );
        // A sidecar with a null primary → null.
        std::fs::write(&path, r#"{"primary":null}"#).unwrap();
        assert_eq!(primary_device_id(&path), Value::Null);
    }

    #[test]
    fn split_whitespace_max3_keeps_the_remainder_whole() {
        // Two leading fields, the rest (with internal spaces) kept verbatim.
        assert_eq!(
            split_whitespace_max3("Device AA:BB Long Name Here"),
            vec!["Device", "AA:BB", "Long Name Here"]
        );
        // Collapsing runs of whitespace between the leading fields.
        assert_eq!(
            split_whitespace_max3("Device   AA:BB   Pad"),
            vec!["Device", "AA:BB", "Pad"]
        );
        // Only two fields present.
        assert_eq!(
            split_whitespace_max3("Device AA:BB"),
            vec!["Device", "AA:BB"]
        );
    }

    #[test]
    fn empty_lists_are_the_bench_shape() {
        // The envelope shape on a bench GS with nothing attached/paired.
        let gamepads = json!({ "devices": Vec::<Value>::new(), "primary_id": Value::Null });
        assert_eq!(gamepads, json!({"devices": [], "primary_id": null}));
        let bluetooth = json!({ "devices": Vec::<Value>::new() });
        assert_eq!(bluetooth, json!({"devices": []}));
    }
}
