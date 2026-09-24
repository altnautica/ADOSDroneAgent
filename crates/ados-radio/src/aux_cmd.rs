//! Command socket for the auxiliary application stream.
//!
//! A plugin that needs an isolated low-rate channel between nodes asks the
//! plugin host to open one; the host forwards the request to this socket, and the
//! running radio service brings up an additive transmit/receive pair on a
//! separate radio-port (it never touches the data or control planes). A matching
//! `close` (or the plugin disconnecting and the host closing on its behalf) tears
//! the pair down.
//!
//! SAFE-BY-DEFAULT: nothing here runs at boot. The aux pair exists ONLY between
//! an explicit `open` and the matching `close`. The radio service spawns this
//! socket per bring-up with the SAME process handle the watchdogs + operator
//! command socket hold, so an `open` reaches the live radio group.
//!
//! Wire protocol (mirrors the operator command socket): one newline-terminated
//! JSON request. `open` / `close` / `status` / `send` / `publish` reply with one
//! newline-terminated JSON response and close (one-shot). `subscribe` flips the
//! connection into a streaming subscriber: it replies `{"ok":true}` then pushes
//! every decoded application datagram as a newline-terminated JSON line until
//! the client disconnects or asks to close.
//!
//! ```text
//! {"op":"open"}
//!     -> {"ok":true,"active":true,"tx_port":5602,"rx_port":5603}
//! {"op":"close"}
//!     -> {"ok":true,"active":false}
//! {"op":"status"}
//!     -> {"ok":true,"active":false}
//! {"op":"send","frame":[170,2,1,8,0,5,104,101,108,108,111]}
//!     -> {"ok":true}
//! {"op":"publish","channel":8,"payload":[104,101,108,108,111]}
//!     -> {"ok":true,"delivered":1}
//! {"op":"subscribe"}
//!     -> {"ok":true}
//!     -> {"channel":8,"payload":[104,101,108,108,111]}
//!     -> ...
//! ```
//!
//! ## Why `publish` exists — the inbound half lives in another process
//!
//! `send` writes OUTBOUND: an aux-framed datagram to the local transmit
//! ingress, which `wfb_tx` radiates. `publish` is the INBOUND direction, and it
//! is a separate op because this service does not own the receive side.
//!
//! The aux-RX loopback port that `wfb_rx -p3` decodes onto is owned by
//! `ados-mavlink-router`'s aux-uplink consumer, which needs it for the MAVLink,
//! relay-RPC, link-feedback and config-tunnel channels on that same lane. This
//! service bound it too for a while; nothing set SO_REUSEADDR, so one of the two
//! always lost and a whole lane went dark with no surface reporting it. One
//! owner now: the consumer decodes, and the two APPLICATION channels
//! (`AppStream` 8 / `AppCommand` 9) come back here through `publish`, which
//! injects them into the same broadcast every `subscribe` connection reads.
//!
//! `delivered` is the number of attached subscribers the datagram reached. Zero
//! is NOT a failure — it is the normal state of a drone with no plugin
//! subscribed — so a caller counting drops must key on `ok:false`, not on a zero
//! delivered count, or a healthy rig reports total loss.
//!
//! A failed apply (a spawn failure on `open`) replies `{"ok":false,"error":"..."}`
//! and leaves the aux pair closed, so the host can surface the error. The socket
//! only mutates the aux pair it owns; it never round-trips the on-disk config.
//!
//! ## The same socket on a ground station
//!
//! A ground station serves this exact protocol from its receive-plane service,
//! so a plugin half written against it runs unchanged on either node. The
//! difference is only what backs it (see [`AuxCmdState::ground`]): the ground
//! uplink `wfb_tx` belongs to the receive chain and already carries MAVLink,
//! relay RPC, link feedback and the config tunnel, so there is no pair to spawn.
//! `open`/`close` gate this socket's application egress instead, `send` accepts
//! only the two application channels (a plugin must never be able to inject a
//! MAVLink frame onto that shared uplink), and inbound `AppStream` datagrams
//! arrive in-process from the service's own aux consumer rather than over
//! `publish`, which stays available with the same semantics.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ados_protocol::aux_egress::{AuxEgress, AuxEgressError};
use ados_protocol::aux_mux::{self, AuxChannel};
use ados_protocol::ipc::{bind_command_socket, read_newline_line, OperatorListener};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, Mutex};

use crate::config::WfbConfig;
use crate::process::RadioProcesses;

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The shared state the aux command handlers act on: what carries the lane, the
/// operator dead-switch, and the inbound application fan-out.
/// Constructed once per bring-up on a drone ([`Self::radio`]) or once per
/// service on a ground station ([`Self::ground`]) and shared with every accepted
/// connection.
#[derive(Clone)]
pub struct AuxCmdState {
    lane: AuxLane,
    /// The operator dead-switch (`aux_enable`). Off refuses every op that would
    /// move application traffic in either direction.
    aux_enable: bool,
    /// Fan-out for inbound application datagrams. `subscribe` connections stream
    /// from this; [`Self::publish`] (the `publish` op, or a ground station's
    /// in-process aux consumer) feeds it. Items are `(channel as u8, payload)`.
    rx_tx: broadcast::Sender<(u8, Vec<u8>)>,
}

/// What an outbound `send` goes through, and what `open` / `close` / `status`
/// act on.
#[derive(Clone)]
enum AuxLane {
    /// Drone: this service owns the additive aux transmit/receive pair. `proc` is
    /// the SAME handle the watchdogs + operator command socket hold, so an `open`
    /// reaches the live radio group; `cfg` is the boot config, the source of the
    /// effective aux ports / FEC / MCS an `open` applies.
    Radio {
        proc: Arc<Mutex<RadioProcesses>>,
        cfg: Arc<WfbConfig>,
    },
    /// Ground station: the uplink `wfb_tx` is owned by the receive chain and
    /// shared with every other plane on the lane, so `open` / `close` only gate
    /// this socket's application egress. `open` is shared by every connection.
    Uplink {
        egress: Arc<AuxEgress>,
        tx_port: u16,
        open: Arc<AtomicBool>,
    },
}

impl AuxCmdState {
    /// The drone side: `open` / `close` drive the additive aux pair in `proc`,
    /// `send` writes to its loopback transmit ingress, and the aux-uplink
    /// consumer in the MAVLink router feeds `rx_tx` over the `publish` op.
    pub fn radio(
        proc: Arc<Mutex<RadioProcesses>>,
        cfg: Arc<WfbConfig>,
        rx_tx: broadcast::Sender<(u8, Vec<u8>)>,
    ) -> Self {
        Self {
            aux_enable: cfg.aux_enable,
            lane: AuxLane::Radio { proc, cfg },
            rx_tx,
        }
    }

    /// The ground-station side: `send` goes out through an egress connected to
    /// the uplink transmit ingress `tx_port` (the receive chain's aux `wfb_tx`),
    /// and the caller feeds inbound application datagrams through
    /// [`Self::publish`]. Fails only when the loopback egress socket cannot be
    /// created.
    pub async fn ground(
        tx_port: u16,
        aux_enable: bool,
        rx_tx: broadcast::Sender<(u8, Vec<u8>)>,
    ) -> Result<Self, AuxEgressError> {
        let egress = AuxEgress::connected_to_udp(tx_port).await?;
        Ok(Self {
            lane: AuxLane::Uplink {
                egress: Arc::new(egress),
                tx_port,
                open: Arc::new(AtomicBool::new(false)),
            },
            aux_enable,
            rx_tx,
        })
    }

    /// Inject one inbound application payload into the subscriber fan-out and
    /// return the reply the `publish` op would send (see
    /// [`publish_app_datagram`]): refused when the lane is disabled or the
    /// channel is not an application channel, otherwise the subscriber count.
    pub fn publish(&self, channel: u8, payload: Vec<u8>) -> Value {
        publish_app_datagram(self.aux_enable, &self.rx_tx, channel, payload)
    }
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    frame: Option<Vec<u8>>,
    /// `publish` only: the aux channel the payload arrived on (8 or 9).
    #[serde(default)]
    channel: Option<u8>,
    /// `publish` only: the already-decoded application bytes, WITHOUT the aux
    /// frame header — `subscribe` streams exactly these.
    #[serde(default)]
    payload: Option<Vec<u8>>,
}

/// Bind the aux command socket and serve connections until the listener errors.
/// Run as its own task from the service main loop. The shared helper owns the
/// create-dir / remove-stale / bind / chmod (0660, group `ados-operator`) hygiene
/// and admits only root and operator-group peers. Each connection serves one request:
/// `open`/`close`/`status`/`send` reply once and close; `subscribe` replies then
/// streams application datagrams until the client disconnects or asks to close.
///
/// A Unix-domain listener supports multiple concurrent connecting clients, so a
/// subscriber holding a connection open never blocks a one-shot caller.
pub async fn serve(state: AuxCmdState, sock_path: &Path) -> std::io::Result<()> {
    let listener = bind_command_socket(sock_path, 0o660)?;
    tracing::info!(path = %sock_path.display(), "aux command socket listening");

    accept_loop(listener, state).await;
    Ok(())
}

/// Accept loop over the aux command listener. Backs off on a transient accept
/// error rather than dying (a command socket that dies while the service stays
/// up would need a manual restart), mirroring the shared one-shot helper. Each
/// connection runs on its own task; a streaming subscriber does not block the
/// other connections.
async fn accept_loop(listener: OperatorListener, state: AuxCmdState) {
    loop {
        let mut stream = match listener.accept().await {
            Ok((s, _addr)) => s,
            Err(e) => {
                tracing::warn!(error = %e, "aux command socket accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_connection(&mut stream, &state).await {
                tracing::debug!(error = %e, "aux command connection ended");
            }
        });
    }
}

/// Serve one connection: read one newline-terminated request, then either reply
/// once (`open`/`close`/`status`/`send`) or switch into the streaming subscriber
/// path (`subscribe`).
async fn serve_connection(stream: &mut UnixStream, state: &AuxCmdState) -> io::Result<()> {
    let request = match read_newline_line(stream, MAX_REQUEST_BYTES).await {
        Ok(Some(req)) => req,
        Ok(None) | Err(_) => return Ok(()),
    };
    match parse_command(&request) {
        Parsed::Cmd(cmd) => {
            let resp = apply(cmd, state).await;
            write_json_line(stream, &resp).await?;
        }
        Parsed::Reply(v) => write_json_line(stream, &v).await?,
        Parsed::Subscribe => serve_subscriber(stream, state).await?,
    }
    Ok(())
}

/// Write one newline-terminated JSON line and flush. Mirrors the shared one-shot
/// helper's framing (line + trailing newline, then flush).
async fn write_json_line<W: AsyncWriteExt + Unpin>(w: &mut W, v: &Value) -> io::Result<()> {
    let mut bytes =
        serde_json::to_vec(v).map_err(|e| io::Error::other(format!("E_ENCODE: {e}")))?;
    bytes.push(b'\n');
    w.write_all(&bytes).await?;
    w.flush().await
}

/// Hold a subscribe connection open, streaming each decoded application datagram
/// as a newline-terminated `{"channel":N,"payload":[...]}` line, until the client
/// disconnects or asks to close. The operator dead-switch is checked first so a
/// disabled deployment refuses the subscribe without holding a connection open.
async fn serve_subscriber(stream: &mut UnixStream, state: &AuxCmdState) -> io::Result<()> {
    if !state.aux_enable {
        return write_json_line(stream, &json!({"ok": false, "error": "E_AUX_DISABLED"})).await;
    }
    write_json_line(stream, &json!({"ok": true})).await?;
    let mut rx = state.rx_tx.subscribe();
    loop {
        tokio::select! {
            res = rx.recv() => {
                match res {
                    Ok((channel, payload)) => {
                        let line = json!({"channel": channel, "payload": payload});
                        if write_json_line(stream, &line).await.is_err() {
                            break; // client gone (write failed)
                        }
                    }
                    // Subscriber fell behind; the oldest frames were dropped. Keep
                    // going — an application lane is lossy-tolerant, and dropping
                    // is better than stalling the connection.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            req = read_newline_line(stream, MAX_REQUEST_BYTES) => {
                // The client sent a line (e.g. {"op":"close"}) or disconnected
                // (clean EOF / error). Either way the stream ends.
                let _ = req;
                break;
            }
        }
    }
    Ok(())
}

/// A request that has been parsed + validated and is ready to apply.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// Bring up the additive aux transmit/receive pair (idempotent).
    Open,
    /// Tear down the aux pair (idempotent).
    Close,
    /// Report whether the aux pair is currently up.
    Status,
    /// Write one already-aux-framed UDP datagram (see
    /// [`ados_protocol::aux_mux::encode`]) to the local aux transmit ingress
    /// (`127.0.0.1:<cfg.aux_tx_port>`), so `wfb_tx` radiates it to the paired
    /// node. One-shot: no per-send state, the stream is between open and close.
    Send { frame: Vec<u8> },
    /// Inject one INBOUND application payload into the subscriber fan-out. The
    /// aux-uplink consumer in the MAVLink router owns the receive port and calls
    /// this for the two application channels it decodes; see the module doc for
    /// why the receive side is not in this process.
    Publish { channel: u8, payload: Vec<u8> },
}

/// The outcome of parsing a request line: an apply-ready [`Command`], a terminal
/// response for a malformed/unknown request, or a `subscribe` request that the
/// connection handler turns into a streaming subscriber.
enum Parsed {
    Cmd(Command),
    Reply(Value),
    Subscribe,
}

/// Parse + validate one request line. Pure: no radio access, no I/O, fully
/// unit-testable. A bad-JSON / unknown-op request resolves to a terminal
/// [`Parsed::Reply`]; a well-formed request resolves to a [`Command`] or
/// [`Parsed::Subscribe`].
fn parse_command(line: &[u8]) -> Parsed {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => {
            return Parsed::Reply(json!({"ok": false, "error": format!("E_BAD_REQUEST: {e}")}))
        }
    };
    match req.op.as_str() {
        "open" => Parsed::Cmd(Command::Open),
        "close" => Parsed::Cmd(Command::Close),
        "status" => Parsed::Cmd(Command::Status),
        "send" => match req.frame {
            Some(frame) => Parsed::Cmd(Command::Send { frame }),
            None => Parsed::Reply(json!({"ok": false, "error": "E_BAD_REQUEST: missing frame"})),
        },
        "publish" => match (req.channel, req.payload) {
            // The channel is validated on apply, not here: `parse_command` is
            // pure over the line and the accepted set belongs with the fan-out
            // it feeds.
            (Some(channel), Some(payload)) => Parsed::Cmd(Command::Publish { channel, payload }),
            (None, _) => {
                Parsed::Reply(json!({"ok": false, "error": "E_BAD_REQUEST: missing channel"}))
            }
            (_, None) => {
                Parsed::Reply(json!({"ok": false, "error": "E_BAD_REQUEST: missing payload"}))
            }
        },
        "subscribe" => Parsed::Subscribe,
        other => Parsed::Reply(json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")})),
    }
}

/// The operator dead-switch decision for an `open`: when `aux_enable` is false,
/// return the terminal `E_AUX_DISABLED` reply so the caller refuses the open
/// before touching the radio (no process is spawned). `None` means the open may
/// proceed. Pure so the refusal is unit-testable without a live radio group.
fn aux_disabled_reply(aux_enable: bool) -> Option<Value> {
    if aux_enable {
        None
    } else {
        Some(json!({"ok": false, "error": "E_AUX_DISABLED"}))
    }
}

/// Apply a validated command to the lane this state carries.
async fn apply(cmd: Command, state: &AuxCmdState) -> Value {
    match cmd {
        Command::Open => {
            // The operator dead-switch is checked first so a disabled deployment
            // gets a clear, distinct error rather than a generic open failure,
            // and so NO process is spawned (the lock + open below is never
            // reached). (`open_aux_stream` enforces the same guard structurally,
            // so a cap-holding caller can never open the stream when disabled.)
            if let Some(reply) = aux_disabled_reply(state.aux_enable) {
                return reply;
            }
            match &state.lane {
                AuxLane::Radio { proc, cfg } => {
                    // Idempotent open: brings up the additive aux pair on the
                    // config's effective aux ports/FEC/MCS. Never touches the
                    // data/control planes.
                    if proc.lock().await.open_aux_stream(cfg).await {
                        json!({
                            "ok": true,
                            "active": true,
                            "tx_port": cfg.aux_tx_port,
                            "rx_port": cfg.aux_rx_port,
                        })
                    } else {
                        json!({"ok": false, "error": "E_AUX_OPEN_FAILED"})
                    }
                }
                // The uplink transmitter already runs; opening admits this
                // socket's sends. No `rx_port`: inbound traffic reaches a client
                // only through `subscribe`, fed per drone slot in-process, so no
                // single receive port describes it.
                AuxLane::Uplink { tx_port, open, .. } => {
                    open.store(true, Ordering::Release);
                    json!({"ok": true, "active": true, "tx_port": tx_port})
                }
            }
        }
        Command::Close => {
            match &state.lane {
                AuxLane::Radio { proc, .. } => proc.lock().await.close_aux_stream().await,
                // Closes the gate, never the shared uplink transmitter.
                AuxLane::Uplink { open, .. } => open.store(false, Ordering::Release),
            }
            json!({"ok": true, "active": false})
        }
        Command::Status => {
            let active = match &state.lane {
                AuxLane::Radio { proc, .. } => proc.lock().await.aux_active(),
                AuxLane::Uplink { open, .. } => open.load(Ordering::Acquire),
            };
            json!({"ok": true, "active": active})
        }
        Command::Send { frame } => {
            // The operator dead-switch is checked first (no datagram is written
            // while the aux lane is disabled by policy), and the pair must be
            // open — the stream exists only between an open and its close.
            if let Some(reply) = aux_disabled_reply(state.aux_enable) {
                return reply;
            }
            match &state.lane {
                AuxLane::Radio { proc, cfg } => {
                    if !proc.lock().await.aux_active() {
                        return json!({"ok": false, "error": "E_AUX_NOT_OPEN"});
                    }
                    // Write the already-aux-framed datagram to the local aux
                    // transmit ingress; wfb_tx radiates it and the paired node's
                    // aux-rx re-emits it to its own subscribers. A fresh socket
                    // per send (bound to an ephemeral port) is dropped after the
                    // datagram — one-shot, like open/close, with no per-send
                    // state.
                    let sock = match tokio::net::UdpSocket::bind(("127.0.0.1", 0)).await {
                        Ok(s) => s,
                        Err(e) => {
                            return json!({"ok": false, "error": format!("E_AUX_SEND_UDP: {e}")})
                        }
                    };
                    if let Err(e) = sock.send_to(&frame, ("127.0.0.1", cfg.aux_tx_port)).await {
                        return json!({"ok": false, "error": format!("E_AUX_SEND: {e}")});
                    }
                    json!({"ok": true})
                }
                AuxLane::Uplink { egress, open, .. } => {
                    if !open.load(Ordering::Acquire) {
                        return json!({"ok": false, "error": "E_AUX_NOT_OPEN"});
                    }
                    send_uplink(egress, &frame).await
                }
            }
        }
        Command::Publish { channel, payload } => state.publish(channel, payload),
    }
}

/// Emit one `send` frame onto a ground station's shared aux uplink.
///
/// The frame is decoded before it goes anywhere: that uplink also carries the
/// drone's MAVLink, relay RPC, link feedback and config tunnel, so only the two
/// application channels may pass. A frame that does not decode (bad magic, a
/// length that disagrees with the bytes, a payload over
/// [`aux_mux::AUX_MAX_PAYLOAD`]) is refused rather than radiated as garbage.
async fn send_uplink(egress: &AuxEgress, frame: &[u8]) -> Value {
    let (channel, payload) = match aux_mux::decode(frame) {
        Ok(decoded) => decoded,
        Err(e) => return json!({"ok": false, "error": format!("E_BAD_FRAME: {e:?}")}),
    };
    if !matches!(channel, AuxChannel::AppStream | AuxChannel::AppCommand) {
        return json!({"ok": false, "error": "E_BAD_CHANNEL"});
    }
    match egress.send(channel, payload).await {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": format!("E_AUX_SEND: {e}")}),
    }
}

/// Inject one inbound application payload into the subscriber fan-out.
///
/// Split out from [`apply`] because it touches no radio state at all — it takes
/// the dead-switch flag and the sender rather than the whole `AuxCmdState`, so
/// the accept/refuse decision AND the actual delivery are exercisable without a
/// live `wfb_tx` process group. The channel gate is the load-bearing part: the
/// caller is a different process on the other side of a socket, and a frame from
/// any other plane on that lane must never reach a plugin's application stream.
fn publish_app_datagram(
    aux_enable: bool,
    rx_tx: &broadcast::Sender<(u8, Vec<u8>)>,
    channel: u8,
    payload: Vec<u8>,
) -> Value {
    // Same operator dead-switch as `send` / `subscribe`: a deployment that
    // turned the aux lane off must not have application traffic arriving
    // through the back door either.
    if let Some(reply) = aux_disabled_reply(aux_enable) {
        return reply;
    }
    // Only the two APPLICATION channels. The other planes on this lane (MAVLink,
    // relay RPC, link feedback, config tunnel) have their own consumers in the
    // router process, and leaking one into the application fan-out would corrupt
    // a plugin's stream with frames it has no framing for.
    if channel != AuxChannel::AppStream as u8 && channel != AuxChannel::AppCommand as u8 {
        return json!({"ok": false, "error": "E_BAD_CHANNEL"});
    }
    // `send` returns Err only when there is no subscriber, which is the normal
    // state of a drone with no plugin attached — not a failure. The count is
    // reported so a caller can distinguish "nobody is listening" from "the lane
    // is broken" without guessing.
    let delivered = rx_tx.send((channel, payload)).unwrap_or(0);
    json!({"ok": true, "delivered": delivered})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract the early-reply `Value`, or panic if the parse produced anything
    /// but a terminal reply.
    fn reply(line: &[u8]) -> Value {
        match parse_command(line) {
            Parsed::Reply(v) => v,
            Parsed::Cmd(c) => panic!("expected an early reply, got command {c:?}"),
            Parsed::Subscribe => panic!("expected an early reply, got a subscribe"),
        }
    }

    /// Extract the apply-ready `Command`, or panic if the parse produced a reply
    /// or a subscribe.
    fn cmd(line: &[u8]) -> Command {
        match parse_command(line) {
            Parsed::Cmd(c) => c,
            Parsed::Reply(v) => panic!("expected a command, got reply {v}"),
            Parsed::Subscribe => panic!("expected a command, got a subscribe"),
        }
    }

    /// Assert the request parses to [`Parsed::Subscribe`].
    fn subscribe(line: &[u8]) {
        assert!(
            matches!(parse_command(line), Parsed::Subscribe),
            "expected a subscribe request"
        );
    }

    #[test]
    fn open_close_status_parse_to_commands() {
        assert_eq!(cmd(br#"{"op":"open"}"#), Command::Open);
        assert_eq!(cmd(br#"{"op":"close"}"#), Command::Close);
        assert_eq!(cmd(br#"{"op":"status"}"#), Command::Status);
    }

    #[test]
    fn send_parses_its_aux_framed_payload() {
        // A `send` carries the already-aux-framed datagram bytes. Well-formed
        // sends round-trip the exact frame; a send without a frame is a clean
        // E_BAD_REQUEST, never a silent no-op or a panic.
        let frame: Vec<u8> = vec![
            0xAD, 0x02, 0x01, 0x08, 0x00, 0x05, b'h', b'e', b'l', b'l', b'o',
        ];
        let c = cmd(br#"{"op":"send","frame":[173,2,1,8,0,5,104,101,108,108,111]}"#);
        assert_eq!(c, Command::Send { frame });
        let v = reply(br#"{"op":"send"}"#);
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "E_BAD_REQUEST: missing frame");
    }

    #[test]
    fn subscribe_parses_to_the_streaming_request() {
        subscribe(br#"{"op":"subscribe"}"#);
    }

    #[test]
    fn publish_parses_the_decoded_channel_and_payload() {
        // `publish` carries the DECODED inner payload, not an aux frame — the
        // consumer already stripped the header, and `subscribe` streams exactly
        // these bytes. A request missing either field is a clean E_BAD_REQUEST
        // naming which one, so the caller can fix its encoder.
        let c = cmd(br#"{"op":"publish","channel":8,"payload":[104,105]}"#);
        assert_eq!(
            c,
            Command::Publish {
                channel: 8,
                payload: vec![b'h', b'i']
            }
        );
        assert_eq!(
            reply(br#"{"op":"publish","payload":[1]}"#)["error"],
            "E_BAD_REQUEST: missing channel"
        );
        assert_eq!(
            reply(br#"{"op":"publish","channel":8}"#)["error"],
            "E_BAD_REQUEST: missing payload"
        );
    }

    #[tokio::test]
    async fn a_published_datagram_reaches_every_subscriber() {
        // The end the aux-uplink consumer now feeds: it decodes off the port it
        // owns and hands the two application channels back here, so a plugin
        // holding a `subscribe` connection still receives inbound app traffic
        // even though this process no longer binds the receive port.
        let (tx, _keepalive) = broadcast::channel::<(u8, Vec<u8>)>(16);
        let mut plugin = tx.subscribe();
        let v = publish_app_datagram(true, &tx, AuxChannel::AppCommand as u8, b"pong".to_vec());
        assert_eq!(v["ok"], true);
        assert_eq!(
            v["delivered"], 2,
            "the reply counts the subscribers reached"
        );
        assert_eq!(
            plugin.try_recv().expect("the subscriber must receive it"),
            (AuxChannel::AppCommand as u8, b"pong".to_vec())
        );
    }

    #[test]
    fn publish_refuses_every_channel_that_is_not_an_application_lane() {
        // The aux lane also carries MAVLink, relay RPC, link feedback and the
        // config tunnel, each with its own consumer in the router process. The
        // caller is another process across a socket, so this gate is the only
        // thing stopping a MAVLink frame from being delivered to a plugin as
        // application bytes it has no framing for.
        let (tx, _keepalive) = broadcast::channel::<(u8, Vec<u8>)>(16);
        for channel in [
            AuxChannel::Mavlink as u8,
            AuxChannel::Request as u8,
            AuxChannel::LinkFeedback as u8,
            AuxChannel::ConfigTunnel as u8,
        ] {
            let v = publish_app_datagram(true, &tx, channel, vec![0xFF]);
            assert_eq!(v["ok"], false, "channel {channel} must be refused");
            assert_eq!(v["error"], "E_BAD_CHANNEL");
        }
        // And both application channels are accepted.
        for channel in [AuxChannel::AppStream as u8, AuxChannel::AppCommand as u8] {
            assert_eq!(
                publish_app_datagram(true, &tx, channel, vec![1])["ok"],
                true
            );
        }
    }

    #[test]
    fn publish_with_no_subscriber_is_success_not_a_drop() {
        // The normal state of a drone with no plugin attached. Reporting this as
        // a failure would make a healthy rig's caller count 100% loss on a lane
        // that is working exactly as designed.
        let (tx, _) = broadcast::channel::<(u8, Vec<u8>)>(16);
        let v = publish_app_datagram(true, &tx, AuxChannel::AppStream as u8, vec![7]);
        assert_eq!(v["ok"], true);
        assert_eq!(v["delivered"], 0);
    }

    #[test]
    fn publish_is_refused_when_the_operator_disabled_the_aux_lane() {
        let (tx, mut sub) = broadcast::channel::<(u8, Vec<u8>)>(16);
        let v = publish_app_datagram(false, &tx, AuxChannel::AppStream as u8, vec![1]);
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "E_AUX_DISABLED");
        assert!(
            sub.try_recv().is_err(),
            "a refused publish must not reach the fan-out"
        );
    }

    #[test]
    fn send_is_refused_when_aux_is_disabled_or_the_pair_is_not_open() {
        // The dead-switch is checked before the datagram write: with
        // aux_enable=false a send refuses with E_AUX_DISABLED and never touches
        // the radio (the pure decision proves the short-circuit).
        let disabled = aux_disabled_reply(false).expect("disabled refuses the send");
        assert_eq!(disabled["ok"], false);
        assert_eq!(disabled["error"], "E_AUX_DISABLED");
        // With the dead-switch on the send proceeds to the open-state check
        // (which needs the live process group, covered on-rig).
        assert!(aux_disabled_reply(true).is_none());
    }

    #[test]
    fn bad_json_is_rejected_before_any_radio_access() {
        // A malformed line never becomes a Command, so the service replies
        // without ever locking the process group (and never starts the aux pair).
        let v = reply(b"not json");
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
    }

    #[test]
    fn unknown_op_is_rejected() {
        let v = reply(br#"{"op":"frob"}"#);
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_UNKNOWN_OP"));
    }

    #[test]
    fn open_with_aux_disabled_is_refused_before_any_radio_access() {
        // The operator dead-switch: with aux_enable=false the open is refused
        // with the distinct E_AUX_DISABLED error and never reaches the radio
        // group, so no wfb_tx/wfb_rx process is spawned. (The apply path locks
        // the process group only AFTER this check, which this pure decision
        // proves is short-circuited.)
        let disabled = aux_disabled_reply(false).expect("disabled refuses the open");
        assert_eq!(disabled["ok"], false);
        assert_eq!(disabled["error"], "E_AUX_DISABLED");
        // With the dead-switch on, the open is allowed to proceed.
        assert!(aux_disabled_reply(true).is_none());
    }

    #[test]
    fn an_empty_line_is_a_bad_request_not_a_panic() {
        // The framing strips the trailing newline before dispatch, so the handler
        // can hand an empty slice to the parser (EOF before any byte). It must be
        // a clean E_BAD_REQUEST, never a panic — and critically, never an open.
        let v = reply(b"");
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
    }

    /// A ground-station state whose uplink egress points at a loopback socket
    /// the test reads, standing in for the receive chain's aux `wfb_tx`.
    async fn uplink(aux_enable: bool) -> (AuxCmdState, tokio::net::UdpSocket) {
        let wire = tokio::net::UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let port = wire.local_addr().unwrap().port();
        let (tx, _) = broadcast::channel(16);
        let state = AuxCmdState::ground(port, aux_enable, tx).await.unwrap();
        (state, wire)
    }

    fn send_cmd(channel: AuxChannel, payload: &[u8]) -> Command {
        Command::Send {
            frame: aux_mux::encode(channel, payload).unwrap(),
        }
    }

    /// Nothing arrived at the stand-in transmitter within a short window.
    async fn assert_nothing_radiated(wire: &tokio::net::UdpSocket) {
        let mut buf = [0u8; 64];
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                wire.recv_from(&mut buf)
            )
            .await
            .is_err(),
            "nothing may reach the uplink"
        );
    }

    #[tokio::test]
    async fn the_ground_uplink_sends_only_between_open_and_close() {
        // The drone contract, kept on the ground: a send before open (or after
        // close) is E_AUX_NOT_OPEN, and an open admits the send, which reaches
        // the uplink ingress as the identical aux frame.
        let (state, wire) = uplink(true).await;
        let port = wire.local_addr().unwrap().port();
        let v = apply(send_cmd(AuxChannel::AppCommand, b"go"), &state).await;
        assert_eq!(v["error"], "E_AUX_NOT_OPEN");
        assert_nothing_radiated(&wire).await;

        let v = apply(Command::Open, &state).await;
        assert_eq!(v, json!({"ok": true, "active": true, "tx_port": port}));
        assert_eq!(apply(Command::Status, &state).await["active"], true);

        let v = apply(send_cmd(AuxChannel::AppCommand, b"go"), &state).await;
        assert_eq!(v, json!({"ok": true}));
        let mut buf = [0u8; 64];
        let (n, _) = wire.recv_from(&mut buf).await.unwrap();
        assert_eq!(
            &buf[..n],
            aux_mux::encode(AuxChannel::AppCommand, b"go")
                .unwrap()
                .as_slice()
        );

        assert_eq!(
            apply(Command::Close, &state).await,
            json!({"ok": true, "active": false})
        );
        let v = apply(send_cmd(AuxChannel::AppStream, b"x"), &state).await;
        assert_eq!(v["error"], "E_AUX_NOT_OPEN");
    }

    #[tokio::test]
    async fn the_ground_uplink_refuses_every_frame_that_is_not_application_traffic() {
        // The ground uplink also carries the drone's MAVLink, relay RPC, link
        // feedback and config tunnel. A socket client must never inject into
        // those planes, and a malformed frame must never be radiated.
        let (state, wire) = uplink(true).await;
        apply(Command::Open, &state).await;
        for channel in [
            AuxChannel::Mavlink,
            AuxChannel::Request,
            AuxChannel::LinkFeedback,
            AuxChannel::ConfigTunnel,
        ] {
            let v = apply(send_cmd(channel, &[0xFD, 0x09]), &state).await;
            assert_eq!(v["error"], "E_BAD_CHANNEL", "channel {}", channel as u8);
        }
        let mut truncated = aux_mux::encode(AuxChannel::AppStream, b"hello").unwrap();
        truncated.pop();
        let v = apply(Command::Send { frame: truncated }, &state).await;
        assert!(v["error"].as_str().unwrap().starts_with("E_BAD_FRAME"));
        let v = apply(
            Command::Send {
                frame: b"raw".to_vec(),
            },
            &state,
        )
        .await;
        assert!(v["error"].as_str().unwrap().starts_with("E_BAD_FRAME"));
        assert_nothing_radiated(&wire).await;
    }

    #[tokio::test]
    async fn the_ground_uplink_honours_the_operator_dead_switch() {
        let (state, wire) = uplink(false).await;
        assert_eq!(
            apply(Command::Open, &state).await["error"],
            "E_AUX_DISABLED"
        );
        let v = apply(send_cmd(AuxChannel::AppCommand, b"go"), &state).await;
        assert_eq!(v["error"], "E_AUX_DISABLED");
        assert_eq!(
            state.publish(AuxChannel::AppStream as u8, vec![1])["error"],
            "E_AUX_DISABLED"
        );
        assert_nothing_radiated(&wire).await;
    }
}
