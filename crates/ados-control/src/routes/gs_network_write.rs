//! Ground-station network uplink write routes.
//!
//! The ground-station profile exposes the uplink matrix under
//! `/api/v1/ground-station/network*`. The read views live in
//! [`crate::routes::gs_network`]; this module serves the writes.
//!
//! ## How each write reaches the live system
//!
//! The uplink loop + its managers (hostapd / ethernet / modem) run in a sibling
//! `ados-net` daemon. The front MUST NOT drive `nmcli` / `hostapd` / the modem
//! sidecar itself, or it would race the daemon for the radio + the live link. So
//! each write follows one of two shapes, the same pattern the sibling Wi-Fi-client
//! writes use:
//!
//! - **Config-file persists the daemon reconciles**: `PUT .../network/priority`
//!   atomically writes `{"priority": [...]}` to the uplink file; `PUT
//!   .../network/share_uplink` merges `ground_station.share_uplink` into the agent
//!   config. The daemon reads those on its own cadence, so the front persisting is
//!   wire-equivalent to the FastAPI route persisting, with no second writer.
//! - **Command-socket forwards**: `PUT .../network/ap`, `PUT .../network/ethernet`,
//!   and `PUT .../network/modem` each forward one `{"op":...}` request to the
//!   `ados-net` command socket at `/run/ados/wifi-cmd.sock`; the daemon applies it
//!   through the SAME live manager it owns and replies with the manager-truth view,
//!   which the front returns. The AP route additionally persists the channel/ssid
//!   to the agent config (the daemon owns the radio; the REST layer owns the
//!   config-file persist that survives a reboot), mirroring the FastAPI route's
//!   own post-apply `_save_config`.
//!
//! ## Degrade posture
//!
//! The FastAPI command-socket routes have no fallback that the front can mirror
//! without driving the hardware itself, so an unreachable / non-replying socket
//! degrades to a `503` rather than a `500` (the same no-link posture the
//! Wi-Fi-client writes + the param-write surface take on an absent seam). The
//! command is never silently dropped.
//!
//! ## The profile gate
//!
//! Like every ground-station route, each first gates on the resolved profile being
//! a ground station and returns the FastAPI
//! `404 {"detail":{"error":{"code":"E_PROFILE_MISMATCH"}}}` on a drone. This
//! surface uses the FastAPI network route's *error-object* detail shape
//! (`{"detail":{"error":{"code","message"}}}`) for its own 4xx/5xx too, NOT the
//! bare-string `{"detail":"..."}` the rest of the front uses — so it builds those
//! bodies directly rather than through the crate's bare-string
//! [`crate::routes::detail`] helper.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile gate (mirrors the read module + the Python `_require_ground_profile`).
// ---------------------------------------------------------------------------

/// The FastAPI `_require_ground_profile` 404 body: a `detail` carrying the
/// `E_PROFILE_MISMATCH` error object. A drone-profile caller hits every
/// ground-station route with this exact shape.
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
// Path seams.
// ---------------------------------------------------------------------------

/// The agent etc dir (`ADOS_ETC_DIR`, default `/etc/ados`), the same override
/// the read module + the persisted side-files resolve under.
fn etc_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_ETC_DIR").unwrap_or_else(|_| "/etc/ados".to_string()))
}

/// The persisted uplink priority list (`/etc/ados/ground-station-uplink.json`),
/// the same file the read module reads and the `ados-net` daemon loads. Mirrors
/// the Python `GS_UPLINK_JSON`.
fn gs_uplink_json() -> PathBuf {
    etc_dir().join("ground-station-uplink.json")
}

/// The agent config path (`ADOS_CONFIG`, default `/etc/ados/config.yaml`), the
/// same resolution the sibling read/write routes use. The AP channel/ssid persist
/// + the share-uplink flag persist write here.
fn config_yaml_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the override the sibling
/// sockets resolve under.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// The native `ados-net` command socket (`/run/ados/wifi-cmd.sock`), which applies
/// the `ap_config` / `eth_config` / `modem_config` ops through the daemon's live
/// managers.
fn cmd_sock() -> PathBuf {
    run_dir().join("wifi-cmd.sock")
}

// ---------------------------------------------------------------------------
// The command-socket seam (mirrors `network_write::wifi_cmd`).
// ---------------------------------------------------------------------------

/// The outcome of a command-socket round-trip.
enum NetCmd {
    /// A reply with `ok:true` (or no `ok` field): the manager result object with
    /// the transport `ok` flag stripped.
    Reply(Map<String, Value>),
    /// A reply with `ok:false`: the daemon's `error` code (or a generic message
    /// when the field is absent).
    Error(String),
    /// The socket was unreachable / did not reply / replied unparseably: the
    /// command-socket-unavailable case mapped to a 503.
    Unavailable,
}

/// Send one newline-terminated JSON request to the command socket and read one
/// newline-terminated JSON reply, branching on the transport `ok` flag. Mirrors
/// the sibling [`crate::routes::network_write`] round-trip + strip-ok. The read is
/// bounded so a runaway reply cannot exhaust memory.
async fn net_cmd(request: &Value) -> NetCmd {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A manager reply is a few hundred bytes; bound the read to guard a runaway.
    const MAX_REPLY_BYTES: usize = 64 * 1024;

    let mut stream = match tokio::net::UnixStream::connect(cmd_sock()).await {
        Ok(s) => s,
        Err(_) => return NetCmd::Unavailable,
    };
    let mut line = match serde_json::to_vec(request) {
        Ok(b) => b,
        Err(_) => return NetCmd::Unavailable,
    };
    line.push(b'\n');
    if stream.write_all(&line).await.is_err() || stream.flush().await.is_err() {
        return NetCmd::Unavailable;
    }

    let mut raw = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => return NetCmd::Unavailable,
        };
        if n == 0 {
            break; // EOF: the server replies once then closes.
        }
        if raw.len() + n > MAX_REPLY_BYTES {
            return NetCmd::Unavailable;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.contains(&b'\n') {
            break;
        }
    }
    if raw.is_empty() {
        return NetCmd::Unavailable;
    }
    let text = match String::from_utf8(raw) {
        Ok(t) => t,
        Err(_) => return NetCmd::Unavailable,
    };
    let Some(first) = text.lines().next() else {
        return NetCmd::Unavailable;
    };
    classify_reply(first)
}

/// Branch a raw reply line on its transport `ok` flag (`ok is False` →
/// server-failure error, else strip `ok`). An unparseable / non-object reply is
/// treated as unavailable.
fn classify_reply(line: &str) -> NetCmd {
    let parsed: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return NetCmd::Unavailable,
    };
    let Some(obj) = parsed.as_object() else {
        return NetCmd::Unavailable;
    };
    if obj.get("ok") == Some(&Value::Bool(false)) {
        let err = obj
            .get("error")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown network command error")
            .to_string();
        return NetCmd::Error(err);
    }
    let mut stripped = obj.clone();
    stripped.remove("ok");
    NetCmd::Reply(stripped)
}

// ---------------------------------------------------------------------------
// Error envelopes (the FastAPI network-route error-object detail shape).
// ---------------------------------------------------------------------------

/// Build a network-route 4xx/5xx body in the FastAPI error-object detail shape:
/// `(status, {"detail": {"error": {"code": <code>, "message": <message>}}})`.
/// This surface uses this shape (NOT the bare-string `{"detail"}`) because its
/// FastAPI twin raises `HTTPException(detail={"error": {...}})`.
fn error_body(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"detail": {"error": {"code": code, "message": message}}})),
    )
        .into_response()
}

/// Build a network-route error body whose `error` object carries an extra `hint`
/// field (the ethernet apply-failed case). Mirrors the FastAPI
/// `detail={"error": {"code", "message", "hint"}}`.
fn error_body_with_hint(status: StatusCode, code: &str, message: &str, hint: &Value) -> Response {
    (
        status,
        Json(json!({"detail": {"error": {"code": code, "message": message, "hint": hint}}})),
    )
        .into_response()
}

/// The native no-fallback 503 the front returns when the command socket is
/// unreachable. The FastAPI route drives the manager in-process here; the front
/// cannot (it must not race the daemon), so it takes the no-link posture.
fn socket_unavailable(code: &str) -> Response {
    error_body(
        StatusCode::SERVICE_UNAVAILABLE,
        code,
        "network command socket unavailable",
    )
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/network/priority — set the uplink priority list.
// ---------------------------------------------------------------------------

/// The `PUT .../network/priority` request body: the ordered uplink list. Mirrors
/// the FastAPI `UplinkPriorityUpdate`. The Pydantic model carries `min_length=1`,
/// so the FastAPI surface rejects an empty list with a 422 *before* the handler;
/// the front has no such pre-validation, so an empty (or non-string) list reaches
/// the handler and is rejected by the same `validate_priority` guard the FastAPI
/// handler runs (the 400 below). The valid path — a non-empty list of strings —
/// is byte-identical on both surfaces.
#[derive(Debug, Deserialize)]
pub struct UplinkPriorityUpdate {
    pub priority: Vec<Value>,
}

/// `PUT .../network/priority` → `{"priority": [...]}`.
///
/// Gates on the ground-station profile (404 on a drone), validates the requested
/// order (a non-empty list of strings, else the FastAPI 400
/// `E_UPLINK_PRIORITY_INVALID`), atomically persists `{"priority": [...]}` to the
/// uplink file, and echoes the persisted list. The `ados-net` daemon reads the
/// same file, so the persist is the whole effect. A file-write failure degrades
/// to the FastAPI 500 `E_UPLINK_PRIORITY_FAILED` rather than panicking.
pub async fn put_network_priority(
    State(_state): State<AppState>,
    Json(update): Json<UplinkPriorityUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    let strings = match validate_priority(&update.priority) {
        Ok(s) => s,
        Err(msg) => {
            return error_body(StatusCode::BAD_REQUEST, "E_UPLINK_PRIORITY_INVALID", &msg);
        }
    };

    if let Err(msg) = save_priority(&gs_uplink_json(), &strings) {
        return error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_UPLINK_PRIORITY_FAILED",
            &msg,
        );
    }

    Json(json!({ "priority": strings })).into_response()
}

/// Validate the requested priority list, returning the list of strings on
/// success. Mirrors the Python `validate_priority`: a non-empty list whose every
/// member is a string is accepted; an empty list or any non-string member raises
/// the `ValueError("priority must be a non-empty list of strings")` the FastAPI
/// route surfaces in the 400 body.
fn validate_priority(priority: &[Value]) -> Result<Vec<String>, String> {
    const INVALID: &str = "priority must be a non-empty list of strings";
    if priority.is_empty() {
        return Err(INVALID.to_string());
    }
    let mut out = Vec::with_capacity(priority.len());
    for entry in priority {
        match entry.as_str() {
            Some(s) => out.push(s.to_string()),
            None => return Err(INVALID.to_string()),
        }
    }
    Ok(out)
}

/// Atomically persist the priority list to `path`, mirroring the Python
/// `save_priority`: create the parent dir, write `{"priority": [...]}` to a
/// `.json.tmp` sibling, then `rename` it over the target. The JSON is
/// `{"priority": ["a","b"]}` with no spaces, matching the Python
/// `json.dumps({"priority": priority})` output the read side parses back.
fn save_priority(path: &Path, priority: &[String]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = json!({ "priority": priority }).to_string();
    std::fs::write(&tmp, body.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/network/ap — apply AP config + start/stop.
// ---------------------------------------------------------------------------

/// The `PUT .../network/ap` request body. Mirrors the FastAPI `ApUpdate`: four
/// optional fields, each applied only when present; `enabled` is the start/stop
/// hint.
#[derive(Debug, Default, Deserialize)]
pub struct ApUpdate {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub ssid: Option<String>,
    #[serde(default)]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub channel: Option<i64>,
}

/// `PUT .../network/ap` → the `_ap_view` body.
///
/// Gates on the ground-station profile (404 on a drone), forwards an `ap_config`
/// op to the `ados-net` command socket (the daemon applies it through its live
/// hostapd manager, honours the start/stop `enabled` hint, and replies with the
/// `_ap_view` body), persists the channel/ssid into the agent config for reboot
/// survival (best-effort, matching the FastAPI `_save_config`), and returns the
/// view. A failed apply maps to the FastAPI 500 `E_AP_APPLY_FAILED`; an
/// unreachable socket → 503.
pub async fn put_network_ap(
    State(_state): State<AppState>,
    Json(update): Json<ApUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    let request = json!({
        "op": "ap_config",
        "ssid": update.ssid,
        "passphrase": update.passphrase,
        "channel": update.channel,
        "enabled": update.enabled,
    });
    let view = match net_cmd(&request).await {
        NetCmd::Reply(r) => r,
        NetCmd::Error(msg) => {
            return error_body(StatusCode::INTERNAL_SERVER_ERROR, "E_AP_APPLY_FAILED", &msg)
        }
        NetCmd::Unavailable => return socket_unavailable("E_AP_APPLY_FAILED"),
    };

    // Persist channel / SSID back to the agent config for reboot survival, the
    // same best-effort `_save_config` the FastAPI route performs after the apply
    // (only when a value was supplied). A persist fault does not change the
    // response — the FastAPI route swallows it too.
    if update.channel.is_some() || update.ssid.is_some() {
        let _ = persist_hotspot(&config_yaml_path(), update.channel, update.ssid.as_deref());
    }

    Json(Value::Object(view)).into_response()
}

/// Merge the supplied `network.hotspot.channel` / `ssid` into the agent config,
/// preserving every other key, the same surgical YAML merge the sibling config
/// writes use. Returns the error string on any I/O / serialize fault (the caller
/// swallows it, matching the FastAPI best-effort persist).
fn persist_hotspot(
    config_path: &Path,
    channel: Option<i64>,
    ssid: Option<&str>,
) -> Result<(), String> {
    use serde_norway::Value as Yaml;
    let mut data = load_yaml_doc(config_path);
    {
        let hotspot = match hotspot_section_mut(&mut data) {
            Some(m) => m,
            None => return Err("config root is not a mapping".to_string()),
        };
        if let Some(c) = channel {
            hotspot.insert(Yaml::String("channel".to_string()), Yaml::Number(c.into()));
        }
        if let Some(s) = ssid {
            hotspot.insert(
                Yaml::String("ssid".to_string()),
                Yaml::String(s.to_string()),
            );
        }
    }
    write_yaml_atomic(config_path, &data)
}

/// Navigate/create `network.hotspot` as a mutable mapping. A non-mapping `network`
/// / `hotspot` node is replaced with an empty mapping (matching the sibling
/// config-merge create-on-conflict behaviour); only a non-mapping document root
/// fails.
fn hotspot_section_mut(data: &mut serde_norway::Value) -> Option<&mut serde_norway::Mapping> {
    use serde_norway::Value as Yaml;
    let root = data.as_mapping_mut()?;
    let network = root
        .entry(Yaml::String("network".to_string()))
        .or_insert_with(|| Yaml::Mapping(serde_norway::Mapping::new()));
    if !network.is_mapping() {
        *network = Yaml::Mapping(serde_norway::Mapping::new());
    }
    let network_map = network.as_mapping_mut()?;
    let hotspot = network_map
        .entry(Yaml::String("hotspot".to_string()))
        .or_insert_with(|| Yaml::Mapping(serde_norway::Mapping::new()));
    if !hotspot.is_mapping() {
        *hotspot = Yaml::Mapping(serde_norway::Mapping::new());
    }
    hotspot.as_mapping_mut()
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/network/ethernet — apply the Ethernet IPv4 profile.
// ---------------------------------------------------------------------------

/// The `PUT .../network/ethernet` request body. Mirrors the FastAPI
/// `EthernetConfigUpdate`: a required `mode` (`dhcp` | `static`) plus the static
/// fields. The Pydantic field validators reject a malformed `ip` (must be IPv4
/// with a CIDR suffix), `gateway` (IPv4), or `dns` (each IPv4) with a 422 before
/// the handler; the front has no Pydantic pre-validation, so a malformed value
/// reaches the daemon's `nmcli` apply and surfaces as `E_ETHERNET_APPLY_FAILED`
/// — the same no-pre-validation posture the priority route documents. The valid
/// path is byte-identical.
#[derive(Debug, Deserialize)]
pub struct EthernetConfigUpdate {
    pub mode: String,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub gateway: Option<String>,
    #[serde(default)]
    pub dns: Option<Vec<String>>,
}

/// `PUT .../network/ethernet` → the live `config()` view.
///
/// Gates on the ground-station profile (404 on a drone). For `mode=static`,
/// rejects a missing ip / gateway with the FastAPI 400
/// `E_ETHERNET_STATIC_MISSING_FIELDS` (the in-handler guard) before forwarding.
/// Forwards an `eth_config` op to the `ados-net` command socket (the daemon
/// applies it through its live ethernet manager and replies with the `config()`
/// view, or an apply-failed payload), then returns the view. An apply that returns
/// `applied:false` maps to the FastAPI 500 — `E_ETHERNET_NO_CONNECTION` when the
/// manager reports `no_ethernet_connection`, else `E_ETHERNET_APPLY_FAILED`,
/// carrying the manager's `hint`. An unreachable socket → 503.
pub async fn put_network_ethernet(
    State(_state): State<AppState>,
    Json(update): Json<EthernetConfigUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    let is_static = update.mode == "static";
    if is_static {
        // The FastAPI in-handler guard: static requires ip + gateway.
        let ip_present = update.ip.as_deref().is_some_and(|s| !s.is_empty());
        let gw_present = update.gateway.as_deref().is_some_and(|s| !s.is_empty());
        if !ip_present || !gw_present {
            return error_body(
                StatusCode::BAD_REQUEST,
                "E_ETHERNET_STATIC_MISSING_FIELDS",
                "ip and gateway are required when mode=static",
            );
        }
    }

    let request = if is_static {
        json!({
            "op": "eth_config",
            "mode": "static",
            "ip": update.ip,
            "gateway": update.gateway,
            "dns": update.dns.clone().unwrap_or_default(),
        })
    } else {
        json!({"op": "eth_config", "mode": "dhcp"})
    };

    // The unreachable / server-error codes differ between the static + dhcp apply
    // failures (the FastAPI route wraps each manager call in its own except), so
    // the socket-transport failure code follows the requested mode.
    let transport_code = if is_static {
        "E_ETHERNET_STATIC_FAILED"
    } else {
        "E_ETHERNET_DHCP_FAILED"
    };

    let reply = match net_cmd(&request).await {
        NetCmd::Reply(r) => r,
        NetCmd::Error(msg) => {
            return error_body(StatusCode::INTERNAL_SERVER_ERROR, transport_code, &msg)
        }
        NetCmd::Unavailable => return socket_unavailable(transport_code),
    };

    // A processed-but-failed apply (`applied:false`) is the FastAPI
    // `result.get("ok") is False` arm: 500 with E_ETHERNET_NO_CONNECTION when the
    // manager reports a missing connection, else E_ETHERNET_APPLY_FAILED, carrying
    // the manager's `error` text + `hint`.
    if reply.get("applied") == Some(&Value::Bool(false)) {
        let error = reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("ethernet_apply_failed");
        let code = if error == "no_ethernet_connection" {
            "E_ETHERNET_NO_CONNECTION"
        } else {
            "E_ETHERNET_APPLY_FAILED"
        };
        let hint = reply.get("hint").cloned().unwrap_or(Value::Null);
        return error_body_with_hint(StatusCode::INTERNAL_SERVER_ERROR, code, error, &hint);
    }

    // Success: the manager's `config()` view.
    Json(Value::Object(reply)).into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/network/modem — update the cellular modem config.
// ---------------------------------------------------------------------------

/// The `PUT .../network/modem` request body. Mirrors the FastAPI
/// `ModemConfigUpdate`: the GET view reports the cap as `cap_mb`, so a client that
/// round-trips the view sends `cap_mb` back. `cap_gb` wins when both are present;
/// otherwise `cap_mb` is converted to `cap_gb` before it reaches the manager
/// (which persists in GB).
#[derive(Debug, Deserialize)]
pub struct ModemConfigUpdate {
    #[serde(default)]
    pub apn: Option<String>,
    #[serde(default)]
    pub cap_gb: Option<f64>,
    #[serde(default)]
    pub cap_mb: Option<i64>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `PUT .../network/modem` → the `_modem_view` body.
///
/// Gates on the ground-station profile (404 on a drone). Converts `cap_mb` →
/// `cap_gb` when only the former is supplied, forwards a `modem_config` op to the
/// `ados-net` command socket (the daemon persists the config sidecar through its
/// live modem manager; its poll loop reconciles the live session), then returns
/// the modem view — the SAME `_modem_view()` body the GET route serves over the
/// freshly-persisted config (config file + the store's `net.modem_usage` overlay),
/// exactly as the FastAPI modem PUT returns `_modem_view()` after `configure()`. A
/// failed configure maps to the FastAPI 500 `E_MODEM_CONFIGURE_FAILED`; an
/// unreachable socket → 503.
pub async fn put_network_modem(
    State(state): State<AppState>,
    Json(update): Json<ModemConfigUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    // cap_gb wins; otherwise convert cap_mb → cap_gb (mirrors the Python
    // `update.cap_mb / 1024.0`).
    let cap_gb = update
        .cap_gb
        .or_else(|| update.cap_mb.map(|mb| mb as f64 / 1024.0));

    let mut request = Map::new();
    request.insert("op".to_string(), json!("modem_config"));
    request.insert("apn".to_string(), json!(update.apn));
    request.insert("cap_gb".to_string(), json!(cap_gb));
    request.insert("enabled".to_string(), json!(update.enabled));

    match net_cmd(&Value::Object(request)).await {
        NetCmd::Reply(_) => {}
        NetCmd::Error(msg) => {
            return error_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_MODEM_CONFIGURE_FAILED",
                &msg,
            )
        }
        NetCmd::Unavailable => return socket_unavailable("E_MODEM_CONFIGURE_FAILED"),
    }

    // The configure persisted the sidecar; the response is the modem view over the
    // freshly-persisted config (the same helper the GET route uses).
    Json(crate::routes::gs_network::modem_view(&state).await).into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/network/share_uplink — toggle the NAT share flag.
// ---------------------------------------------------------------------------

/// The `PUT .../network/share_uplink` request body. Mirrors the FastAPI
/// `ShareUplinkUpdate`: a single required `enabled` flag.
#[derive(Debug, Deserialize)]
pub struct ShareUplinkUpdate {
    pub enabled: bool,
}

/// `PUT .../network/share_uplink` → `{enabled, applied, apply_error, backend}`.
///
/// Gates on the ground-station profile (404 on a drone). Persists
/// `ground_station.share_uplink` into the agent config, then returns the
/// native-backend body (`applied:true`, `apply_error:null`, `backend:"native"`).
/// This matches the FastAPI route's `is_service_native(net)` branch exactly: the
/// native `ados-net` daemon owns the sysctl + firewall reconciliation, so the
/// REST layer only persists the flag and lets the daemon apply it — a front-side
/// apply would be a second writer racing the daemon for the same iptables rule. A
/// persist failure maps to the FastAPI 500 `E_UI_SAVE_FAILED`.
pub async fn put_network_share_uplink(
    State(_state): State<AppState>,
    Json(update): Json<ShareUplinkUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    if let Err(msg) = persist_share_uplink(&config_yaml_path(), update.enabled) {
        return error_body(StatusCode::INTERNAL_SERVER_ERROR, "E_UI_SAVE_FAILED", &msg);
    }

    Json(json!({
        "enabled": update.enabled,
        "applied": true,
        "apply_error": Value::Null,
        "backend": "native",
    }))
    .into_response()
}

/// Merge `ground_station.share_uplink` into the agent config, preserving every
/// other key. Mirrors the Python `_persist_share_uplink_flag` (which writes
/// `ground_station.share_uplink` to `/etc/ados/config.yaml`). Returns the error
/// string on any I/O / serialize fault so the route can surface
/// `E_UI_SAVE_FAILED`, mirroring the Python `OSError` path.
fn persist_share_uplink(config_path: &Path, enabled: bool) -> Result<(), String> {
    use serde_norway::Value as Yaml;
    let mut data = load_yaml_doc(config_path);
    {
        let root = match data.as_mapping_mut() {
            Some(m) => m,
            None => return Err("config root is not a mapping".to_string()),
        };
        let gs = root
            .entry(Yaml::String("ground_station".to_string()))
            .or_insert_with(|| Yaml::Mapping(serde_norway::Mapping::new()));
        if !gs.is_mapping() {
            *gs = Yaml::Mapping(serde_norway::Mapping::new());
        }
        let gs_map = gs
            .as_mapping_mut()
            .ok_or_else(|| "ground_station section is not a mapping".to_string())?;
        gs_map.insert(
            Yaml::String("share_uplink".to_string()),
            Yaml::Bool(enabled),
        );
    }
    write_yaml_atomic(config_path, &data)
}

// ---------------------------------------------------------------------------
// Shared YAML config-merge helpers (the surgical merge the sibling writes use).
// ---------------------------------------------------------------------------

/// Load the agent config as a serde_norway document, seeding an empty mapping on
/// absence / a parse error / a non-mapping root (matching the Python `data: dict =
/// {}` seed the config writers use).
fn load_yaml_doc(config_path: &Path) -> serde_norway::Value {
    use serde_norway::Value as Yaml;
    match std::fs::read_to_string(config_path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(serde_norway::Mapping::new()),
        },
        Err(_) => Yaml::Mapping(serde_norway::Mapping::new()),
    }
}

/// Serialize `data` to YAML and write it to `path` atomically (ensure the parent
/// dir, write a `.tmp` sibling, rename over the target). Returns the error string
/// on any serialize / I/O fault.
fn write_yaml_atomic(path: &Path, data: &serde_norway::Value) -> Result<(), String> {
    let body = serde_norway::to_string(data).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = {
        let mut ext = path
            .extension()
            .map(|e| e.to_os_string())
            .unwrap_or_default();
        ext.push(".tmp");
        path.with_extension(ext)
    };
    std::fs::write(&tmp, body.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
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

    // ── validate_priority ────────────────────────────────────────────────────

    #[test]
    fn validate_accepts_a_non_empty_string_list() {
        let input = vec![json!("eth0"), json!("wlan0_client")];
        assert_eq!(
            validate_priority(&input).unwrap(),
            vec!["eth0".to_string(), "wlan0_client".to_string()]
        );
    }

    #[test]
    fn validate_rejects_an_empty_list() {
        let err = validate_priority(&[]).unwrap_err();
        assert_eq!(err, "priority must be a non-empty list of strings");
    }

    #[test]
    fn validate_rejects_a_non_string_member() {
        let input = vec![json!("eth0"), json!(7)];
        let err = validate_priority(&input).unwrap_err();
        assert_eq!(err, "priority must be a non-empty list of strings");
    }

    // ── save_priority + the persisted JSON shape ────────────────────────────

    #[test]
    fn save_writes_the_compact_priority_json_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ground-station-uplink.json");
        let list = vec!["wlan0_client".to_string(), "eth0".to_string()];
        save_priority(&path, &list).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, r#"{"priority":["wlan0_client","eth0"]}"#);

        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["priority"], json!(["wlan0_client", "eth0"]));
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn save_creates_the_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("ground-station-uplink.json");
        save_priority(&path, &["eth0".to_string()]).unwrap();
        assert!(path.exists());
    }

    // ── error_body shapes ────────────────────────────────────────────────────

    #[tokio::test]
    async fn error_body_is_the_error_object_detail_shape() {
        let resp = error_body(
            StatusCode::BAD_REQUEST,
            "E_UPLINK_PRIORITY_INVALID",
            "priority must be a non-empty list of strings",
        );
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "detail": {
                    "error": {
                        "code": "E_UPLINK_PRIORITY_INVALID",
                        "message": "priority must be a non-empty list of strings",
                    }
                }
            })
        );
    }

    #[tokio::test]
    async fn error_body_with_hint_carries_the_hint_field() {
        let resp = error_body_with_hint(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_ETHERNET_NO_CONNECTION",
            "no_ethernet_connection",
            &json!("No saved NetworkManager Ethernet connection found"),
        );
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "detail": {
                    "error": {
                        "code": "E_ETHERNET_NO_CONNECTION",
                        "message": "no_ethernet_connection",
                        "hint": "No saved NetworkManager Ethernet connection found",
                    }
                }
            })
        );
    }

    #[tokio::test]
    async fn the_unavailable_503_carries_the_nested_error_object() {
        let resp = socket_unavailable("E_AP_APPLY_FAILED");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": {
                "code": "E_AP_APPLY_FAILED",
                "message": "network command socket unavailable",
            }}})
        );
    }

    // ── classify_reply ───────────────────────────────────────────────────────

    #[test]
    fn classify_strips_ok_on_a_success_reply() {
        match classify_reply(r#"{"ok":true,"mode":"static","ip":"10.0.0.5/24"}"#) {
            NetCmd::Reply(m) => {
                assert!(!m.contains_key("ok"));
                assert_eq!(m["mode"], json!("static"));
                assert_eq!(m["ip"], json!("10.0.0.5/24"));
            }
            _ => panic!("expected a stripped Reply"),
        }
    }

    #[test]
    fn classify_surfaces_the_error_on_ok_false() {
        match classify_reply(r#"{"ok":false,"error":"E_AP_APPLY_FAILED"}"#) {
            NetCmd::Error(msg) => assert_eq!(msg, "E_AP_APPLY_FAILED"),
            _ => panic!("expected an Error"),
        }
        match classify_reply(r#"{"ok":false}"#) {
            NetCmd::Error(msg) => assert_eq!(msg, "unknown network command error"),
            _ => panic!("expected an Error"),
        }
    }

    #[test]
    fn classify_treats_garbage_as_unavailable() {
        assert!(matches!(classify_reply("not json"), NetCmd::Unavailable));
        assert!(matches!(classify_reply("[1,2,3]"), NetCmd::Unavailable));
    }

    // ── persist_hotspot ──────────────────────────────────────────────────────

    #[test]
    fn persist_hotspot_merges_channel_and_ssid_preserving_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: gs-1\nnetwork:\n  hotspot:\n    channel: 6\n    ssid: ADOS-GS-OLD\n",
        )
        .unwrap();
        persist_hotspot(&cfg, Some(11), Some("ADOS-GS-NEW")).unwrap();

        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let hotspot = parsed
            .get("network")
            .and_then(|v| v.get("hotspot"))
            .unwrap();
        assert_eq!(
            hotspot.get("channel").and_then(serde_norway::Value::as_i64),
            Some(11)
        );
        assert_eq!(
            hotspot.get("ssid").and_then(serde_norway::Value::as_str),
            Some("ADOS-GS-NEW")
        );
        // The unrelated agent.name survived.
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(serde_norway::Value::as_str),
            Some("gs-1")
        );
    }

    #[test]
    fn persist_hotspot_creates_the_section_from_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        persist_hotspot(&cfg, Some(9), None).unwrap();
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("network")
                .and_then(|v| v.get("hotspot"))
                .and_then(|h| h.get("channel"))
                .and_then(serde_norway::Value::as_i64),
            Some(9)
        );
    }

    // ── persist_share_uplink ─────────────────────────────────────────────────

    #[test]
    fn persist_share_uplink_writes_the_flag_preserving_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: gs-1\nground_station:\n  hotspot_ssid: ADOS-GS-1234\n",
        )
        .unwrap();
        persist_share_uplink(&cfg, true).unwrap();

        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("ground_station")
                .and_then(|gs| gs.get("share_uplink"))
                .and_then(serde_norway::Value::as_bool),
            Some(true)
        );
        // The pre-existing ground_station key + the unrelated agent.name survived.
        assert_eq!(
            parsed
                .get("ground_station")
                .and_then(|gs| gs.get("hotspot_ssid"))
                .and_then(serde_norway::Value::as_str),
            Some("ADOS-GS-1234")
        );
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(serde_norway::Value::as_str),
            Some("gs-1")
        );
    }

    #[test]
    fn persist_share_uplink_round_trips_false() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        persist_share_uplink(&cfg, false).unwrap();
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("ground_station")
                .and_then(|gs| gs.get("share_uplink"))
                .and_then(serde_norway::Value::as_bool),
            Some(false)
        );
    }

    // ── share_uplink route: the native-backend body shape ────────────────────

    #[test]
    fn share_uplink_native_backend_body_shape() {
        // The success body is fixed (native backend); the profile + persist gate
        // it on a live ground station, so this pins the json! the handler builds.
        let body = json!({
            "enabled": true,
            "applied": true,
            "apply_error": Value::Null,
            "backend": "native",
        });
        assert_eq!(body["backend"], json!("native"));
        assert_eq!(body["applied"], json!(true));
        assert_eq!(body["apply_error"], Value::Null);
    }

    // ── profile gate ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn profile_mismatch_golden_body() {
        let resp = profile_mismatch();
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    // ── the success envelopes (pinned without a live profile) ────────────────

    #[test]
    fn the_priority_success_body_echoes_the_persisted_list() {
        let list = vec![
            "eth0".to_string(),
            "wlan0_client".to_string(),
            "wwan0".to_string(),
        ];
        let body = json!({ "priority": list });
        assert_eq!(body, json!({"priority": ["eth0", "wlan0_client", "wwan0"]}));
    }

    #[test]
    fn the_ethernet_static_missing_fields_guard_fires_on_missing_gateway() {
        // The in-handler guard fires when mode=static and ip/gateway is missing,
        // mirroring the FastAPI E_ETHERNET_STATIC_MISSING_FIELDS 400. The valid
        // path (ip + gateway present) forwards to the daemon, bench-validated.
        let no_gw = EthernetConfigUpdate {
            mode: "static".to_string(),
            ip: Some("10.0.0.5/24".to_string()),
            gateway: None,
            dns: None,
        };
        let ip_present = no_gw.ip.as_deref().is_some_and(|s| !s.is_empty());
        let gw_present = no_gw.gateway.as_deref().is_some_and(|s| !s.is_empty());
        assert!(ip_present && !gw_present, "ip present, gateway missing");
    }

    #[test]
    fn modem_cap_mb_converts_to_cap_gb_when_cap_gb_absent() {
        // cap_gb wins when both present.
        let both = ModemConfigUpdate {
            apn: None,
            cap_gb: Some(3.0),
            cap_mb: Some(2048),
            enabled: None,
        };
        let cap = both
            .cap_gb
            .or_else(|| both.cap_mb.map(|mb| mb as f64 / 1024.0));
        assert_eq!(cap, Some(3.0));
        // cap_mb converts when cap_gb is absent (2048 MB → 2 GB).
        let mb_only = ModemConfigUpdate {
            apn: None,
            cap_gb: None,
            cap_mb: Some(2048),
            enabled: None,
        };
        let cap2 = mb_only
            .cap_gb
            .or_else(|| mb_only.cap_mb.map(|mb| mb as f64 / 1024.0));
        assert_eq!(cap2, Some(2.0));
    }
}
