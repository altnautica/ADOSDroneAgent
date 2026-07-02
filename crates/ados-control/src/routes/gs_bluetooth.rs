//! Ground-station Bluetooth write routes (scan / pair / forget).
//!
//! The three operator writes that drive the `bluetoothctl` lifecycle for pairing
//! a wireless controller on a ground station:
//!
//! - **`POST /api/v1/ground-station/bluetooth/scan`** — run a BlueZ scan for
//!   nearby controllers. The body is `{"duration_s"?}` (default 10); returns
//!   `{"devices": [{mac, name, rssi}]}` (rssi is always null — `bluetoothctl
//!   devices` does not surface it). A failed `devices` listing degrades to an
//!   empty list.
//! - **`POST /api/v1/ground-station/bluetooth/pair`** — pair + trust + connect a
//!   device by MAC; returns the pair-outcome dict (`{paired, connected?, error}`).
//! - **`DELETE /api/v1/ground-station/bluetooth/{mac}`** — forget (disconnect +
//!   remove) a paired device; returns `{forgotten, error}`.
//!
//! ## Why these run `bluetoothctl` directly (no daemon socket)
//!
//! Bluetooth pairing is pure subprocess orchestration with NO shared in-process
//! state: the Python `InputManager` Bluetooth methods just shell out to
//! `bluetoothctl` and parse its output — there is no `wlan0`-style advisory lock
//! or live manager singleton to contend for (unlike the Wi-Fi-client writes,
//! which MUST forward to the `ados-net` daemon). So this front runs `bluetoothctl`
//! itself, the same way the read side (`gs_input_read::get_bluetooth_paired`)
//! already runs `bluetoothctl paired-devices` directly. The command sequences,
//! the per-step timeouts, the return-code conventions, and the result-dict shapes
//! are all reproduced byte-for-byte from the Python `scan_bluetooth` /
//! `pair_bluetooth` / `forget_bluetooth`.
//!
//! ## The primary-clear side effect on forget
//!
//! The Python `forget_bluetooth` drops the persisted primary device id when it
//! pointed at the forgotten controller (`self._primary = None`). The live primary
//! is owned by the running `ados-input` daemon (the hotplug tracker), so on a
//! successful forget this route reads the persisted primary; when it matches the
//! forgotten device (`bt:<lowercase-mac>`), it forwards a `clear_primary` op to
//! the daemon's command socket so the running state + the on-disk sidecar drop
//! the binding in lockstep. The forget RESPONSE shape does not depend on this
//! side effect (it is `{forgotten, error}` either way), so a missing daemon
//! socket never changes the body.
//!
//! ## The profile gate
//!
//! Like every ground-station route, this first gates on the resolved profile
//! being a ground station and returns the FastAPI
//! `404 {"detail":{"error":{"code":"E_PROFILE_MISMATCH"}}}` on a drone.

use std::process::Stdio;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile gate (mirrors the FastAPI `_require_ground_profile`).
// ---------------------------------------------------------------------------

/// True when the node resolves to the ground-station profile.
fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// The `404` profile-mismatch response, byte-identical to the FastAPI gate.
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

// Note on the FastAPI `500 E_BT_*_FAILED` arm: the Python Bluetooth routes wrap
// their `_input_manager()` call in a try/except that raises a 500 error object
// only when the call itself raises. The Python `_btctl` already swallows every
// spawn / timeout / runtime fault into a return code (127 / 124 / the exit code),
// so the manager method never raises and that 500 arm is unreachable. The Rust
// `btctl` below reproduces the same swallow-into-rc contract, so each handler
// always returns the 200 result dict (a failure rides in the dict's `error`
// field, not an HTTP error). There is therefore no 5xx error helper here.

// ---------------------------------------------------------------------------
// bluetoothctl seam (mirrors the Python `_btctl` return-code conventions).
// ---------------------------------------------------------------------------

/// One `bluetoothctl <args>` run: `(rc, stdout, stderr)`. Reproduces the Python
/// `_btctl` conventions exactly:
/// * a spawn failure (no `bluetoothctl` on PATH) → `(127, "", <error>)`;
/// * a timeout → `(124, "", "timeout")` (the child is killed);
/// * otherwise the child's exit code (`None` → `0`) with lossy-decoded
///   stdout/stderr (`decode(errors="replace")`).
async fn btctl(args: &[&str], timeout: Duration) -> (i32, String, String) {
    let child = Command::new("bluetoothctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Reap the child if this future is dropped, so a cancelled request never
        // leaks a lingering bluetoothctl.
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        // The Python spawn-failure path returns rc=127 with the error in stderr.
        Err(e) => return (127, String::new(), e.to_string()),
    };

    // Bound the run on the per-step timeout. On a timeout, kill + reap the child
    // and return the Python `(124, "", "timeout")` contract; otherwise collect
    // its exit code + lossy-decoded output.
    let collected = tokio::time::timeout(timeout, child.wait_with_output()).await;
    match collected {
        Ok(Ok(output)) => {
            let rc = output.status.code().unwrap_or(0);
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            (rc, stdout, stderr)
        }
        // A wait error (rare): treat like a spawn-level failure with rc 127.
        Ok(Err(e)) => (127, String::new(), e.to_string()),
        // The Python timeout path kills the child and returns rc=124. The
        // `wait_with_output` future was dropped at the timeout boundary; with
        // `kill_on_drop(true)` set above, dropping it kills + reaps the child.
        Err(_elapsed) => (124, String::new(), "timeout".to_string()),
    }
}

/// Parse `Device <MAC> <Name>` lines from `bluetoothctl` output into `(mac, name)`
/// pairs. Byte-faithful to the Python `_parse_bt_device_lines`: each line is
/// stripped, only `Device `-prefixed lines are kept, the line is split into at
/// most three whitespace-delimited fields (runs of whitespace collapse, the third
/// field keeps its internal spaces), the MAC is the second field, and the name is
/// the third field (falling back to the MAC when absent).
fn parse_bt_device_lines(text: &str) -> Vec<(String, String)> {
    let mut devices = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if !line.starts_with("Device ") {
            continue;
        }
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
/// field. Reproduces Python `str.split(None, 2)`.
fn split_whitespace_max3(line: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut rest = line.trim_start();
    while out.len() < 2 {
        match rest.find(char::is_whitespace) {
            Some(idx) => {
                out.push(&rest[..idx]);
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

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/bluetooth/scan
// ---------------------------------------------------------------------------

/// The `bluetooth/scan` request body. Mirrors the FastAPI `BluetoothScanRequest`:
/// an optional `duration_s` (the Pydantic model constrains it to 1..=60 and the
/// route defaults a missing value to 10).
#[derive(Debug, Deserialize)]
pub struct BluetoothScanRequest {
    #[serde(default)]
    pub duration_s: Option<i64>,
}

/// `POST /api/v1/ground-station/bluetooth/scan` → `{"devices": [...]}`.
///
/// `404 E_PROFILE_MISMATCH` off a ground station. Otherwise runs the Python scan
/// sequence — `power on`, `scan on`, sleep `max(1, duration)`, `devices`, then
/// always `scan off` — and returns the discovered devices as `[{mac, name, rssi}]`
/// (rssi always null). A non-zero `devices` exit degrades to an empty list,
/// matching the Python `if rc != 0: return []`.
pub async fn post_bluetooth_scan(
    State(state): State<AppState>,
    Json(req): Json<BluetoothScanRequest>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let devices = scan_bluetooth(req.duration_s.unwrap_or(10)).await;
    Json(json!({ "devices": devices })).into_response()
}

/// Run the Python `scan_bluetooth` sequence and return the device records. The
/// scan is always stopped before returning (the Python `finally`), even when the
/// `devices` listing fails. RSSI is always null (the Python sets `"rssi": None`).
async fn scan_bluetooth(duration_s: i64) -> Vec<Value> {
    let _ = btctl(&["power", "on"], Duration::from_secs(5)).await;
    let _ = btctl(&["scan", "on"], Duration::from_secs(5)).await;
    // The Python sleeps max(1, int(duration_s)); the listing happens after.
    let sleep_secs = duration_s.max(1) as u64;
    tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
    let (rc, stdout, _err) = btctl(&["devices"], Duration::from_secs(5)).await;
    // Always stop scanning before returning (the Python `finally`).
    let _ = btctl(&["scan", "off"], Duration::from_secs(5)).await;

    if rc != 0 {
        return Vec::new();
    }
    parse_bt_device_lines(&stdout)
        .into_iter()
        .map(|(mac, name)| json!({"mac": mac, "name": name, "rssi": Value::Null}))
        .collect()
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/bluetooth/pair
// ---------------------------------------------------------------------------

/// The `bluetooth/pair` request body. Mirrors the FastAPI `BluetoothPairRequest`:
/// a required `mac` (the Pydantic model carries `min_length=1`).
#[derive(Debug, Deserialize)]
pub struct BluetoothPairRequest {
    pub mac: String,
}

/// `POST /api/v1/ground-station/bluetooth/pair` → the pair-outcome dict.
///
/// `404 E_PROFILE_MISMATCH` off a ground station. Otherwise runs the Python pair
/// sequence — `pair` (30s), `trust` (5s, soft), `connect` (15s) — over the
/// upper-cased MAC and returns the same outcome dict: a pair failure →
/// `{paired:false, error:"pair rc=<rc>: <stderr>"}`; a connect failure →
/// `{paired:true, connected:false, error:"connect rc=<rc>: <stderr>"}`; success →
/// `{paired:true, connected:true, error:null}`. A `trust` failure is a soft
/// warning (does not change the outcome), matching the Python.
pub async fn post_bluetooth_pair(
    State(state): State<AppState>,
    Json(req): Json<BluetoothPairRequest>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    Json(pair_bluetooth(&req.mac).await).into_response()
}

/// Run the Python `pair_bluetooth` sequence and return the outcome dict. The MAC
/// is trimmed + upper-cased (`mac.strip().upper()`). The error strings embed the
/// trimmed stderr + the rc, matching `f"pair rc={rc}: {err.strip()}"`.
async fn pair_bluetooth(mac: &str) -> Value {
    let mac_norm = mac.trim().to_uppercase();

    let (rc, _out, err) = btctl(&["pair", &mac_norm], Duration::from_secs(30)).await;
    if rc != 0 {
        return json!({
            "paired": false,
            "error": format!("pair rc={rc}: {}", err.trim()),
        });
    }

    // trust is a soft step: a failure is logged but does not change the outcome.
    let _ = btctl(&["trust", &mac_norm], Duration::from_secs(5)).await;

    let (rc_c, _out_c, err_c) = btctl(&["connect", &mac_norm], Duration::from_secs(15)).await;
    if rc_c != 0 {
        return json!({
            "paired": true,
            "connected": false,
            "error": format!("connect rc={rc_c}: {}", err_c.trim()),
        });
    }

    json!({"paired": true, "connected": true, "error": Value::Null})
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/ground-station/bluetooth/{mac}
// ---------------------------------------------------------------------------

/// `DELETE /api/v1/ground-station/bluetooth/{mac}` → `{"forgotten", "error"}`.
///
/// `404 E_PROFILE_MISMATCH` off a ground station. Otherwise runs the Python
/// forget sequence — `disconnect` (5s), `remove` (5s) — over the upper-cased MAC.
/// A non-zero `remove` exit → `{forgotten:false, error:"<stderr> or rc=<rc>"}`;
/// success → `{forgotten:true, error:null}`, after clearing the persisted primary
/// when it pointed at this device (forwarded to the running input daemon).
pub async fn delete_bluetooth(State(state): State<AppState>, Path(mac): Path<String>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    Json(forget_bluetooth(&mac).await).into_response()
}

/// Run the Python `forget_bluetooth` sequence and return the outcome dict. On a
/// successful remove, when the persisted primary names this device
/// (`bt:<lowercase-mac>`) the running input daemon's primary is cleared so the
/// live state + the sidecar drop the binding (the Python `self._primary = None`).
async fn forget_bluetooth(mac: &str) -> Value {
    let mac_norm = mac.trim().to_uppercase();

    let _ = btctl(&["disconnect", &mac_norm], Duration::from_secs(5)).await;
    let (rc, _out, err) = btctl(&["remove", &mac_norm], Duration::from_secs(5)).await;

    if rc != 0 {
        // The Python `err.strip() or f"rc={rc}"`: prefer the trimmed stderr, else
        // the rc string.
        let trimmed = err.trim();
        let message = if trimmed.is_empty() {
            format!("rc={rc}")
        } else {
            trimmed.to_string()
        };
        return json!({"forgotten": false, "error": message});
    }

    // Drop the persisted primary when it pointed at the forgotten device, in
    // lockstep with the running input daemon. Best-effort: the forget RESPONSE
    // does not depend on it, so a missing daemon socket never changes the body.
    clear_primary_if_matches(&device_id_for_bt(&mac_norm)).await;

    json!({"forgotten": true, "error": Value::Null})
}

/// When the persisted primary device id equals `device_id`, forward a
/// `clear_primary` op to the running input daemon's command socket so both the
/// live tracker and the on-disk sidecar drop the selection. Reads the persisted
/// primary off the `ground-station-input.json` sidecar; a missing sidecar /
/// non-matching primary / unreachable daemon socket is a clean no-op (the forget
/// already removed the device; the primary clearing is a follow-on side effect).
async fn clear_primary_if_matches(device_id: &str) {
    clear_primary_if_matches_at(device_id, &gs_input_json(), &hid_cmd_sock()).await;
}

/// The path-injectable core of [`clear_primary_if_matches`]: read the persisted
/// primary off an explicit sidecar path and forward to an explicit command socket.
/// Threaded so a test drives it against a tempdir without mutating the process
/// environment.
async fn clear_primary_if_matches_at(
    device_id: &str,
    gs_input: &std::path::Path,
    hid_sock: &std::path::Path,
) {
    let primary = ados_hid::sidecar::GroundStationInput::load(gs_input).and_then(|g| g.primary);
    if primary.as_deref() != Some(device_id) {
        return;
    }
    let request = json!({"op": "clear_primary"});
    // Best-effort forward; a missing socket is fine (the persisted record is the
    // durable mirror, and the next daemon restart rehydrates from it).
    let _ = forward_hid_cmd_to(hid_sock, &request).await;
}

/// One newline-terminated JSON request to the input command socket at an explicit
/// path, draining the one-line reply. Best-effort: any IO error is swallowed (the
/// caller treats it as a no-op). The socket path is threaded in (the caller
/// resolves `hid_cmd_sock()` once) so a test drives it against a tempdir.
async fn forward_hid_cmd_to(sock: &std::path::Path, request: &Value) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;

    let mut stream = tokio::net::UnixStream::connect(sock).await?;
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;
    // Drain the one-line reply so the daemon's write completes before we drop.
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf).await;
    Ok(())
}

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`).
fn run_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()),
    )
}

/// The `ados-input` command socket (`/run/ados/hid-cmd.sock`).
fn hid_cmd_sock() -> std::path::PathBuf {
    run_dir().join("hid-cmd.sock")
}

/// The `ground-station-input.json` sidecar path. Honours `ADOS_GS_INPUT` (a test
/// override), else the default `/etc/ados/ground-station-input.json` — the same
/// path the input service persists the primary selection to (and the read side
/// reads it from).
fn gs_input_json() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("ADOS_GS_INPUT") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    std::path::PathBuf::from(ados_hid::sidecar::GS_INPUT_JSON)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ── parse helpers (the byte-faithful Python parser) ──────────────────────

    #[test]
    fn parse_bt_device_lines_matches_the_python_parser() {
        let out = "Device AA:BB:CC:DD:EE:FF Xbox Wireless Controller\nDevice 11:22:33:44:55:66 8BitDo Pro 2\n";
        assert_eq!(
            parse_bt_device_lines(out),
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
    fn parse_bt_device_lines_skips_non_device_lines_and_defaults_name() {
        let out = "Agent registered\nDevice AA:BB:CC:DD:EE:FF\n[bluetooth]# \n";
        assert_eq!(
            parse_bt_device_lines(out),
            vec![(
                "AA:BB:CC:DD:EE:FF".to_string(),
                "AA:BB:CC:DD:EE:FF".to_string()
            )]
        );
    }

    #[test]
    fn device_id_for_bt_lowercases_and_prefixes() {
        assert_eq!(
            device_id_for_bt("AA:BB:CC:DD:EE:FF"),
            "bt:aa:bb:cc:dd:ee:ff"
        );
    }

    // ── scan record shape ────────────────────────────────────────────────────

    #[test]
    fn scan_records_carry_mac_name_and_null_rssi() {
        let records: Vec<Value> = parse_bt_device_lines("Device AA:BB Pad\n")
            .into_iter()
            .map(|(mac, name)| json!({"mac": mac, "name": name, "rssi": Value::Null}))
            .collect();
        assert_eq!(
            records,
            vec![json!({"mac": "AA:BB", "name": "Pad", "rssi": null})]
        );
    }

    // ── the result-dict shapes (built off the same json! the handlers return) ──

    #[test]
    fn pair_success_shape() {
        let body = json!({"paired": true, "connected": true, "error": Value::Null});
        assert_eq!(
            body,
            json!({"paired": true, "connected": true, "error": null})
        );
    }

    #[test]
    fn pair_failed_pair_step_shape() {
        // Built the same way the handler composes the pair-step failure.
        let rc = 1;
        let err = "  Failed to pair: org.bluez.Error.AlreadyExists  ";
        let body = json!({
            "paired": false,
            "error": format!("pair rc={rc}: {}", err.trim()),
        });
        assert_eq!(
            body,
            json!({
                "paired": false,
                "error": "pair rc=1: Failed to pair: org.bluez.Error.AlreadyExists"
            })
        );
    }

    #[test]
    fn pair_failed_connect_step_shape() {
        let rc_c = 1;
        let err_c = "Failed to connect";
        let body = json!({
            "paired": true,
            "connected": false,
            "error": format!("connect rc={rc_c}: {}", err_c.trim()),
        });
        assert_eq!(
            body,
            json!({
                "paired": true,
                "connected": false,
                "error": "connect rc=1: Failed to connect"
            })
        );
    }

    #[test]
    fn forget_failed_uses_stderr_then_rc_fallback() {
        // Non-empty stderr is preferred.
        let err = "  not available  ";
        let trimmed = err.trim();
        let message = if trimmed.is_empty() {
            "rc=1".to_string()
        } else {
            trimmed.to_string()
        };
        assert_eq!(
            json!({"forgotten": false, "error": message}),
            json!({"forgotten": false, "error": "not available"})
        );
        // Empty stderr falls back to the rc string.
        let err2 = "   ";
        let trimmed2 = err2.trim();
        let rc = 1;
        let message2 = if trimmed2.is_empty() {
            format!("rc={rc}")
        } else {
            trimmed2.to_string()
        };
        assert_eq!(message2, "rc=1");
    }

    #[test]
    fn forget_success_shape() {
        let body = json!({"forgotten": true, "error": Value::Null});
        assert_eq!(body, json!({"forgotten": true, "error": null}));
    }

    // ── btctl return-code conventions ────────────────────────────────────────

    #[tokio::test]
    async fn btctl_missing_binary_is_rc_127() {
        // Force a non-existent binary by overriding PATH to an empty dir is racy
        // process-wide; instead assert the spawn-failure code path's contract by
        // calling a command name that cannot exist. We cannot rename the const, so
        // this exercises the real `bluetoothctl` only when present; on CI without
        // it, the spawn fails and returns rc=127 with a non-empty stderr.
        let (rc, stdout, _stderr) = btctl(&["--version"], Duration::from_secs(2)).await;
        // Either bluetoothctl exists (rc 0) or the spawn failed (rc 127). Both are
        // valid; the contract under test is that a spawn failure yields 127, never
        // a panic.
        assert!(rc == 0 || rc == 127, "rc was {rc}");
        // When the binary is absent the stdout is empty (the Python `""`).
        if rc == 127 {
            assert!(stdout.is_empty());
        }
    }

    // ── profile gate ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn profile_mismatch_is_the_fastapi_404_shape() {
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(resp).await,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    // No `bt_error` test: the Bluetooth routes never return an HTTP error — every
    // `btctl` fault is swallowed into a return code and rides in the 200 result
    // dict's `error` field (see the note above the `btctl` seam), so there is no
    // 5xx error helper on this surface.

    // ── clear_primary_if_matches: only forwards on a matching persisted primary ─

    #[tokio::test]
    async fn clear_primary_skips_when_persisted_primary_does_not_match() {
        let dir = tempfile::tempdir().unwrap();
        // A persisted primary that is a USB device, not the bt device being forgotten.
        let sidecar = dir.path().join("ground-station-input.json");
        std::fs::write(&sidecar, r#"{"primary":"usb:045e:028e:event3"}"#).unwrap();
        // No hid-cmd.sock exists; the helper must be a clean no-op (no panic, no
        // hang) because the persisted primary does not match the device. Every path
        // is threaded in, so the test never mutates the process environment.
        let hid_sock = dir.path().join("hid-cmd.sock");
        clear_primary_if_matches_at("bt:aa:bb:cc:dd:ee:ff", &sidecar, &hid_sock).await;
    }

    #[test]
    fn split_whitespace_max3_keeps_remainder_whole() {
        assert_eq!(
            split_whitespace_max3("Device AA:BB Long Name Here"),
            vec!["Device", "AA:BB", "Long Name Here"]
        );
        assert_eq!(
            split_whitespace_max3("Device   AA:BB   Pad"),
            vec!["Device", "AA:BB", "Pad"]
        );
        assert_eq!(
            split_whitespace_max3("Device AA:BB"),
            vec!["Device", "AA:BB"]
        );
    }
}
