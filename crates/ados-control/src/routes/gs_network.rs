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
//!   from the config hotspot (the manager-absent fallback shape). `wifi_client`
//!   from the `ados-net` Wi-Fi command socket's `wifi_status` op (+ the on-boot
//!   flag from the client config file), degrading to the all-default shape when
//!   the socket is unreachable. `ethernet` to its all-default shape (no live
//!   seam on the front). `modem_4g` from the modem config file (enabled / apn /
//!   cap) with the cumulative-usage legs overlaid from the store's most-recent
//!   `net.modem_usage` event. `active_uplink` from the store's most-recent
//!   `net.uplink_active` event (the daemon's selected uplink), else `null`.
//!   `priority` from the uplink priority file (the default chain when absent).
//!   `share_uplink` from the config flag.
//! - **`GET .../network/ethernet`** — the persisted ethernet profile + live
//!   link, degrading to the no-connection default shape.
//! - **`GET .../network/client/scan`** — nearby-network scan; the front has no
//!   scan seam, so it returns the empty-list shape (`{"networks": []}`), the
//!   same body the Python route returns when the scan finds nothing.
//! - **`GET .../network/modem`** — the modem view (same leg as `modem_4g`).
//! - **`GET .../network/priority`** — the uplink priority list.
//! - **`GET .../modem-status`** — the cellular detail snapshot; the front has no
//!   `mmcli` seam, so it returns the no-modem shape
//!   (`{"present": false, "reason": "no_modem"}`).

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

/// The AP leg. The front has no hostapd command seam, so it serves the Python
/// `_ap_view` manager-absent fallback: the AP is reported not-running, with the
/// configured SSID + channel read off `network.hotspot`. Mirrors the
/// `except` branch of `_ap_view`.
fn ap_view(cfg: &Value) -> Value {
    let hotspot = cfg
        .get("network")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("hotspot"))
        .filter(|v| v.is_object());
    let ssid = hotspot
        .and_then(|h| h.get("ssid"))
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or(Value::Null);
    let channel = hotspot
        .and_then(|h| h.get("channel"))
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or(Value::Null);
    json!({
        "enabled": false,
        "running": false,
        "ssid": ssid,
        "channel": channel,
        "interface": Value::Null,
        "gateway": Value::Null,
        "connected_clients": [],
    })
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
/// disconnected defaults; the `enabled` / `apn` / cap come off the modem config
/// file, and the cumulative-usage legs (`data_used_mb`, `cap_mb`, `percent`) are
/// overlaid from the store's most-recent `net.modem_usage` event when present.
/// Mirrors `_modem_view` with the manager status absent + the store overlay.
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
        "iface": Value::Null,
        "ip": Value::Null,
        "signal_quality": Value::Null,
        "technology": Value::Null,
        "apn": apn,
        "operator": Value::Null,
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
/// link state. 404s on a drone. The front has no live ethernet seam, so it
/// serves the no-connection default shape (`mode:"dhcp"`, every other field
/// empty / false / null), matching the Python `config()` over a box with no
/// active ethernet profile.
pub async fn get_network_ethernet() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    Json(json!({
        "mode": "dhcp",
        "connection_name": Value::Null,
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

/// `GET .../modem-status` → the cellular detail snapshot. 404s on a drone. The
/// front has no `mmcli` seam, so it serves the no-modem shape
/// (`{"present": false, "reason": "no_modem"}`), one of the
/// `present:false` degrade shapes the Python `_build_snapshot` returns.
pub async fn get_modem_status() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    Json(json!({"present": false, "reason": "no_modem"})).into_response()
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
    /// manager-absent defaults for every leg.
    #[test]
    fn network_aggregate_default_shape_is_the_golden_fixture() {
        let cfg = json!({});
        let ap = ap_view(&cfg);
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
                "ssid": null,
                "channel": null,
                "interface": null,
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
    fn ap_view_reads_the_configured_hotspot_ssid_and_channel() {
        // A config with a hotspot section seeds the ssid + channel; the AP is
        // still reported not-running (no command seam on the front).
        let cfg = json!({"network": {"hotspot": {"ssid": "ADOS-GS-abcd", "channel": 6}}});
        let ap = ap_view(&cfg);
        assert_eq!(ap["ssid"], json!("ADOS-GS-abcd"));
        assert_eq!(ap["channel"], json!(6));
        assert_eq!(ap["running"], json!(false));
        assert_eq!(ap["connected_clients"], json!([]));
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
    fn modem_status_no_modem_shape() {
        assert_eq!(
            json!({"present": false, "reason": "no_modem"}),
            json!({"present": false, "reason": "no_modem"})
        );
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
            "iface": Value::Null,
            "ip": Value::Null,
            "signal_quality": Value::Null,
            "technology": Value::Null,
            "apn": apn,
            "operator": Value::Null,
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
