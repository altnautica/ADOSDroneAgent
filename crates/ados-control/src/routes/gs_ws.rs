//! Ground-station WebSocket relays served natively by the front.
//!
//! These streams are upgraded past the HTTP auth edge, so each handler
//! enforces the agent's WebSocket auth contract itself (mirroring the residual
//! handlers, which did the same because the upgrade bypasses the HTTP gate):
//!
//! * **Unpaired** ⇒ open (the bench operator can read before pairing).
//! * **Paired + a valid `X-ADOS-Key` handshake header** ⇒ open (native clients
//!   that control handshake headers).
//! * **Paired + a valid `Sec-WebSocket-Protocol: ados-ws-ticket, <token>`** ⇒
//!   open (a browser cannot set a custom handshake header, so the GCS mints a
//!   one-shot HMAC ticket via `POST /api/_ws/ticket` and presents it through the
//!   subprotocol list; the agent echoes the marker back per RFC 6455). The
//!   ticket is bound to a per-route scope so a ticket for one stream cannot be
//!   replayed against another.
//! * **Otherwise** ⇒ rejected with close code 4401 before any frame flows.
//!
//! Both routes are profile-gated: on a drone-profile node the handshake is
//! accepted briefly so a JSON error reaches the client, then closed, matching
//! the residual `1008` profile-mismatch posture.
//!
//! ## `/ws/uplink`
//!
//! Streams uplink-matrix change events. The uplink health loop runs in the
//! native `ados-net` daemon, which ships `net.uplink_active` / `net.modem_usage`
//! to the durable store; this handler polls those events back and emits when the
//! snapshot changes (the in-process router never ticks in this process). Each
//! frame is the `{kind: "health_changed", active_uplink, available,
//! internet_reachable, data_cap_state, timestamp_ms}` shape the GCS consumes.
//!
//! ## `/pic/events`
//!
//! Relays the native PIC arbiter's transition stream. The `ados-pic` daemon owns
//! the arbiter and binds `/run/ados/pic.sock`; its `subscribe` op emits one
//! newline-JSON object per transition. This handler subscribes to that socket
//! and forwards each line verbatim as a WebSocket text frame.
//!
//! ## `/ws/mesh`
//!
//! Fans two cross-process journals into one socket: the mesh-event journal
//! (`/run/ados/mesh-events.jsonl`, written by the native data-plane relay /
//! receiver loops, stamped `bus:"mesh"`) and the pairing-event journal
//! (`/run/ados/pair-events.jsonl`, mirrored by the field-pairing manager,
//! stamped `bus:"pair"`). Each line already carries the
//! `{bus, kind, timestamp_ms, payload}` envelope, so the handler follows both
//! files and forwards each well-formed line verbatim, the same fan the residual
//! `ws_mesh_events` did off the two in-process buses.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use ados_protocol::pairing_posture::Pairing;
use ados_protocol::ws_ticket::{now_unix, WsTicketIssuer};

use crate::state::AppState;

/// The WebSocket subprotocol marker carrying an auth ticket, matching the Python
/// `ws_auth.WS_TICKET_PROTOCOL`, the GCS marker, and the MAVLink WS proxy.
const WS_TICKET_SUBPROTOCOL: &str = "ados-ws-ticket";

/// The scope a `/ws/uplink` ticket must be minted for.
const SCOPE_UPLINK_EVENTS: &str = "gs.uplink_events";

/// The scope a `/pic/events` ticket must be minted for.
const SCOPE_PIC_EVENTS: &str = "gs.pic_events";

/// The scope a `/ws/mesh` ticket must be minted for.
const SCOPE_MESH_EVENTS: &str = "gs.mesh_events";

/// How long to sleep between durable-store polls for the uplink stream. The
/// router daemon emits `net.uplink_active` / `net.modem_usage` at a low rate, so
/// a short poll keeps latency low without busy-waiting the store query socket.
const UPLINK_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long to sleep between journal polls for the mesh stream when neither
/// journal has a new line. Both journals are append-only and low-rate, so a
/// short poll keeps latency low without busy-waiting; matches the cadence the
/// prior Python tailer used to republish journal lines onto the bus.
const MESH_POLL_INTERVAL: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Handshake auth (mirrors the Python `authenticate_websocket`).
// ---------------------------------------------------------------------------

/// The outcome of the WebSocket handshake auth decision.
enum WsAuth {
    /// Admit, echoing no subprotocol (the unpaired path or the `X-ADOS-Key`
    /// header path; there is nothing to echo).
    AcceptPlain,
    /// Admit, echoing the `ados-ws-ticket` marker (the browser ticket path; per
    /// RFC 6455 the server must select an offered subprotocol).
    AcceptTicket,
    /// Reject: paired, off-box-credential-less, with no valid key or ticket.
    Reject,
}

/// Read the offered WebSocket subprotocols from the handshake headers. Values may
/// be split across multiple `Sec-WebSocket-Protocol` headers and/or comma-joined
/// within one; flatten both forms (the ticket itself carries no comma, so it
/// survives the split intact).
fn offered_subprotocols(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(','))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Pull the ticket value following the `ados-ws-ticket` marker in the offered
/// list (`["ados-ws-ticket", "<token>"]`).
fn extract_ticket(offered: &[String]) -> Option<&str> {
    let pos = offered.iter().position(|p| p == WS_TICKET_SUBPROTOCOL)?;
    offered.get(pos + 1).map(String::as_str)
}

/// Decide the handshake: open on an unpaired agent; on a paired agent require a
/// matching `X-ADOS-Key` header OR a valid ticket for `scope`. Mirrors the Python
/// `authenticate_websocket` order (unpaired → header → ticket → reject).
fn decide_ws_auth(state: &AppState, headers: &HeaderMap, scope: &str) -> WsAuth {
    let pairing = state.pairing.current();
    let Pairing::Paired(key) = pairing else {
        // Unpaired: open posture, no subprotocol to echo.
        return WsAuth::AcceptPlain;
    };

    // A native client (the CLI, integration tests) sets the key on the handshake.
    if let Some(presented) = headers.get("x-ados-key").and_then(|v| v.to_str().ok()) {
        if ados_protocol::pairing_posture::constant_time_eq(presented.as_bytes(), key.as_bytes()) {
            return WsAuth::AcceptPlain;
        }
        // A bad header still falls through to the ticket path, matching the Python.
    }

    // A browser presents a one-shot HMAC ticket through the subprotocol list.
    let offered = offered_subprotocols(headers);
    if let Some(token) = extract_ticket(&offered) {
        if WsTicketIssuer::from_api_key(&key)
            .verify(token, scope, now_unix())
            .is_ok()
        {
            return WsAuth::AcceptTicket;
        }
    }

    WsAuth::Reject
}

/// Resolve the `on_upgrade` subprotocol selection from the auth outcome: the
/// ticket path echoes the marker (RFC 6455 requires selecting an offered
/// subprotocol), the plain path echoes nothing.
fn upgrade_with(ws: WebSocketUpgrade, auth: WsAuth) -> Option<(WebSocketUpgrade, WsAuth)> {
    match auth {
        WsAuth::Reject => None,
        WsAuth::AcceptTicket => {
            let ws = ws.protocols([WS_TICKET_SUBPROTOCOL]);
            Some((ws, WsAuth::AcceptTicket))
        }
        WsAuth::AcceptPlain => Some((ws, WsAuth::AcceptPlain)),
    }
}

// ---------------------------------------------------------------------------
// Profile gate (mirrors the FastAPI `_require_ground_profile`).
// ---------------------------------------------------------------------------

/// True when the node resolves to the ground-station profile, the same source of
/// truth the node advertises on the wire.
fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sibling sockets resolve under.
fn run_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()),
    )
}

// ---------------------------------------------------------------------------
// /ws/uplink
// ---------------------------------------------------------------------------

/// The `/ws/uplink` upgrade entry point. Resolves the handshake auth and the
/// profile gate, then drives the polling loop on the upgraded socket.
pub async fn ws_uplink(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_UPLINK_EVENTS);
    let Some((ws, auth)) = upgrade_with(ws, auth) else {
        return ws_reject();
    };
    ws.on_upgrade(move |socket| uplink_loop(socket, state, auth))
}

/// Drive the uplink stream: profile-gate after accept (so a wrong-profile node
/// closes 1008 the way the residual handler did), then poll the store and emit on
/// change until the client disconnects.
async fn uplink_loop(mut socket: WebSocket, state: AppState, _auth: WsAuth) {
    if !is_ground_station(&state) {
        let _ = socket
            .send(Message::Close(Some(close_frame(
                1008,
                "E_PROFILE_MISMATCH",
            ))))
            .await;
        return;
    }

    let mut last_sent: Option<Value> = None;
    loop {
        if let Some(uplink) = latest_event_detail(&state, "net.uplink_active").await {
            let usage = latest_event_detail(&state, "net.modem_usage").await;
            let payload = uplink_ws_payload(&uplink, usage.as_ref());
            if last_sent.as_ref() != Some(&payload) {
                let body = match serde_json::to_string(&payload) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                if socket.send(Message::Text(body)).await.is_err() {
                    return; // client gone
                }
                last_sent = Some(payload);
            }
        }
        // Stop promptly if the client closed under us, otherwise wait out the poll
        // interval. A select keeps the loop responsive to a disconnect.
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    None | Some(Ok(Message::Close(_))) | Some(Err(_)) => return,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(UPLINK_POLL_INTERVAL) => {}
        }
    }
}

/// Shape a stored `net.uplink_active` body into the uplink WS frame, preferring
/// the live `net.modem_usage` `state` for `data_cap_state` and falling back to the
/// uplink event's own `data_cap_state`. Mirrors the Python `_uplink_ws_payload`
/// byte-for-byte (same keys, same order, same default coercions).
fn uplink_ws_payload(uplink: &Map<String, Value>, usage: Option<&Map<String, Value>>) -> Value {
    // `data_cap_state = usage.get("state")` then fall back when it is None — the
    // Python checks `is None` specifically (a JSON null), not general falsiness.
    let mut data_cap_state = usage.and_then(|u| u.get("state")).cloned();
    if data_cap_state.is_none() || data_cap_state == Some(Value::Null) {
        data_cap_state = uplink.get("data_cap_state").cloned();
    }
    json!({
        "kind": "health_changed",
        "active_uplink": uplink.get("active_uplink").cloned().unwrap_or(Value::Null),
        // `uplink.get("available") or []`: a missing OR present-but-falsy value
        // (null / empty list) collapses to an empty list.
        "available": available_or_empty(uplink.get("available")),
        // `bool(uplink.get("internet_reachable"))`: Python truthiness, so a present
        // non-bool value follows the same truthy rule a bool would.
        "internet_reachable": json_truthy(uplink.get("internet_reachable")),
        "data_cap_state": data_cap_state.unwrap_or(Value::Null),
        "timestamp_ms": uplink.get("timestamp_ms").cloned().unwrap_or(Value::Null),
    })
}

/// Mirror Python's `value or []`: keep a present, truthy value; collapse a missing
/// or falsy (`null`, `[]`, `false`, `0`, `""`, `{}`) value to an empty list.
fn available_or_empty(value: Option<&Value>) -> Value {
    match value {
        Some(v) if json_truthy(Some(v)) => v.clone(),
        _ => json!([]),
    }
}

/// Mirror Python's `bool(x)` truthiness for a JSON value: `null`/absent is false;
/// a bool is itself; a number is false only at zero; a string/array/object is true
/// only when non-empty.
fn json_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

// ---------------------------------------------------------------------------
// /pic/events
// ---------------------------------------------------------------------------

/// The `/pic/events` upgrade entry point. Resolves the handshake auth and the
/// profile gate, then relays the PIC arbiter's subscribe stream on the socket.
pub async fn ws_pic_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_PIC_EVENTS);
    let Some((ws, auth)) = upgrade_with(ws, auth) else {
        return ws_reject();
    };
    ws.on_upgrade(move |socket| pic_loop(socket, state, auth))
}

/// Relay the native arbiter's transition stream: profile-gate after accept, open
/// the PIC control socket, send `{"op":"subscribe"}`, and forward each
/// newline-JSON object verbatim as a text frame until either side ends.
async fn pic_loop(mut socket: WebSocket, state: AppState, _auth: WsAuth) {
    if !is_ground_station(&state) {
        let _ = socket
            .send(Message::Close(Some(close_frame(
                1008,
                "E_PROFILE_MISMATCH",
            ))))
            .await;
        return;
    }

    let pic_sock = run_dir().join("pic.sock");
    let stream = match tokio::net::UnixStream::connect(&pic_sock).await {
        Ok(s) => s,
        Err(exc) => {
            // Match the residual handler: report the unavailable bus, then close.
            let body = json!({
                "event": "error",
                "code": "E_PIC_BUS_UNAVAILABLE",
                "message": exc.to_string(),
            });
            if let Ok(text) = serde_json::to_string(&body) {
                let _ = socket.send(Message::Text(text)).await;
            }
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    let (read_half, mut write_half) = stream.into_split();
    if write_half
        .write_all(b"{\"op\":\"subscribe\"}\n")
        .await
        .is_err()
        || write_half.flush().await.is_err()
    {
        return;
    }

    let mut lines = BufReader::new(read_half).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        // Forward only well-formed JSON, dropping a malformed line
                        // (the residual handler `continue`s on a JSON decode error).
                        match serde_json::from_str::<Value>(&line) {
                            Ok(event) => {
                                let text = match serde_json::to_string(&event) {
                                    Ok(t) => t,
                                    Err(_) => continue,
                                };
                                if socket.send(Message::Text(text)).await.is_err() {
                                    return; // client gone
                                }
                            }
                            Err(_) => continue,
                        }
                    }
                    Ok(None) | Err(_) => return, // arbiter socket closed
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    None | Some(Ok(Message::Close(_))) | Some(Err(_)) => return,
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// /ws/mesh
// ---------------------------------------------------------------------------

/// The `/ws/mesh` upgrade entry point. Resolves the handshake auth and the
/// profile gate, then fans the mesh-event journal + the pairing-event journal
/// into the one socket.
pub async fn ws_mesh(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_MESH_EVENTS);
    let Some((ws, auth)) = upgrade_with(ws, auth) else {
        return ws_reject();
    };
    ws.on_upgrade(move |socket| mesh_loop(socket, state, auth))
}

/// Fan the two journals into the socket: profile-gate after accept (so a
/// wrong-profile node closes 1008 the way the residual handler did), then follow
/// both `mesh-events.jsonl` and `pair-events.jsonl` and forward each well-formed
/// line verbatim. Each journal line already carries the
/// `{bus, kind, timestamp_ms, payload}` envelope the residual `ws_mesh_events`
/// sent (the mesh journal stamps `bus:"mesh"`, the pairing manager stamps
/// `bus:"pair"`), so forwarding the line IS the byte-faithful frame. There is no
/// initial snapshot and no keepalive, matching the residual handler which only
/// subscribed-and-forwarded.
async fn mesh_loop(mut socket: WebSocket, state: AppState, _auth: WsAuth) {
    if !is_ground_station(&state) {
        let _ = socket
            .send(Message::Close(Some(close_frame(
                1008,
                "E_PROFILE_MISMATCH",
            ))))
            .await;
        return;
    }

    let mut mesh_tail = JournalTail::new(run_dir().join("mesh-events.jsonl"));
    let mut pair_tail = JournalTail::new(run_dir().join("pair-events.jsonl"));

    loop {
        // Drain whatever new lines each journal has, forwarding each verbatim.
        // A journal that is missing/rotating yields nothing and is retried on the
        // next poll, so one absent journal never starves the other.
        let mut forwarded_any = false;
        for tail in [&mut mesh_tail, &mut pair_tail] {
            while let Some(line) = tail.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // Forward only well-formed JSON, dropping a malformed line (the
                // residual tailer `continue`s past a JSON decode error).
                let event: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let text = match serde_json::to_string(&event) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if socket.send(Message::Text(text)).await.is_err() {
                    return; // client gone
                }
                forwarded_any = true;
            }
        }

        // If we forwarded at least one line this pass, loop straight back to
        // drain any further backlog without sleeping; otherwise wait out the poll
        // interval. Either way a select keeps the loop responsive to a disconnect:
        // a zero timeout when there was a backlog, the poll interval otherwise.
        let wait = if forwarded_any {
            Duration::ZERO
        } else {
            MESH_POLL_INTERVAL
        };
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    None | Some(Ok(Message::Close(_))) | Some(Err(_)) => return,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// A follower over an append-only newline-JSON journal: seek to end on first
/// open (skip the backlog so a long-lived journal never replays stale events
/// into a freshly connected client), tolerate a missing file (retried on the
/// next call), and re-open on truncation/recreation (the tmpfs wipe on a service
/// restart). Mirrors the Python `tail_mesh_events` loop.
struct JournalTail {
    path: std::path::PathBuf,
    reader: Option<BufReader<tokio::fs::File>>,
    offset: u64,
}

impl JournalTail {
    fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            reader: None,
            offset: 0,
        }
    }

    /// Return the next complete line from the journal, or `None` when there is
    /// nothing new to read right now (missing file, no new bytes, or a partial
    /// trailing line). Opens the file lazily and seeks to its end on first open.
    async fn next_line(&mut self) -> Option<String> {
        // `read_line` comes from the top-level `AsyncBufReadExt`; `seek` needs
        // the seek extension trait in scope here.
        use tokio::io::AsyncSeekExt;

        if self.reader.is_none() {
            let mut file = tokio::fs::File::open(&self.path).await.ok()?;
            // Seek to end: skip the backlog so only post-connect events stream.
            let end = file.seek(std::io::SeekFrom::End(0)).await.ok()?;
            self.offset = end;
            self.reader = Some(BufReader::new(file));
        }

        let reader = self.reader.as_mut()?;
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            // EOF with no bytes: nothing new. Detect truncation/recreation (the
            // file shrank below our offset) and re-open from the new end.
            Ok(0) => {
                if let Ok(meta) = tokio::fs::metadata(&self.path).await {
                    if meta.len() < self.offset {
                        self.reader = None;
                        self.offset = 0;
                    }
                } else {
                    // The file vanished; drop the handle and retry on next poll.
                    self.reader = None;
                    self.offset = 0;
                }
                None
            }
            Ok(n) => {
                // A line is complete only when it ends in a newline; a partial
                // trailing write is held back until the writer finishes it.
                if line.ends_with('\n') {
                    self.offset += n as u64;
                    Some(line)
                } else {
                    // Rewind so the partial line is re-read once it is complete.
                    let reader = self.reader.as_mut()?;
                    let _ = reader.seek(std::io::SeekFrom::Start(self.offset)).await;
                    None
                }
            }
            Err(_) => {
                self.reader = None;
                self.offset = 0;
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared seams.
// ---------------------------------------------------------------------------

/// Query the store for the newest `events` row of one `event_kind` and return its
/// `detail` body, or `None` when the store is unreachable / the response is an
/// error / there is no such event / the detail is absent / non-object / empty.
/// Mirrors the Python `query_rows("events", 1, event_kind=...)` read and the
/// sibling `gs_network::latest_event_detail`.
async fn latest_event_detail(state: &AppState, event_kind: &str) -> Option<Map<String, Value>> {
    let path = format!(
        "/v1/query?kind=events&limit=1&event_kind={}",
        urlencode(event_kind)
    );
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

/// Percent-encode the few characters an `event_kind` value could carry that are
/// unsafe in a query string. The event kinds here are dotted identifiers, so this
/// is conservative belt-and-braces, not a full URL encoder.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// A minimal HTTP/1.1 `GET` over the logging-store query Unix socket, returning
/// the status code + the decoded body. Mirrors `gs_network::logd_get`.
async fn logd_get(state: &AppState, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
    use tokio::io::AsyncReadExt;

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

/// Split a raw HTTP/1.1 response into the status code + decoded body, de-chunking
/// a chunked body. Mirrors `gs_network::parse_http_response`.
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

/// De-chunk a `Transfer-Encoding: chunked` body.
fn de_chunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") {
        let size_str = String::from_utf8_lossy(&rest[..eol]);
        let size = usize::from_str_radix(size_str.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let data_start = eol + 2;
        let data_end = data_start + size;
        if data_end > rest.len() {
            break;
        }
        out.extend_from_slice(&rest[data_start..data_end]);
        // Skip the trailing CRLF after the chunk data.
        rest = &rest[(data_end + 2).min(rest.len())..];
    }
    out
}

/// Build a WebSocket close frame with the given code + reason.
fn close_frame(code: u16, reason: &str) -> axum::extract::ws::CloseFrame<'static> {
    axum::extract::ws::CloseFrame {
        code,
        reason: reason.to_string().into(),
    }
}

/// The rejection an `on_upgrade` callback cannot express: when the handshake auth
/// fails we never call `on_upgrade`, so the HTTP response is a `401` with the
/// FastAPI-shaped detail, matching the residual handler closing 4401 (a browser
/// reads a refused handshake either way; an HTTP 401 carries a clear body).
fn ws_reject() -> Response {
    crate::routes::detail(
        axum::http::StatusCode::UNAUTHORIZED,
        "Missing or invalid WebSocket credentials.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    // -- frame-shape parity ------------------------------------------------

    fn obj(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .cloned()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[test]
    fn uplink_payload_matches_the_python_shape() {
        let uplink = obj(&[
            ("active_uplink", json!("ethernet")),
            ("available", json!(["ethernet", "modem"])),
            ("internet_reachable", json!(true)),
            ("data_cap_state", json!("ok")),
            ("timestamp_ms", json!(1234)),
        ]);
        let payload = uplink_ws_payload(&uplink, None);
        assert_eq!(
            payload,
            json!({
                "kind": "health_changed",
                "active_uplink": "ethernet",
                "available": ["ethernet", "modem"],
                "internet_reachable": true,
                "data_cap_state": "ok",
                "timestamp_ms": 1234,
            })
        );
    }

    #[test]
    fn uplink_payload_prefers_modem_usage_state_for_data_cap() {
        let uplink = obj(&[
            ("active_uplink", json!("modem")),
            ("data_cap_state", json!("ok")),
            ("timestamp_ms", json!(7)),
        ]);
        let usage = obj(&[("state", json!("warning"))]);
        let payload = uplink_ws_payload(&uplink, Some(&usage));
        // The live modem-usage `state` wins over the uplink event's own value.
        assert_eq!(payload["data_cap_state"], json!("warning"));
    }

    #[test]
    fn uplink_payload_falls_back_when_usage_state_is_null_or_absent() {
        let uplink = obj(&[
            ("active_uplink", json!("modem")),
            ("data_cap_state", json!("blocked")),
        ]);
        // usage present but its state is null → fall back to the uplink value.
        let usage_null = obj(&[("state", Value::Null)]);
        assert_eq!(
            uplink_ws_payload(&uplink, Some(&usage_null))["data_cap_state"],
            json!("blocked")
        );
        // usage absent entirely → same fallback.
        assert_eq!(
            uplink_ws_payload(&uplink, None)["data_cap_state"],
            json!("blocked")
        );
    }

    #[test]
    fn uplink_payload_defaults_missing_fields_like_python() {
        // An empty uplink body still yields the full key set with null/empty
        // defaults (the Python `.get(...)` / `or []` / `bool(...)` defaults).
        let uplink = obj(&[]);
        let payload = uplink_ws_payload(&uplink, None);
        assert_eq!(payload["active_uplink"], Value::Null);
        assert_eq!(payload["available"], json!([]));
        assert_eq!(payload["internet_reachable"], json!(false));
        assert_eq!(payload["data_cap_state"], Value::Null);
        assert_eq!(payload["timestamp_ms"], Value::Null);
    }

    #[test]
    fn uplink_payload_coerces_a_present_null_available_like_python_or() {
        // `available: null` is falsy → Python `or []` collapses it to []; a real
        // present list survives.
        let null_avail = obj(&[("available", Value::Null)]);
        assert_eq!(uplink_ws_payload(&null_avail, None)["available"], json!([]));
        let list = obj(&[("available", json!(["ethernet"]))]);
        assert_eq!(
            uplink_ws_payload(&list, None)["available"],
            json!(["ethernet"])
        );
    }

    #[test]
    fn uplink_payload_internet_reachable_follows_python_truthiness() {
        // A present non-bool value follows Python `bool(...)`: a non-empty string
        // is true, zero is false, an empty string is false.
        assert_eq!(
            uplink_ws_payload(&obj(&[("internet_reachable", json!("x"))]), None)
                ["internet_reachable"],
            json!(true)
        );
        assert_eq!(
            uplink_ws_payload(&obj(&[("internet_reachable", json!(0))]), None)
                ["internet_reachable"],
            json!(false)
        );
        assert_eq!(
            uplink_ws_payload(&obj(&[("internet_reachable", json!(""))]), None)
                ["internet_reachable"],
            json!(false)
        );
    }

    #[test]
    fn json_truthy_matches_python_bool() {
        assert!(!json_truthy(None));
        assert!(!json_truthy(Some(&Value::Null)));
        assert!(json_truthy(Some(&json!(true))));
        assert!(!json_truthy(Some(&json!(false))));
        assert!(json_truthy(Some(&json!(1))));
        assert!(!json_truthy(Some(&json!(0))));
        assert!(json_truthy(Some(&json!("x"))));
        assert!(!json_truthy(Some(&json!(""))));
        assert!(json_truthy(Some(&json!(["a"]))));
        assert!(!json_truthy(Some(&json!([]))));
    }

    // -- subprotocol parsing ----------------------------------------------

    #[test]
    fn offered_subprotocols_flattens_comma_and_multi_header() {
        let h = headers_with(&[("sec-websocket-protocol", "ados-ws-ticket, v1|s|1|2|ff")]);
        assert_eq!(
            offered_subprotocols(&h),
            vec!["ados-ws-ticket".to_string(), "v1|s|1|2|ff".to_string()]
        );
        let mut multi = HeaderMap::new();
        multi.append(
            "sec-websocket-protocol",
            HeaderValue::from_static("ados-ws-ticket"),
        );
        multi.append(
            "sec-websocket-protocol",
            HeaderValue::from_static("v1|s|1|2|ff"),
        );
        assert_eq!(
            offered_subprotocols(&multi),
            vec!["ados-ws-ticket".to_string(), "v1|s|1|2|ff".to_string()]
        );
    }

    #[test]
    fn extract_ticket_finds_the_value_after_the_marker() {
        let offered = vec!["ados-ws-ticket".to_string(), "v1|s|1|2|ff".to_string()];
        assert_eq!(extract_ticket(&offered), Some("v1|s|1|2|ff"));
        assert_eq!(extract_ticket(&["ados-ws-ticket".to_string()]), None);
        assert_eq!(extract_ticket(&["mavlink".to_string()]), None);
    }

    // -- auth decision ----------------------------------------------------

    fn state_with_pairing(body: &str) -> (tempfile::TempDir, AppState) {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let pairing_json = dir.path().join("pairing.json");
        std::fs::File::create(&pairing_json)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        let pairing =
            std::sync::Arc::new(crate::auth::PairingState::with_path(pairing_json.clone()));
        let paths = crate::state::PairingPaths {
            config: dir.path().join("config.yaml"),
            pairing_json,
            wfb_key_dir: dir.path().join("wfb"),
            bind_state: dir.path().join("bind-state.json"),
            profile_conf: dir.path().join("profile.conf"),
            mesh_role: dir.path().join("mesh-role"),
        };
        let state = AppState::new(
            pairing,
            crate::ipc::StateIpcClient::disconnected(),
            crate::ipc::MavlinkIpcClient::new(dir.path().join("mavlink.sock")),
            crate::ipc::LogdQueryClient::new(dir.path().join("logd-query.sock")),
            dir.path().join("board.json"),
            paths,
        );
        (dir, state)
    }

    #[test]
    fn unpaired_admits_without_a_credential() {
        let (_d, state) = state_with_pairing(r#"{"paired": false}"#);
        assert!(matches!(
            decide_ws_auth(&state, &HeaderMap::new(), SCOPE_UPLINK_EVENTS),
            WsAuth::AcceptPlain
        ));
    }

    #[test]
    fn paired_admits_with_the_key_header() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let h = headers_with(&[("x-ados-key", "k")]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS),
            WsAuth::AcceptPlain
        ));
    }

    #[test]
    fn paired_rejects_a_wrong_key_and_no_ticket() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let h = headers_with(&[("x-ados-key", "wrong")]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS),
            WsAuth::Reject
        ));
        // No credential at all is also rejected.
        assert!(matches!(
            decide_ws_auth(&state, &HeaderMap::new(), SCOPE_UPLINK_EVENTS),
            WsAuth::Reject
        ));
    }

    #[test]
    fn paired_admits_with_a_valid_scoped_ticket() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let token = WsTicketIssuer::from_api_key("k")
            .mint(SCOPE_UPLINK_EVENTS, 30)
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS),
            WsAuth::AcceptTicket
        ));
    }

    #[test]
    fn a_ticket_for_the_wrong_scope_is_rejected() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        // Minted for pic_events but presented to the uplink scope.
        let token = WsTicketIssuer::from_api_key("k")
            .mint(SCOPE_PIC_EVENTS, 30)
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS),
            WsAuth::Reject
        ));
    }

    #[test]
    fn a_ticket_for_the_wrong_key_is_rejected() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let token = WsTicketIssuer::from_api_key("other-key")
            .mint(SCOPE_UPLINK_EVENTS, 30)
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS),
            WsAuth::Reject
        ));
    }

    #[test]
    fn de_chunk_reassembles_a_chunked_body() {
        // "hello world" split into two chunks: 5 + 6 then a zero-chunk terminator.
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(de_chunk(raw), b"hello world");
    }

    #[test]
    fn parse_http_response_reads_status_and_body() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"data\":[]}";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, br#"{"data":[]}"#);
    }

    // -- mesh stream: auth scope + the two-journal fan -------------------

    #[test]
    fn mesh_stream_admits_with_a_valid_mesh_scoped_ticket() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let token = WsTicketIssuer::from_api_key("k")
            .mint(SCOPE_MESH_EVENTS, 30)
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_MESH_EVENTS),
            WsAuth::AcceptTicket
        ));
    }

    #[test]
    fn mesh_stream_rejects_a_wrong_scope_ticket() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        // A uplink-scoped ticket presented to the mesh scope is rejected.
        let token = WsTicketIssuer::from_api_key("k")
            .mint(SCOPE_UPLINK_EVENTS, 30)
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_MESH_EVENTS),
            WsAuth::Reject
        ));
    }

    #[tokio::test]
    async fn journal_tail_skips_backlog_then_follows_new_lines() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        // A pre-existing backlog line: the tail seeks to end on first open, so
        // this is never replayed.
        {
            let mut f = std::fs::File::create(&p).unwrap();
            writeln!(
                f,
                r#"{{"bus":"mesh","kind":"backlog","timestamp_ms":1,"payload":{{}}}}"#
            )
            .unwrap();
        }
        let mut tail = JournalTail::new(p.clone());
        // First poll opens + seeks to end: nothing to read.
        assert!(tail.next_line().await.is_none());
        // Append a fresh line; the tail reads it verbatim.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            writeln!(f, r#"{{"bus":"mesh","kind":"relay_connected","timestamp_ms":2,"payload":{{"relay_mac":"aa:bb"}}}}"#)
                .unwrap();
        }
        let line = tail.next_line().await.expect("a new line");
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["bus"], "mesh");
        assert_eq!(v["kind"], "relay_connected");
        assert_eq!(v["payload"]["relay_mac"], "aa:bb");
        // No further lines pending.
        assert!(tail.next_line().await.is_none());
    }

    #[tokio::test]
    async fn journal_tail_tolerates_a_missing_file_then_picks_it_up() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("late.jsonl");
        let mut tail = JournalTail::new(p.clone());
        // The file does not exist yet: a poll yields nothing without erroring.
        assert!(tail.next_line().await.is_none());
        // It appears with one line; the tail opens, seeks to end, and (because
        // the line was written before the first successful open) treats it as
        // backlog. A line appended after the open is the one that streams.
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&p).unwrap();
            writeln!(f, r#"{{"bus":"pair","kind":"accept_window_opened","timestamp_ms":3,"payload":{{"duration_s":60}}}}"#)
                .unwrap();
        }
        // Opens + seeks to end of the existing content.
        assert!(tail.next_line().await.is_none());
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            writeln!(f, r#"{{"bus":"pair","kind":"join_approved","timestamp_ms":4,"payload":{{"device_id":"d1"}}}}"#)
                .unwrap();
        }
        let line = tail.next_line().await.expect("the post-open line");
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["bus"], "pair");
        assert_eq!(v["kind"], "join_approved");
        assert_eq!(v["payload"]["device_id"], "d1");
    }

    #[tokio::test]
    async fn two_journals_fan_with_bus_envelopes_intact() {
        // The frame the handler forwards is the journal line itself: the mesh
        // journal stamps `bus:"mesh"`, the pairing journal stamps `bus:"pair"`,
        // and both carry the `{bus,kind,timestamp_ms,payload}` envelope the
        // residual `ws_mesh_events` sent. Tail both and assert each line
        // round-trips with its bus marker.
        let dir = tempfile::tempdir().unwrap();
        let mesh_p = dir.path().join("mesh-events.jsonl");
        let pair_p = dir.path().join("pair-events.jsonl");
        std::fs::write(&mesh_p, "").unwrap();
        std::fs::write(&pair_p, "").unwrap();
        let mut mesh_tail = JournalTail::new(mesh_p.clone());
        let mut pair_tail = JournalTail::new(pair_p.clone());
        // Prime both tails (open + seek to end of the empty files).
        assert!(mesh_tail.next_line().await.is_none());
        assert!(pair_tail.next_line().await.is_none());

        use std::io::Write as _;
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&mesh_p)
                .unwrap();
            writeln!(f, r#"{{"bus":"mesh","kind":"role_changed","timestamp_ms":10,"payload":{{"role":"relay"}}}}"#)
                .unwrap();
        }
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&pair_p)
                .unwrap();
            writeln!(f, r#"{{"bus":"pair","kind":"join_request_received","timestamp_ms":11,"payload":{{"device_id":"d2"}}}}"#)
                .unwrap();
        }

        let mesh_line = mesh_tail.next_line().await.expect("mesh line");
        let mv: Value = serde_json::from_str(mesh_line.trim()).unwrap();
        assert_eq!(mv["bus"], "mesh");
        assert_eq!(mv["kind"], "role_changed");
        assert_eq!(mv["timestamp_ms"], 10);
        assert_eq!(mv["payload"]["role"], "relay");

        let pair_line = pair_tail.next_line().await.expect("pair line");
        let pv: Value = serde_json::from_str(pair_line.trim()).unwrap();
        assert_eq!(pv["bus"], "pair");
        assert_eq!(pv["kind"], "join_request_received");
        assert_eq!(pv["timestamp_ms"], 11);
        assert_eq!(pv["payload"]["device_id"], "d2");
    }
}
