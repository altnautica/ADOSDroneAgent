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
//! native `ados-net` daemon (the `ados-uplink-router` unit), which keeps the
//! active-uplink sentinel `/run/ados/uplink-active` current: present with the
//! selected uplink, unlinked when there is none. This handler polls that live
//! sentinel (it does not depend on the optional durable log store) and emits
//! when the snapshot changes. Each frame is the `{kind: "health_changed",
//! active_uplink, available, internet_reachable, data_cap_state, timestamp_ms,
//! stale}` shape the GCS consumes; `stale` is true unless the router unit is
//! confirmed running, because a stopped router leaves its last sentinel behind.
//!
//! ## Keepalive and bounded sends
//!
//! Every stream sends a `{"kind":"keepalive"}` text frame plus a WebSocket Ping
//! every 5 s, bounds each send by a timeout, and closes a client it has not
//! heard from (no Pong, no frame) for 30 s, so a client that vanished without a
//! FIN releases its task and its upstream subscription.
//!
//! ## `/pic/events`
//!
//! Relays the native PIC arbiter's transition stream. The `ados-pic` daemon owns
//! the arbiter and binds `/run/ados/pic.sock`; its `subscribe` op emits one
//! newline-JSON object per transition. This handler subscribes to that socket
//! and forwards each line verbatim as a WebSocket text frame.
//!
//! ## `/ws/buttons`
//!
//! Relays the front-panel button stream so a browser (the HDMI cockpit web app)
//! can be driven by the ground station's four GPIO buttons. The `ados-pic` daemon
//! owns the GPIO reader, classifies each press as short / long / cancel through
//! its shared classifier, and binds a dedicated `/run/ados/buttons.sock`; its
//! `subscribe` op emits one newline-JSON `{button, kind, action, timestamp_ms}`
//! object per press. This handler subscribes to that socket and forwards each
//! line verbatim as a WebSocket text frame — it re-derives no button semantics.
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

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::{Instant, MissedTickBehavior};

use ados_protocol::pairing_posture::{CallerClass, Pairing};
use ados_protocol::ws_ticket::{now_unix, WsTicketIssuer};

use crate::state::AppState;

/// The WebSocket subprotocol marker carrying an auth ticket, matching the Python
/// `ws_auth.WS_TICKET_PROTOCOL`, the GCS marker, and the MAVLink WS proxy.
const WS_TICKET_SUBPROTOCOL: &str = "ados-ws-ticket";

/// The scope a `/ws/uplink` ticket must be minted for.
const SCOPE_UPLINK_EVENTS: &str = "gs.uplink_events";

/// The scope a `/pic/events` ticket must be minted for.
const SCOPE_PIC_EVENTS: &str = "gs.pic_events";

/// The scope a `/ws/buttons` ticket must be minted for.
const SCOPE_BUTTON_EVENTS: &str = "gs.button_events";

/// The scope a `/ws/mesh` ticket must be minted for.
const SCOPE_MESH_EVENTS: &str = "gs.mesh_events";

/// How often the uplink stream re-reads the active-uplink sentinel. The router
/// rewrites it only on a change, so a short poll keeps latency low for the cost
/// of one small tmpfs read.
const UPLINK_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How often the uplink stream re-asks systemd whether the router unit runs. The
/// answer only drives the `stale` flag, so it need not cost a process spawn on
/// every sentinel poll.
const ROUTER_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

/// The unit that runs the uplink router and so owns the sentinel.
const UPLINK_ROUTER_UNIT: &str = "ados-uplink-router.service";

/// How long to sleep between journal polls for the mesh stream when neither
/// journal has a new line. Both journals are append-only and low-rate, so a
/// short poll keeps latency low without busy-waiting; matches the cadence the
/// prior Python tailer used to republish journal lines onto the bus.
const MESH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How often every stream sends [`KEEPALIVE_FRAME`] and a Ping. Each stream
/// emits only on change, so without this a quiet healthy stream and a peer that
/// vanished without a FIN look identical to the client, which drops and redials
/// a socket that stays silent for 15 s.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// The liveness frame. It carries no event; the GCS discards it after feeding
/// its liveness timer.
const KEEPALIVE_FRAME: &str = r#"{"kind":"keepalive"}"#;

/// Upper bound on one outbound frame. A client that stops reading fills the TCP
/// window; past this the stream gives up on it instead of blocking forever.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// A client silent this long (no Pong to six Pings, no frame) is treated as gone.
const PEER_SILENCE_LIMIT: Duration = Duration::from_secs(30);

/// A ticker whose first tick lands one [`KEEPALIVE_INTERVAL`] after accept; a
/// late tick is delayed rather than burst.
fn keepalive_ticker() -> tokio::time::Interval {
    let mut ticker =
        tokio::time::interval_at(Instant::now() + KEEPALIVE_INTERVAL, KEEPALIVE_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker
}

/// Send one frame, giving up after [`SEND_TIMEOUT`]. False when the client is
/// gone or not draining its socket.
async fn send_bounded(socket: &mut WebSocket, msg: Message) -> bool {
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, socket.send(msg)).await,
        Ok(Ok(()))
    )
}

/// Serialize `value` and send it as one bounded text frame.
async fn send_json(socket: &mut WebSocket, value: &Value) -> bool {
    match serde_json::to_string(value) {
        Ok(text) => send_bounded(socket, Message::Text(text.into())).await,
        // A value that cannot serialize is skipped, not a dead client.
        Err(_) => true,
    }
}

/// One keepalive beat: give up on a client silent past [`PEER_SILENCE_LIMIT`],
/// otherwise send the keepalive text frame and a Ping (whose Pong counts as
/// hearing from the client). False when the stream should end.
async fn keepalive_beat(socket: &mut WebSocket, last_heard: Instant) -> bool {
    if last_heard.elapsed() > PEER_SILENCE_LIMIT {
        return false;
    }
    send_bounded(socket, Message::Text(KEEPALIVE_FRAME.into())).await
        && send_bounded(socket, Message::Ping(Default::default())).await
}

/// Whether an inbound read leaves the client connected. Any frame, a Pong
/// included, means the client is alive.
fn client_still_open(incoming: &Option<Result<Message, axum::Error>>) -> bool {
    !matches!(incoming, None | Some(Ok(Message::Close(_))) | Some(Err(_)))
}

/// Close a stream opened on a node that is not a ground station: 1008 after
/// accept, so the client reads the reason.
async fn close_profile_mismatch(socket: &mut WebSocket) {
    let _ = send_bounded(
        socket,
        Message::Close(Some(close_frame(1008, "E_PROFILE_MISMATCH"))),
    )
    .await;
}

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

/// Decide the handshake.
///
/// - **Unpaired:** open to the caller classes that reach an unpaired node's
///   data plane (the local operator, the first-boot lifelines, the operator's
///   LAN), never to a remote caller (a public-WAN host, a tunnelled request).
/// - **Unreadable pairing state:** only the on-box operator. A corrupt
///   `pairing.json` may be a paired node's record on a failing card; reading
///   it as unpaired would open these streams to anyone.
/// - **Paired:** a valid credential in `X-ADOS-Key` (the pairing key or the
///   configured key) OR a valid, unspent ticket for `scope`.
///
/// `caller` is the class the TCP edge stamped; the Unix socket edge stamps
/// none and is the trusted on-box plane.
fn decide_ws_auth(
    state: &AppState,
    headers: &HeaderMap,
    scope: &str,
    caller: CallerClass,
) -> WsAuth {
    let pairing = state.pairing.current();
    let key = match pairing {
        Pairing::Unpaired => {
            return if caller == CallerClass::Remote {
                WsAuth::Reject
            } else {
                WsAuth::AcceptPlain
            };
        }
        Pairing::Unreadable => {
            return if caller == CallerClass::OnBox {
                WsAuth::AcceptPlain
            } else {
                WsAuth::Reject
            };
        }
        Pairing::Paired(key) => key,
    };

    // A native client (the CLI, integration tests) sets the key on the handshake.
    let presented = headers.get("x-ados-key").and_then(|v| v.to_str().ok());
    if state.pairing.credential_valid(presented) {
        return WsAuth::AcceptPlain;
    }
    // A bad header still falls through to the ticket path, matching the Python.

    // A browser presents a single-use HMAC ticket through the subprotocol list.
    let offered = offered_subprotocols(headers);
    if let Some(token) = extract_ticket(&offered) {
        if WsTicketIssuer::from_api_key(&key)
            .verify_once(token, scope, now_unix())
            .is_ok()
        {
            return WsAuth::AcceptTicket;
        }
    }

    WsAuth::Reject
}

/// The caller class the TCP edge stamped, or the on-box class on the Unix
/// socket edge, which stamps none and is reachable only from the box itself.
fn caller_of(caller: Option<Extension<CallerClass>>) -> CallerClass {
    caller.map_or(CallerClass::OnBox, |Extension(c)| c)
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
    caller: Option<Extension<CallerClass>>,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_UPLINK_EVENTS, caller_of(caller));
    let Some((ws, auth)) = upgrade_with(ws, auth) else {
        return ws_reject();
    };
    ws.on_upgrade(move |socket| uplink_loop(socket, state, auth))
}

/// Drive the uplink stream: profile-gate after accept (so a wrong-profile node
/// closes 1008 the way the residual handler did), then poll the sentinel and emit
/// on change, with the keepalive beat, until the client disconnects.
async fn uplink_loop(mut socket: WebSocket, state: AppState, _auth: WsAuth) {
    if !is_ground_station(&state) {
        close_profile_mismatch(&mut socket).await;
        return;
    }

    let mut source = UplinkSource::new(run_dir().join("uplink-active"));
    let mut last_sent: Option<Value> = None;
    let mut poll = tokio::time::interval(UPLINK_POLL_INTERVAL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut keepalive = keepalive_ticker();
    let mut last_heard = Instant::now();
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                if !client_still_open(&incoming) {
                    return;
                }
                last_heard = Instant::now();
            }
            _ = keepalive.tick() => {
                if !keepalive_beat(&mut socket, last_heard).await {
                    return;
                }
            }
            _ = poll.tick() => {
                let Some(payload) = source.snapshot().await else {
                    continue;
                };
                if last_sent.as_ref() != Some(&payload) {
                    if !send_json(&mut socket, &payload).await {
                        return;
                    }
                    last_sent = Some(payload);
                }
            }
        }
    }
}

/// The live uplink source: the router's sentinel plus a cached answer to whether
/// the router unit is running.
struct UplinkSource {
    flag: PathBuf,
    router_running: Option<bool>,
    router_checked_at: Option<Instant>,
}

impl UplinkSource {
    fn new(flag: PathBuf) -> Self {
        Self {
            flag,
            router_running: None,
            router_checked_at: None,
        }
    }

    /// The current uplink frame, or `None` when the sentinel cannot be read.
    async fn snapshot(&mut self) -> Option<Value> {
        let due = self
            .router_checked_at
            .is_none_or(|at| at.elapsed() >= ROUTER_RECHECK_INTERVAL);
        if due {
            self.router_running = crate::probe::unit_state(UPLINK_ROUTER_UNIT).await;
            self.router_checked_at = Some(Instant::now());
        }
        uplink_ws_payload(&read_uplink_flag(&self.flag).await, self.router_running)
    }
}

/// What the active-uplink sentinel holds right now.
enum UplinkFlag {
    /// The router has an active uplink; the body it wrote.
    Present(Map<String, Value>),
    /// No file: the router has no uplink (or has never run).
    Absent,
    /// The file exists but cannot be read or is not a JSON object.
    Unreadable,
}

async fn read_uplink_flag(path: &Path) -> UplinkFlag {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(body)) => UplinkFlag::Present(body),
            _ => UplinkFlag::Unreadable,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UplinkFlag::Absent,
        Err(_) => UplinkFlag::Unreadable,
    }
}

/// Shape the sentinel into the uplink WS frame. An absent sentinel is the
/// "no uplink" frame; an unreadable one yields nothing this poll. `stale` is true
/// unless systemd confirms the router unit is running: a stopped or crashed
/// router leaves its last sentinel behind, which is not the current state.
fn uplink_ws_payload(flag: &UplinkFlag, router_running: Option<bool>) -> Option<Value> {
    let no_uplink = Map::new();
    let body = match flag {
        UplinkFlag::Present(body) => body,
        UplinkFlag::Absent => &no_uplink,
        UplinkFlag::Unreadable => return None,
    };
    let field = |key: &str, keep: fn(&Value) -> bool| {
        body.get(key)
            .filter(|v| keep(v))
            .cloned()
            .unwrap_or(Value::Null)
    };
    Some(json!({
        "kind": "health_changed",
        "active_uplink": field("active_uplink", Value::is_string),
        "available": body
            .get("available")
            .filter(|v| v.is_array())
            .cloned()
            .unwrap_or_else(|| json!([])),
        "internet_reachable": body
            .get("internet_reachable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "data_cap_state": field("data_cap_state", Value::is_string),
        "timestamp_ms": field("timestamp_ms", Value::is_u64),
        "stale": router_running != Some(true),
    }))
}

// ---------------------------------------------------------------------------
// /pic/events
// ---------------------------------------------------------------------------

/// The `/pic/events` upgrade entry point. Resolves the handshake auth and the
/// profile gate, then relays the PIC arbiter's subscribe stream on the socket.
pub async fn ws_pic_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    caller: Option<Extension<CallerClass>>,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_PIC_EVENTS, caller_of(caller));
    let Some((ws, auth)) = upgrade_with(ws, auth) else {
        return ws_reject();
    };
    ws.on_upgrade(move |socket| pic_loop(socket, state, auth))
}

/// Relay the native arbiter's transition stream: profile-gate after accept, then
/// forward the PIC control socket's subscribe stream.
async fn pic_loop(socket: WebSocket, state: AppState, _auth: WsAuth) {
    relay_subscription(socket, &state, "pic.sock", "E_PIC_BUS_UNAVAILABLE").await;
}

/// Open `<run dir>/<sock_name>`, send `{"op":"subscribe"}`, and forward each
/// well-formed newline-JSON object verbatim as a text frame, with the keepalive
/// beat, until either side ends. A wrong-profile node closes 1008; an
/// unreachable socket reports `unavailable_code` and closes.
async fn relay_subscription(
    mut socket: WebSocket,
    state: &AppState,
    sock_name: &str,
    unavailable_code: &str,
) {
    if !is_ground_station(state) {
        close_profile_mismatch(&mut socket).await;
        return;
    }

    let stream = match tokio::net::UnixStream::connect(run_dir().join(sock_name)).await {
        Ok(s) => s,
        Err(exc) => {
            // Report the unavailable bus, then close.
            let body = json!({
                "event": "error",
                "code": unavailable_code,
                "message": exc.to_string(),
            });
            if send_json(&mut socket, &body).await {
                let _ = send_bounded(&mut socket, Message::Close(None)).await;
            }
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
    let mut keepalive = keepalive_ticker();
    let mut last_heard = Instant::now();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else {
                    return; // the daemon socket closed
                };
                // Forward only well-formed JSON; a blank or malformed line is
                // dropped.
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if !send_json(&mut socket, &event).await {
                    return;
                }
            }
            incoming = socket.recv() => {
                if !client_still_open(&incoming) {
                    return;
                }
                last_heard = Instant::now();
            }
            _ = keepalive.tick() => {
                if !keepalive_beat(&mut socket, last_heard).await {
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// /ws/buttons
// ---------------------------------------------------------------------------

/// The `/ws/buttons` upgrade entry point. Resolves the handshake auth and the
/// profile gate, then relays the front-panel button stream on the socket.
pub async fn ws_buttons(
    State(state): State<AppState>,
    headers: HeaderMap,
    caller: Option<Extension<CallerClass>>,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_BUTTON_EVENTS, caller_of(caller));
    let Some((ws, auth)) = upgrade_with(ws, auth) else {
        return ws_reject();
    };
    ws.on_upgrade(move |socket| buttons_loop(socket, state, auth))
}

/// Relay the button fanout stream: profile-gate after accept, then forward the
/// button socket's subscribe stream. The `ados-pic` reader is the single source
/// of the short/long/cancel classification + the config mapping, so this
/// forwards the already-classified events untouched.
async fn buttons_loop(socket: WebSocket, state: AppState, _auth: WsAuth) {
    relay_subscription(socket, &state, "buttons.sock", "E_BUTTON_BUS_UNAVAILABLE").await;
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
    caller: Option<Extension<CallerClass>>,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_MESH_EVENTS, caller_of(caller));
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
/// initial snapshot; the keepalive beat runs alongside.
async fn mesh_loop(mut socket: WebSocket, state: AppState, _auth: WsAuth) {
    if !is_ground_station(&state) {
        close_profile_mismatch(&mut socket).await;
        return;
    }

    let mut mesh_tail = JournalTail::new(run_dir().join("mesh-events.jsonl"));
    let mut pair_tail = JournalTail::new(run_dir().join("pair-events.jsonl"));
    let mut keepalive = keepalive_ticker();
    let mut last_heard = Instant::now();

    loop {
        // Drain whatever new lines each journal has, forwarding each verbatim.
        // A journal that is missing/rotating yields nothing and is retried on the
        // next poll, so one absent journal never starves the other.
        let mut forwarded_any = false;
        for tail in [&mut mesh_tail, &mut pair_tail] {
            while let Some(line) = tail.next_line().await {
                // Forward only well-formed JSON; a blank or malformed line is
                // dropped.
                let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                if !send_json(&mut socket, &event).await {
                    return;
                }
                forwarded_any = true;
            }
        }

        // After forwarding, loop straight back to drain any further backlog;
        // otherwise wait out the poll interval. The select keeps the loop
        // responsive to a disconnect and to the keepalive beat either way.
        let wait = if forwarded_any {
            Duration::ZERO
        } else {
            MESH_POLL_INTERVAL
        };
        tokio::select! {
            incoming = socket.recv() => {
                if !client_still_open(&incoming) {
                    return;
                }
                last_heard = Instant::now();
            }
            _ = tokio::time::sleep(wait) => {}
            _ = keepalive.tick() => {
                if !keepalive_beat(&mut socket, last_heard).await {
                    return;
                }
            }
        }
    }
}

/// A follower over an append-only newline-JSON journal.
///
/// When the journal already exists as the stream opens, the tail seeks to its
/// end, so a long-lived journal never replays stale events into a freshly
/// connected client. Any file the tail opens after that (the journal appearing
/// late, or being replaced by rename or truncated by a writer restart) holds only
/// events written since the client connected, so it is read from its start.
/// Replacement is detected by the path's device and inode differing from the
/// open handle's, not by size, because a new file can outgrow the old offset
/// before the next poll.
struct JournalTail {
    path: PathBuf,
    reader: Option<BufReader<tokio::fs::File>>,
    /// `(device, inode)` of the open handle.
    identity: Option<(u64, u64)>,
    offset: u64,
    /// Whether the next open reads from the start instead of seeking to the end.
    from_start: bool,
}

/// The `(device, inode)` pair naming the file behind `meta`.
fn file_identity(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

impl JournalTail {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            reader: None,
            identity: None,
            offset: 0,
            from_start: false,
        }
    }

    /// Drop the handle; the next call reopens the path from its start.
    fn reopen_from_start(&mut self) {
        self.reader = None;
        self.identity = None;
        self.offset = 0;
        self.from_start = true;
    }

    /// Return the next complete line from the journal, or `None` when there is
    /// nothing new to read right now (missing file, no new bytes, a partial
    /// trailing line, or a replacement just detected).
    async fn next_line(&mut self) -> Option<String> {
        // `read_line` comes from the top-level `AsyncBufReadExt`; `seek` needs
        // the seek extension trait in scope here.
        use tokio::io::AsyncSeekExt;

        if self.reader.is_none() {
            let mut file = match tokio::fs::File::open(&self.path).await {
                Ok(file) => file,
                Err(_) => {
                    // Missing now: whatever appears later is post-connect.
                    self.from_start = true;
                    return None;
                }
            };
            let identity = file_identity(&file.metadata().await.ok()?);
            let start = if self.from_start {
                0
            } else {
                file.seek(std::io::SeekFrom::End(0)).await.ok()?
            };
            self.offset = start;
            self.identity = Some(identity);
            self.reader = Some(BufReader::new(file));
            self.from_start = true;
        }

        let reader = self.reader.as_mut()?;
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            // EOF with no bytes: nothing new on this handle. If the path now
            // names another file, or ours shrank below what we read, or it is
            // gone, reopen from the start on the next call.
            Ok(0) => {
                let current = tokio::fs::metadata(&self.path).await.ok();
                let same = current.as_ref().is_some_and(|meta| {
                    Some(file_identity(meta)) == self.identity && meta.len() >= self.offset
                });
                if !same {
                    self.reopen_from_start();
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
                self.reopen_from_start();
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared seams.
// ---------------------------------------------------------------------------

/// Build a WebSocket close frame with the given code + reason.
fn close_frame(code: u16, reason: &str) -> axum::extract::ws::CloseFrame {
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

    // -- uplink frame from the live sentinel -------------------------------

    #[tokio::test]
    async fn uplink_frame_comes_from_the_sentinel_without_the_log_store() {
        // The durable log store is off by default; the stream must still emit
        // from the router's sentinel, which is the only source it reads.
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("uplink-active");
        std::fs::write(
            &flag,
            json!({
                "active_uplink": "eth0",
                "internet_reachable": true,
                "timestamp_ms": 1234,
                "data_cap_state": "warn_80",
            })
            .to_string(),
        )
        .unwrap();
        let payload = uplink_ws_payload(&read_uplink_flag(&flag).await, Some(true));
        assert_eq!(
            payload,
            Some(json!({
                "kind": "health_changed",
                "active_uplink": "eth0",
                "available": [],
                "internet_reachable": true,
                "data_cap_state": "warn_80",
                "timestamp_ms": 1234,
                "stale": false,
            }))
        );
    }

    #[tokio::test]
    async fn an_absent_sentinel_is_no_uplink_and_an_unreadable_one_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("uplink-active");
        let none = uplink_ws_payload(&read_uplink_flag(&flag).await, Some(true)).unwrap();
        assert_eq!(none["active_uplink"], Value::Null);
        assert_eq!(none["internet_reachable"], json!(false));
        assert_eq!(none["data_cap_state"], Value::Null);
        assert_eq!(none["stale"], json!(false));

        std::fs::write(&flag, "{ torn").unwrap();
        assert!(uplink_ws_payload(&read_uplink_flag(&flag).await, Some(true)).is_none());
    }

    #[tokio::test]
    async fn a_sentinel_left_by_a_stopped_router_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("uplink-active");
        std::fs::write(
            &flag,
            json!({"active_uplink": "wwan0", "internet_reachable": true}).to_string(),
        )
        .unwrap();
        let body = read_uplink_flag(&flag).await;
        // Stopped, or systemd gave no answer: the sentinel is not current.
        for router in [Some(false), None] {
            let frame = uplink_ws_payload(&body, router).unwrap();
            assert_eq!(frame["active_uplink"], json!("wwan0"));
            assert_eq!(frame["stale"], json!(true), "{router:?}");
        }
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
            relay_secret: dir.path().join("relay-peer-secret"),
        };
        let state = AppState::new(
            pairing,
            crate::ipc::StateIpcClient::disconnected(),
            crate::ipc::MavlinkIpcClient::new(dir.path().join("mavlink.sock")),
            crate::ipc::LogdQueryClient::new(dir.path().join("logd-query.sock")),
            dir.path().join("board.json"),
            paths,
            std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.path().join("dashboard-pin.json"),
            )),
            std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.path().join("mcp-token.json"),
            )),
        );
        (dir, state)
    }

    #[test]
    fn unpaired_admits_without_a_credential() {
        let (_d, state) = state_with_pairing(r#"{"paired": false}"#);
        assert!(matches!(
            decide_ws_auth(
                &state,
                &HeaderMap::new(),
                SCOPE_UPLINK_EVENTS,
                CallerClass::OperatorLan
            ),
            WsAuth::AcceptPlain
        ));
    }

    #[test]
    fn an_unpaired_node_refuses_a_remote_caller() {
        // Every data route refuses a remote caller while unpaired; these
        // streams used to be the exception.
        let (_d, state) = state_with_pairing(r#"{"paired": false}"#);
        assert!(matches!(
            decide_ws_auth(
                &state,
                &HeaderMap::new(),
                SCOPE_UPLINK_EVENTS,
                CallerClass::Remote
            ),
            WsAuth::Reject
        ));
    }

    #[test]
    fn an_unreadable_pairing_file_serves_only_the_on_box_operator() {
        let (_d, state) = state_with_pairing("this is not json");
        for caller in [
            CallerClass::Remote,
            CallerClass::OperatorLan,
            CallerClass::Lifeline,
        ] {
            assert!(
                matches!(
                    decide_ws_auth(&state, &HeaderMap::new(), SCOPE_MESH_EVENTS, caller),
                    WsAuth::Reject
                ),
                "{caller:?}"
            );
        }
        assert!(matches!(
            decide_ws_auth(
                &state,
                &HeaderMap::new(),
                SCOPE_MESH_EVENTS,
                CallerClass::OnBox
            ),
            WsAuth::AcceptPlain
        ));
    }

    #[test]
    fn a_ticket_opens_one_socket() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let token = WsTicketIssuer::from_api_key("k")
            .mint(SCOPE_PIC_EVENTS, 30)
            .unwrap()
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_PIC_EVENTS, CallerClass::OperatorLan),
            WsAuth::AcceptTicket
        ));
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_PIC_EVENTS, CallerClass::OperatorLan),
            WsAuth::Reject
        ));
    }

    #[test]
    fn paired_admits_with_the_key_header() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let h = headers_with(&[("x-ados-key", "k")]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS, CallerClass::OperatorLan),
            WsAuth::AcceptPlain
        ));
    }

    #[test]
    fn paired_rejects_a_wrong_key_and_no_ticket() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let h = headers_with(&[("x-ados-key", "wrong")]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS, CallerClass::OperatorLan),
            WsAuth::Reject
        ));
        // No credential at all is also rejected.
        assert!(matches!(
            decide_ws_auth(
                &state,
                &HeaderMap::new(),
                SCOPE_UPLINK_EVENTS,
                CallerClass::OperatorLan
            ),
            WsAuth::Reject
        ));
    }

    #[test]
    fn paired_admits_with_a_valid_scoped_ticket() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let token = WsTicketIssuer::from_api_key("k")
            .mint(SCOPE_UPLINK_EVENTS, 30)
            .unwrap()
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS, CallerClass::OperatorLan),
            WsAuth::AcceptTicket
        ));
    }

    #[test]
    fn a_ticket_for_the_wrong_scope_is_rejected() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        // Minted for pic_events but presented to the uplink scope.
        let token = WsTicketIssuer::from_api_key("k")
            .mint(SCOPE_PIC_EVENTS, 30)
            .unwrap()
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS, CallerClass::OperatorLan),
            WsAuth::Reject
        ));
    }

    #[test]
    fn a_ticket_for_the_wrong_key_is_rejected() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let token = WsTicketIssuer::from_api_key("other-key")
            .mint(SCOPE_UPLINK_EVENTS, 30)
            .unwrap()
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_UPLINK_EVENTS, CallerClass::OperatorLan),
            WsAuth::Reject
        ));
    }

    // -- mesh stream: auth scope + the two-journal fan -------------------

    #[test]
    fn mesh_stream_admits_with_a_valid_mesh_scoped_ticket() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let token = WsTicketIssuer::from_api_key("k")
            .mint(SCOPE_MESH_EVENTS, 30)
            .unwrap()
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_MESH_EVENTS, CallerClass::OperatorLan),
            WsAuth::AcceptTicket
        ));
    }

    #[test]
    fn mesh_stream_rejects_a_wrong_scope_ticket() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        // A uplink-scoped ticket presented to the mesh scope is rejected.
        let token = WsTicketIssuer::from_api_key("k")
            .mint(SCOPE_UPLINK_EVENTS, 30)
            .unwrap()
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_MESH_EVENTS, CallerClass::OperatorLan),
            WsAuth::Reject
        ));
    }

    // -- button stream: auth scope --------------------------------------------

    #[test]
    fn button_stream_admits_with_a_valid_button_scoped_ticket() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        let token = WsTicketIssuer::from_api_key("k")
            .mint(SCOPE_BUTTON_EVENTS, 30)
            .unwrap()
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_BUTTON_EVENTS, CallerClass::OperatorLan),
            WsAuth::AcceptTicket
        ));
    }

    #[test]
    fn button_stream_rejects_a_wrong_scope_ticket() {
        let (_d, state) = state_with_pairing(r#"{"paired": true, "api_key": "k"}"#);
        // A pic-scoped ticket presented to the button scope is rejected.
        let token = WsTicketIssuer::from_api_key("k")
            .mint(SCOPE_PIC_EVENTS, 30)
            .unwrap()
            .token;
        let h = headers_with(&[(
            "sec-websocket-protocol",
            &format!("ados-ws-ticket, {token}"),
        )]);
        assert!(matches!(
            decide_ws_auth(&state, &h, SCOPE_BUTTON_EVENTS, CallerClass::OperatorLan),
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
    async fn a_journal_created_after_connect_is_read_from_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("late.jsonl");
        let mut tail = JournalTail::new(p.clone());
        // The file does not exist yet: a poll yields nothing without erroring.
        assert!(tail.next_line().await.is_none());
        // It appears later, so everything in it was written after the client
        // connected and streams.
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&p).unwrap();
            writeln!(f, r#"{{"bus":"pair","kind":"accept_window_opened","timestamp_ms":3,"payload":{{"duration_s":60}}}}"#)
                .unwrap();
        }
        let line = tail
            .next_line()
            .await
            .expect("the first line of the new file");
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["kind"], "accept_window_opened");
        assert!(tail.next_line().await.is_none());
    }

    #[tokio::test]
    async fn a_journal_replaced_by_rename_is_followed_from_its_start() {
        // A writer restart recreates the journal by rename; the new file can
        // already be longer than the old offset when the tail next looks.
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mesh-events.jsonl");
        std::fs::write(&p, "{\"kind\":\"old\"}\n").unwrap();
        let mut tail = JournalTail::new(p.clone());
        // Opens and seeks past the pre-connect backlog.
        assert!(tail.next_line().await.is_none());

        let replacement = dir.path().join("mesh-events.jsonl.tmp");
        {
            let mut f = std::fs::File::create(&replacement).unwrap();
            writeln!(
                f,
                r#"{{"bus":"mesh","kind":"relay_connected","timestamp_ms":5,"payload":{{}}}}"#
            )
            .unwrap();
            writeln!(
                f,
                r#"{{"bus":"mesh","kind":"receiver_unreachable","timestamp_ms":6,"payload":{{}}}}"#
            )
            .unwrap();
        }
        std::fs::rename(&replacement, &p).unwrap();

        // The old handle is at EOF; the tail notices the new inode, then reads
        // the new file from its first line.
        let mut kinds = Vec::new();
        for _ in 0..4 {
            if let Some(line) = tail.next_line().await {
                let v: Value = serde_json::from_str(line.trim()).unwrap();
                kinds.push(v["kind"].as_str().unwrap().to_string());
            }
        }
        assert_eq!(kinds, ["relay_connected", "receiver_unreachable"]);
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
