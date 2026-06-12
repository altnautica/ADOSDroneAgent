//! Operator command socket for the WiFi-client uplink.
//!
//! Joining / forgetting an upstream WiFi network is an operator on-demand action
//! the REST layer drives. When the native `ados-net` daemon is the running
//! uplink owner the REST handler has no in-process Python manager to call (and
//! must NOT drive `nmcli` on `wlan0` itself, or it would race the daemon's own
//! WiFi-client manager for the radio). So it forwards each action to this socket
//! instead; the running service applies it through its [`WifiClientManager`],
//! the single owner of the `wlan0` AP/STA lock.
//!
//! Wire protocol (mirrors the radio command socket): one newline-terminated JSON
//! request, one newline-terminated JSON response per connection, then close.
//!
//! ```text
//! {"op":"wifi_join","ssid":"Net","passphrase":"secret","force":false}
//!     -> {"ok":true,"joined":true,"ip":"...","gateway":"...","error":null}
//! {"op":"wifi_forget","name":"Net"}
//!     -> {"ok":true,"forgot":true,"name":"Net","error":null}
//! {"op":"wifi_leave"}
//!     -> {"ok":true,"left":true,"previous_ssid":"Net"}
//! {"op":"wifi_status"}
//!     -> {"ok":true,"connected":true,"ssid":"Net","ip":"...",...}
//! ```
//!
//! A failed apply replies with `ok:false` and the manager's `error`, so the REST
//! layer can surface it. The socket only mutates the WiFi client it owns; it
//! never round-trips the on-disk config (the REST layer owns persistence).

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::managers::WifiClientManager;

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The shared WiFi-client manager the command handlers drive. The manager holds
/// the `wlan0` AP/STA advisory lock across a join's stop-hostapd→connect window,
/// so it is the SINGLE owner reached through this `Mutex`. Constructed once at
/// service start and shared with every accepted connection.
#[derive(Clone)]
pub struct CmdState {
    pub wifi: Arc<Mutex<WifiClientManager>>,
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    ssid: Option<String>,
    #[serde(default)]
    passphrase: Option<String>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    name: Option<String>,
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
    tracing::info!(path = %sock_path.display(), "wifi command socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, state).await {
                        tracing::debug!(error = %e, "wifi command conn error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "wifi command accept failed");
                // Brief backoff so a persistent accept error can't hot-spin.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Read one newline-terminated request, dispatch it, write one newline-
/// terminated response. Matches the radio command socket's framing.
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

/// A parsed + field-validated request, ready to apply. Parsing this OUT of the
/// raw bytes is pure (no manager access), so a malformed request is rejected
/// before the service ever locks the WiFi manager.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Join {
        ssid: String,
        passphrase: Option<String>,
        force: bool,
    },
    Forget {
        name: String,
    },
    Leave,
    Status,
}

/// The outcome of parsing a request line: an apply-ready [`Command`], or a
/// terminal response for a malformed/unknown request (so the caller can reply
/// without touching the manager).
enum Parsed {
    Cmd(Command),
    Reply(Value),
}

/// Parse + field-validate one request line. Pure: no manager access, no I/O,
/// fully unit-testable. A bad-JSON / missing-field / unknown-op request resolves
/// to a terminal [`Parsed::Reply`]; a well-formed request resolves to a command.
fn parse_command(line: &[u8]) -> Parsed {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => {
            return Parsed::Reply(json!({"ok": false, "error": format!("E_BAD_REQUEST: {e}")}))
        }
    };
    match req.op.as_str() {
        "wifi_join" => match req.ssid {
            Some(ssid) if !ssid.is_empty() => Parsed::Cmd(Command::Join {
                ssid,
                passphrase: req.passphrase.filter(|p| !p.is_empty()),
                force: req.force,
            }),
            _ => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_SSID"})),
        },
        "wifi_forget" => match req.name {
            Some(name) if !name.is_empty() => Parsed::Cmd(Command::Forget { name }),
            _ => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_NAME"})),
        },
        "wifi_leave" => Parsed::Cmd(Command::Leave),
        "wifi_status" => Parsed::Cmd(Command::Status),
        other => Parsed::Reply(json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")})),
    }
}

/// Parse + route one request to the WiFi manager. The parse half is pure
/// (covered by the `parse_command` tests); the apply half locks the manager,
/// which drives `nmcli`, so it is covered on-rig + by the manager's own tests.
async fn dispatch(line: &[u8], state: &CmdState) -> Value {
    let cmd = match parse_command(line) {
        Parsed::Cmd(c) => c,
        Parsed::Reply(v) => return v,
    };
    apply(cmd, state).await
}

/// Apply a validated command to the live WiFi-client manager. The `ok` flag is
/// derived from the manager's own success field so the REST forward client can
/// branch on it.
async fn apply(cmd: Command, state: &CmdState) -> Value {
    match cmd {
        Command::Join {
            ssid,
            passphrase,
            force,
        } => {
            let res = state
                .wifi
                .lock()
                .await
                .join(&ssid, passphrase.as_deref(), force)
                .await;
            with_ok(res)
        }
        Command::Forget { name } => {
            let res = state.wifi.lock().await.forget(&name).await;
            with_ok(res)
        }
        Command::Leave => {
            let res = state.wifi.lock().await.leave().await;
            with_ok(res)
        }
        Command::Status => {
            let st: Map<String, Value> = state.wifi.lock().await.status().await;
            let mut v = Value::Object(st);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("ok".to_string(), json!(true));
            }
            v
        }
    }
}

/// Stamp `ok:true` onto a processed manager result object. A processed command
/// always replies `ok:true` — its own success field (`joined`/`forgot`/`left`)
/// carries whether the action succeeded, which the REST layer inspects (a failed
/// join is a normal result with a 409/error path, NOT a transport error). Only a
/// parse/encode failure or a non-object manager result yields `ok:false`.
fn with_ok(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        obj.insert("ok".to_string(), json!(true));
        v
    } else {
        json!({"ok": false, "error": "E_BAD_MANAGER_RESULT"})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(line: &[u8]) -> Value {
        match parse_command(line) {
            Parsed::Reply(v) => v,
            Parsed::Cmd(c) => panic!("expected an early reply, got command {c:?}"),
        }
    }

    fn cmd(line: &[u8]) -> Command {
        match parse_command(line) {
            Parsed::Cmd(c) => c,
            Parsed::Reply(v) => panic!("expected a command, got reply {v}"),
        }
    }

    #[test]
    fn bad_json_is_rejected_before_any_manager_access() {
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
    fn join_requires_a_nonempty_ssid() {
        assert_eq!(reply(br#"{"op":"wifi_join"}"#)["error"], "E_MISSING_SSID");
        assert_eq!(
            reply(br#"{"op":"wifi_join","ssid":""}"#)["error"],
            "E_MISSING_SSID"
        );
        // An open network (no passphrase) parses; an empty passphrase normalises
        // to None so the manager treats it as open, not as a blank key.
        assert_eq!(
            cmd(br#"{"op":"wifi_join","ssid":"Open"}"#),
            Command::Join {
                ssid: "Open".to_string(),
                passphrase: None,
                force: false,
            }
        );
        assert_eq!(
            cmd(br#"{"op":"wifi_join","ssid":"Net","passphrase":"","force":true}"#),
            Command::Join {
                ssid: "Net".to_string(),
                passphrase: None,
                force: true,
            }
        );
        assert_eq!(
            cmd(br#"{"op":"wifi_join","ssid":"Net","passphrase":"pw"}"#),
            Command::Join {
                ssid: "Net".to_string(),
                passphrase: Some("pw".to_string()),
                force: false,
            }
        );
    }

    #[test]
    fn forget_requires_a_nonempty_name() {
        assert_eq!(reply(br#"{"op":"wifi_forget"}"#)["error"], "E_MISSING_NAME");
        assert_eq!(
            reply(br#"{"op":"wifi_forget","name":""}"#)["error"],
            "E_MISSING_NAME"
        );
        assert_eq!(
            cmd(br#"{"op":"wifi_forget","name":"Net"}"#),
            Command::Forget {
                name: "Net".to_string()
            }
        );
    }

    #[test]
    fn leave_and_status_parse_with_no_fields() {
        assert_eq!(cmd(br#"{"op":"wifi_leave"}"#), Command::Leave);
        assert_eq!(cmd(br#"{"op":"wifi_status"}"#), Command::Status);
    }

    #[test]
    fn with_ok_marks_a_processed_result_ok_and_preserves_fields() {
        // A processed command is ok:true regardless of the join outcome — the
        // `joined` field carries success, which the REST layer inspects.
        let joined = with_ok(json!({"joined": true, "ip": "1.2.3.4"}));
        assert_eq!(joined["ok"], true);
        assert_eq!(joined["ip"], "1.2.3.4");
        let failed = with_ok(json!({"joined": false, "error": "wlan0_busy_ap_active"}));
        assert_eq!(failed["ok"], true);
        assert_eq!(failed["error"], "wlan0_busy_ap_active");
        // A non-object result is a hard transport error.
        assert_eq!(with_ok(json!("oops"))["ok"], false);
    }

    #[test]
    fn an_empty_line_is_a_bad_request_not_a_panic() {
        let v = reply(b"");
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
    }
}
