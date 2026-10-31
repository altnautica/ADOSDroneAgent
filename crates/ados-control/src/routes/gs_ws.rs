//! Ground-station WebSocket relays served natively by the front.
//!
//! These streams are upgraded past the HTTP auth edge, so each handler
//! enforces the agent's WebSocket auth contract itself (mirroring the residual
//! handlers, which did the same because the upgrade bypasses the HTTP gate):
//!
//! * **Unpaired** ⇒ open to the on-box operator and the first-boot lifelines;
//!   an operator-LAN caller presents a ticket minted under the dashboard-PIN
//!   salt (the same PIN bar every unpaired data route sets).
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

mod mesh;
mod uplink;

pub use mesh::ws_mesh;
#[cfg(test)]
use mesh::*;
pub use uplink::ws_uplink;
#[cfg(test)]
use uplink::*;

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
// Handshake auth (the same decision as the Python `authenticate_websocket`).
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
/// - **Unpaired:** the local operator and the first-boot lifelines are
///   admitted plainly. An operator-LAN caller needs a ticket minted while
///   unpaired, keyed by the dashboard-PIN salt (the ticket route is PIN-gated
///   for that caller), the same bar every unpaired data route sets. A remote
///   caller (a public-WAN host, a tunnelled request) is refused.
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
            return match caller {
                CallerClass::OnBox | CallerClass::Lifeline => WsAuth::AcceptPlain,
                CallerClass::OperatorLan => {
                    let issuer = state
                        .dashboard_pin
                        .salt()
                        .map(|salt| WsTicketIssuer::issuer_unpaired(&salt));
                    let offered = offered_subprotocols(headers);
                    match (issuer, extract_ticket(&offered)) {
                        (Some(issuer), Some(token))
                            if issuer.verify_once(token, scope, now_unix()).is_ok() =>
                        {
                            WsAuth::AcceptTicket
                        }
                        _ => WsAuth::Reject,
                    }
                }
                CallerClass::Remote => WsAuth::Reject,
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
// Profile gate: ground-station profile only.
// ---------------------------------------------------------------------------

/// True when the node resolves to the ground-station profile, the same source of
/// truth the node advertises on the wire.
fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
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

    let stream = match tokio::net::UnixStream::connect(state.run_dir.join(sock_name)).await {
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
mod tests;
