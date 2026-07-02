//! Operator command socket for a coordinated channel hop.
//!
//! The FHSS hop loop changes channel reactively (on link degradation) or on a
//! periodic scan; before this socket the native radio had no external command
//! surface, so an operator (or the REST layer) could not ask for a hop on
//! demand. This socket lets a caller trigger a single COORDINATED hop to a
//! requested channel through the existing announce + dwell-sync path, so the
//! ground station follows — it never bypasses the announce.
//!
//! Wire protocol (mirrors the operator knob socket framing): one
//! newline-terminated JSON request, one newline-terminated JSON response per
//! connection, then the server closes.
//!
//! ```text
//! {"op":"hop","channel":157}
//!     -> {"ok":true,"channel":157}        (validation passed; the coordinated
//!        hop has been initiated — it announces, waits for the GS ack, then
//!        commits the channel)
//!     -> {"ok":false,"error":"invalid channel"}
//!     -> {"ok":false,"error":"not paired"}
//!     -> {"ok":false,"error":"no peer"}
//!     -> {"ok":false,"error":"mid-bind"}
//!     -> {"ok":false,"error":"unavailable"}  (the hop supervisor is not
//!        running — auto-hop fully disabled — or it shut down before replying)
//! {"op":"status"}
//!     -> {"ok":true,"channel":149}        (the current operating channel)
//! ```
//!
//! The socket only validates the channel FORMAT (a known WFB channel) itself;
//! the paired / peer-present / not-mid-bind checks live with the hop loop (which
//! owns that state) and ride back on the oneshot reply. The reply is sent the
//! moment validation completes — BEFORE the multi-second announce — so the
//! socket connection never blocks on the air handshake.

use std::path::Path;

use ados_protocol::ipc::{bind_command_socket, serve_rpc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// A validated manual-hop request handed to the hop supervisor. The supervisor
/// runs the paired / peer / mid-bind checks, sends the verdict back on `reply`
/// (before the announce), and on acceptance drives the coordinated hop.
#[derive(Debug)]
pub struct ManualHopRequest {
    /// The requested target channel (already format-validated as a known WFB
    /// channel by the socket; the supervisor still re-asserts in-band membership
    /// against the regulatory-enabled set it holds).
    pub channel: u8,
    /// One-shot reply the supervisor fills with the accept/reject verdict.
    pub reply: oneshot::Sender<HopVerdict>,
}

/// The hop supervisor's verdict on a manual-hop request. Carried back on the
/// request's oneshot so the socket can render the wire reply without reaching
/// into supervisor state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HopVerdict {
    /// Validation passed; the coordinated hop to `channel` has been initiated.
    Accepted { channel: u8 },
    /// Rejected with a stable machine-readable reason.
    Rejected { reason: &'static str },
}

impl HopVerdict {
    fn to_reply(&self) -> Value {
        match self {
            HopVerdict::Accepted { channel } => json!({"ok": true, "channel": channel}),
            HopVerdict::Rejected { reason } => json!({"ok": false, "error": reason}),
        }
    }
}

/// Shared read-only state the socket needs to answer a `status` request without
/// a round-trip to the hop supervisor: the live operating channel (the same
/// atomic the heartbeat reads). The hop-request channel forwards `hop` ops to
/// the supervisor.
#[derive(Clone)]
pub struct CmdState {
    /// Forward validated manual-hop requests to the hop supervisor.
    pub hop_tx: mpsc::Sender<ManualHopRequest>,
    /// The live operating channel (rendezvous home until a hop commits a move).
    pub operating_channel: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    channel: Option<u8>,
}

/// A request that has been parsed + format-validated and is ready to route. The
/// channel-format rejection happens here (pure), before the supervisor is asked.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// A coordinated hop to a known WFB channel.
    Hop { channel: u8 },
    /// Report the current operating channel.
    Status,
}

/// The outcome of parsing a request line: a routed [`Command`], or a terminal
/// reply for a malformed/unknown/out-of-set request (answered without touching
/// the supervisor).
enum Parsed {
    Cmd(Command),
    Reply(Value),
}

/// True when `channel` is one of the standard WFB channels. The hop target must
/// be a known channel before the request is ever forwarded; the supervisor then
/// re-asserts in-band/regulatory membership against the live enabled set.
fn is_known_channel(channel: u8) -> bool {
    crate::channel::STANDARD_CHANNELS
        .iter()
        .any(|(c, _)| *c == channel)
}

/// Parse + format-validate one request line. Pure: no socket/supervisor access,
/// fully unit-testable. Bad JSON, an unknown op, a missing channel, or a channel
/// that is not a known WFB channel resolve to a terminal [`Parsed::Reply`].
fn parse_command(line: &[u8]) -> Parsed {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => {
            return Parsed::Reply(json!({"ok": false, "error": format!("E_BAD_REQUEST: {e}")}))
        }
    };
    match req.op.as_str() {
        "hop" => match req.channel {
            Some(channel) if is_known_channel(channel) => Parsed::Cmd(Command::Hop { channel }),
            Some(_) => Parsed::Reply(json!({"ok": false, "error": "invalid channel"})),
            None => Parsed::Reply(json!({"ok": false, "error": "invalid channel"})),
        },
        "status" => Parsed::Cmd(Command::Status),
        other => Parsed::Reply(json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")})),
    }
}

/// Bind the command socket and serve one-shot requests until the listener
/// errors. Run as its own task from the service main loop. The shared helper
/// owns the create-dir / remove-stale / bind / chmod hygiene; [`set_socket_perms`]
/// then hands group ownership to the `ados` group so a non-root operator (the API
/// service) can write it. Each connection is one newline-terminated request ->
/// one newline-terminated response (the trailing newline is added by the shared
/// serve loop; the handler returns the response body).
pub async fn serve(state: CmdState, sock_path: &Path) -> std::io::Result<()> {
    let listener = bind_command_socket(sock_path, 0o660)?;
    set_socket_perms(sock_path);
    tracing::info!(path = %sock_path.display(), "radio command socket listening");

    serve_rpc(listener, MAX_REQUEST_BYTES, move |req: Vec<u8>| {
        let state = state.clone();
        async move {
            let resp = dispatch(&req, &state).await;
            serde_json::to_vec(&resp)
                .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec())
        }
    })
    .await;
    Ok(())
}

/// 0o660 + group-own to `ados` so a non-root operator in that group can reach
/// the trusted local plane. The mode only grants the group once the group
/// actually owns the file, so both steps are required. Best-effort: an absent
/// group (a dev host) is a quiet no-op. Linux-only.
#[cfg(target_os = "linux")]
fn set_socket_perms(sock_path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o660));
    match nix::unistd::Group::from_name("ados") {
        Ok(Some(g)) => {
            if let Err(err) = nix::unistd::chown(sock_path, None, Some(g.gid)) {
                tracing::debug!(error = %err, path = %sock_path.display(), "chgrp radio command socket failed");
            }
        }
        Ok(None) => tracing::debug!("ados group not present; leaving socket group as-is"),
        Err(err) => tracing::debug!(error = %err, "resolving ados group failed"),
    }
}

/// Non-Linux: socket group ownership is a Linux-only concern (the service runs
/// on the target SBC). A no-op so the dev-host build links.
#[cfg(not(target_os = "linux"))]
fn set_socket_perms(_sock_path: &Path) {}

/// Parse + route one request. The parse half is pure (covered by tests); a
/// `hop` is forwarded to the supervisor and the verdict awaited, a `status`
/// reads the shared operating channel directly.
async fn dispatch(line: &[u8], state: &CmdState) -> Value {
    match parse_command(line) {
        Parsed::Reply(v) => v,
        Parsed::Cmd(Command::Status) => {
            let ch = state
                .operating_channel
                .load(std::sync::atomic::Ordering::Relaxed) as u8;
            json!({"ok": true, "channel": ch})
        }
        Parsed::Cmd(Command::Hop { channel }) => request_hop(channel, state).await,
    }
}

/// Forward a validated hop request to the supervisor and await its verdict.
///
/// The supervisor runs the paired / peer / mid-bind checks and sends the verdict
/// back on the oneshot BEFORE starting the multi-second announce, so this await
/// resolves quickly. A closed channel (the hop supervisor is not running, e.g.
/// auto-hop fully disabled) or a dropped reply both surface as `unavailable`
/// rather than hanging the connection.
async fn request_hop(channel: u8, state: &CmdState) -> Value {
    let (reply_tx, reply_rx) = oneshot::channel();
    let req = ManualHopRequest {
        channel,
        reply: reply_tx,
    };
    // `try_send` (not `send`) so a stuck/absent supervisor never blocks the
    // socket task: a full or closed channel reports `unavailable` at once.
    if state.hop_tx.try_send(req).is_err() {
        return json!({"ok": false, "error": "unavailable"});
    }
    match reply_rx.await {
        Ok(verdict) => verdict.to_reply(),
        // The supervisor dropped the reply without answering (it shut down
        // between accept and reply); report unavailable rather than hang.
        Err(_) => json!({"ok": false, "error": "unavailable"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract the early-reply `Value`, or panic if the parse produced a routed
    /// command instead.
    fn reply(line: &[u8]) -> Value {
        match parse_command(line) {
            Parsed::Reply(v) => v,
            Parsed::Cmd(c) => panic!("expected an early reply, got command {c:?}"),
        }
    }

    /// Extract the routed `Command`, or panic if the parse produced an early
    /// reply.
    fn cmd(line: &[u8]) -> Command {
        match parse_command(line) {
            Parsed::Cmd(c) => c,
            Parsed::Reply(v) => panic!("expected a command, got reply {v}"),
        }
    }

    #[test]
    fn bad_json_is_rejected_before_any_supervisor_access() {
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
    fn hop_requires_a_channel() {
        // No channel field → the invalid-channel reply (never a routed Hop, so
        // the supervisor is never asked).
        assert_eq!(reply(br#"{"op":"hop"}"#)["error"], "invalid channel");
    }

    #[test]
    fn hop_rejects_a_non_wfb_channel() {
        // 6 is not a standard WFB channel → rejected at parse time.
        assert_eq!(
            reply(br#"{"op":"hop","channel":6}"#)["error"],
            "invalid channel"
        );
        // 200 is out of the standard set too.
        assert_eq!(
            reply(br#"{"op":"hop","channel":200}"#)["error"],
            "invalid channel"
        );
    }

    #[test]
    fn hop_accepts_each_known_wfb_channel() {
        // Every standard channel parses to a routed Hop carrying it verbatim.
        for (channel, _freq) in crate::channel::STANDARD_CHANNELS {
            let line = format!(r#"{{"op":"hop","channel":{channel}}}"#);
            assert_eq!(cmd(line.as_bytes()), Command::Hop { channel: *channel });
        }
    }

    #[test]
    fn status_parses_to_the_status_command() {
        assert_eq!(cmd(br#"{"op":"status"}"#), Command::Status);
    }

    #[test]
    fn is_known_channel_matches_the_standard_set() {
        // The U-NII-3 home channels and a U-NII-1 channel are known.
        assert!(is_known_channel(149));
        assert!(is_known_channel(165));
        assert!(is_known_channel(36));
        // A 2.4 GHz / non-standard channel is not.
        assert!(!is_known_channel(6));
        assert!(!is_known_channel(0));
    }

    #[test]
    fn an_empty_line_is_a_bad_request_not_a_panic() {
        let v = reply(b"");
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
    }

    #[test]
    fn verdict_renders_accept_and_reject_wire_shapes() {
        assert_eq!(
            HopVerdict::Accepted { channel: 157 }.to_reply(),
            json!({"ok": true, "channel": 157})
        );
        assert_eq!(
            HopVerdict::Rejected { reason: "no peer" }.to_reply(),
            json!({"ok": false, "error": "no peer"})
        );
    }

    /// A closed hop channel (the supervisor is not running) resolves `hop` to
    /// `unavailable` rather than hanging the connection.
    #[tokio::test]
    async fn hop_on_a_closed_channel_reports_unavailable() {
        let (hop_tx, hop_rx) = mpsc::channel::<ManualHopRequest>(1);
        // Drop the receiver so the channel is closed: try_send must fail.
        drop(hop_rx);
        let state = CmdState {
            hop_tx,
            operating_channel: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(149)),
        };
        let v = dispatch(br#"{"op":"hop","channel":157}"#, &state).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "unavailable");
    }

    /// The supervisor answers a forwarded request on the oneshot; the socket
    /// renders that verdict. Drives the full forward → verdict → wire path.
    #[tokio::test]
    async fn hop_forwards_to_the_supervisor_and_returns_its_verdict() {
        let (hop_tx, mut hop_rx) = mpsc::channel::<ManualHopRequest>(1);
        let state = CmdState {
            hop_tx,
            operating_channel: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(149)),
        };
        // Stand in for the supervisor: receive the request, assert the channel,
        // and answer Accepted on the oneshot.
        let supervisor = tokio::spawn(async move {
            let req = hop_rx.recv().await.expect("request forwarded");
            assert_eq!(req.channel, 157);
            req.reply
                .send(HopVerdict::Accepted { channel: 157 })
                .expect("reply sent");
        });
        let v = dispatch(br#"{"op":"hop","channel":157}"#, &state).await;
        assert_eq!(v, json!({"ok": true, "channel": 157}));
        supervisor.await.unwrap();
    }

    /// A `status` op reads the shared operating channel directly (no supervisor
    /// round-trip).
    #[tokio::test]
    async fn status_reports_the_live_operating_channel() {
        let (hop_tx, _hop_rx) = mpsc::channel::<ManualHopRequest>(1);
        let state = CmdState {
            hop_tx,
            operating_channel: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(161)),
        };
        let v = dispatch(br#"{"op":"status"}"#, &state).await;
        assert_eq!(v, json!({"ok": true, "channel": 161}));
    }
}
