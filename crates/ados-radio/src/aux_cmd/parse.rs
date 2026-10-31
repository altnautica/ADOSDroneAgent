//! Request parsing and validation.

use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct Request {
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

/// A request that has been parsed + validated and is ready to apply.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
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
pub(super) enum Parsed {
    Cmd(Command),
    Reply(Value),
    Subscribe,
}

/// Parse + validate one request line. Pure: no radio access, no I/O, fully
/// unit-testable. A bad-JSON / unknown-op request resolves to a terminal
/// [`Parsed::Reply`]; a well-formed request resolves to a [`Command`] or
/// [`Parsed::Subscribe`].
pub(super) fn parse_command(line: &[u8]) -> Parsed {
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
pub(super) fn aux_disabled_reply(aux_enable: bool) -> Option<Value> {
    if aux_enable {
        None
    } else {
        Some(json!({"ok": false, "error": "E_AUX_DISABLED"}))
    }
}
