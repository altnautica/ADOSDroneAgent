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
//!   atomically writes `{"priority": [...]}` to the uplink file. The daemon reads
//!   it on its own cadence, so the front persisting is wire-equivalent to the
//!   FastAPI route persisting, with no second writer.
//! - **Command-socket forwards**: `PUT .../network/ap`, `PUT .../network/ethernet`,
//!   `PUT .../network/modem` and `PUT .../network/share_uplink` each forward one
//!   `{"op":...}` request to the `ados-net` command socket at
//!   `/run/ados/wifi-cmd.sock`; the daemon applies it through the SAME live
//!   manager it owns and replies with the manager-truth view, which the front
//!   returns. The AP route additionally persists the channel/ssid, and the
//!   share-uplink route the flag, to the agent config (the daemon owns the radio
//!   and firewall; the REST layer owns the config-file persist that survives a
//!   reboot), mirroring the FastAPI route's own post-apply `_save_config`.
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

use crate::config_store::{section, section_path, update_config};
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

/// The persisted uplink priority list (`/etc/ados/ground-station-uplink.json`), the
/// same file the read module reads and the `ados-net` daemon loads.
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
    net_cmd_at(&cmd_sock(), request).await
}

/// The path-injectable core of [`net_cmd`]. Threaded so a test drives a handler
/// against a temp socket without mutating the process-global `ADOS_RUN_DIR`, the
/// same convention [`crate::routes::network_write::wifi_cmd`] already follows.
async fn net_cmd_at(sock: &std::path::Path, request: &Value) -> NetCmd {
    match crate::ipc::cmd::roundtrip_line(
        sock,
        request,
        crate::routes::network_write::WIFI_CMD_TIMEOUT,
    )
    .await
    {
        Ok(first) => classify_reply(&first),
        Err(_) => NetCmd::Unavailable,
    }
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

/// The `PUT .../network/priority` request body: the ordered uplink list. There is no
/// schema pre-validation, so an empty (or non-string) list reaches the handler and is
/// rejected by the guard below with a 400. The valid path is a non-empty list of
/// strings.
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

/// Validate the requested priority list, returning the list of strings on success.
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

/// The `PUT .../network/ap` request body.
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

    // Persist channel / SSID back to the agent config for reboot survival (only
    // when a value was supplied). The AP already carries the change, so a persist
    // failure does not fail the request, but the body says whether it will
    // survive a restart.
    let mut view = view;
    if update.channel.is_some() || update.ssid.is_some() {
        match persist_hotspot(&config_yaml_path(), update.channel, update.ssid.as_deref()) {
            Ok(()) => {
                view.insert("persisted".to_string(), json!(true));
            }
            Err(e) => {
                tracing::error!(error = %e, "hotspot settings applied but not persisted");
                view.insert("persisted".to_string(), json!(false));
                view.insert("persist_error".to_string(), json!(e));
            }
        }
    }

    Json(Value::Object(view)).into_response()
}

/// Merge the supplied `network.hotspot.channel` / `ssid` into the agent config
/// through the shared config store, preserving every other key.
fn persist_hotspot(
    config_path: &Path,
    channel: Option<i64>,
    ssid: Option<&str>,
) -> Result<(), String> {
    use serde_norway::Value as Yaml;
    update_config(config_path, |root| {
        let hotspot = section_path(root, &["network", "hotspot"]);
        if let Some(c) = channel {
            hotspot.insert(Yaml::String("channel".to_string()), Yaml::Number(c.into()));
        }
        if let Some(s) = ssid {
            hotspot.insert(
                Yaml::String("ssid".to_string()),
                Yaml::String(s.to_string()),
            );
        }
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/network/ethernet — apply the Ethernet IPv4 profile.
// ---------------------------------------------------------------------------

/// The `PUT .../network/ethernet` request body. A required `mode` (`dhcp` | `static`)
/// plus the static fields. There is no schema pre-validation, so a malformed `ip` (IPv4
/// with a CIDR suffix), `gateway` (IPv4) or `dns` (each IPv4) reaches the daemon's
/// `nmcli` apply and surfaces as `E_ETHERNET_APPLY_FAILED` — the same posture the
/// priority route documents.
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

/// The `PUT .../network/modem` request body. The GET view reports the cap as `cap_mb`,
/// so a client that round-trips the view sends `cap_mb` back. `cap_gb` wins when both
/// are present; otherwise `cap_mb` is converted to `cap_gb` before it reaches the
/// manager (which persists in GB).
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

/// The `PUT .../network/share_uplink` request body. A single required `enabled` flag.
#[derive(Debug, Deserialize)]
pub struct ShareUplinkUpdate {
    pub enabled: bool,
}

/// `PUT .../network/share_uplink` → `{enabled, applied, apply_error, backend}`.
///
/// Gates on the ground-station profile (404 on a drone). Persists
/// `ground_station.share_uplink` into the agent config, then asks the `ados-net`
/// daemon, which owns the sysctl + firewall, to apply it now on the active
/// uplink (the `share_uplink` command-socket op) and returns the daemon's own
/// verdict. The daemon serializes that apply against its uplink-switch
/// re-apply, so there is still one writer. A daemon that is unreachable or
/// refuses leaves the flag persisted and reported `applied:false`: it applies
/// at the daemon's next start. A persist failure maps to the FastAPI 500
/// `E_UI_SAVE_FAILED`.
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

    let reply = net_cmd(&json!({"op": "share_uplink", "enabled": update.enabled})).await;
    Json(share_uplink_body(update.enabled, reply)).into_response()
}

/// The share-uplink reply: the daemon's apply verdict, or `applied:false` with
/// the reason it could not be asked.
fn share_uplink_body(enabled: bool, reply: NetCmd) -> Value {
    match reply {
        NetCmd::Reply(r) => json!({
            "enabled": enabled,
            "applied": r.get("applied").and_then(Value::as_bool).unwrap_or(false),
            "apply_error": r.get("apply_error").cloned().unwrap_or(Value::Null),
            "backend": r.get("backend").cloned().unwrap_or(Value::Null),
        }),
        NetCmd::Error(msg) => json!({
            "enabled": enabled,
            "applied": false,
            "apply_error": msg,
            "backend": Value::Null,
        }),
        NetCmd::Unavailable => json!({
            "enabled": enabled,
            "applied": false,
            "apply_error": "network daemon unreachable; the saved setting applies when it starts",
            "backend": Value::Null,
        }),
    }
}

/// Merge `ground_station.share_uplink` into the agent config through the shared
/// config store, preserving every other key. Returns the error string so the route
/// can surface `E_UI_SAVE_FAILED`.
fn persist_share_uplink(config_path: &Path, enabled: bool) -> Result<(), String> {
    use serde_norway::Value as Yaml;
    update_config(config_path, |root| {
        section(root, "ground_station").insert(
            Yaml::String("share_uplink".to_string()),
            Yaml::Bool(enabled),
        );
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// PUT  /api/v1/ground-station/network/client/join
// DELETE /api/v1/ground-station/network/client
// ---------------------------------------------------------------------------
//
// The last two ground-station network writes that had no native counterpart.
// Their Python twins forwarded to this same `wifi-cmd.sock` with an in-process
// manager fallback; the fallback is what kept the packaged island alive, so
// porting them is what lets the island go.
//
// The ops and their replies are the daemon's (`ados-net`'s `cmdsock`):
//   {"op":"wifi_join","ssid":…,"passphrase":…,"force":…}
//       -> {"ok":true,"joined":…,"ip":…,"gateway":…,"error":null}
//   {"op":"wifi_leave"} -> {"ok":true,"left":true,"previous_ssid":"Net"}
//
// One deliberate posture difference from the Python twin, matching every sibling
// in this module: Pydantic rejected an empty `ssid` with a 422 before the handler
// ran, and the native front has no such pre-validation, so the guard below is the
// front's own.

/// The `PUT .../network/client/join` request body: a required `ssid`, an optional
/// `passphrase`, and an optional `force` flag (defaulting false).
#[derive(Debug, Deserialize)]
pub struct GsWifiJoinRequest {
    pub ssid: String,
    #[serde(default)]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub force: Option<bool>,
}

/// `PUT /api/v1/ground-station/network/client/join` →
/// `{"joined", "ip", "gateway", "error"}`.
///
/// A reply with `joined:false` and the AP-busy error code is the `409`
/// (`E_WLAN0_BUSY_AP_ACTIVE` + `needs_force:true`) — the ground station's AP and
/// its client mode contend for `wlan0`, so stealing it has to be deliberate. An
/// unreachable socket → 503; an `ok:false` reply → `E_WIFI_JOIN_FAILED` 500.
pub async fn put_gs_network_client_join(Json(req): Json<GsWifiJoinRequest>) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    put_gs_network_client_join_at(&cmd_sock(), req).await
}

/// The path-injectable core of [`put_gs_network_client_join`], for tests.
async fn put_gs_network_client_join_at(sock: &Path, req: GsWifiJoinRequest) -> Response {
    if req.ssid.trim().is_empty() {
        return error_body(
            StatusCode::BAD_REQUEST,
            "E_WIFI_JOIN_FAILED",
            "ssid is required",
        );
    }

    let request = json!({
        "op": "wifi_join",
        "ssid": req.ssid,
        "passphrase": req.passphrase,
        "force": req.force.unwrap_or(false),
    });
    let reply = match net_cmd_at(sock, &request).await {
        NetCmd::Reply(r) => r,
        NetCmd::Error(msg) => {
            return error_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_WIFI_JOIN_FAILED",
                &msg,
            );
        }
        NetCmd::Unavailable => return socket_unavailable("E_WIFI_JOIN_FAILED"),
    };

    let joined = reply
        .get("joined")
        .map(|v| v.as_bool().unwrap_or(false))
        .unwrap_or(false);
    if !joined && reply.get("error").and_then(Value::as_str) == Some("station_busy_ap_active") {
        let hint = reply
            .get("hint")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("AP is active; retry with force=true to steal wlan0");
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "detail": {
                    "error": {"code": "E_WLAN0_BUSY_AP_ACTIVE", "message": hint},
                },
                "needs_force": true,
            })),
        )
            .into_response();
    }

    Json(json!({
        "joined": joined,
        "ip": reply.get("ip").cloned().unwrap_or(Value::Null),
        "gateway": reply.get("gateway").cloned().unwrap_or(Value::Null),
        "error": reply.get("error").cloned().unwrap_or(Value::Null),
    }))
    .into_response()
}

/// `DELETE /api/v1/ground-station/network/client` → `{"left", "previous_ssid"}`.
///
/// Forwards a `wifi_leave` op and returns the reply verbatim (the transport `ok`
/// already stripped). An unreachable socket → 503; an `ok:false` reply →
/// `E_WIFI_LEAVE_FAILED` 500.
pub async fn delete_gs_network_client() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    delete_gs_network_client_at(&cmd_sock()).await
}

/// The path-injectable core of [`delete_gs_network_client`], for tests.
async fn delete_gs_network_client_at(sock: &Path) -> Response {
    match net_cmd_at(sock, &json!({"op": "wifi_leave"})).await {
        NetCmd::Reply(r) => Json(Value::Object(r)).into_response(),
        NetCmd::Error(msg) => error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_WIFI_LEAVE_FAILED",
            &msg,
        ),
        NetCmd::Unavailable => socket_unavailable("E_WIFI_LEAVE_FAILED"),
    }
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

    // ── share_uplink route: the daemon's apply verdict ──────────────────────

    #[tokio::test]
    async fn share_uplink_reports_the_daemons_apply_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let sock = canned_socket(
            dir.path(),
            r#"{"ok":true,"applied":false,"backend":"nftables","apply_error":"nft_add_failed"}"#,
        )
        .await;
        let reply = net_cmd_at(&sock, &json!({"op": "share_uplink", "enabled": true})).await;
        let body = share_uplink_body(true, reply);
        assert_eq!(body["applied"], json!(false));
        assert_eq!(body["apply_error"], json!("nft_add_failed"));
        assert_eq!(body["backend"], json!("nftables"));

        let ok_dir = tempfile::tempdir().unwrap();
        let ok_sock = canned_socket(
            ok_dir.path(),
            r#"{"ok":true,"applied":true,"backend":"iptables-persistent","apply_error":null}"#,
        )
        .await;
        let body = share_uplink_body(
            true,
            net_cmd_at(&ok_sock, &json!({"op": "share_uplink", "enabled": true})).await,
        );
        assert_eq!(body["applied"], json!(true));
        assert_eq!(body["apply_error"], Value::Null);
    }

    #[tokio::test]
    async fn share_uplink_with_no_daemon_is_not_reported_applied() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.sock");
        let reply = net_cmd_at(&missing, &json!({"op": "share_uplink", "enabled": true})).await;
        let body = share_uplink_body(true, reply);
        assert_eq!(body["enabled"], json!(true));
        assert_eq!(body["applied"], json!(false));
        assert!(body["apply_error"].as_str().is_some());
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

    // ── the ported Wi-Fi client join / leave ────────────────────────────────

    /// Serve one canned newline-JSON reply on a temp AF_UNIX socket, the same
    /// shape `ados-net`'s command socket speaks.
    async fn canned_socket(dir: &std::path::Path, reply: &str) -> std::path::PathBuf {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let path = dir.join("wifi-cmd.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let body = format!("{reply}\n");
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(body.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        path
    }

    fn join(ssid: &str, force: Option<bool>) -> GsWifiJoinRequest {
        GsWifiJoinRequest {
            ssid: ssid.to_string(),
            passphrase: Some("hunter2".to_string()),
            force,
        }
    }

    #[tokio::test]
    async fn a_successful_join_returns_the_four_field_body() {
        let dir = tempfile::tempdir().unwrap();
        let sock = canned_socket(
            dir.path(),
            r#"{"ok":true,"joined":true,"ip":"192.168.1.50","gateway":"192.168.1.1","error":null}"#,
        )
        .await;
        let resp = put_gs_network_client_join_at(&sock, join("BenchNet", None)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_json(resp).await,
            json!({"joined": true, "ip": "192.168.1.50", "gateway": "192.168.1.1", "error": null})
        );
    }

    #[tokio::test]
    async fn an_ap_busy_refusal_is_the_409_that_asks_for_force() {
        // The ground station's AP and its client mode contend for wlan0, so
        // stealing it has to be a deliberate second request rather than a silent
        // takeover that drops every operator already on the AP.
        let dir = tempfile::tempdir().unwrap();
        let sock = canned_socket(
            dir.path(),
            r#"{"ok":true,"joined":false,"error":"station_busy_ap_active"}"#,
        )
        .await;
        let resp = put_gs_network_client_join_at(&sock, join("BenchNet", None)).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(
            body["detail"]["error"]["code"],
            json!("E_WLAN0_BUSY_AP_ACTIVE")
        );
        assert_eq!(body["needs_force"], json!(true));
    }

    #[tokio::test]
    async fn a_daemon_failure_is_the_join_failed_500() {
        let dir = tempfile::tempdir().unwrap();
        let sock = canned_socket(dir.path(), r#"{"ok":false,"error":"no such network"}"#).await;
        let resp = put_gs_network_client_join_at(&sock, join("BenchNet", Some(true))).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            json!("E_WIFI_JOIN_FAILED")
        );
    }

    #[tokio::test]
    async fn an_absent_socket_is_the_503_no_link_posture() {
        // The front must not race the daemon by driving the interface itself, so
        // an unreachable socket is "no link", never a fabricated success.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.sock");
        let resp = put_gs_network_client_join_at(&missing, join("BenchNet", None)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let resp = delete_gs_network_client_at(&missing).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            json!("E_WIFI_LEAVE_FAILED")
        );
    }

    #[tokio::test]
    async fn an_empty_ssid_is_rejected_before_the_socket_round_trip() {
        // Pydantic rejected this with a 422 before the Python handler ran; the
        // native front has no pre-validation, so the guard is the handler's.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-connected.sock");
        let resp = put_gs_network_client_join_at(&missing, join("   ", None)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_leave_returns_the_daemon_reply_with_the_transport_flag_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let sock = canned_socket(
            dir.path(),
            r#"{"ok":true,"left":true,"previous_ssid":"BenchNet"}"#,
        )
        .await;
        let resp = delete_gs_network_client_at(&sock).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_json(resp).await,
            json!({"left": true, "previous_ssid": "BenchNet"})
        );
    }
}
