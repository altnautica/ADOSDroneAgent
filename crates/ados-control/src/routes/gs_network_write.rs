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

mod ethernet_modem;
mod priority_ap;
mod share_uplink;

pub use ethernet_modem::put_network_ethernet;
pub use ethernet_modem::put_network_modem;
#[cfg(test)]
use ethernet_modem::*;
pub use priority_ap::put_network_ap;
pub use priority_ap::put_network_priority;
use priority_ap::*;
pub use share_uplink::put_network_share_uplink;
#[cfg(test)]
use share_uplink::*;

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

/// The native `ados-net` command socket (`/run/ados/wifi-cmd.sock`), which applies
/// the `ap_config` / `eth_config` / `modem_config` ops through the daemon's live
/// managers.
fn cmd_sock() -> PathBuf {
    ados_protocol::paths::run_path("wifi-cmd")
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
    /// The daemon accepted the command but had not answered by the deadline. The
    /// change may still complete, so a Wi-Fi client op reports it in progress.
    TimedOut,
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
        Err(crate::ipc::cmd::CmdFailure::Timeout) => NetCmd::TimedOut,
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

/// The 504 for a Wi-Fi client change the daemon accepted but had not finished by
/// the deadline. It may still complete, so the caller polls the network view
/// instead of retrying into the in-flight operation.
fn command_in_progress(code: &str) -> Response {
    error_body(
        StatusCode::GATEWAY_TIMEOUT,
        code,
        "The Wi-Fi change is still in progress. Poll /api/v1/ground-station/network for the result.",
    )
}

/// Check the AP settings before they reach the daemon, with the same rules the
/// daemon applies before they reach `hostapd.conf`: SSID 1-32 bytes with no
/// control characters, passphrase 8-63 printable ASCII, channel 1-13. An empty
/// string means "unchanged", as it does to the daemon.
fn validate_ap_update(update: &ApUpdate) -> Result<(), String> {
    let channel = update.channel.map(|c| u32::try_from(c).unwrap_or(0));
    ados_net::managers::validate_ap_settings(
        update.ssid.as_deref().filter(|s| !s.is_empty()),
        update.passphrase.as_deref().filter(|p| !p.is_empty()),
        channel,
    )
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
/// unreachable socket → 503; a daemon that has not answered by the deadline →
/// 504 `E_WIFI_JOIN_IN_PROGRESS`; an `ok:false` reply → `E_WIFI_JOIN_FAILED` 500.
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
        NetCmd::TimedOut => return command_in_progress("E_WIFI_JOIN_IN_PROGRESS"),
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
/// already stripped). An unreachable socket → 503; a daemon that has not
/// answered by the deadline → 504 `E_WIFI_LEAVE_IN_PROGRESS`; an `ok:false`
/// reply → `E_WIFI_LEAVE_FAILED` 500.
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
        NetCmd::TimedOut => command_in_progress("E_WIFI_LEAVE_IN_PROGRESS"),
    }
}

#[cfg(test)]
mod tests;
