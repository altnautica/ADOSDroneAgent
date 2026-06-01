//! Operator command socket for the live radio knobs.
//!
//! The data plane's FEC ratio, MCS index, TX power, and the auto/manual link
//! tier are operator-facing knobs the REST layer drives. When the native radio
//! is the running transmit plane the REST handler has no Python manager to call,
//! so it forwards each knob change to this socket instead; the running service
//! applies it to the live [`RadioProcesses`] (and, for the tier toggle, to the
//! adaptive controller).
//!
//! Wire protocol (mirrors the supervisor control socket): one newline-terminated
//! JSON request, one newline-terminated JSON response per connection, then the
//! server closes.
//!
//! ```text
//! {"op":"set_fec","fec_k":8,"fec_n":12}
//!     -> {"ok":true,"fec_k":8,"fec_n":12}
//! {"op":"set_mcs","mcs_index":3}
//!     -> {"ok":true,"mcs_index":3}
//! {"op":"set_tx_power","tx_power_dbm":10}
//!     -> {"ok":true,"effective_dbm":10}   (effective can ramp UP from a
//!        rejected low request; null when every step was rejected)
//! {"op":"set_tier","mode":"auto"}
//!     -> {"ok":true,"mode":"auto","adaptive_bitrate_enabled":true}
//! {"op":"set_tier","mode":"manual","mcs_index":3,"fec_k":8,"fec_n":10}
//!     -> {"ok":true,"mode":"manual","mcs_index":3,"fec_k":8,"fec_n":10}
//! {"op":"status"}
//!     -> {"ok":true,"fec_k":..,"fec_n":..,"mcs_index":..,
//!         "adaptive_bitrate_enabled":..}
//! ```
//!
//! A failed apply (an invalid ratio/index, or a respawn failure) replies
//! `{"ok":false,"error":"..."}` and leaves the running tunables unchanged, so the
//! REST layer can surface the error and fall back to persisting the operator's
//! preference. The socket only mutates the radio it owns; it never round-trips
//! the on-disk config (the REST layer owns persistence).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::process::RadioProcesses;

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The shared radio state the command handlers mutate: the live process group
/// (for the FEC/MCS/TX-power/manual-tier knobs) and the adaptive-controller
/// enable flag (for the auto/manual toggle). Both outlive a single radio
/// bring-up — the `proc` mutex is swapped in place on a respawn and the flag is
/// read by the bitrate controller each tick — so this handle is constructed once
/// at service start and shared with every accepted connection.
#[derive(Clone)]
pub struct CmdState {
    pub proc: Arc<Mutex<RadioProcesses>>,
    pub adaptive_enabled: Arc<AtomicBool>,
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    fec_k: Option<u8>,
    #[serde(default)]
    fec_n: Option<u8>,
    #[serde(default)]
    mcs_index: Option<u8>,
    #[serde(default)]
    tx_power_dbm: Option<i8>,
    #[serde(default)]
    mode: Option<String>,
}

/// Bind the command socket and serve requests until the listener errors. Run as
/// its own task from the service main loop. Removes a stale socket first and
/// chmods it 0660 (root-owned; the api service runs as root on target). Returns
/// only on a bind error; the accept loop never exits on the happy path.
pub async fn serve(state: CmdState, sock_path: &Path) -> std::io::Result<()> {
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
    tracing::info!(path = %sock_path.display(), "wfb command socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, state).await {
                        tracing::debug!(error = %e, "wfb command conn error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "wfb command accept failed");
                // Brief backoff so a persistent accept error can't hot-spin.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Read one newline-terminated request, dispatch it, write one newline-
/// terminated response. Matches the supervisor control socket's framing.
async fn handle_conn(mut stream: UnixStream, state: CmdState) -> std::io::Result<()> {
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

/// A request that has been parsed + field-validated and is ready to apply to
/// the radio. Parsing this OUT of the raw bytes is pure (no radio access), so
/// every malformed-request rejection happens before the service ever locks the
/// process group.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    SetFec {
        fec_k: u8,
        fec_n: u8,
    },
    SetMcs {
        mcs_index: u8,
    },
    SetTxPower {
        tx_power_dbm: i8,
    },
    /// Re-arm the adaptive controller (it resumes stepping FEC on link quality).
    TierAuto,
    /// Hold the controller off and pin the operator's trio onto the data plane.
    TierManual {
        mcs_index: u8,
        fec_k: u8,
        fec_n: u8,
    },
    Status,
}

/// The outcome of parsing a request line: an apply-ready [`Command`], or a
/// terminal response for a malformed/unknown request (so the caller can reply
/// without touching the radio).
enum Parsed {
    Cmd(Command),
    Reply(Value),
}

/// Parse + field-validate one request line. Pure: no radio access, no I/O, fully
/// unit-testable. A bad-JSON / missing-field / unknown-op request resolves to a
/// terminal [`Parsed::Reply`]; a well-formed request resolves to a [`Command`].
/// Numeric range validation (FEC ratio, MCS range) is deferred to the setters so
/// the error vocabulary stays identical to the packaged manager's.
fn parse_command(line: &[u8]) -> Parsed {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => {
            return Parsed::Reply(json!({"ok": false, "error": format!("E_BAD_REQUEST: {e}")}))
        }
    };
    match req.op.as_str() {
        "set_fec" => match (req.fec_k, req.fec_n) {
            (Some(fec_k), Some(fec_n)) => Parsed::Cmd(Command::SetFec { fec_k, fec_n }),
            _ => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_FEC"})),
        },
        "set_mcs" => match req.mcs_index {
            Some(mcs_index) => Parsed::Cmd(Command::SetMcs { mcs_index }),
            None => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_MCS"})),
        },
        "set_tx_power" => match req.tx_power_dbm {
            Some(tx_power_dbm) => Parsed::Cmd(Command::SetTxPower { tx_power_dbm }),
            None => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_TX_POWER"})),
        },
        "set_tier" => match req.mode.as_deref() {
            Some("auto") => Parsed::Cmd(Command::TierAuto),
            Some("manual") => match (req.mcs_index, req.fec_k, req.fec_n) {
                (Some(mcs_index), Some(fec_k), Some(fec_n)) => Parsed::Cmd(Command::TierManual {
                    mcs_index,
                    fec_k,
                    fec_n,
                }),
                _ => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_MANUAL_TIER"})),
            },
            Some(other) => {
                Parsed::Reply(json!({"ok": false, "error": format!("E_BAD_TIER_MODE: {other}")}))
            }
            None => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_TIER_MODE"})),
        },
        "status" => Parsed::Cmd(Command::Status),
        other => Parsed::Reply(json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")})),
    }
}

/// Parse + route one request to the radio state. The parse half is pure (covered
/// by the `parse_command` tests); the apply half locks the live process group,
/// which forks `wfb_tx`, so it is covered on-rig + by the `process.rs` setter
/// tests rather than here.
async fn dispatch(line: &[u8], state: &CmdState) -> Value {
    let cmd = match parse_command(line) {
        Parsed::Cmd(c) => c,
        Parsed::Reply(v) => return v,
    };
    apply(cmd, state).await
}

/// Apply a validated command to the live radio + adaptive controller.
///
/// The auto/manual tier toggle flips the enable flag BEFORE the manual trio is
/// applied so the controller does not race the pinned values back on its next
/// tick.
async fn apply(cmd: Command, state: &CmdState) -> Value {
    match cmd {
        Command::SetFec { fec_k, fec_n } => {
            if state.proc.lock().await.set_fec(fec_k, fec_n).await {
                json!({"ok": true, "fec_k": fec_k, "fec_n": fec_n})
            } else {
                json!({"ok": false, "error": "E_SET_FEC_FAILED"})
            }
        }
        Command::SetMcs { mcs_index } => {
            if state.proc.lock().await.set_mcs(mcs_index).await {
                json!({"ok": true, "mcs_index": mcs_index})
            } else {
                json!({"ok": false, "error": "E_SET_MCS_FAILED"})
            }
        }
        Command::SetTxPower { tx_power_dbm } => {
            // TX power retunes the live adapter in place (no respawn). A driver
            // that rejects every ramp step yields null; the REST layer still
            // persists the operator's preference on that path.
            let effective = state.proc.lock().await.apply_tx_power(tx_power_dbm).await;
            json!({"ok": true, "effective_dbm": effective})
        }
        Command::TierAuto => {
            state.adaptive_enabled.store(true, Ordering::Relaxed);
            json!({"ok": true, "mode": "auto", "adaptive_bitrate_enabled": true})
        }
        Command::TierManual {
            mcs_index,
            fec_k,
            fec_n,
        } => {
            // Disable the controller first so it does not contend with the pin.
            state.adaptive_enabled.store(false, Ordering::Relaxed);
            if state
                .proc
                .lock()
                .await
                .set_manual_tier(mcs_index, fec_k, fec_n)
                .await
            {
                json!({"ok": true, "mode": "manual", "mcs_index": mcs_index, "fec_k": fec_k, "fec_n": fec_n})
            } else {
                json!({"ok": false, "error": "E_SET_MANUAL_TIER_FAILED"})
            }
        }
        Command::Status => {
            let (fec_k, fec_n, mcs) = {
                let p = state.proc.lock().await;
                let (k, n) = p.data_fec();
                (k, n, p.data_mcs())
            };
            json!({
                "ok": true,
                "fec_k": fec_k,
                "fec_n": fec_n,
                "mcs_index": mcs,
                "adaptive_bitrate_enabled": state.adaptive_enabled.load(Ordering::Relaxed),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract the early-reply `Value` from a parse, or panic if the parse
    /// produced an apply-ready command instead.
    fn reply(line: &[u8]) -> Value {
        match parse_command(line) {
            Parsed::Reply(v) => v,
            Parsed::Cmd(c) => panic!("expected an early reply, got command {c:?}"),
        }
    }

    /// Extract the apply-ready `Command`, or panic if the parse produced an
    /// early reply.
    fn cmd(line: &[u8]) -> Command {
        match parse_command(line) {
            Parsed::Cmd(c) => c,
            Parsed::Reply(v) => panic!("expected a command, got reply {v}"),
        }
    }

    #[test]
    fn bad_json_is_rejected_before_any_radio_access() {
        // A malformed line never becomes a Command, so the service replies
        // without ever locking the process group.
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
    fn set_fec_requires_both_shards() {
        // Missing fec_n → early reply (not a Command), so no respawn is risked on
        // a half-specified ratio.
        assert_eq!(
            reply(br#"{"op":"set_fec","fec_k":8}"#)["error"],
            "E_MISSING_FEC"
        );
        assert_eq!(
            reply(br#"{"op":"set_fec","fec_n":12}"#)["error"],
            "E_MISSING_FEC"
        );
        // Both present → an apply-ready SetFec carrying the ratio verbatim. Range
        // validation is the setter's job (so the error vocabulary matches Python).
        assert_eq!(
            cmd(br#"{"op":"set_fec","fec_k":8,"fec_n":12}"#),
            Command::SetFec {
                fec_k: 8,
                fec_n: 12
            }
        );
    }

    #[test]
    fn set_mcs_requires_the_index() {
        assert_eq!(reply(br#"{"op":"set_mcs"}"#)["error"], "E_MISSING_MCS");
        assert_eq!(
            cmd(br#"{"op":"set_mcs","mcs_index":3}"#),
            Command::SetMcs { mcs_index: 3 }
        );
    }

    #[test]
    fn set_tx_power_requires_the_dbm_and_accepts_negative() {
        assert_eq!(
            reply(br#"{"op":"set_tx_power"}"#)["error"],
            "E_MISSING_TX_POWER"
        );
        assert_eq!(
            cmd(br#"{"op":"set_tx_power","tx_power_dbm":10}"#),
            Command::SetTxPower { tx_power_dbm: 10 }
        );
        // A signed dBm parses (the field is i8, so a negative request is valid).
        assert_eq!(
            cmd(br#"{"op":"set_tx_power","tx_power_dbm":-5}"#),
            Command::SetTxPower { tx_power_dbm: -5 }
        );
    }

    #[test]
    fn set_tier_auto_parses_with_no_trio() {
        // The auto toggle needs no other fields; it re-arms the controller.
        assert_eq!(
            cmd(br#"{"op":"set_tier","mode":"auto"}"#),
            Command::TierAuto
        );
    }

    #[test]
    fn set_tier_manual_requires_the_full_trio() {
        // A partial trio is rejected before the radio is touched.
        assert_eq!(
            reply(br#"{"op":"set_tier","mode":"manual","mcs_index":3}"#)["error"],
            "E_MISSING_MANUAL_TIER"
        );
        assert_eq!(
            reply(br#"{"op":"set_tier","mode":"manual","mcs_index":3,"fec_k":8}"#)["error"],
            "E_MISSING_MANUAL_TIER"
        );
        // The complete trio parses verbatim; range checks land in the setter.
        assert_eq!(
            cmd(br#"{"op":"set_tier","mode":"manual","mcs_index":5,"fec_k":8,"fec_n":10}"#),
            Command::TierManual {
                mcs_index: 5,
                fec_k: 8,
                fec_n: 10
            }
        );
    }

    #[test]
    fn set_tier_unknown_or_missing_mode_is_rejected() {
        assert!(reply(br#"{"op":"set_tier","mode":"turbo"}"#)["error"]
            .as_str()
            .unwrap()
            .starts_with("E_BAD_TIER_MODE"));
        assert_eq!(
            reply(br#"{"op":"set_tier"}"#)["error"],
            "E_MISSING_TIER_MODE"
        );
    }

    #[test]
    fn status_parses_to_the_status_command() {
        assert_eq!(cmd(br#"{"op":"status"}"#), Command::Status);
    }

    #[test]
    fn an_empty_line_is_a_bad_request_not_a_panic() {
        // The framing strips the trailing newline before dispatch, so the
        // handler can hand an empty slice to the parser (EOF before any byte).
        // It must be a clean E_BAD_REQUEST, never a panic.
        let v = reply(b"");
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
    }
}
