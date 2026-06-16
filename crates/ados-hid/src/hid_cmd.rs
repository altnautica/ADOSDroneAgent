//! Operator command socket for the input-device daemon.
//!
//! Selecting the primary gamepad is an operator on-demand write the REST layer
//! drives. The `ados-input` daemon owns the running [`crate::input::HotplugTracker`]
//! (the live primary the hotplug poll consults so it does not re-promote a
//! different device). When the native front owns the LAN port it has no in-process
//! tracker to call, and writing only the on-disk sidecar would leave the running
//! daemon's primary stale until its next restart. So the front forwards the write
//! to this socket; the running daemon applies it through its tracker (the single
//! owner of the running primary) and persists the sidecar, keeping the live state
//! and the on-disk record in lockstep — the exact two things the Python
//! `InputManager.set_primary` does (update the in-process singleton + persist).
//!
//! Wire protocol (mirrors the radio + WiFi command sockets): one newline-terminated
//! JSON request, one newline-terminated JSON response per connection, then close.
//!
//! ```text
//! {"op":"set_primary","device_id":"usb:045e:028e:event3"}
//!     -> {"ok":true,"primary_id":"usb:045e:028e:event3"}
//! {"op":"get_primary"}
//!     -> {"ok":true,"primary_id":"usb:045e:028e:event3"}   (or null when unset)
//! {"op":"clear_primary"}
//!     -> {"ok":true,"primary_id":null}
//! ```
//!
//! `clear_primary` drops the running primary (and persists the cleared sidecar),
//! used when the selected device is forgotten (a paired Bluetooth controller is
//! removed): the Python `forget_bluetooth` drops `self._primary` when it pointed
//! at the forgotten device, so the running tracker must drop it too.
//!
//! A malformed / unknown request replies `ok:false` with an `error` code, parsed
//! out of the bytes before the tracker is ever locked. A persist fault on
//! `set_primary` is non-fatal: the running primary is updated and the reply is
//! still `ok:true` (the running state is the authority; the sidecar is the
//! durable mirror), with a `persist_error` field so the caller can surface it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::input::HotplugTracker;

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The shared state the command handlers drive: the running hotplug tracker (the
/// single owner of the live primary) and the on-disk input-selection sidecar path
/// the tracker persists the primary to.
#[derive(Clone)]
pub struct CmdState {
    pub tracker: Arc<Mutex<HotplugTracker>>,
    pub sidecar_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    device_id: Option<String>,
}

/// Bind the command socket and serve requests until the listener errors. Run as
/// its own task from the daemon main loop. Removes a stale socket first and chmods
/// it 0660 (root-owned; the api service runs as root on target). Returns only on a
/// bind error; the accept loop never exits on the happy path.
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
    tracing::info!(path = %sock_path.display(), "input command socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, state).await {
                        tracing::debug!(error = %e, "input command conn error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "input command accept failed");
                // Brief backoff so a persistent accept error can't hot-spin.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Read one newline-terminated request, dispatch it, write one newline-terminated
/// response. Matches the sibling command sockets' framing.
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

/// A parsed + field-validated request. Parsing this OUT of the raw bytes is pure
/// (no tracker access), so a malformed request is rejected before the daemon ever
/// locks the tracker.
///
/// The shared `Primary` suffix is deliberate: this socket exists solely to drive
/// the primary-gamepad selection, so each variant naming the thing it acts on
/// reads clearer at the call site than a bare `Set` / `Get` / `Clear`.
#[derive(Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
enum Command {
    SetPrimary { device_id: String },
    GetPrimary,
    ClearPrimary,
}

/// The outcome of parsing a request line: an apply-ready [`Command`], or a
/// terminal response for a malformed/unknown request.
enum Parsed {
    Cmd(Command),
    Reply(Value),
}

/// Parse + field-validate one request line. Pure: no tracker access, no I/O. A
/// bad-JSON / missing-field / unknown-op request resolves to a terminal
/// [`Parsed::Reply`]; a well-formed request resolves to a command.
fn parse_command(line: &[u8]) -> Parsed {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => {
            return Parsed::Reply(json!({"ok": false, "error": format!("E_BAD_REQUEST: {e}")}))
        }
    };
    match req.op.as_str() {
        "set_primary" => match req.device_id {
            Some(id) if !id.is_empty() => Parsed::Cmd(Command::SetPrimary { device_id: id }),
            _ => Parsed::Reply(json!({"ok": false, "error": "E_MISSING_DEVICE_ID"})),
        },
        "get_primary" => Parsed::Cmd(Command::GetPrimary),
        "clear_primary" => Parsed::Cmd(Command::ClearPrimary),
        other => Parsed::Reply(json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")})),
    }
}

/// Parse + route one request to the tracker. The parse half is pure (covered by
/// the `parse_command` tests); the apply half locks the tracker + writes the
/// sidecar.
async fn dispatch(line: &[u8], state: &CmdState) -> Value {
    let cmd = match parse_command(line) {
        Parsed::Cmd(c) => c,
        Parsed::Reply(v) => return v,
    };
    apply(cmd, state).await
}

/// Apply a validated command to the running tracker. `set_primary` updates the
/// live primary then persists the sidecar (a persist fault is non-fatal — the
/// running state is the authority); `get_primary` reads the current primary.
async fn apply(cmd: Command, state: &CmdState) -> Value {
    match cmd {
        Command::SetPrimary { device_id } => {
            let mut tracker = state.tracker.lock().await;
            tracker.set_primary(device_id.clone());
            let persist = tracker.save_primary(&state.sidecar_path);
            match persist {
                Ok(()) => json!({"ok": true, "primary_id": device_id}),
                Err(e) => json!({
                    "ok": true,
                    "primary_id": device_id,
                    "persist_error": e.to_string(),
                }),
            }
        }
        Command::GetPrimary => {
            let tracker = state.tracker.lock().await;
            json!({"ok": true, "primary_id": tracker.primary()})
        }
        Command::ClearPrimary => {
            let mut tracker = state.tracker.lock().await;
            tracker.clear_primary();
            // Persist the cleared sidecar in lockstep with the running state. A
            // persist fault is non-fatal (the running state is the authority); the
            // reply still reports the cleared primary so the caller can surface it.
            match tracker.save_primary(&state.sidecar_path) {
                Ok(()) => json!({"ok": true, "primary_id": Value::Null}),
                Err(e) => json!({
                    "ok": true,
                    "primary_id": Value::Null,
                    "persist_error": e.to_string(),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

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

    fn state(dir: &Path) -> CmdState {
        CmdState {
            tracker: Arc::new(Mutex::new(HotplugTracker::new(None))),
            sidecar_path: dir.join("ground-station-input.json"),
        }
    }

    #[test]
    fn bad_json_is_rejected_before_any_tracker_access() {
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
    fn set_primary_requires_a_nonempty_device_id() {
        assert_eq!(
            reply(br#"{"op":"set_primary"}"#)["error"],
            "E_MISSING_DEVICE_ID"
        );
        assert_eq!(
            reply(br#"{"op":"set_primary","device_id":""}"#)["error"],
            "E_MISSING_DEVICE_ID"
        );
        assert_eq!(
            cmd(br#"{"op":"set_primary","device_id":"usb:1"}"#),
            Command::SetPrimary {
                device_id: "usb:1".to_string()
            }
        );
    }

    #[test]
    fn get_primary_parses_with_no_fields() {
        assert_eq!(cmd(br#"{"op":"get_primary"}"#), Command::GetPrimary);
    }

    #[test]
    fn clear_primary_parses_with_no_fields() {
        assert_eq!(cmd(br#"{"op":"clear_primary"}"#), Command::ClearPrimary);
    }

    #[tokio::test]
    async fn apply_clear_primary_drops_the_running_primary_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let st = state(dir.path());
        // Seed a primary, then clear it.
        dispatch(
            br#"{"op":"set_primary","device_id":"bt:aa:bb:cc:dd:ee:ff"}"#,
            &st,
        )
        .await;
        let v = dispatch(br#"{"op":"clear_primary"}"#, &st).await;
        assert_eq!(v["ok"], true);
        assert!(v["primary_id"].is_null());
        // The running tracker no longer holds a primary.
        assert_eq!(st.tracker.lock().await.primary(), None);
        // The sidecar persisted the cleared primary.
        let on_disk =
            crate::sidecar::GroundStationInput::load(&st.sidecar_path).and_then(|g| g.primary);
        assert_eq!(on_disk, None);
    }

    #[tokio::test]
    async fn apply_set_primary_updates_tracker_and_persists_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let st = state(dir.path());
        let v = dispatch(
            br#"{"op":"set_primary","device_id":"usb:045e:028e:event3"}"#,
            &st,
        )
        .await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["primary_id"], "usb:045e:028e:event3");
        // The running tracker reflects the new primary.
        assert_eq!(
            st.tracker.lock().await.primary(),
            Some("usb:045e:028e:event3")
        );
        // The sidecar was persisted with the same value.
        let on_disk =
            crate::sidecar::GroundStationInput::load(&st.sidecar_path).and_then(|g| g.primary);
        assert_eq!(on_disk.as_deref(), Some("usb:045e:028e:event3"));
        // get_primary reads it back over the same socket dispatch.
        let g = dispatch(br#"{"op":"get_primary"}"#, &st).await;
        assert_eq!(g["primary_id"], "usb:045e:028e:event3");
    }

    #[tokio::test]
    async fn end_to_end_socket_set_primary() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hid-cmd.sock");
        let st = state(dir.path());
        let tracker = st.tracker.clone();
        let server = tokio::spawn({
            let st = st.clone();
            let sock = sock.clone();
            async move { serve(st, &sock).await }
        });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let mut client = UnixStream::connect(&sock).await.unwrap();
        client
            .write_all(b"{\"op\":\"set_primary\",\"device_id\":\"usb:7\"}\n")
            .await
            .unwrap();
        let mut line = String::new();
        use tokio::io::AsyncBufReadExt;
        BufReader::new(&mut client)
            .read_line(&mut line)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["primary_id"], "usb:7");
        assert_eq!(tracker.lock().await.primary(), Some("usb:7"));

        server.abort();
    }
}
