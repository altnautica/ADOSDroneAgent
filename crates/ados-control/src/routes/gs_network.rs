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
//!   `ethernet` to its all-default shape (no live seam on the front). `modem_4g`
//!   from the modem config file (enabled / apn / cap) with the connectivity legs
//!   carrying the live manager's no-modem defaults (`iface:"wwan0"`,
//!   `signal_quality:-1`, `technology:"unknown"`, `operator:""`) and the
//!   cumulative-usage legs overlaid from the store's most-recent
//!   `net.modem_usage` event. `active_uplink` from the store's most-recent
//!   `net.uplink_active` event (the daemon's selected uplink), else `null`.
//!   `priority` from the uplink priority file (the default chain when absent).
//!   `share_uplink` from the config flag.
//! - **`GET .../network/ethernet`** — the no-connection default shape for the
//!   live IPv4 / link legs, with `connection_name` reproduced from a read-only
//!   `nmcli` connection list (the active ethernet profile's name, else `null`).
//! - **`GET .../network/client/scan`** — nearby-network scan; the front has no
//!   scan seam, so it returns the empty-list shape (`{"networks": []}`), the
//!   same body the Python route returns when the scan finds nothing.
//! - **`GET .../network/modem`** — the modem view (same leg as `modem_4g`).
//! - **`GET .../network/priority`** — the uplink priority list.
//! - **`GET .../modem-status`** — the cellular detail snapshot; the front has no
//!   `mmcli` polling seam, so it serves a `present:false` shape, reproducing the
//!   Python `which mmcli` gate: `modemmanager_not_installed` when ModemManager is
//!   absent, else `no_modem`.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::state::AppState;

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

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sibling sockets + sidecars resolve under.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// The agent etc dir (`ADOS_ETC_DIR`, default `/etc/ados`) holding the
/// ground-station config side-files (priority / modem / wifi-client JSON).
fn etc_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_ETC_DIR").unwrap_or_else(|_| "/etc/ados".to_string()))
}

/// The native `ados-net` Wi-Fi command socket (`/run/ados/wifi-cmd.sock`), which
/// answers the `wifi_status` op with the live station status.
fn wifi_cmd_sock() -> PathBuf {
    run_dir().join("wifi-cmd.sock")
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

/// The default uplink priority chain, returned when the priority file is absent
/// / unparseable / carries an empty or non-string list. Mirrors the Python
/// `DEFAULT_PRIORITY`.
const DEFAULT_PRIORITY: [&str; 4] = ["eth0", "wlan0_client", "wwan0", "usb0"];

/// The hostapd systemd unit the live AP runs as. `systemctl is-active` on this
/// unit is the front's `running` source, matching the hostapd manager's
/// `_HOSTAPD_UNIT`.
const HOSTAPD_UNIT: &str = "ados-hostapd.service";

/// The default AP interface the hostapd manager binds (`_AP_IFACE`). The front
/// honours a configured `network.hotspot.interface` when present, else this.
const AP_IFACE: &str = "wlan0";

/// The AP gateway address the hostapd manager assigns to the AP interface
/// (`_AP_ADDR`), reported as the AP `gateway` while the AP is running.
const AP_GATEWAY_IP: &str = "192.168.4.1";

/// The hostapd manager's default channel (`_hostapd_manager` falls back to `6`
/// when `network.hotspot.channel` is absent), so the AP channel is always an
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
    let cfg = load_config_value();
    let active_uplink = latest_uplink_active(&state).await.unwrap_or(Value::Null);
    let body = json!({
        "ap": ap_view(&cfg),
        "wifi_client": wifi_client_view().await,
        "ethernet": ethernet_view_default(),
        "modem_4g": modem_view(&state).await,
        "active_uplink": active_uplink,
        "priority": priority_list(),
        "share_uplink": share_uplink_flag(&cfg),
    });
    Json(body).into_response()
}

/// The AP leg, reproducing the Python `_ap_view` over the live hostapd state.
///
/// The Python `_ap_view` reaches its live `try` branch on any ground station (the
/// hostapd `status()` never raises) and returns that manager's live status:
/// `running` from `systemctl is-active ados-hostapd`, the resolved SSID, the
/// channel, the AP interface, the `192.168.4.1` gateway, and the associated
/// station MACs. The front has no in-process hostapd manager, but every one of
/// those legs reads off the same live seams the manager reads, so the front
/// probes them directly instead of reporting a static not-running shape.
///
/// - `running` / `enabled` — `systemctl is-active ados-hostapd.service == active`.
/// - `ssid` — resolved the way `_hostapd_manager` + `_build_ssid` resolve it: a
///   configured SSID is honoured only when it is non-empty, carries no
///   `{device_id}` placeholder, and is already an `ADOS-GS-` name; otherwise it
///   is built as `ADOS-GS-<first 4 hex of device_id, uppercased, zero-padded>`.
/// - `channel` — the configured hotspot channel, defaulting to `6` (the manager's
///   default), so the channel is always an integer.
/// - `interface` — the configured AP interface, defaulting to `wlan0`.
/// - `gateway` — `192.168.4.1` while the AP is running, else null (the manager's
///   `status()` reports the gateway only when the unit is up).
/// - `connected_clients` — the associated station MACs from
///   `iw dev <iface> station dump` while running, else the empty list.
fn ap_view(cfg: &Value) -> Value {
    let hotspot = cfg
        .get("network")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("hotspot"))
        .filter(|v| v.is_object());
    let configured_ssid = hotspot
        .and_then(|h| h.get("ssid"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let device_id = cfg
        .get("agent")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("device_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let ssid = resolve_ap_ssid(configured_ssid, device_id);
    let channel = hotspot
        .and_then(|h| h.get("channel"))
        .and_then(json_to_i64)
        .unwrap_or(AP_DEFAULT_CHANNEL);
    let interface = ap_interface(cfg);

    let running = hostapd_running();
    let clients = if running {
        station_dump_macs(&interface)
    } else {
        Vec::new()
    };
    ap_view_compose(&ssid, channel, &interface, running, clients)
}

/// Compose the `_ap_view` body from already-probed pieces: the resolved SSID, the
/// channel, the AP interface, the live running flag, and the associated station
/// MACs. Split out so the shape + the running-vs-not-running gating are unit
/// tested without the `systemctl` / `iw` IO. Mirrors the Python `_ap_view` live
/// branch field-for-field, gating the gateway + clients on `running`.
fn ap_view_compose(
    ssid: &str,
    channel: i64,
    interface: &str,
    running: bool,
    clients: Vec<String>,
) -> Value {
    json!({
        "enabled": running,
        "running": running,
        "ssid": ssid,
        "channel": channel,
        "interface": interface,
        "gateway": if running { Value::String(AP_GATEWAY_IP.to_string()) } else { Value::Null },
        "connected_clients": clients,
    })
}

/// The configured AP interface (`network.hotspot.interface`), defaulting to
/// `wlan0` (`_AP_IFACE`) when absent / blank / non-string. Mirrors the hostapd
/// manager's `interface` default.
fn ap_interface(cfg: &Value) -> String {
    cfg.get("network")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("hotspot"))
        .filter(|v| v.is_object())
        .and_then(|h| h.get("interface"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(AP_IFACE)
        .to_string()
}

/// True when the hostapd unit is active, reproducing the manager's
/// `_is_unit_active`: run `systemctl is-active ados-hostapd.service` and treat a
/// trimmed `active` stdout as running. A missing `systemctl` / spawn error reads
/// as not running, matching the manager's `except` returning `False`.
fn hostapd_running() -> bool {
    let output = match std::process::Command::new("systemctl")
        .args(["is-active", HOSTAPD_UNIT])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    String::from_utf8_lossy(&output.stdout).trim() == "active"
}

/// The associated station MACs from `iw dev <iface> station dump`, parsing the
/// `Station <mac> (on <iface>)` header lines and lowercasing each MAC, reproducing
/// the manager's `_connected_clients`. A missing `iw` / a non-zero exit / a spawn
/// error all yield the empty list, matching the manager's `except` / `not ok`
/// returns.
fn station_dump_macs(interface: &str) -> Vec<String> {
    let output = match std::process::Command::new("iw")
        .args(["dev", interface, "station", "dump"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_station_dump(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the MAC addresses out of `iw … station dump` output: each associated
/// station begins with a `Station <mac> …` line whose second whitespace token is
/// the MAC, lowercased. Mirrors the manager's `_connected_clients` line scan.
fn parse_station_dump(text: &str) -> Vec<String> {
    let mut macs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Station ") {
            if let Some(mac) = rest.split_whitespace().next() {
                macs.push(mac.to_lowercase());
            }
        }
    }
    macs
}

/// Resolve the AP SSID the way the live hostapd manager does: honor a configured
/// SSID only when it is non-empty, has no `{device_id}` placeholder, and already
/// starts with `ADOS-GS-`; otherwise build `ADOS-GS-<short_id>` from the device
/// id. Mirrors `_hostapd_manager`'s `ssid_override` gate + `_build_ssid`.
fn resolve_ap_ssid(configured: &str, device_id: &str) -> String {
    if !configured.is_empty()
        && !configured.contains("{device_id}")
        && configured.starts_with("ADOS-GS-")
    {
        return configured.to_string();
    }
    format!("ADOS-GS-{}", short_id(device_id))
}

/// The first 4 hex chars of the device id, uppercased, zero-padded to 4 when the
/// id has fewer than 4 hex chars after stripping non-hex characters. Mirrors the
/// Python `_short_id`.
fn short_id(device_id: &str) -> String {
    let hex_only: String = device_id.chars().filter(char::is_ascii_hexdigit).collect();
    let padded = if hex_only.len() >= 4 {
        hex_only
    } else {
        format!("{hex_only}0000")
    };
    padded.chars().take(4).collect::<String>().to_uppercase()
}

/// The Wi-Fi client leg. Reads the live station status from the `ados-net`
/// command socket's `wifi_status` op and the `enabled_on_boot` flag from the
/// client config file, reshaping to the `_wifi_client_view` shape
/// `{enabled_on_boot, connected, ssid, signal, ip}`. An unreachable socket
/// degrades to the full default the Python `except` returns.
async fn wifi_client_view() -> Value {
    let status = match wifi_status().await {
        Some(s) => s,
        None => {
            return json!({
                "enabled_on_boot": false,
                "connected": false,
                "ssid": Value::Null,
                "signal": Value::Null,
                "ip": Value::Null,
            });
        }
    };
    let enabled_on_boot = load_json_object(&gs_wifi_client_json())
        .and_then(|m| m.get("enabled_on_boot").map(json_truthy))
        .unwrap_or(false);
    json!({
        "enabled_on_boot": enabled_on_boot,
        "connected": status.get("connected").map(json_truthy).unwrap_or(false),
        "ssid": status.get("ssid").cloned().unwrap_or(Value::Null),
        "signal": status.get("signal").cloned().unwrap_or(Value::Null),
        "ip": status.get("ip").cloned().unwrap_or(Value::Null),
    })
}

/// The ethernet leg of the aggregate view: the no-live-seam default shape
/// `{link:false, speed_mbps:null, ip:null, gateway:null}`, the same shape the
/// Python `_ethernet_view` returns when its manager raises.
fn ethernet_view_default() -> Value {
    json!({
        "link": false,
        "speed_mbps": Value::Null,
        "ip": Value::Null,
        "gateway": Value::Null,
    })
}

/// The modem leg of the aggregate view (also the `GET .../network/modem` body).
///
/// The front has no live modem-status seam, so the connectivity legs are the
/// no-modem defaults the live `ModemManager.status()` returns when no modem is
/// connected: `iface:"wwan0"`, `signal_quality:-1`, `technology:"unknown"`,
/// `operator:""`, `connected:false`. The `enabled` / `apn` / cap come off the
/// modem config file, and the cumulative-usage legs (`data_used_mb`, `cap_mb`,
/// `percent`) are overlaid from the store's most-recent `net.modem_usage` event
/// when present. Mirrors `_modem_view` over a box whose `status()` reports no
/// modem + the store overlay.
async fn modem_view(state: &AppState) -> Value {
    let cfg = load_json_object(&gs_modem_json()).unwrap_or_default();
    let enabled = cfg.get("enabled").map(json_truthy).unwrap_or(false);
    let apn = cfg
        .get("apn")
        .filter(|v| v.is_string())
        .cloned()
        .unwrap_or(Value::Null);

    // cap_mb from the config cap_gb, mirroring the Python int(float(cap_gb)*1024).
    let mut cap_mb: i64 = cfg
        .get("cap_gb")
        .and_then(json_to_f64)
        .map(|gb| (gb * 1024.0) as i64)
        .unwrap_or(0);
    let mut data_used_mb: i64 = 0;
    let mut percent: f64 = 0.0;

    // Store overlay: the daemon's data-cap tracker ships the cumulative usage as
    // a net.modem_usage event; a hit carries the daemon's truth.
    if let Some(store) = latest_modem_usage(state).await {
        if let Some(v) = store.get("data_used_mb").and_then(json_to_f64) {
            data_used_mb = v as i64;
        }
        if let Some(v) = store.get("cap_mb").and_then(json_to_f64) {
            cap_mb = v as i64;
        }
        if let Some(v) = store.get("percent").and_then(json_to_f64) {
            percent = round2(v);
        }
    }

    json!({
        "enabled": enabled,
        "connected": false,
        "iface": "wwan0",
        "ip": Value::Null,
        "signal_quality": -1,
        "technology": "unknown",
        "apn": apn,
        "operator": "",
        "data_used_mb": data_used_mb,
        "cap_mb": cap_mb,
        "percent": percent,
        "state": "disconnected",
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/network/ethernet — ethernet profile + live link.
// ---------------------------------------------------------------------------

/// `GET .../network/ethernet` → the persisted ethernet profile config plus live
/// link state. 404s on a drone.
///
/// The front has no live ethernet IPv4 / link seam, so the `mode` / IP / gateway
/// / dns / link legs degrade to the no-connection default shape (`mode:"dhcp"`,
/// every other live field empty / false / null). The `connection_name` leg is
/// the exception: the Python `config()` reports the discovered NM connection
/// name, so the front reproduces that source with a read-only `nmcli` connection
/// list (`discover_primary_connection_name`), reporting the active ethernet
/// profile's name (e.g. `"netplan-eth0"`) and `null` only when no NM-managed
/// ethernet profile exists, matching the Python `_discover_primary_connection`.
pub async fn get_network_ethernet() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    let connection_name = discover_primary_connection_name(ETH_IFACE)
        .map(Value::String)
        .unwrap_or(Value::Null);
    Json(json!({
        "mode": "dhcp",
        "connection_name": connection_name,
        "ip": Value::Null,
        "gateway": Value::Null,
        "dns": [],
        "link": false,
        "speed_mbps": Value::Null,
        "current_ip": Value::Null,
        "current_gateway": Value::Null,
    }))
    .into_response()
}

/// The ethernet interface the connection discovery prefers, mirroring the Python
/// `EthernetManager` default (`eth0`).
const ETH_IFACE: &str = "eth0";

/// Discover the primary ethernet NM connection NAME, mirroring the Python
/// `_discover_primary_connection`. Reads the saved + active NM connection lists
/// with a read-only `nmcli` and picks the primary name. Returns `None` when
/// `nmcli` is absent / errors / lists no ethernet profile — the same `null` the
/// Python view reports on a non-NM box.
fn discover_primary_connection_name(interface: &str) -> Option<String> {
    let saved = nmcli_connections(&["NAME", "TYPE", "DEVICE"], false);
    let active = nmcli_connections(&["NAME", "TYPE", "DEVICE"], true);
    pick_primary_connection_name(&saved, &active, interface)
}

/// Pick the primary ethernet connection name from the parsed saved + active
/// terse rows, mirroring the Python `_discover_primary_connection` precedence:
/// an ACTIVE 802-3-ethernet connection on the interface (or with no device
/// pinned) wins, else a saved ethernet connection pinned to the interface, else
/// the first saved ethernet connection of any device, else `None`.
fn pick_primary_connection_name(
    saved_rows: &[Vec<String>],
    active_rows: &[Vec<String>],
    interface: &str,
) -> Option<String> {
    // (name, device) of every saved 802-3-ethernet connection.
    let saved: Vec<(&str, &str)> = saved_rows
        .iter()
        .filter(|row| row.get(1).map(String::as_str) == Some("802-3-ethernet"))
        .map(|row| {
            let name = row.first().map(String::as_str).unwrap_or("");
            let dev = row.get(2).map(String::as_str).unwrap_or("");
            (name, dev)
        })
        .collect();

    // The names of the active connections (the `--active` view).
    let active_names: std::collections::HashSet<&str> = active_rows
        .iter()
        .filter_map(|row| row.first().map(String::as_str))
        .filter(|name| !name.is_empty())
        .collect();

    // An active ethernet connection on the interface (or with no device pinned).
    for (name, dev) in &saved {
        if active_names.contains(name) && (*dev == interface || dev.is_empty()) {
            return Some((*name).to_string());
        }
    }
    // A saved ethernet connection pinned to the interface.
    for (name, dev) in &saved {
        if *dev == interface {
            return Some((*name).to_string());
        }
    }
    // Else the first saved ethernet connection of any device.
    saved.first().map(|(name, _dev)| (*name).to_string())
}

/// Run a read-only `nmcli -t -f <fields> connection show [--active]` and parse
/// the terse rows. Each row is truncated to `fields.len()` columns. An absent
/// `nmcli` / a non-zero exit / a spawn error all yield an empty list, so the
/// caller degrades to the no-connection `null`.
fn nmcli_connections(fields: &[&str], active: bool) -> Vec<Vec<String>> {
    let mut args = vec!["-t", "-f"];
    let field_spec = fields.join(",");
    args.push(&field_spec);
    args.push("connection");
    args.push("show");
    if active {
        args.push("--active");
    }
    let output = match std::process::Command::new("nmcli").args(&args).output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_nmcli_terse(&text, fields.len())
}

/// Parse `nmcli -t` (terse) multi-line output into rows, skipping blank lines and
/// keeping only rows with at least `fields` columns (each truncated to `fields`).
/// Mirrors the Python `_parse_nmcli_terse_fields` per line + the manager's
/// per-line column guard.
fn parse_nmcli_terse(text: &str, fields: usize) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts = parse_nmcli_terse_line(line);
        if parts.len() >= fields {
            rows.push(parts.into_iter().take(fields).collect());
        }
    }
    rows
}

/// Split one `nmcli -t` terse line into fields, honoring `\:` and `\\` escapes.
/// An odd trailing backslash is treated as a literal backslash, matching the
/// Python `_parse_nmcli_terse_fields` `i + 1 < len(line)` guard.
fn parse_nmcli_terse_line(line: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && i + 1 < chars.len() {
            buf.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if ch == ':' {
            parts.push(std::mem::take(&mut buf));
            i += 1;
            continue;
        }
        buf.push(ch);
        i += 1;
    }
    parts.push(buf);
    parts
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/network/client/scan — nearby-network scan.
// ---------------------------------------------------------------------------

/// `GET .../network/client/scan` → nearby Wi-Fi networks. 404s on a drone. The
/// front has no scan seam (it must not drive `nmcli` on `wlan0` and race the
/// daemon), so it returns the empty-list shape, the same `{"networks": []}` the
/// Python route returns when the scan finds nothing.
pub async fn get_network_client_scan() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    Json(json!({"networks": []})).into_response()
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

/// The uplink priority list from `ground-station-uplink.json`, or the default
/// chain when the file is absent / unparseable / carries an empty or non-string
/// list. Mirrors the Python `load_priority` + `UplinkRouter.get_priority`.
fn priority_list() -> Value {
    let default = || Value::Array(DEFAULT_PRIORITY.iter().map(|s| json!(s)).collect());
    let Some(obj) = load_json_object(&gs_uplink_json()) else {
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
/// the front has no `mmcli` polling seam, so it falls back to the
/// no-modem-detected shape (`{"present": false, "reason": "no_modem"}`), the
/// degrade the Python returns when `mmcli -L` finds no modems — never claiming a
/// modem is present.
pub async fn get_modem_status() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    let reason = if mmcli_available() {
        "no_modem"
    } else {
        "modemmanager_not_installed"
    };
    Json(json!({"present": false, "reason": reason})).into_response()
}

/// True when `mmcli` (the ModemManager CLI) is on PATH, reproducing the Python
/// `_which_mmcli` gate: run `which mmcli` and treat a zero exit with non-empty
/// output as present. A missing `which` binary / any spawn error reads as absent,
/// matching the Python `except` returning `False`.
fn mmcli_available() -> bool {
    let output = match std::process::Command::new("which").arg("mmcli").output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    output.status.success() && !output.stdout.iter().all(u8::is_ascii_whitespace)
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
async fn wifi_status() -> Option<Map<String, Value>> {
    let reply = wifi_cmd_roundtrip(r#"{"op":"wifi_status"}"#).await?;
    let obj = reply.as_object()?;
    if obj.get("ok").map(json_truthy) != Some(true) {
        return None;
    }
    Some(obj.clone())
}

/// Send one newline-terminated JSON request to the Wi-Fi command socket and read
/// one newline-terminated JSON reply. Bounded so a runaway reply cannot exhaust
/// memory. `None` on an unreachable socket, a read error, or an unparseable
/// reply.
async fn wifi_cmd_roundtrip(request: &str) -> Option<Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A status reply is a few hundred bytes; bound the read to guard a runaway.
    const MAX_REPLY_BYTES: usize = 64 * 1024;

    let mut stream = tokio::net::UnixStream::connect(wifi_cmd_sock())
        .await
        .ok()?;
    let line = format!("{request}\n");
    stream.write_all(line.as_bytes()).await.ok()?;
    stream.flush().await.ok()?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break; // EOF: the server replies once then closes.
        }
        if raw.len() + n > MAX_REPLY_BYTES {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        // The reply is one newline-terminated line; stop at the first newline.
        if raw.contains(&b'\n') {
            break;
        }
    }
    let text = String::from_utf8(raw).ok()?;
    let line = text.lines().next()?;
    serde_json::from_str(line).ok()
}

// ---------------------------------------------------------------------------
// Store seams: net.uplink_active + net.modem_usage events.
// ---------------------------------------------------------------------------

/// The store's most-recent `active_uplink` value (the daemon's selected uplink),
/// or `None` when the store is unreachable / has no such event / the body omits
/// the field. A present body with a null `active_uplink` (the daemon emitting
/// "no viable uplink") yields `Some(Value::Null)`, so a store-first reader learns
/// "no uplink" without a separate probe. Mirrors `latest_uplink_active`.
async fn latest_uplink_active(state: &AppState) -> Option<Value> {
    let detail = latest_event_detail(state, "net.uplink_active").await?;
    detail.get("active_uplink").cloned()
}

/// The store's most-recent modem cumulative-usage block, or `None` when the
/// store is unreachable / holds no such event. Mirrors `latest_modem_usage`.
async fn latest_modem_usage(state: &AppState) -> Option<Map<String, Value>> {
    latest_event_detail(state, "net.modem_usage").await
}

/// Query the store for the newest `events` row of one `event_kind` and return
/// its `detail` body, or `None` when the store is unreachable / the response is
/// an error / there is no such event / the detail is absent / non-object /
/// empty. Mirrors the Python `query_rows("events", 1, event_kind=...)` read.
async fn latest_event_detail(state: &AppState, event_kind: &str) -> Option<Map<String, Value>> {
    let params = [
        ("kind", "events".to_string()),
        ("limit", "1".to_string()),
        ("event_kind", event_kind.to_string()),
    ];
    let query = encode_query(&params);
    let path = format!("/v1/query?{query}");
    let (status, body) = logd_get(state, &path).await.ok()?;
    if status >= 400 {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&body).ok()?;
    let rows = parsed.get("data")?.as_array()?;
    let detail = rows.first()?.as_object()?.get("detail")?.as_object()?;
    if detail.is_empty() {
        return None;
    }
    Some(detail.clone())
}

/// A minimal HTTP/1.1 `GET` over the logging-store query Unix socket, returning
/// the status code + the decoded body. The socket path comes from the app
/// state's logd client so a test redirects it. `Connection: close` reads to EOF;
/// a chunked body is de-chunked. Bounded so a runaway response cannot exhaust
/// memory.
async fn logd_get(state: &AppState, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A hard ceiling on the response read; an events page is a few KiB.
    const MAX_READ_BYTES: usize = 4 * 1024 * 1024;

    let socket = state.logd.socket_path();
    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let head = format!("GET {path} HTTP/1.1\r\nHost: logd\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break; // EOF (Connection: close).
        }
        if raw.len() + n > MAX_READ_BYTES {
            return Err(std::io::Error::other("logd response too large"));
        }
        raw.extend_from_slice(&buf[..n]);
    }
    parse_http_response(&raw)
}

/// Split a raw HTTP/1.1 response into the status code + decoded body. De-chunks a
/// `Transfer-Encoding: chunked` body; otherwise returns the body after the header
/// terminator as-is.
fn parse_http_response(raw: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
    let sep = b"\r\n\r\n";
    let split = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| std::io::Error::other("malformed http response (no header terminator)"))?;
    let head = &raw[..split];
    let body = &raw[split + sep.len()..];

    let head_str = String::from_utf8_lossy(head);
    let status = head_str
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("malformed http status line"))?;

    let chunked = head_str
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        de_chunk(body)
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

/// De-chunk a `Transfer-Encoding: chunked` body: `<hexlen>\r\n<data>\r\n`
/// repeated until a zero-length chunk.
fn de_chunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(crlf) = rest.windows(2).position(|w| w == b"\r\n") {
        let len_line = &rest[..crlf];
        let len = usize::from_str_radix(String::from_utf8_lossy(len_line).trim(), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        let data_start = crlf + 2;
        if rest.len() < data_start + len {
            out.extend_from_slice(&rest[data_start..]);
            break;
        }
        out.extend_from_slice(&rest[data_start..data_start + len]);
        let next = data_start + len;
        rest = if rest.len() >= next + 2 {
            &rest[next + 2..]
        } else {
            &[]
        };
    }
    out
}

// ---------------------------------------------------------------------------
// Config + file helpers.
// ---------------------------------------------------------------------------

/// Load `/etc/ados/config.yaml` (or the `ADOS_CONFIG` override) as a raw JSON
/// value, tolerating absence / a parse error / a non-object root with an empty
/// object. Used for the `ap` (hotspot) and `share_uplink` legs.
fn load_config_value() -> Value {
    let path =
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string());
    load_yaml_object(Path::new(&path))
}

/// Read a YAML file into a JSON object value, or `{}` on absence / parse error /
/// non-object root.
fn load_yaml_object(path: &Path) -> Value {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return json!({}),
    };
    match serde_norway::from_str::<Value>(&text) {
        Ok(v) if v.is_object() => v,
        _ => json!({}),
    }
}

/// Read a JSON file into its object map, or `None` on absence / parse error /
/// non-object root. Used for the priority / modem / wifi-client side-files.
fn load_json_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// The `ground_station.share_uplink` flag from the agent config, defaulting to
/// `false` when the section / field is absent. Mirrors the Python
/// `_load_share_uplink_flag` (which returns `False` on any read failure).
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

/// Percent-encode a query-parameter list into a `key=value&...` string.
fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Conservative percent-encoding: pass through the unreserved set
/// (`A-Za-z0-9-._~`) verbatim and percent-encode every other byte.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Python `bool(x)` truthiness over a JSON value: `null`/`false`/`0`/`0.0`/`""`/
/// `[]`/`{}` are falsey, everything else truthy. Mirrors the `bool(...)` coercion
/// the config flag reads use.
fn json_truthy(v: &Value) -> bool {
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
mod tests {
    use super::*;

    /// The exact aggregate-view shape the FastAPI `/network` route returns on a
    /// fresh ground station with no config, no command socket, and no store: the
    /// manager-absent defaults for every leg. The AP leg's running flag is the
    /// live probe, so the fixture composes the static legs over an explicit
    /// not-running AP rather than calling the `systemctl`/`iw` probe.
    #[test]
    fn network_aggregate_default_shape_is_the_golden_fixture() {
        let cfg = json!({});
        // The AP leg as composed when the live probe reports the unit down: the
        // resolved SSID + default channel + default interface, with the gateway +
        // clients gated off.
        let ap = ap_view_compose(
            "ADOS-GS-0000",
            AP_DEFAULT_CHANNEL,
            AP_IFACE,
            false,
            Vec::new(),
        );
        let ethernet = ethernet_view_default();
        let priority = priority_list_from(None);
        let share = share_uplink_flag(&cfg);

        // Compose the body the way the handler does, for the legs that do not need
        // async seams (wifi_client / modem / active_uplink degrade independently;
        // tested separately).
        let body = json!({
            "ap": ap,
            "ethernet": ethernet,
            "priority": priority,
            "share_uplink": share,
        });
        let want = json!({
            "ap": {
                "enabled": false,
                "running": false,
                // An empty config has no device id, so the SSID resolves to the
                // zero-padded short id, matching the live hostapd manager over a
                // fresh box (NOT the raw `ADOS-{device_id}` template).
                "ssid": "ADOS-GS-0000",
                // The channel is the manager's default (6), never null.
                "channel": 6,
                // The interface is the manager's default AP interface, never null.
                "interface": "wlan0",
                // The gateway + clients are gated off while the AP is down.
                "gateway": null,
                "connected_clients": [],
            },
            "ethernet": {
                "link": false,
                "speed_mbps": null,
                "ip": null,
                "gateway": null,
            },
            "priority": ["eth0", "wlan0_client", "wwan0", "usb0"],
            "share_uplink": false,
        });
        assert_eq!(body, want);
    }

    #[test]
    fn ap_view_compose_running_shape_carries_gateway_and_clients() {
        // The live shape the bench observed: a running AP reports enabled +
        // running true, the resolved SSID, channel, interface, the 192.168.4.1
        // gateway, and the associated station MAC.
        let view = ap_view_compose(
            "ADOS-GS-D9DB",
            6,
            "wlan0",
            true,
            vec!["dc:ea:e7:30:74:a6".to_string()],
        );
        let want = json!({
            "enabled": true,
            "running": true,
            "ssid": "ADOS-GS-D9DB",
            "channel": 6,
            "interface": "wlan0",
            "gateway": "192.168.4.1",
            "connected_clients": ["dc:ea:e7:30:74:a6"],
        });
        assert_eq!(view, want);
    }

    #[test]
    fn ap_view_compose_not_running_gates_the_gateway_and_clients() {
        // A down AP reports enabled + running false, keeps the resolved SSID +
        // channel + interface, and gates the gateway to null + the clients to the
        // empty list (the manager's status reports the gateway only when up).
        let view = ap_view_compose("ADOS-GS-ABCD", 11, "wlan0", false, Vec::new());
        assert_eq!(view["enabled"], json!(false));
        assert_eq!(view["running"], json!(false));
        assert_eq!(view["ssid"], json!("ADOS-GS-ABCD"));
        assert_eq!(view["channel"], json!(11));
        assert_eq!(view["interface"], json!("wlan0"));
        assert_eq!(view["gateway"], Value::Null);
        assert_eq!(view["connected_clients"], json!([]));
    }

    #[test]
    fn ap_view_resolves_the_ssid_channel_and_interface_from_config() {
        // The SSID / channel / interface legs are sourced from config. A configured
        // ADOS-GS- SSID is honoured verbatim; the channel + interface come from the
        // hotspot section. The running flag itself is the live probe (asserted
        // separately via ap_view_compose), so this test pins only the config-sourced
        // legs.
        let cfg = json!({"network": {"hotspot": {"ssid": "ADOS-GS-ABCD", "channel": 6}}});
        let hotspot = cfg.get("network").and_then(|n| n.get("hotspot")).unwrap();
        let ssid = resolve_ap_ssid(
            hotspot.get("ssid").and_then(Value::as_str).unwrap_or(""),
            "",
        );
        let channel = hotspot
            .get("channel")
            .and_then(json_to_i64)
            .unwrap_or(AP_DEFAULT_CHANNEL);
        assert_eq!(ssid, "ADOS-GS-ABCD");
        assert_eq!(channel, 6);
        assert_eq!(ap_interface(&cfg), "wlan0");
    }

    #[test]
    fn ap_view_resolves_the_device_id_template_to_a_built_ssid() {
        // The default hotspot SSID carries the `{device_id}` placeholder, which
        // the live hostapd manager never echoes: it builds `ADOS-GS-<short id>`
        // from the device id. The front reproduces that, so the route never
        // leaks the raw `ADOS-{device_id}` template the prior shape emitted.
        let cfg = json!({
            "agent": {"device_id": "deadbeef1234"},
            "network": {"hotspot": {"ssid": "ADOS-{device_id}", "channel": 11}},
        });
        let hotspot = cfg.get("network").and_then(|n| n.get("hotspot")).unwrap();
        let device_id = cfg
            .get("agent")
            .and_then(|a| a.get("device_id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let ssid = resolve_ap_ssid(
            hotspot.get("ssid").and_then(Value::as_str).unwrap_or(""),
            device_id,
        );
        let channel = hotspot
            .get("channel")
            .and_then(json_to_i64)
            .unwrap_or(AP_DEFAULT_CHANNEL);
        assert_eq!(ssid, "ADOS-GS-DEAD");
        assert_eq!(channel, 11);
    }

    #[test]
    fn ap_view_ignores_a_non_ados_gs_configured_ssid() {
        // A configured SSID that does not start with ADOS-GS- is NOT honored
        // (mirrors the hostapd manager's ssid_override gate); the built SSID
        // wins instead.
        let ssid = resolve_ap_ssid("MyOwnNetwork", "00ff");
        assert_eq!(ssid, "ADOS-GS-00FF");
    }

    #[test]
    fn ap_interface_defaults_to_wlan0_and_honours_an_override() {
        // No hotspot section → the default AP interface.
        assert_eq!(ap_interface(&json!({})), "wlan0");
        // A blank interface falls back to the default.
        assert_eq!(
            ap_interface(&json!({"network": {"hotspot": {"interface": "  "}}})),
            "wlan0"
        );
        // A configured interface is honoured verbatim.
        assert_eq!(
            ap_interface(&json!({"network": {"hotspot": {"interface": "ap0"}}})),
            "ap0"
        );
    }

    #[test]
    fn ap_view_default_channel_is_six_when_unset() {
        // No channel field → the manager's default 6, never null.
        let cfg = json!({"network": {"hotspot": {}}});
        let channel = cfg
            .get("network")
            .and_then(|n| n.get("hotspot"))
            .and_then(|h| h.get("channel"))
            .and_then(json_to_i64)
            .unwrap_or(AP_DEFAULT_CHANNEL);
        assert_eq!(channel, 6);
    }

    #[test]
    fn parse_station_dump_extracts_lowercased_macs() {
        // The `iw … station dump` output: each station starts with a
        // `Station <mac> (on <iface>)` line; the MAC is the second token,
        // lowercased. Indented detail lines (tx bytes, signal, etc.) are ignored.
        let dump = "Station DC:EA:E7:30:74:A6 (on wlan0)\n\
                    \tinactive time:\t40 ms\n\
                    \trx bytes:\t12345\n\
                    \tsignal:  \t-42 dBm\n\
                    Station aa:bb:cc:dd:ee:ff (on wlan0)\n\
                    \ttx bytes:\t67890\n";
        assert_eq!(
            parse_station_dump(dump),
            vec![
                "dc:ea:e7:30:74:a6".to_string(),
                "aa:bb:cc:dd:ee:ff".to_string(),
            ]
        );
        // No stations → the empty list.
        assert_eq!(parse_station_dump(""), Vec::<String>::new());
        assert_eq!(
            parse_station_dump("Some unrelated output\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn short_id_matches_the_python_short_id() {
        // First 4 hex chars, uppercased.
        assert_eq!(short_id("deadbeef"), "DEAD");
        // Non-hex characters are stripped before taking the first 4.
        assert_eq!(short_id("xy-12-34-56"), "1234");
        // Fewer than 4 hex chars zero-pad on the right.
        assert_eq!(short_id("a1"), "A100");
        // No hex at all falls back to the all-zero placeholder.
        assert_eq!(short_id(""), "0000");
        assert_eq!(short_id("zzzz"), "0000");
    }

    #[test]
    fn resolve_ap_ssid_gate_matches_the_hostapd_override_rule() {
        // Honored: non-empty, no template, ADOS-GS- prefix.
        assert_eq!(resolve_ap_ssid("ADOS-GS-1234", "ffff"), "ADOS-GS-1234");
        // Rejected: carries the template placeholder.
        assert_eq!(
            resolve_ap_ssid("ADOS-GS-{device_id}", "abcd"),
            "ADOS-GS-ABCD"
        );
        // Rejected: empty.
        assert_eq!(resolve_ap_ssid("", "abcd"), "ADOS-GS-ABCD");
        // Rejected: wrong prefix.
        assert_eq!(resolve_ap_ssid("Other-1234", "abcd"), "ADOS-GS-ABCD");
    }

    #[test]
    fn modem_view_default_legs_without_a_store() {
        // The config legs (enabled / apn) come off the modem JSON; the cap is
        // computed from cap_gb; the usage legs are zero with no store overlay.
        let cfg: Map<String, Value> =
            serde_json::from_value(json!({"enabled": true, "apn": "internet", "cap_gb": 2.0}))
                .unwrap();
        let v = modem_view_from(&cfg, None);
        assert_eq!(v["enabled"], json!(true));
        assert_eq!(v["apn"], json!("internet"));
        assert_eq!(v["cap_mb"], json!(2048)); // 2 GB → 2048 MB
        assert_eq!(v["data_used_mb"], json!(0));
        assert_eq!(v["percent"], json!(0.0));
        assert_eq!(v["connected"], json!(false));
        assert_eq!(v["state"], json!("disconnected"));
        // The connectivity legs carry the live manager's no-modem defaults, NOT
        // nulls: iface "wwan0", signal_quality -1, technology "unknown",
        // operator "".
        assert_eq!(v["iface"], json!("wwan0"));
        assert_eq!(v["signal_quality"], json!(-1));
        assert_eq!(v["technology"], json!("unknown"));
        assert_eq!(v["operator"], json!(""));
        // An absent apn reads as null, matching the Python st.get("apn") or
        // cfg.get("apn") over an empty modem config.
        let empty = Map::new();
        let v2 = modem_view_from(&empty, None);
        assert_eq!(v2["apn"], Value::Null);
        assert_eq!(v2["enabled"], json!(false));
        assert_eq!(v2["cap_mb"], json!(0));
    }

    #[test]
    fn modem_view_overlays_the_store_usage_block() {
        let cfg: Map<String, Value> =
            serde_json::from_value(json!({"enabled": true, "cap_gb": 1.0})).unwrap();
        let store: Map<String, Value> =
            serde_json::from_value(json!({"data_used_mb": 512, "cap_mb": 1000, "percent": 51.234}))
                .unwrap();
        let v = modem_view_from(&cfg, Some(&store));
        assert_eq!(v["data_used_mb"], json!(512));
        assert_eq!(v["cap_mb"], json!(1000)); // store cap wins over the config cap
        assert_eq!(v["percent"], json!(51.23)); // rounded to 2 decimals
    }

    #[test]
    fn wifi_client_view_default_shape_when_the_socket_is_down() {
        let v = wifi_client_view_from(None, false);
        let want = json!({
            "enabled_on_boot": false,
            "connected": false,
            "ssid": null,
            "signal": null,
            "ip": null,
        });
        assert_eq!(v, want);
    }

    #[test]
    fn wifi_client_view_reshapes_a_status_reply() {
        // A wifi_status reply carries the full station status; the view picks the
        // four live legs + the enabled_on_boot flag from the client config.
        let status: Map<String, Value> = serde_json::from_value(json!({
            "ok": true,
            "connected": true,
            "ssid": "HomeNet",
            "bssid": "aa:bb:cc:dd:ee:ff",
            "signal": 72,
            "ip": "192.168.1.50",
            "gateway": "192.168.1.1",
            "security": "WPA2",
        }))
        .unwrap();
        let v = wifi_client_view_from(Some(&status), true);
        let want = json!({
            "enabled_on_boot": true,
            "connected": true,
            "ssid": "HomeNet",
            "signal": 72,
            "ip": "192.168.1.50",
        });
        assert_eq!(v, want);
    }

    #[test]
    fn priority_list_falls_back_to_the_default_chain() {
        // Absent file → the default chain.
        assert_eq!(
            priority_list_from(None),
            json!(["eth0", "wlan0_client", "wwan0", "usb0"])
        );
        // An empty list → the default chain.
        let empty: Map<String, Value> = serde_json::from_value(json!({"priority": []})).unwrap();
        assert_eq!(
            priority_list_from(Some(&empty)),
            json!(["eth0", "wlan0_client", "wwan0", "usb0"])
        );
        // A non-string member → the default chain.
        let bad: Map<String, Value> =
            serde_json::from_value(json!({"priority": ["eth0", 7]})).unwrap();
        assert_eq!(
            priority_list_from(Some(&bad)),
            json!(["eth0", "wlan0_client", "wwan0", "usb0"])
        );
    }

    #[test]
    fn priority_list_honours_a_valid_custom_list() {
        let custom: Map<String, Value> =
            serde_json::from_value(json!({"priority": ["wlan0_client", "eth0"]})).unwrap();
        assert_eq!(
            priority_list_from(Some(&custom)),
            json!(["wlan0_client", "eth0"])
        );
    }

    #[test]
    fn share_uplink_flag_reads_the_config_and_defaults_false() {
        assert!(!share_uplink_flag(&json!({})));
        assert!(!share_uplink_flag(&json!({"ground_station": {}})));
        assert!(share_uplink_flag(
            &json!({"ground_station": {"share_uplink": true}})
        ));
        assert!(!share_uplink_flag(
            &json!({"ground_station": {"share_uplink": false}})
        ));
    }

    #[test]
    fn ethernet_view_default_is_the_no_connection_shape() {
        // The live IPv4 / link legs degrade to the no-connection defaults; the
        // connection_name is `null` only when no NM ethernet profile is found.
        let want = json!({
            "mode": "dhcp",
            "connection_name": null,
            "ip": null,
            "gateway": null,
            "dns": [],
            "link": false,
            "speed_mbps": null,
            "current_ip": null,
            "current_gateway": null,
        });
        assert_eq!(ethernet_config_default(), want);
    }

    #[test]
    fn pick_primary_connection_prefers_the_active_ethernet_on_the_interface() {
        // The bench shape: an active netplan-eth0 802-3-ethernet on eth0 is the
        // primary, so connection_name reads "netplan-eth0" (NOT null).
        let saved = vec![
            vec![
                "netplan-eth0".into(),
                "802-3-ethernet".into(),
                "eth0".into(),
            ],
            vec![
                "preconfigured".into(),
                "802-11-wireless".into(),
                "wlan0".into(),
            ],
        ];
        let active = vec![vec![
            "netplan-eth0".into(),
            "802-3-ethernet".into(),
            "eth0".into(),
        ]];
        assert_eq!(
            pick_primary_connection_name(&saved, &active, "eth0"),
            Some("netplan-eth0".to_string())
        );
    }

    #[test]
    fn pick_primary_connection_falls_back_to_a_saved_profile_on_the_interface() {
        // No active ethernet, but a saved profile pinned to eth0 → that name.
        let saved = vec![vec![
            "Wired connection 1".into(),
            "802-3-ethernet".into(),
            "eth0".into(),
        ]];
        let active: Vec<Vec<String>> = vec![];
        assert_eq!(
            pick_primary_connection_name(&saved, &active, "eth0"),
            Some("Wired connection 1".to_string())
        );
    }

    #[test]
    fn pick_primary_connection_falls_back_to_the_first_ethernet_of_any_device() {
        // No match on the interface; the first saved ethernet of any device wins.
        let saved = vec![vec![
            "Office".into(),
            "802-3-ethernet".into(),
            "enp3s0".into(),
        ]];
        let active: Vec<Vec<String>> = vec![];
        assert_eq!(
            pick_primary_connection_name(&saved, &active, "eth0"),
            Some("Office".to_string())
        );
    }

    #[test]
    fn pick_primary_connection_returns_none_without_an_ethernet_profile() {
        // Only wireless connections → null, the non-NM-ethernet default.
        let saved = vec![vec![
            "HomeWiFi".into(),
            "802-11-wireless".into(),
            "wlan0".into(),
        ]];
        let active: Vec<Vec<String>> = vec![];
        assert_eq!(pick_primary_connection_name(&saved, &active, "eth0"), None);
        // An empty NM list (no nmcli / no connections) is also null.
        assert_eq!(pick_primary_connection_name(&[], &[], "eth0"), None);
    }

    #[test]
    fn parse_nmcli_terse_handles_escapes_and_short_rows() {
        // Plain colon split, truncated to the requested column count.
        assert_eq!(
            parse_nmcli_terse("netplan-eth0:802-3-ethernet:eth0\n", 3),
            vec![vec![
                "netplan-eth0".to_string(),
                "802-3-ethernet".to_string(),
                "eth0".to_string()
            ]]
        );
        // A name with an escaped colon stays one field.
        assert_eq!(
            parse_nmcli_terse_line(r"My\:Conn:802-3-ethernet:eth0"),
            vec!["My:Conn", "802-3-ethernet", "eth0"]
        );
        // Blank lines are skipped and rows shorter than the field count dropped.
        assert_eq!(
            parse_nmcli_terse("a:b:c\n\nx:y\n", 3),
            vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]]
        );
        // A missing trailing device column is preserved as an empty field.
        assert_eq!(
            parse_nmcli_terse_line("name:802-3-ethernet:"),
            vec!["name", "802-3-ethernet", ""]
        );
    }

    #[test]
    fn modem_status_reason_tracks_the_mmcli_gate() {
        // The reason is the Python `which mmcli` gate: `modemmanager_not_installed`
        // when mmcli is absent, else `no_modem`. The CI host has no ModemManager,
        // so the reason here is `modemmanager_not_installed`; assert the reason
        // selection is consistent with the gate rather than pinning the host's
        // mmcli presence.
        let reason = if mmcli_available() {
            "no_modem"
        } else {
            "modemmanager_not_installed"
        };
        let body = json!({"present": false, "reason": reason});
        assert_eq!(body["present"], json!(false));
        // Both reasons are non-empty and are exactly the two Python `_build_snapshot`
        // `present:false` strings the front can serve.
        assert!(["no_modem", "modemmanager_not_installed"].contains(&reason));
        // On the CI host (no ModemManager) the gate resolves to the
        // not-installed reason — the shape the bench observed against the live
        // Python, NOT the prior hard-coded `no_modem`.
        if !mmcli_available() {
            assert_eq!(body["reason"], json!("modemmanager_not_installed"));
        }
    }

    #[test]
    fn profile_mismatch_body_is_the_object_detail() {
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn json_truthy_matches_python_bool() {
        assert!(!json_truthy(&Value::Null));
        assert!(!json_truthy(&json!(false)));
        assert!(json_truthy(&json!(true)));
        assert!(!json_truthy(&json!(0)));
        assert!(json_truthy(&json!(1)));
        assert!(!json_truthy(&json!("")));
        assert!(json_truthy(&json!("x")));
    }

    #[test]
    fn de_chunk_reassembles_a_chunked_body() {
        let chunked = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(de_chunk(chunked), b"hello world");
    }

    #[test]
    fn parse_http_response_reads_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{}");
    }

    // --- Pure test seams mirroring the async handlers' composition, so the
    // shapes can be asserted without the socket / store wiring. ---

    /// The `wifi_client` leg as composed from a `wifi_status` reply (or `None`)
    /// + the `enabled_on_boot` flag, without the socket / file IO.
    fn wifi_client_view_from(status: Option<&Map<String, Value>>, enabled_on_boot: bool) -> Value {
        match status {
            None => json!({
                "enabled_on_boot": false,
                "connected": false,
                "ssid": Value::Null,
                "signal": Value::Null,
                "ip": Value::Null,
            }),
            Some(st) => json!({
                "enabled_on_boot": enabled_on_boot,
                "connected": st.get("connected").map(json_truthy).unwrap_or(false),
                "ssid": st.get("ssid").cloned().unwrap_or(Value::Null),
                "signal": st.get("signal").cloned().unwrap_or(Value::Null),
                "ip": st.get("ip").cloned().unwrap_or(Value::Null),
            }),
        }
    }

    /// The `modem_4g` leg as composed from the modem config + an optional store
    /// usage block, without the file / store IO.
    fn modem_view_from(cfg: &Map<String, Value>, store: Option<&Map<String, Value>>) -> Value {
        let enabled = cfg.get("enabled").map(json_truthy).unwrap_or(false);
        let apn = cfg
            .get("apn")
            .filter(|v| v.is_string())
            .cloned()
            .unwrap_or(Value::Null);
        let mut cap_mb: i64 = cfg
            .get("cap_gb")
            .and_then(json_to_f64)
            .map(|gb| (gb * 1024.0) as i64)
            .unwrap_or(0);
        let mut data_used_mb: i64 = 0;
        let mut percent: f64 = 0.0;
        if let Some(s) = store {
            if let Some(v) = s.get("data_used_mb").and_then(json_to_f64) {
                data_used_mb = v as i64;
            }
            if let Some(v) = s.get("cap_mb").and_then(json_to_f64) {
                cap_mb = v as i64;
            }
            if let Some(v) = s.get("percent").and_then(json_to_f64) {
                percent = round2(v);
            }
        }
        json!({
            "enabled": enabled,
            "connected": false,
            "iface": "wwan0",
            "ip": Value::Null,
            "signal_quality": -1,
            "technology": "unknown",
            "apn": apn,
            "operator": "",
            "data_used_mb": data_used_mb,
            "cap_mb": cap_mb,
            "percent": percent,
            "state": "disconnected",
        })
    }

    /// The priority list as composed from an optional priority-file object.
    fn priority_list_from(obj: Option<&Map<String, Value>>) -> Value {
        let default = || Value::Array(DEFAULT_PRIORITY.iter().map(|s| json!(s)).collect());
        let Some(obj) = obj else { return default() };
        let Some(arr) = obj.get("priority").and_then(Value::as_array) else {
            return default();
        };
        if arr.is_empty() || !arr.iter().all(Value::is_string) {
            return default();
        }
        Value::Array(arr.clone())
    }

    /// The ethernet `config()` no-connection default, mirroring the route body.
    fn ethernet_config_default() -> Value {
        json!({
            "mode": "dhcp",
            "connection_name": Value::Null,
            "ip": Value::Null,
            "gateway": Value::Null,
            "dns": [],
            "link": false,
            "speed_mbps": Value::Null,
            "current_ip": Value::Null,
            "current_gateway": Value::Null,
        })
    }
}
