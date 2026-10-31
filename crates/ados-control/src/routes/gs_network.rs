//! Ground-station network uplink read routes.
//!
//! The ground-station profile exposes the uplink matrix (AP, Wi-Fi client,
//! ethernet, cellular modem, priority list, share-uplink toggle) under
//! `/api/v1/ground-station/network*` plus a cellular detail snapshot at
//! `/api/v1/ground-station/modem-status`. This module serves the exact-path
//! read views; the writes (the AP / ethernet / client-join / modem / priority /
//! share PUTs and the client DELETE) stay on the residual surface.
//!
//! Every route first gates on the resolved profile being a ground station and
//! returns the FastAPI `404 {"detail":{"error":{"code":"E_PROFILE_MISMATCH"}}}`
//! on a drone, byte-identically to the Python `_require_ground_profile`.
//!
//! On this native front the uplink loop runs in a sibling `ados-net` daemon, so
//! there is no in-process manager to call. Each leg is sourced from the durable
//! seams the front can read and degrades — never 500s — to the same default the
//! Python view helper returns when its own manager raises:
//!
//! - **`GET /api/v1/ground-station/network`** — the aggregate uplink view. `ap`
//!   from the live hostapd state: `running` / `enabled` from
//!   `systemctl is-active ados-hostapd`, the SSID resolved the way the live
//!   hostapd manager resolves it (`ADOS-GS-<short device id>`, not the raw
//!   `ADOS-{device_id}` template), the channel + interface from config (defaults
//!   `6` / `wlan0`), the `192.168.4.1` gateway + the associated station MACs
//!   (`iw dev <iface> station dump`) while running. `wifi_client` from the `ados-net` Wi-Fi
//!   command socket's `wifi_status` op (+ the on-boot flag from the client config
//!   file), degrading to the all-default shape when the socket is unreachable.
//!   `ethernet` from the board's physical wired port, resolved by device class
//!   (`null` only when the board has none). `modem_4g` from the modem config file (enabled / apn
//!   / cap) with every connectivity leg `null` (this front has no modem-status
//!   seam) and the cumulative-usage legs overlaid from the store's most-recent
//!   `net.modem_usage` event. `active_uplink` from the store's most-recent
//!   `net.uplink_active` event (the daemon's selected uplink), else `null`.
//!   `priority` from the uplink priority file (the default chain when absent).
//!   `share_uplink` from the config flag.
//! - **`GET .../network/ethernet`** — `connection_name` from a read-only `nmcli`
//!   connection list (the active ethernet profile's name, else `null`) plus the
//!   wired port's live carrier, speed, address and gateway.
//! - **`GET .../network/client/scan`** — a fresh nearby-network scan run by the
//!   Wi-Fi command socket's `wifi_scan` op (see
//!   `crate::routes::network_write::wifi_scan`).
//! - **`GET .../network/modem`** — the modem view (same leg as `modem_4g`).
//! - **`GET .../network/priority`** — the uplink priority list.
//! - **`GET .../modem-status`** — the cellular detail snapshot; the front has no
//!   `mmcli` polling seam, so it serves `modemmanager_not_installed` when
//!   ModemManager is absent and otherwise `{"present": null, "reason":
//!   "not_probed"}`: it never looked, so it claims neither a modem nor none.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::state::AppState;

mod ap;
mod client_modem;
mod ethernet;

use ap::*;
pub(crate) use client_modem::modem_view;
use client_modem::*;
pub use ethernet::get_network_ethernet;
pub(crate) use ethernet::nmcli_connections;
use ethernet::*;

// ---------------------------------------------------------------------------
// Profile gate.
// ---------------------------------------------------------------------------

/// The FastAPI `_require_ground_profile` 404 body: a `detail` carrying the
/// `E_PROFILE_MISMATCH` error object (NOT the bare-string `detail` the rest of
/// this surface uses). A drone-profile caller hits every ground-station route
/// with this exact shape.
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

/// True when the resolved profile is a ground station. Resolves through the
/// shared profile module (config `agent.profile` + the on-disk sentinels), the
/// same source of truth the node advertises on the wire, mirroring the Python
/// `is_ground_station`.
fn is_ground_station() -> bool {
    let cfg = crate::config::PairingConfig::load();
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

// ---------------------------------------------------------------------------
// Path seams: the run-dir socket + the etc-dir config files.
// ---------------------------------------------------------------------------

/// The agent etc dir (`ADOS_ETC_DIR`, default `/etc/ados`) holding the
/// ground-station config side-files (priority / modem / wifi-client JSON).
fn etc_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_ETC_DIR").unwrap_or_else(|_| "/etc/ados".to_string()))
}

/// The native `ados-net` Wi-Fi command socket (`/run/ados/wifi-cmd.sock`), which
/// answers the `wifi_status` op with the live station status.
fn wifi_cmd_sock() -> PathBuf {
    ados_protocol::paths::run_path("wifi-cmd")
}

/// The persisted uplink priority list (`/etc/ados/ground-station-uplink.json`).
fn gs_uplink_json() -> PathBuf {
    etc_dir().join("ground-station-uplink.json")
}

/// The persisted modem config (`/etc/ados/ground-station-modem.json`): apn,
/// cap_gb, enabled. The view's stable config legs.
fn gs_modem_json() -> PathBuf {
    etc_dir().join("ground-station-modem.json")
}

/// The persisted Wi-Fi client config (`/etc/ados/ground-station-wifi-client.json`):
/// the `enabled_on_boot` flag the client view carries.
fn gs_wifi_client_json() -> PathBuf {
    etc_dir().join("ground-station-wifi-client.json")
}

/// The default uplink priority chain, returned when the priority file is absent /
/// unparseable / carries an empty or non-string list.
const DEFAULT_PRIORITY: [&str; 4] = ["eth0", "wlan0_client", "wwan0", "usb0"];

/// The hostapd systemd unit the live AP runs as. `systemctl is-active` on this
/// unit is the front's `running` source, matching the hostapd manager's
/// `_HOSTAPD_UNIT`.
const HOSTAPD_UNIT: &str = "ados-hostapd.service";

/// The setup-AP guard decision sidecar the `ados-net` daemon writes each
/// reconcile (mirrors `ados_net::paths::AP_GUARD_JSON`). Merged into the AP view
/// so the stand-down decision is diagnosable, matching the Python
/// `_ap_guard_diagnostics`.
const AP_GUARD_SIDECAR: &str = "/run/ados/ap-guard.json";

/// Last-resort AP interface name, used only when resolution finds nothing.
///
/// Not a default in any meaningful sense: interface names are not stable, and
/// this one has been measured naming the flight radio on one boot in three.
/// Anything reported to an operator resolves by driver first.
const AP_IFACE: &str = "wlan0";

/// The AP gateway address the hostapd manager assigns to the AP interface
/// (`_AP_ADDR`), reported as the AP `gateway` while the AP is running.
const AP_GATEWAY_IP: &str = "192.168.4.1";

/// The AP owner's default channel (`ados-net`'s hostapd manager uses `6` when
/// `network.hotspot.channel` is absent), so the AP channel is always an
/// integer, never null.
const AP_DEFAULT_CHANNEL: i64 = 6;

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/network — aggregate uplink view.
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/network` → the aggregate uplink view.
///
/// 404s with the profile-mismatch body on a drone. Otherwise composes the six
/// uplink legs from the durable seams (config, the Wi-Fi command socket, and the
/// store), each degrading to its Python view-helper default rather than failing,
/// so the route is guaranteed-200 on a ground station.
pub async fn get_ground_station_network(State(state): State<AppState>) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    let cfg = crate::config::load_config_object_default();
    let active_uplink = latest_uplink_active(&state).await.unwrap_or(Value::Null);
    let body = json!({
        "ap": ap_view(&cfg).await,
        "wifi_client": wifi_client_view().await,
        "ethernet": ethernet_view().await,
        "modem_4g": modem_view(&state).await,
        "active_uplink": active_uplink,
        "priority": priority_list(),
        "share_uplink": share_uplink_flag(&cfg),
    });
    Json(body).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/network/client/scan — nearby-network scan.
// ---------------------------------------------------------------------------

/// `GET .../network/client/scan` → `{"networks": [...]}`. 404s on a drone.
///
/// The scan runs in the daemon that owns the station radio (the `wifi_scan`
/// op), never here: driving the radio from this process would race the
/// daemon's joins, and on a ground station the same radio may be the AP
/// carrying the operator's own session. An unreachable daemon is a 503
/// `E_SCAN_UNAVAILABLE`, never an empty list that claims a scan ran.
pub async fn get_network_client_scan() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    let result = crate::routes::network_write::wifi_scan().await;
    crate::routes::network_write::wifi_scan_response(result)
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/network/modem — modem view.
// ---------------------------------------------------------------------------

/// `GET .../network/modem` → the modem status + usage + configured cap. 404s on
/// a drone. Shares the `modem_view` leg the aggregate view uses.
pub async fn get_network_modem(State(state): State<AppState>) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    Json(modem_view(&state).await).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/network/priority — uplink priority list.
// ---------------------------------------------------------------------------

/// `GET .../network/priority` → the current uplink priority list. 404s on a
/// drone. Reads the priority file, falling back to the default chain.
pub async fn get_network_priority() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    Json(json!({"priority": priority_list()})).into_response()
}

/// The uplink priority list from `ground-station-uplink.json`, or the default chain
/// when the file is absent / unparseable / carries an empty or non-string list.
fn priority_list() -> Value {
    let default = || Value::Array(DEFAULT_PRIORITY.iter().map(|s| json!(s)).collect());
    let Some(obj) = crate::sidecar::read_object(&gs_uplink_json()) else {
        return default();
    };
    let Some(arr) = obj.get("priority").and_then(Value::as_array) else {
        return default();
    };
    // A list of all-strings with at least one entry is honoured; anything else
    // (empty, or a non-string member) falls back to the default chain.
    if arr.is_empty() || !arr.iter().all(Value::is_string) {
        return default();
    }
    Value::Array(arr.clone())
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/modem-status — cellular detail snapshot.
// ---------------------------------------------------------------------------

/// `GET .../modem-status` → the cellular detail snapshot. 404s on a drone.
///
/// The Python `_build_snapshot` shells out to `mmcli`. Its FIRST branch — the
/// one a box without ModemManager hits — returns
/// `{"present": false, "reason": "modemmanager_not_installed"}` when `mmcli` is
/// not on PATH. The front reproduces exactly that gate (a `which mmcli` probe)
/// and serves that shape when ModemManager is absent. When `mmcli` IS present
/// the front has no `mmcli` polling seam and never looks for a modem, so it
/// answers `present: null` with `reason: "not_probed"` rather than claiming
/// either a modem or none.
pub async fn get_modem_status() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    // The Python `_which_mmcli` gate: `which mmcli` exiting zero. An absent
    // `which` / any spawn error / a timeout all read as ModemManager absent,
    // matching the Python `except` returning `False`.
    Json(modem_status_body(crate::probe::on_path("mmcli").await)).into_response()
}

/// The modem-status body for a box where `mmcli` is (or is not) on PATH.
fn modem_status_body(mmcli_present: bool) -> Value {
    if mmcli_present {
        json!({"present": null, "reason": "not_probed"})
    } else {
        json!({"present": false, "reason": "modemmanager_not_installed"})
    }
}

// ---------------------------------------------------------------------------
// Wi-Fi command socket seam: the `wifi_status` op.
// ---------------------------------------------------------------------------

/// Query the `ados-net` Wi-Fi command socket for the live station status. Sends
/// the one-line `{"op":"wifi_status"}` request and reads the one-line reply,
/// returning the reply object when `ok` is true, else `None`. An unreachable
/// socket / a malformed reply / `ok:false` all yield `None`, so the caller
/// degrades to the manager-absent default shape. Mirrors the framing the radio /
/// Wi-Fi command sockets use (one newline-terminated JSON each way).
pub(crate) async fn wifi_status(sock: &std::path::Path) -> Option<Map<String, Value>> {
    let request = json!({"op": "wifi_status"});
    let reply = crate::ipc::cmd::roundtrip(sock, &request, crate::ipc::cmd::QUICK)
        .await
        .ok()?;
    let obj = reply.as_object()?;
    if obj.get("ok").map(json_truthy) != Some(true) {
        return None;
    }
    Some(obj.clone())
}

// ---------------------------------------------------------------------------
// Store seams: net.uplink_active + net.modem_usage events.
// ---------------------------------------------------------------------------

/// The store's most-recent `active_uplink` value (the daemon's selected uplink), or
/// `None` when the store is unreachable / has no such event / the body omits the field.
/// A present body with a null `active_uplink` (the daemon emitting "no viable uplink")
/// yields `Some(Value::Null)`, so a store-first reader learns "no uplink" without a
/// separate probe.
async fn latest_uplink_active(state: &AppState) -> Option<Value> {
    let detail = state.logd.latest_event_detail("net.uplink_active").await?;
    detail.get("active_uplink").cloned()
}

/// The store's most-recent modem cumulative-usage block, or `None` when the store is
/// unreachable / holds no such event.
async fn latest_modem_usage(state: &AppState) -> Option<Map<String, Value>> {
    state.logd.latest_event_detail("net.modem_usage").await
}

// ---------------------------------------------------------------------------
// Config + file helpers.
// ---------------------------------------------------------------------------

/// The `ground_station.share_uplink` flag from the agent config, defaulting to `false`
/// when the section / field is absent. Any read failure reads the same way.
fn share_uplink_flag(cfg: &Value) -> bool {
    cfg.get("ground_station")
        .filter(|v| v.is_object())
        .and_then(|gs| gs.get("share_uplink"))
        .map(json_truthy)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Small shared helpers.
// ---------------------------------------------------------------------------

/// Python `bool(x)` truthiness over a JSON value: `null`/`false`/`0`/`0.0`/`""`/
/// `[]`/`{}` are falsey, everything else truthy. Mirrors the `bool(...)` coercion
/// the config flag reads use.
pub(crate) fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Coerce a JSON number value to `f64`, accepting an integer or float. `None`
/// for a non-number, mirroring the Python `float(...)` over a numeric value.
fn json_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Coerce a JSON number value to `i64`, accepting an integer or a float (a float
/// truncates toward zero). `None` for a non-number, mirroring the Python
/// `int(getattr(hotspot, "channel", 6))` channel read over a numeric value.
fn json_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Round to two decimal places, matching the Python `round(x, 2)` the `percent`
/// leg uses.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests;
