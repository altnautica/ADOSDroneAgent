//! Profile-agnostic Wi-Fi client write routes (join / leave / forget / autoconnect).
//!
//! Four operator on-demand writes that mutate the upstream Wi-Fi-client link:
//!
//! - **`PUT /api/v1/network/client/join`** — join a Wi-Fi network. The body is
//!   `{"ssid", "passphrase"?, "force"?}`; the route returns
//!   `{"joined", "ip", "gateway", "error"}`, or a `409` with the AP-busy code
//!   when an AP is active and `force` was not set.
//! - **`DELETE /api/v1/network/client`** — disconnect the current Wi-Fi-client
//!   link; returns `{"left", "previous_ssid"}`.
//! - **`DELETE /api/v1/network/client/configured/{name}`** — forget a saved
//!   profile by name; returns `{"forgot", "name", "error"}`, or a `400` when the
//!   delete failed.
//! - **`PUT /api/v1/network/client/configured/{name}/autoconnect`** — toggle the
//!   NetworkManager autoconnect flag of a saved profile. The body is
//!   `{"enabled"?}` (a missing / falsey value is `false`); the route returns
//!   `{"autoconnect", "name", "error"}`, or a `400` when the manager reports an
//!   `error` (a missing connection / an `nmcli` failure / an empty name).
//!
//! ## Why these forward to the `ados-net` command socket (the working write path)
//!
//! On this native front the uplink loop runs in a sibling `ados-net` daemon that
//! holds the `wlan0` AP/STA advisory lock and owns the saved Wi-Fi profiles. The
//! front MUST NOT drive `nmcli` itself, or it would race the daemon's own
//! Wi-Fi-client manager (the same reason the read side serves an empty scan
//! instead of scanning, see [`crate::routes::gs_network`]). So each write forwards
//! to the daemon's command socket at `/run/ados/wifi-cmd.sock` with one
//! newline-terminated JSON request, reads one newline-terminated JSON reply, then
//! strips the transport `ok` flag so the body matches the manager's own shape. The
//! ops are `wifi_join` / `wifi_leave` / `wifi_forget` / `wifi_autoconnect`.
//!
//! ## Degrade posture (parity with the FastAPI route's reachable arm)
//!
//! The FastAPI join/leave/forget routes fall back to the packaged in-process
//! `nmcli` manager when the socket is unreachable. The native front cannot mirror
//! that fallback — it must not drive `nmcli` and race the daemon — so an
//! unreachable / non-replying socket degrades to a `503 "Wi-Fi command socket
//! unavailable"` rather than a `500`, the same no-link posture the param-write
//! surface takes when its own seam (the MAVLink socket) is absent. The command is
//! never silently dropped. The autoconnect route's FastAPI twin has no fallback
//! arm at all (it always calls the manager directly), so its unreachable case
//! takes the same `503` posture.
//!
//! A reply with `ok:false` carries the daemon's `error` code; the route surfaces
//! it as a `500` with the FastAPI `E_WIFI_*_FAILED` error-object body, matching
//! the FastAPI server-reported-failure arm.

use axum::extract::{Path, State};

use crate::state::AppState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// The Wi-Fi command socket seam.
// ---------------------------------------------------------------------------

/// The native `ados-net` Wi-Fi command socket (`/run/ados/wifi-cmd.sock`), which
/// applies the `wifi_join` / `wifi_leave` / `wifi_forget` ops through the daemon's
/// single Wi-Fi-client manager (the owner of the `wlan0` AP/STA lock).
pub(crate) fn wifi_cmd_sock() -> std::path::PathBuf {
    wifi_cmd_sock_in(&ados_protocol::paths::run_dir())
}

/// The Wi-Fi command socket under an explicit runtime directory.
pub(crate) fn wifi_cmd_sock_in(run_dir: &std::path::Path) -> std::path::PathBuf {
    run_dir.join(ados_protocol::paths::WIFI_CMD_SOCK)
}

/// The outcome of a command-socket round-trip.
enum NetCmd {
    /// A reply with `ok:true` (or no `ok` field): the manager result object with
    /// the transport `ok` flag stripped.
    Reply(Map<String, Value>),
    /// A reply with `ok:false`: the daemon's `error` code (or a generic message
    /// when the field is absent), surfaced as the FastAPI server-failure 500.
    Error(String),
    /// The socket was unreachable / did not reply / replied unparseably: the
    /// FastAPI command-socket-unavailable case the front maps to a 503.
    Unavailable,
    /// The daemon accepted the command but had not answered by the deadline. The
    /// operation may still complete (a join waits on NetworkManager), so this is
    /// a 504 telling the caller to poll the station status, never "not running".
    TimedOut,
}

/// Send one newline-terminated JSON request to the Wi-Fi command socket and read
/// one newline-terminated JSON reply, branching on the transport `ok` flag.
///
/// A reachable socket that replies with `ok:true` yields [`NetCmd::Reply`] with
/// the `ok` key removed; `ok:false` yields [`NetCmd::Error`] with the reply's
/// `error` code; a daemon that accepted but did not answer in time yields
/// [`NetCmd::TimedOut`]; an unreachable socket / a read error / an unparseable
/// or non-object reply yield [`NetCmd::Unavailable`] so the caller can take the
/// front's no-fallback 503 posture. The read is bounded so a runaway reply
/// cannot exhaust memory.
async fn wifi_cmd(sock: &std::path::Path, request: &Value) -> NetCmd {
    match crate::ipc::cmd::roundtrip_line(sock, request, WIFI_CMD_TIMEOUT).await {
        Ok(first) => classify_reply(&first),
        Err(crate::ipc::cmd::CmdFailure::Timeout) => NetCmd::TimedOut,
        Err(_) => NetCmd::Unavailable,
    }
}

/// A join waits on NetworkManager (up to 30 s daemon-side) before the manager
/// replies, so the deadline sits above that.
pub(crate) const WIFI_CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Branch a raw reply line on its transport `ok` flag, mirroring the FastAPI
/// round-trip tail (`resp.get("ok") is False` → server-failure error, else strip
/// `ok`). An unparseable / non-object reply is treated as unavailable.
fn classify_reply(line: &str) -> NetCmd {
    let parsed: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return NetCmd::Unavailable,
    };
    let Some(obj) = parsed.as_object() else {
        return NetCmd::Unavailable;
    };
    // The FastAPI client branches only on `ok is False`; any other value
    // (true / absent) proceeds to strip-ok and return the body.
    if obj.get("ok") == Some(&Value::Bool(false)) {
        let err = obj
            .get("error")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown wifi command error")
            .to_string();
        return NetCmd::Error(err);
    }
    // Strip the transport `ok` flag so the body matches the manager's own shape.
    let mut stripped = obj.clone();
    stripped.remove("ok");
    NetCmd::Reply(stripped)
}

// ---------------------------------------------------------------------------
// Error envelopes (the FastAPI E_WIFI_* error-object bodies).
// ---------------------------------------------------------------------------

/// Build the FastAPI Wi-Fi error body: `(status, {"detail": {"error": {"code",
/// "message"}}})`. The Wi-Fi routes raise an error whose `detail` is an error
/// OBJECT (not the bare-string `detail` the rest of this surface uses), so these
/// reproduce that exact nested shape.
fn wifi_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"detail": {"error": {"code": code, "message": message.into()}}})),
    )
        .into_response()
}

/// The native no-fallback 503 the front returns when the command socket is
/// unreachable. The FastAPI route falls back to the in-process `nmcli` manager
/// here; the front cannot (it must not race the daemon for the radio), so it takes
/// the same no-link posture the param-write surface takes on an absent seam.
fn socket_unavailable(code: &str) -> Response {
    wifi_error(
        StatusCode::SERVICE_UNAVAILABLE,
        code,
        "Wi-Fi command socket unavailable",
    )
}

/// The 504 for a command the daemon accepted but had not finished by the
/// deadline. It may still complete, so the caller polls the station status
/// instead of retrying into the in-flight operation.
fn command_in_progress(code: &str) -> Response {
    wifi_error(
        StatusCode::GATEWAY_TIMEOUT,
        code,
        "The Wi-Fi change is still in progress. Poll /api/v1/network/client/status for the result.",
    )
}

// ---------------------------------------------------------------------------
// PUT /api/v1/network/client/join
// ---------------------------------------------------------------------------

/// The `PUT /client/join` request body. Mirrors the FastAPI join request: a
/// required `ssid`, an optional `passphrase`, and an optional `force` flag
/// (defaulting false).
#[derive(Debug, Deserialize)]
pub struct WifiJoinRequest {
    pub ssid: String,
    #[serde(default)]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub force: Option<bool>,
}

/// `PUT /api/v1/network/client/join` → `{"joined", "ip", "gateway", "error"}`.
///
/// Forwards a `wifi_join` op to the `ados-net` command socket. A reply with
/// `joined:false` and the AP-busy error code maps to the FastAPI `409`
/// (`E_WLAN0_BUSY_AP_ACTIVE` + `needs_force:true`); every other reply maps to the
/// `{joined, ip, gateway, error}` success body the FastAPI route composes (each
/// field defaulting to false/null when absent). An unreachable socket → 503; an
/// `ok:false` reply → the FastAPI `E_WIFI_JOIN_FAILED` 500.
pub async fn put_client_join(
    State(state): State<AppState>,
    Json(req): Json<WifiJoinRequest>,
) -> Response {
    put_client_join_at(&wifi_cmd_sock_in(&state.run_dir), req).await
}

/// The path-injectable core of [`put_client_join`]: forward against an explicit
/// command-socket path. Threaded so a test drives it against a tempdir without
/// mutating the process-global `ADOS_RUN_DIR`.
async fn put_client_join_at(sock: &std::path::Path, req: WifiJoinRequest) -> Response {
    let request = json!({
        "op": "wifi_join",
        "ssid": req.ssid,
        "passphrase": req.passphrase,
        "force": req.force.unwrap_or(false),
    });
    let reply = match wifi_cmd(sock, &request).await {
        NetCmd::Reply(r) => r,
        NetCmd::Error(msg) => {
            return wifi_error(StatusCode::INTERNAL_SERVER_ERROR, "E_WIFI_JOIN_FAILED", msg);
        }
        NetCmd::Unavailable => return socket_unavailable("E_WIFI_JOIN_FAILED"),
        NetCmd::TimedOut => return command_in_progress("E_WIFI_JOIN_IN_PROGRESS"),
    };

    // The AP-mutex conflict: a join refused because the AP is up and `force` was
    // not set. The FastAPI route turns this single result into a 409.
    let joined = reply.get("joined").map(json_truthy).unwrap_or(false);
    if !joined {
        let err = reply.get("error").and_then(Value::as_str).unwrap_or("");
        if err == "station_busy_ap_active" {
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
    }

    Json(join_response(&reply)).into_response()
}

/// Build the `{joined, ip, gateway, error}` success body, mirroring the FastAPI
/// route's final dict: each field is read off the reply, defaulting to
/// false/null when absent.
fn join_response(reply: &Map<String, Value>) -> Value {
    json!({
        "joined": reply.get("joined").map(json_truthy).unwrap_or(false),
        "ip": reply.get("ip").cloned().unwrap_or(Value::Null),
        "gateway": reply.get("gateway").cloned().unwrap_or(Value::Null),
        "error": reply.get("error").cloned().unwrap_or(Value::Null),
    })
}

// ---------------------------------------------------------------------------
// Nearby-network scan (shared by the station and ground-station scan reads)
// ---------------------------------------------------------------------------

/// Why a scan produced no network list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WifiScanError {
    /// Nothing serves the Wi-Fi command socket.
    Unavailable,
    /// The daemon accepted the scan but had not answered by the deadline.
    TimedOut,
    /// The daemon ran the scan and it failed (no station radio, `iw` error).
    Failed(String),
}

/// Run a nearby-network scan through the Wi-Fi command socket's `wifi_scan`
/// op. The daemon owns the station radio, so the scan runs there (serialized
/// with joins) rather than in this process. Each row is
/// `{ssid, bssid, signal (dBm), frequency_mhz, channel, security, in_use}`,
/// strongest first.
pub(crate) async fn wifi_scan() -> Result<Vec<Value>, WifiScanError> {
    wifi_scan_at(&wifi_cmd_sock()).await
}

/// The path-injectable core of [`wifi_scan`], for tests.
pub(crate) async fn wifi_scan_at(sock: &std::path::Path) -> Result<Vec<Value>, WifiScanError> {
    match wifi_cmd(sock, &json!({"op": "wifi_scan"})).await {
        NetCmd::Reply(mut r) => Ok(match r.remove("networks") {
            Some(Value::Array(rows)) => rows,
            _ => Vec::new(),
        }),
        NetCmd::Error(msg) => Err(WifiScanError::Failed(msg)),
        NetCmd::Unavailable => Err(WifiScanError::Unavailable),
        NetCmd::TimedOut => Err(WifiScanError::TimedOut),
    }
}

/// The HTTP answer for a scan outcome: `{"networks": [...]}`, a 503
/// `E_SCAN_UNAVAILABLE` when nothing serves the socket, a 504 when the scan
/// outlived the deadline, or a 500 `E_WIFI_SCAN_FAILED` carrying the daemon's
/// reason.
pub(crate) fn wifi_scan_response(result: Result<Vec<Value>, WifiScanError>) -> Response {
    match result {
        Ok(networks) => Json(json!({ "networks": networks })).into_response(),
        Err(WifiScanError::Unavailable) => socket_unavailable("E_SCAN_UNAVAILABLE"),
        Err(WifiScanError::TimedOut) => wifi_error(
            StatusCode::GATEWAY_TIMEOUT,
            "E_WIFI_SCAN_TIMEOUT",
            "The scan did not finish in time. Try again.",
        ),
        Err(WifiScanError::Failed(msg)) => {
            wifi_error(StatusCode::INTERNAL_SERVER_ERROR, "E_WIFI_SCAN_FAILED", msg)
        }
    }
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/network/client
// ---------------------------------------------------------------------------

/// `DELETE /api/v1/network/client` → `{"left", "previous_ssid"}`.
///
/// Forwards a `wifi_leave` op to the `ados-net` command socket and returns the
/// reply verbatim (the `ok` flag already stripped). An unreachable socket → 503;
/// an `ok:false` reply → the FastAPI `E_WIFI_LEAVE_FAILED` 500.
pub async fn delete_client(State(state): State<AppState>) -> Response {
    delete_client_at(&wifi_cmd_sock_in(&state.run_dir)).await
}

/// The path-injectable core of [`delete_client`], for tests.
async fn delete_client_at(sock: &std::path::Path) -> Response {
    match wifi_cmd(sock, &json!({"op": "wifi_leave"})).await {
        NetCmd::Reply(r) => Json(Value::Object(r)).into_response(),
        NetCmd::Error(msg) => wifi_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_WIFI_LEAVE_FAILED",
            msg,
        ),
        NetCmd::Unavailable => socket_unavailable("E_WIFI_LEAVE_FAILED"),
        NetCmd::TimedOut => command_in_progress("E_WIFI_LEAVE_IN_PROGRESS"),
    }
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/network/client/configured/{name}
// ---------------------------------------------------------------------------

/// `DELETE /api/v1/network/client/configured/{name}` →
/// `{"forgot", "name", "error"}`.
///
/// Forwards a `wifi_forget` op to the `ados-net` command socket. A reply with
/// `forgot:false` maps to the FastAPI `400` (`E_WIFI_FORGET_FAILED` carrying the
/// reply's `error`); a `forgot:true` reply is returned verbatim. An unreachable
/// socket → 503; an `ok:false` reply → the FastAPI `E_WIFI_FORGET_FAILED` 500.
pub async fn delete_client_configured(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    delete_client_configured_at(&wifi_cmd_sock_in(&state.run_dir), name).await
}

/// The path-injectable core of [`delete_client_configured`], for tests.
async fn delete_client_configured_at(sock: &std::path::Path, name: String) -> Response {
    let reply = match wifi_cmd(sock, &json!({"op": "wifi_forget", "name": name})).await {
        NetCmd::Reply(r) => r,
        NetCmd::Error(msg) => {
            return wifi_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_WIFI_FORGET_FAILED",
                msg,
            );
        }
        NetCmd::Unavailable => return socket_unavailable("E_WIFI_FORGET_FAILED"),
        NetCmd::TimedOut => return command_in_progress("E_WIFI_FORGET_IN_PROGRESS"),
    };

    // A processed-but-failed forget (forgot:false) is the FastAPI 400, with the
    // reply's `error` as the message (defaulting to nmcli_failed).
    let forgot = reply.get("forgot").map(json_truthy).unwrap_or(false);
    if !forgot {
        let message = reply
            .get("error")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("nmcli_failed")
            .to_string();
        return wifi_error(StatusCode::BAD_REQUEST, "E_WIFI_FORGET_FAILED", message);
    }

    Json(Value::Object(reply)).into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/v1/network/client/configured/{name}/autoconnect
// ---------------------------------------------------------------------------

/// The `PUT .../autoconnect` request body. Mirrors the FastAPI route, which reads
/// `bool(body.get("enabled"))` off an untyped dict: a missing / null / falsey
/// value is `false`, so the field is optional and defaults to `false`.
#[derive(Debug, Default, Deserialize)]
pub struct AutoconnectRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `PUT /api/v1/network/client/configured/{name}/autoconnect` →
/// `{"autoconnect", "name", "error"}`.
///
/// Forwards a `wifi_autoconnect` op to the `ados-net` command socket (the daemon
/// runs `nmcli connection modify <name> connection.autoconnect yes|no` through its
/// Wi-Fi-client manager). A reply with a non-null `error` (a missing connection,
/// an `nmcli` failure, or an empty name → `name_required`) maps to the FastAPI
/// `400` (`E_WIFI_AUTOCONNECT_FAILED` carrying the manager's `error`); an
/// `error:null` reply is returned verbatim. An unreachable socket → 503; an
/// `ok:false` reply → the FastAPI `E_WIFI_AUTOCONNECT_FAILED` 500.
pub async fn put_client_autoconnect(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<AutoconnectRequest>,
) -> Response {
    put_client_autoconnect_at(&wifi_cmd_sock_in(&state.run_dir), name, req).await
}

/// The path-injectable core of [`put_client_autoconnect`], for tests.
async fn put_client_autoconnect_at(
    sock: &std::path::Path,
    name: String,
    req: AutoconnectRequest,
) -> Response {
    let enabled = req.enabled.unwrap_or(false);
    let request = json!({"op": "wifi_autoconnect", "name": name, "enabled": enabled});
    let reply = match wifi_cmd(sock, &request).await {
        NetCmd::Reply(r) => r,
        NetCmd::Error(msg) => {
            return wifi_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_WIFI_AUTOCONNECT_FAILED",
                msg,
            );
        }
        NetCmd::Unavailable => return socket_unavailable("E_WIFI_AUTOCONNECT_FAILED"),
        NetCmd::TimedOut => return command_in_progress("E_WIFI_AUTOCONNECT_IN_PROGRESS"),
    };

    // A processed-but-failed toggle carries a truthy `error` (the manager reports
    // name_required / nmcli_failed / the trimmed nmcli stderr); the FastAPI route
    // turns any truthy `error` into a 400 with that message.
    if let Some(err) = reply.get("error").filter(|v| json_truthy(v)) {
        let message = err
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        return wifi_error(
            StatusCode::BAD_REQUEST,
            "E_WIFI_AUTOCONNECT_FAILED",
            message,
        );
    }

    Json(Value::Object(reply)).into_response()
}

// ---------------------------------------------------------------------------
// Small shared helper.
// ---------------------------------------------------------------------------

/// Python `bool(x)` truthiness over a JSON value, matching the `result.get(...)`
/// truthiness checks the FastAPI route uses on the manager reply.
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

#[cfg(test)]
mod tests;
