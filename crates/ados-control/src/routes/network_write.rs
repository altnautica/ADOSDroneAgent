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

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// The Wi-Fi command socket seam.
// ---------------------------------------------------------------------------

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sibling sockets + sidecars resolve under.
fn run_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()),
    )
}

/// The native `ados-net` Wi-Fi command socket (`/run/ados/wifi-cmd.sock`), which
/// applies the `wifi_join` / `wifi_leave` / `wifi_forget` ops through the daemon's
/// single Wi-Fi-client manager (the owner of the `wlan0` AP/STA lock).
fn wifi_cmd_sock() -> std::path::PathBuf {
    run_dir().join("wifi-cmd.sock")
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
}

/// Send one newline-terminated JSON request to the Wi-Fi command socket and read
/// one newline-terminated JSON reply, branching on the transport `ok` flag.
///
/// Mirrors the FastAPI Wi-Fi command client's round-trip + strip-ok: a reachable
/// socket that replies with `ok:true` yields [`NetCmd::Reply`] with the `ok` key
/// removed; `ok:false` yields [`NetCmd::Error`] with the reply's `error` code; an
/// unreachable socket / a read error / an unparseable or non-object reply all
/// yield [`NetCmd::Unavailable`] so the caller can take the front's no-fallback
/// 503 posture. The read is bounded so a runaway reply cannot exhaust memory.
async fn wifi_cmd(sock: &std::path::Path, request: &Value) -> NetCmd {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A manager reply is a few hundred bytes; bound the read to guard a runaway.
    const MAX_REPLY_BYTES: usize = 64 * 1024;

    let mut stream = match tokio::net::UnixStream::connect(sock).await {
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
        // The reply is one newline-terminated line; stop at the first newline.
        if raw.contains(&b'\n') {
            break;
        }
    }
    if raw.is_empty() {
        // The socket closed before replying — the FastAPI "closed connection
        // before replying" unavailable case.
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
pub async fn put_client_join(Json(req): Json<WifiJoinRequest>) -> Response {
    put_client_join_at(&wifi_cmd_sock(), req).await
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
    };

    // The AP-mutex conflict: a join refused because the AP is up and `force` was
    // not set. The FastAPI route turns this single result into a 409.
    let joined = reply.get("joined").map(json_truthy).unwrap_or(false);
    if !joined {
        let err = reply.get("error").and_then(Value::as_str).unwrap_or("");
        if err == "wlan0_busy_ap_active" {
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
// DELETE /api/v1/network/client
// ---------------------------------------------------------------------------

/// `DELETE /api/v1/network/client` → `{"left", "previous_ssid"}`.
///
/// Forwards a `wifi_leave` op to the `ados-net` command socket and returns the
/// reply verbatim (the `ok` flag already stripped). An unreachable socket → 503;
/// an `ok:false` reply → the FastAPI `E_WIFI_LEAVE_FAILED` 500.
pub async fn delete_client() -> Response {
    delete_client_at(&wifi_cmd_sock()).await
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
pub async fn delete_client_configured(Path(name): Path<String>) -> Response {
    delete_client_configured_at(&wifi_cmd_sock(), name).await
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
    Path(name): Path<String>,
    Json(req): Json<AutoconnectRequest>,
) -> Response {
    put_client_autoconnect_at(&wifi_cmd_sock(), name, req).await
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
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Spin a one-shot Wi-Fi command socket under a tempdir that reads one request
    /// line and replies with `reply`, then runs the handler produced by `run` (which
    /// receives the socket path so it can drive the handler `_at` core directly).
    /// Returns `{request, status, body}` so the test can assert both the op
    /// forwarded and the response. The socket path is threaded in, so the test never
    /// mutates the process-global `ADOS_RUN_DIR`.
    async fn with_socket<F, Fut>(reply: Value, run: F) -> Value
    where
        F: FnOnce(std::path::PathBuf) -> Fut,
        Fut: std::future::Future<Output = Response>,
    {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("wifi-cmd.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = conn.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if raw.contains(&b'\n') {
                    break;
                }
            }
            let line: Vec<u8> = raw.split(|&b| b == b'\n').next().unwrap().to_vec();
            let request: Value = serde_json::from_slice(&line).unwrap();
            let mut body = serde_json::to_vec(&reply).unwrap();
            body.push(b'\n');
            conn.write_all(&body).await.unwrap();
            conn.flush().await.unwrap();
            request
        });

        let resp = run(sock.clone()).await;
        let request = server.await.unwrap();
        let status = resp.status().as_u16();
        let body = body_json(resp).await;
        // The temp dir + listener drop here, closing the socket.
        drop(dir);
        json!({ "request": request, "status": status, "body": body })
    }

    // ── classify_reply ───────────────────────────────────────────────────────

    #[test]
    fn classify_strips_ok_on_a_success_reply() {
        match classify_reply(r#"{"ok":true,"joined":true,"ip":"1.2.3.4"}"#) {
            NetCmd::Reply(m) => {
                assert!(!m.contains_key("ok"), "the transport ok flag is stripped");
                assert_eq!(m["joined"], json!(true));
                assert_eq!(m["ip"], json!("1.2.3.4"));
            }
            _ => panic!("expected a stripped Reply"),
        }
    }

    #[test]
    fn classify_surfaces_the_error_on_ok_false() {
        match classify_reply(r#"{"ok":false,"error":"E_MISSING_SSID"}"#) {
            NetCmd::Error(msg) => assert_eq!(msg, "E_MISSING_SSID"),
            _ => panic!("expected an Error"),
        }
        // An ok:false with no error falls back to the generic message.
        match classify_reply(r#"{"ok":false}"#) {
            NetCmd::Error(msg) => assert_eq!(msg, "unknown wifi command error"),
            _ => panic!("expected an Error"),
        }
    }

    #[test]
    fn classify_treats_a_non_object_or_garbage_reply_as_unavailable() {
        assert!(matches!(classify_reply("not json"), NetCmd::Unavailable));
        assert!(matches!(classify_reply("[1,2,3]"), NetCmd::Unavailable));
    }

    #[test]
    fn classify_treats_an_absent_ok_as_a_success_reply() {
        // The FastAPI client only branches on `ok is False`; a reply with no ok
        // proceeds to strip-ok (a no-op here) and returns the body.
        match classify_reply(r#"{"forgot":true,"name":"Net"}"#) {
            NetCmd::Reply(m) => {
                assert_eq!(m["forgot"], json!(true));
                assert_eq!(m["name"], json!("Net"));
            }
            _ => panic!("expected a Reply"),
        }
    }

    // ── join_response ────────────────────────────────────────────────────────

    #[test]
    fn join_response_pins_the_success_body_shape() {
        let reply: Map<String, Value> = serde_json::from_value(json!({
            "joined": true,
            "ip": "192.168.1.50",
            "gateway": "192.168.1.1",
            "error": Value::Null,
        }))
        .unwrap();
        assert_eq!(
            join_response(&reply),
            json!({
                "joined": true,
                "ip": "192.168.1.50",
                "gateway": "192.168.1.1",
                "error": null,
            })
        );
    }

    #[test]
    fn join_response_defaults_every_field_when_absent() {
        let reply = Map::new();
        assert_eq!(
            join_response(&reply),
            json!({"joined": false, "ip": null, "gateway": null, "error": null})
        );
    }

    // ── error envelopes ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn the_unavailable_503_carries_the_nested_error_object() {
        let resp = socket_unavailable("E_WIFI_JOIN_FAILED");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": {
                "code": "E_WIFI_JOIN_FAILED",
                "message": "Wi-Fi command socket unavailable",
            }}})
        );
    }

    // ── the handlers against a no-socket seam (the 503 path) ─────────────────
    //
    // Each threads an absent socket path (under a tempdir) into the handler `_at`
    // core, so no test mutates the process-global ADOS_RUN_DIR.

    #[tokio::test]
    async fn join_with_no_socket_is_a_503() {
        // An absent socket path → the connect fails fast → 503.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("wifi-cmd.sock");
        let resp = put_client_join_at(
            &sock,
            WifiJoinRequest {
                ssid: "Net".to_string(),
                passphrase: Some("pw".to_string()),
                force: None,
            },
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(body["detail"]["error"]["code"], json!("E_WIFI_JOIN_FAILED"));
    }

    #[tokio::test]
    async fn leave_with_no_socket_is_a_503() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("wifi-cmd.sock");
        let resp = delete_client_at(&sock).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(
            body["detail"]["error"]["code"],
            json!("E_WIFI_LEAVE_FAILED")
        );
    }

    #[tokio::test]
    async fn forget_with_no_socket_is_a_503() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("wifi-cmd.sock");
        let resp = delete_client_configured_at(&sock, "Net".to_string()).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(
            body["detail"]["error"]["code"],
            json!("E_WIFI_FORGET_FAILED")
        );
    }

    // ── the handlers against a live mock socket (the full forward path) ───────

    #[tokio::test]
    async fn join_forwards_a_wifi_join_op_and_returns_the_success_body() {
        let out = with_socket(
            json!({"ok": true, "joined": true, "ip": "10.0.0.5", "gateway": "10.0.0.1", "error": null}),
            |sock| async move {
                put_client_join_at(
                    &sock,
                    WifiJoinRequest {
                        ssid: "HomeNet".to_string(),
                        passphrase: Some("secret".to_string()),
                        force: Some(true),
                    },
                )
                .await
            },
        )
        .await;
        // The exact op + fields forwarded to the daemon.
        assert_eq!(out["request"]["op"], json!("wifi_join"));
        assert_eq!(out["request"]["ssid"], json!("HomeNet"));
        assert_eq!(out["request"]["passphrase"], json!("secret"));
        assert_eq!(out["request"]["force"], json!(true));
        // The success body shape.
        assert_eq!(out["status"], json!(200));
        assert_eq!(
            out["body"],
            json!({"joined": true, "ip": "10.0.0.5", "gateway": "10.0.0.1", "error": null})
        );
    }

    #[tokio::test]
    async fn join_maps_the_ap_busy_result_to_a_409() {
        let out = with_socket(
            json!({"ok": true, "joined": false, "error": "wlan0_busy_ap_active"}),
            |sock| async move {
                put_client_join_at(
                    &sock,
                    WifiJoinRequest {
                        ssid: "HomeNet".to_string(),
                        passphrase: None,
                        force: None,
                    },
                )
                .await
            },
        )
        .await;
        // The default force is forwarded as false.
        assert_eq!(out["request"]["force"], json!(false));
        assert_eq!(out["status"], json!(409));
        assert_eq!(
            out["body"]["detail"]["error"]["code"],
            json!("E_WLAN0_BUSY_AP_ACTIVE")
        );
        assert_eq!(out["body"]["needs_force"], json!(true));
        // The default hint when the reply carries none.
        assert_eq!(
            out["body"]["detail"]["error"]["message"],
            json!("AP is active; retry with force=true to steal wlan0")
        );
    }

    #[tokio::test]
    async fn join_surfaces_an_ok_false_reply_as_a_500() {
        let out = with_socket(
            json!({"ok": false, "error": "E_MISSING_SSID"}),
            |sock| async move {
                put_client_join_at(
                    &sock,
                    WifiJoinRequest {
                        ssid: "x".to_string(),
                        passphrase: None,
                        force: None,
                    },
                )
                .await
            },
        )
        .await;
        assert_eq!(out["status"], json!(500));
        assert_eq!(
            out["body"]["detail"]["error"]["code"],
            json!("E_WIFI_JOIN_FAILED")
        );
        assert_eq!(
            out["body"]["detail"]["error"]["message"],
            json!("E_MISSING_SSID")
        );
    }

    #[tokio::test]
    async fn leave_forwards_a_wifi_leave_op_and_returns_the_reply() {
        let out = with_socket(
            json!({"ok": true, "left": true, "previous_ssid": "HomeNet"}),
            |sock| async move { delete_client_at(&sock).await },
        )
        .await;
        assert_eq!(out["request"]["op"], json!("wifi_leave"));
        assert_eq!(out["status"], json!(200));
        // The ok flag is stripped; the manager body is returned verbatim.
        assert_eq!(
            out["body"],
            json!({"left": true, "previous_ssid": "HomeNet"})
        );
    }

    #[tokio::test]
    async fn forget_forwards_a_wifi_forget_op_and_returns_the_reply() {
        let out = with_socket(
            json!({"ok": true, "forgot": true, "name": "HomeNet", "error": null}),
            |sock| async move { delete_client_configured_at(&sock, "HomeNet".to_string()).await },
        )
        .await;
        assert_eq!(out["request"]["op"], json!("wifi_forget"));
        assert_eq!(out["request"]["name"], json!("HomeNet"));
        assert_eq!(out["status"], json!(200));
        assert_eq!(
            out["body"],
            json!({"forgot": true, "name": "HomeNet", "error": null})
        );
    }

    #[tokio::test]
    async fn forget_maps_a_failed_forget_to_a_400() {
        let out = with_socket(
            json!({"ok": true, "forgot": false, "name": "HomeNet", "error": "nmcli_failed"}),
            |sock| async move { delete_client_configured_at(&sock, "HomeNet".to_string()).await },
        )
        .await;
        assert_eq!(out["status"], json!(400));
        assert_eq!(
            out["body"]["detail"]["error"]["code"],
            json!("E_WIFI_FORGET_FAILED")
        );
        assert_eq!(
            out["body"]["detail"]["error"]["message"],
            json!("nmcli_failed")
        );
    }

    #[test]
    fn json_truthy_matches_python_bool() {
        assert!(!json_truthy(&Value::Null));
        assert!(json_truthy(&json!(true)));
        assert!(!json_truthy(&json!(false)));
        assert!(!json_truthy(&json!("")));
        assert!(json_truthy(&json!("x")));
    }

    // ── autoconnect: no-socket 503 + the full forward path ───────────────────

    #[tokio::test]
    async fn autoconnect_with_no_socket_is_a_503() {
        // An absent socket path → the connect fails fast → 503.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("wifi-cmd.sock");
        let resp = put_client_autoconnect_at(
            &sock,
            "HomeNet".to_string(),
            AutoconnectRequest {
                enabled: Some(true),
            },
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(
            body["detail"]["error"]["code"],
            json!("E_WIFI_AUTOCONNECT_FAILED")
        );
    }

    #[tokio::test]
    async fn autoconnect_forwards_the_op_and_returns_the_reply() {
        let out = with_socket(
            json!({"ok": true, "autoconnect": true, "name": "HomeNet", "error": null}),
            |sock| async move {
                put_client_autoconnect_at(
                    &sock,
                    "HomeNet".to_string(),
                    AutoconnectRequest {
                        enabled: Some(true),
                    },
                )
                .await
            },
        )
        .await;
        // The exact op + fields forwarded to the daemon.
        assert_eq!(out["request"]["op"], json!("wifi_autoconnect"));
        assert_eq!(out["request"]["name"], json!("HomeNet"));
        assert_eq!(out["request"]["enabled"], json!(true));
        // The success body is the manager reply with the ok flag stripped.
        assert_eq!(out["status"], json!(200));
        assert_eq!(
            out["body"],
            json!({"autoconnect": true, "name": "HomeNet", "error": null})
        );
    }

    #[tokio::test]
    async fn autoconnect_defaults_a_missing_enabled_to_false() {
        let out = with_socket(
            json!({"ok": true, "autoconnect": false, "name": "HomeNet", "error": null}),
            |sock| async move {
                put_client_autoconnect_at(
                    &sock,
                    "HomeNet".to_string(),
                    AutoconnectRequest { enabled: None },
                )
                .await
            },
        )
        .await;
        // A missing `enabled` forwards as false (Python `bool(body.get("enabled"))`).
        assert_eq!(out["request"]["enabled"], json!(false));
        assert_eq!(out["status"], json!(200));
    }

    #[tokio::test]
    async fn autoconnect_maps_a_manager_error_to_a_400() {
        let out = with_socket(
            json!({"ok": true, "autoconnect": true, "name": "Nope", "error": "unknown connection 'Nope'"}),
            |sock| async move {
                put_client_autoconnect_at(
                    &sock,
                    "Nope".to_string(),
                    AutoconnectRequest {
                        enabled: Some(true),
                    },
                )
                .await
            },
        )
        .await;
        // A truthy `error` on the reply → the FastAPI 400 carrying the message.
        assert_eq!(out["status"], json!(400));
        assert_eq!(
            out["body"]["detail"]["error"]["code"],
            json!("E_WIFI_AUTOCONNECT_FAILED")
        );
        assert_eq!(
            out["body"]["detail"]["error"]["message"],
            json!("unknown connection 'Nope'")
        );
    }

    #[tokio::test]
    async fn autoconnect_surfaces_an_ok_false_reply_as_a_500() {
        let out = with_socket(
            json!({"ok": false, "error": "E_MISSING_NAME"}),
            |sock| async move {
                put_client_autoconnect_at(
                    &sock,
                    "".to_string(),
                    AutoconnectRequest {
                        enabled: Some(true),
                    },
                )
                .await
            },
        )
        .await;
        assert_eq!(out["status"], json!(500));
        assert_eq!(
            out["body"]["detail"]["error"]["code"],
            json!("E_WIFI_AUTOCONNECT_FAILED")
        );
    }
}
