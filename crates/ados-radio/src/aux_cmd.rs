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
//! JSON request, one newline-terminated JSON response per connection, then the
//! server closes.
//!
//! ```text
//! {"op":"open"}
//!     -> {"ok":true,"active":true,"tx_port":5602,"rx_port":5603}
//! {"op":"close"}
//!     -> {"ok":true,"active":false}
//! {"op":"status"}
//!     -> {"ok":true,"active":false}
//! ```
//!
//! A failed apply (a spawn failure on `open`) replies `{"ok":false,"error":"..."}`
//! and leaves the aux pair closed, so the host can surface the error. The socket
//! only mutates the aux pair it owns; it never round-trips the on-disk config.

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

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
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
}

/// Bind the aux command socket and serve requests until the listener errors. Run
/// as its own task from the service main loop. Removes a stale socket first and
/// chmods it 0660 (root-owned; the api/plugin host runs as root on target).
/// Returns only on a bind error; the accept loop never exits on the happy path.
pub async fn serve(state: AuxCmdState, sock_path: &Path) -> std::io::Result<()> {
    // A stale socket from a prior run makes bind() fail with EADDRINUSE.
    let _ = std::fs::remove_file(sock_path);
    if let Some(parent) = sock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = UnixListener::bind(sock_path)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o660));
    }
    tracing::info!(path = %sock_path.display(), "aux command socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, state).await {
                        tracing::debug!(error = %e, "aux command conn error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "aux command accept failed");
                // Brief backoff so a persistent accept error can't hot-spin.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Read one newline-terminated request, dispatch it, write one newline-
/// terminated response. Matches the operator command socket's framing.
async fn handle_conn(mut stream: UnixStream, state: AuxCmdState) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break; // EOF before newline — dispatch whatever we have.
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') || buf.len() > MAX_REQUEST_BYTES {
            break;
        }
    }
    let line = match buf.iter().position(|&b| b == b'\n') {
        Some(i) => &buf[..i],
        None => &buf[..],
    };
    let resp = dispatch(line, &state).await;
    let mut body = serde_json::to_vec(&resp)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec());
    body.push(b'\n');
    stream.write_all(&body).await?;
    stream.flush().await?;
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
}

/// The outcome of parsing a request line: an apply-ready [`Command`], or a
/// terminal response for a malformed/unknown request (so the caller can reply
/// without touching the radio).
enum Parsed {
    Cmd(Command),
    Reply(Value),
}

/// Parse + validate one request line. Pure: no radio access, no I/O, fully
/// unit-testable. A bad-JSON / unknown-op request resolves to a terminal
/// [`Parsed::Reply`]; a well-formed request resolves to a [`Command`].
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
        other => Parsed::Reply(json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")})),
    }
}

/// Parse + route one request to the radio state. The parse half is pure (covered
/// by the `parse_command` tests); the apply half locks the live process group,
/// which forks `wfb_tx`/`wfb_rx`, so it is covered on-rig + by the `process.rs`
/// open/close tests rather than here.
async fn dispatch(line: &[u8], state: &AuxCmdState) -> Value {
    let cmd = match parse_command(line) {
        Parsed::Cmd(c) => c,
        Parsed::Reply(v) => return v,
    };
    apply(cmd, state).await
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract the early-reply `Value`, or panic if the parse produced a command.
    fn reply(line: &[u8]) -> Value {
        match parse_command(line) {
            Parsed::Reply(v) => v,
            Parsed::Cmd(c) => panic!("expected an early reply, got command {c:?}"),
        }
    }

    /// Extract the apply-ready `Command`, or panic if the parse produced a reply.
    fn cmd(line: &[u8]) -> Command {
        match parse_command(line) {
            Parsed::Cmd(c) => c,
            Parsed::Reply(v) => panic!("expected a command, got reply {v}"),
        }
    }

    #[test]
    fn open_close_status_parse_to_commands() {
        assert_eq!(cmd(br#"{"op":"open"}"#), Command::Open);
        assert_eq!(cmd(br#"{"op":"close"}"#), Command::Close);
        assert_eq!(cmd(br#"{"op":"status"}"#), Command::Status);
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
