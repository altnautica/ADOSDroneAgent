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
//! JSON request. `open` / `close` / `status` / `send` reply with one
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
//! {"op":"subscribe"}
//!     -> {"ok":true}
//!     -> {"channel":8,"payload":[104,101,108,108,111]}
//!     -> ...
//! ```
//!
//! A failed apply (a spawn failure on `open`) replies `{"ok":false,"error":"..."}`
//! and leaves the aux pair closed, so the host can surface the error. The socket
//! only mutates the aux pair it owns; it never round-trips the on-disk config.

use std::io;
use std::path::Path;
use std::sync::Arc;

use ados_protocol::ipc::{bind_command_socket, read_newline_line};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex};

use crate::config::WfbConfig;
use crate::process::RadioProcesses;

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The shared state the aux command handlers act on: the live process group (to
/// open / close the aux pair) and the boot config (the source of the effective
/// aux ports / FEC / MCS an `open` applies). The `proc` mutex is the SAME handle
/// the watchdogs + operator command socket hold, so this socket reaches the live
/// radio. Constructed once per bring-up and shared with every accepted
/// connection.
#[derive(Clone)]
pub struct AuxCmdState {
    pub proc: Arc<Mutex<RadioProcesses>>,
    pub cfg: Arc<WfbConfig>,
    /// Fan-out for application datagrams decoded off the aux-RX loopback (see
    /// [`crate::aux_rx`]). `subscribe` connections stream from this; each decoded
    /// `AppStream` / `AppCommand` frame is broadcast to every attached subscriber
    /// as `(channel as u8, payload)`. The same sender feeds the receive task.
    pub rx_tx: broadcast::Sender<(u8, Vec<u8>)>,
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    frame: Option<Vec<u8>>,
}

/// Bind the aux command socket and serve connections until the listener errors.
/// Run as its own task from the service main loop. The shared helper owns the
/// create-dir / remove-stale / bind / chmod (0660; root-owned, the api/plugin
/// host runs as root on target) hygiene. Each connection serves one request:
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
async fn accept_loop(listener: UnixListener, state: AuxCmdState) {
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
    if !state.cfg.aux_enable {
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

/// Apply a validated command to the live aux pair.
async fn apply(cmd: Command, state: &AuxCmdState) -> Value {
    match cmd {
        Command::Open => {
            // The operator dead-switch is checked first so a disabled deployment
            // gets a clear, distinct error rather than a generic open failure,
            // and so NO process is spawned (the lock + open below is never
            // reached). (`open_aux_stream` enforces the same guard structurally,
            // so a cap-holding caller can never open the stream when disabled.)
            if let Some(reply) = aux_disabled_reply(state.cfg.aux_enable) {
                return reply;
            }
            // Idempotent open: brings up the additive aux pair on the config's
            // effective aux ports/FEC/MCS. Never touches the data/control planes.
            if state.proc.lock().await.open_aux_stream(&state.cfg).await {
                json!({
                    "ok": true,
                    "active": true,
                    "tx_port": state.cfg.aux_tx_port,
                    "rx_port": state.cfg.aux_rx_port,
                })
            } else {
                json!({"ok": false, "error": "E_AUX_OPEN_FAILED"})
            }
        }
        Command::Close => {
            state.proc.lock().await.close_aux_stream().await;
            json!({"ok": true, "active": false})
        }
        Command::Status => {
            let active = state.proc.lock().await.aux_active();
            json!({"ok": true, "active": active})
        }
        Command::Send { frame } => {
            // The operator dead-switch is checked first (no datagram is written
            // while the aux lane is disabled by policy), and the pair must be
            // open — the stream exists only between an open and its close.
            if let Some(reply) = aux_disabled_reply(state.cfg.aux_enable) {
                return reply;
            }
            if !state.proc.lock().await.aux_active() {
                return json!({"ok": false, "error": "E_AUX_NOT_OPEN"});
            }
            // Write the already-aux-framed datagram to the local aux transmit
            // ingress; wfb_tx radiates it and the paired node's aux-rx re-emits
            // it to its own subscribers. A fresh socket per send (bound to an
            // ephemeral port) is dropped after the datagram — one-shot, like
            // open/close, with no per-send state.
            let sock = match tokio::net::UdpSocket::bind(("127.0.0.1", 0)).await {
                Ok(s) => s,
                Err(e) => return json!({"ok": false, "error": format!("E_AUX_SEND_UDP: {e}")}),
            };
            if let Err(e) = sock
                .send_to(&frame, ("127.0.0.1", state.cfg.aux_tx_port))
                .await
            {
                return json!({"ok": false, "error": format!("E_AUX_SEND: {e}")});
            }
            json!({"ok": true})
        }
    }
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
}
