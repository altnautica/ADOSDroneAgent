//! PIC IPC seam — the single-owner socket for the arbiter state.
//!
//! The `ados-pic` daemon is the SOLE owner of the [`crate::pic::PicArbiter`]
//! state. Every other process that needs PIC (the FastAPI `/pic/*` routes, the
//! HDMI kiosk, the input-hotplug hooks) reaches it over a Unix-domain socket at
//! [`PIC_SOCK`] rather than each instantiating its own arbiter — that
//! split-brain (one arbiter per process) is exactly the bug this seam closes.
//!
//! Wire shape mirrors the supervisor control socket
//! (`ados-supervisor/src/bind/control.rs`): one newline-JSON request -> one (or
//! a stream of) newline-JSON response(s) per connection. Python clients consume
//! it with `socket.recv` + `json.loads`, so the contract is the field
//! names/types, not whitespace.
//!
//! On every reply `ok:true` means the RPC itself succeeded; the FSM outcome
//! rides in the body (`claimed` / `released` / `ok_heartbeat`), and `status`
//! carries the HTTP code the FastAPI route relays (409 confirm-needed, 403
//! not-current-pic, 410 no-active-claim). Request ops and their replies:
//!
//! ```text
//! {"op":"claim","client_id":"...","confirm_token":null,"force":false}
//!   ->  {"ok":true,"claimed":true,"mode":"fresh","claimed_by":"...","claim_counter":N}
//!   or  {"ok":true,"claimed":false,"error":"already_claimed",
//!        "current_pic":"...","needs_confirm":true,"status":409}
//! {"op":"release","client_id":"..."}    -> {"ok":true,"released":<bool>,...}
//! {"op":"get_state"}                     -> {"ok":true,"state":"...","claimed_by":...,...}
//! {"op":"confirm_token","client_id":"..."} -> {"ok":true,"token":"<32hex>"}
//! {"op":"heartbeat","client_id":"..."}   -> {"ok":true,"ok_heartbeat":<bool>,...}
//! {"op":"disconnect"}                    -> {"ok":true}   (PIC client drop hook)
//! {"op":"gamepad_connected","device_id":"...","client_id_hint":"..."}
//!   -> {"ok":true}   (auto-claim PIC for the hint + bind the gamepad as the
//!      primary if nobody holds PIC; no-op when already claimed)
//! {"op":"subscribe"}                     -> streams one line per transition:
//!   {"event":"claimed|released|disconnected","client_id":...,
//!    "claim_counter":N,"timestamp_ms":M}  until the client disconnects.
//! {"op":"subscribe_buttons"}             -> streams one line per front-panel
//!   button press: {"button":N,"kind":"short|long","action":<str|null>,
//!   "timestamp_ms":M}  until the client disconnects. The display/OLED layer is
//!   the consumer.
//! ```

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::eventbus::{ButtonEventBus, PicEventKind};
use crate::pic::{ClaimOutcome, HeartbeatOutcome, PicArbiter, PicState, ReleaseOutcome};

/// PIC control socket path (sibling to supervisor.sock / mavlink.sock).
pub const PIC_SOCK: &str = "/run/ados/pic.sock";

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The arbiter handle the server and the daemon's watchdog/hotplug tasks share.
pub type SharedArbiter = Arc<Mutex<PicArbiter>>;

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    confirm_token: Option<String>,
    #[serde(default)]
    force: bool,
    /// Gamepad device id for the `gamepad_connected` op.
    #[serde(default)]
    device_id: Option<String>,
    /// Client id the gamepad auto-claim runs under (defaults to the kiosk hint).
    #[serde(default)]
    client_id_hint: Option<String>,
}

/// Default client id the gamepad auto-claim binds to when the caller does not
/// pass `client_id_hint`. Matches the kiosk hint the Python hotplug integration
/// uses.
const DEFAULT_CLIENT_HINT: &str = "hdmi-kiosk";

/// Bind the control socket and serve requests until the listener errors. Run as
/// its own task. Removes a stale socket first and chmods it 0660 (root-owned;
/// the api service runs as root on target). Returns only on a bind error; the
/// accept loop never exits on the happy path.
///
/// The `buttons` handle is the fanout the `subscribe_buttons` op streams from;
/// pass a clone of the daemon's button bus so the display/OLED consumer can
/// reach front-panel presses over the same socket.
pub async fn serve(
    arbiter: SharedArbiter,
    buttons: ButtonEventBus,
    sock_path: &Path,
) -> std::io::Result<()> {
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
    tracing::info!(path = %sock_path.display(), "pic control socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let arbiter = arbiter.clone();
                let buttons = buttons.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, arbiter, buttons).await {
                        tracing::debug!(error = %e, "pic conn error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "pic accept failed");
                // Brief backoff so a persistent accept error can't hot-spin.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Read one newline-terminated request and either write one newline-terminated
/// response (the request/response ops) or stream events (the subscribe ops).
async fn handle_conn(
    mut stream: UnixStream,
    arbiter: SharedArbiter,
    buttons: ButtonEventBus,
) -> std::io::Result<()> {
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

    // The subscribe ops hold the connection open and stream events; everything
    // else is one request -> one response.
    match op_of(line).as_deref() {
        Some("subscribe") => return stream_events(stream, arbiter).await,
        Some("subscribe_buttons") => return stream_button_events(stream, buttons).await,
        _ => {}
    }

    let resp = dispatch(line, &arbiter).await;
    let mut body = serde_json::to_vec(&resp)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec());
    body.push(b'\n');
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

/// The `op` field of a request line, if it parses; used to route the streaming
/// ops before the one-shot dispatch.
fn op_of(line: &[u8]) -> Option<String> {
    serde_json::from_slice::<Request>(line).ok().map(|r| r.op)
}

/// Parse + route one request to the arbiter. Pure async over the shared handle
/// — unit-testable without a socket.
async fn dispatch(line: &[u8], arbiter: &SharedArbiter) -> Value {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": format!("E_BAD_REQUEST: {e}")}),
    };
    match req.op.as_str() {
        "claim" => {
            let Some(cid) = req.client_id.as_deref() else {
                return json!({"ok": false, "error": "E_MISSING_CLIENT_ID"});
            };
            let mut arb = arbiter.lock().await;
            let outcome = arb.claim(cid, req.confirm_token.as_deref(), req.force);
            claim_to_json(&outcome)
        }
        "release" => {
            let Some(cid) = req.client_id.as_deref() else {
                return json!({"ok": false, "error": "E_MISSING_CLIENT_ID"});
            };
            let mut arb = arbiter.lock().await;
            release_to_json(&arb.release(cid))
        }
        "get_state" => {
            let arb = arbiter.lock().await;
            state_to_json(&arb)
        }
        "confirm_token" => {
            let Some(cid) = req.client_id.as_deref() else {
                return json!({"ok": false, "error": "E_MISSING_CLIENT_ID"});
            };
            let mut arb = arbiter.lock().await;
            let token = arb.create_confirm_token(cid);
            json!({"ok": true, "token": token})
        }
        "heartbeat" => {
            let Some(cid) = req.client_id.as_deref() else {
                return json!({"ok": false, "error": "E_MISSING_CLIENT_ID"});
            };
            let mut arb = arbiter.lock().await;
            heartbeat_to_json(&arb.heartbeat(cid))
        }
        "disconnect" => {
            let mut arb = arbiter.lock().await;
            arb.on_pic_disconnected();
            json!({"ok": true})
        }
        "gamepad_connected" => {
            let Some(device_id) = req.device_id.as_deref() else {
                return json!({"ok": false, "error": "E_MISSING_DEVICE_ID"});
            };
            let hint = req.client_id_hint.as_deref().unwrap_or(DEFAULT_CLIENT_HINT);
            // The arbiter records this gamepad as the PIC-bound primary and
            // auto-claims for the hint when nobody holds PIC. No-op when held.
            let mut arb = arbiter.lock().await;
            arb.on_gamepad_connected(device_id, hint);
            json!({"ok": true})
        }
        // the subscribe ops are handled before dispatch; reaching here means a
        // routing miss, so report it rather than silently hanging.
        "subscribe" => json!({"ok": false, "error": "E_SUBSCRIBE_NOT_STREAMED"}),
        "subscribe_buttons" => json!({"ok": false, "error": "E_SUBSCRIBE_NOT_STREAMED"}),
        other => json!({"ok": false, "error": format!("E_UNKNOWN_OP: {other}")}),
    }
}

fn claim_to_json(outcome: &ClaimOutcome) -> Value {
    match outcome {
        ClaimOutcome::Fresh {
            claimed_by,
            claim_counter,
        } => json!({
            "ok": true, "claimed": true, "mode": "fresh",
            "claimed_by": claimed_by, "claim_counter": claim_counter,
        }),
        ClaimOutcome::Idempotent {
            claimed_by,
            claim_counter,
        } => json!({
            "ok": true, "claimed": true, "mode": "idempotent",
            "claimed_by": claimed_by, "claim_counter": claim_counter,
        }),
        ClaimOutcome::Forced {
            claimed_by,
            claim_counter,
            previous_pic,
        } => json!({
            "ok": true, "claimed": true, "mode": "forced",
            "claimed_by": claimed_by, "claim_counter": claim_counter,
            "previous_pic": previous_pic,
        }),
        ClaimOutcome::Transferred {
            claimed_by,
            claim_counter,
            transferred_from,
        } => json!({
            "ok": true, "claimed": true, "mode": "transferred",
            "claimed_by": claimed_by, "claim_counter": claim_counter,
            "transferred_from": transferred_from,
        }),
        ClaimOutcome::InvalidConfirmToken {
            current_pic,
            status,
        } => json!({
            "ok": true, "claimed": false, "error": "invalid_confirm_token",
            "current_pic": current_pic, "needs_confirm": true, "status": status,
        }),
        ClaimOutcome::AlreadyClaimed {
            current_pic,
            status,
        } => json!({
            "ok": true, "claimed": false, "error": "already_claimed",
            "current_pic": current_pic, "needs_confirm": true, "status": status,
        }),
    }
}

fn release_to_json(outcome: &ReleaseOutcome) -> Value {
    match outcome {
        ReleaseOutcome::Released { previous_pic } => json!({
            "ok": true, "released": true, "previous_pic": previous_pic,
        }),
        ReleaseOutcome::NotCurrentPic {
            current_pic,
            status,
        } => json!({
            "ok": true, "released": false, "error": "not_current_pic",
            "current_pic": current_pic, "status": status,
        }),
    }
}

fn heartbeat_to_json(outcome: &HeartbeatOutcome) -> Value {
    match outcome {
        HeartbeatOutcome::Ok {
            claimed_by,
            claim_counter,
            last_heartbeat_ts,
        } => json!({
            "ok": true, "ok_heartbeat": true,
            "claimed_by": claimed_by, "claim_counter": claim_counter,
            "last_heartbeat_ts": last_heartbeat_ts,
        }),
        HeartbeatOutcome::NoActiveClaim {
            current_pic,
            status,
        } => json!({
            "ok": true, "ok_heartbeat": false, "error": "no_active_claim",
            "current_pic": current_pic, "status": status,
        }),
    }
}

fn state_to_json(arb: &PicArbiter) -> Value {
    let s = arb.get_state();
    let state = match s.state {
        PicState::Unclaimed => "unclaimed",
        PicState::Claimed => "claimed",
    };
    json!({
        "ok": true,
        "state": state,
        "claimed_by": s.claimed_by,
        "claimed_since": s.claimed_since,
        "claim_counter": s.claim_counter,
        "primary_gamepad_id": s.primary_gamepad_id,
    })
}

/// Stream PIC transition events to a subscriber as newline-JSON until the client
/// disconnects (the write fails) or the bus is dropped. Each subscriber gets its
/// own bounded receiver; a lagging client drops the oldest events rather than
/// stalling the publisher.
async fn stream_events(mut stream: UnixStream, arbiter: SharedArbiter) -> std::io::Result<()> {
    let mut rx = {
        let arb = arbiter.lock().await;
        arb.bus().subscribe()
    };
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let kind = match ev.kind {
                    PicEventKind::Claimed => "claimed",
                    PicEventKind::Released => "released",
                    PicEventKind::Disconnected => "disconnected",
                };
                let mut body = serde_json::to_vec(&json!({
                    "event": kind,
                    "client_id": ev.client_id,
                    "claim_counter": ev.claim_counter,
                    "timestamp_ms": ev.timestamp_ms,
                }))
                .unwrap_or_default();
                body.push(b'\n');
                // A write error means the subscriber went away — stop cleanly.
                if stream.write_all(&body).await.is_err() {
                    break;
                }
                if stream.flush().await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}

/// Stream front-panel button presses to a subscriber as newline-JSON until the
/// client disconnects or the bus is dropped. The display/OLED layer is the
/// consumer; the wire shape (`button` / `kind` / `action` / `timestamp_ms`)
/// matches the Python button event so either runtime can read it.
async fn stream_button_events(
    mut stream: UnixStream,
    buttons: ButtonEventBus,
) -> std::io::Result<()> {
    let mut rx = buttons.subscribe();
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let mut body = serde_json::to_vec(&json!({
                    "button": ev.button,
                    "kind": ev.kind,
                    "action": ev.action,
                    "timestamp_ms": ev.timestamp_ms,
                }))
                .unwrap_or_default();
                body.push(b'\n');
                if stream.write_all(&body).await.is_err() {
                    break;
                }
                if stream.flush().await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn fresh() -> SharedArbiter {
        Arc::new(Mutex::new(PicArbiter::new()))
    }

    #[tokio::test]
    async fn dispatch_get_state_idle() {
        let arb = fresh();
        let v = dispatch(br#"{"op":"get_state"}"#, &arb).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["state"], "unclaimed");
        assert!(v["claimed_by"].is_null());
    }

    #[tokio::test]
    async fn dispatch_bad_json_and_unknown_op_and_missing_cid() {
        let arb = fresh();
        assert_eq!(dispatch(b"not json", &arb).await["ok"], false);
        assert_eq!(dispatch(br#"{"op":"frob"}"#, &arb).await["ok"], false);
        let v = dispatch(br#"{"op":"claim"}"#, &arb).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "E_MISSING_CLIENT_ID");
    }

    #[tokio::test]
    async fn full_claim_409_confirm_takeover_release_round_trip() {
        let arb = fresh();

        // op-a claims fresh.
        let v = dispatch(br#"{"op":"claim","client_id":"op-a"}"#, &arb).await;
        assert_eq!(v["claimed"], true);
        assert_eq!(v["mode"], "fresh");
        assert_eq!(v["claim_counter"], 1);

        // op-b is rejected with 409 + needs_confirm.
        let v = dispatch(br#"{"op":"claim","client_id":"op-b"}"#, &arb).await;
        assert_eq!(v["claimed"], false);
        assert_eq!(v["error"], "already_claimed");
        assert_eq!(v["needs_confirm"], true);
        assert_eq!(v["status"], 409);
        assert_eq!(v["current_pic"], "op-a");

        // op-b mints a confirm token.
        let v = dispatch(br#"{"op":"confirm_token","client_id":"op-b"}"#, &arb).await;
        assert_eq!(v["ok"], true);
        let token = v["token"].as_str().unwrap().to_string();
        assert_eq!(token.len(), 32);

        // op-b confirms the takeover with the token.
        let line = format!(r#"{{"op":"claim","client_id":"op-b","confirm_token":"{token}"}}"#);
        let v = dispatch(line.as_bytes(), &arb).await;
        assert_eq!(v["claimed"], true);
        assert_eq!(v["mode"], "transferred");
        assert_eq!(v["transferred_from"], "op-a");
        assert_eq!(v["claim_counter"], 2);

        // op-b releases.
        let v = dispatch(br#"{"op":"release","client_id":"op-b"}"#, &arb).await;
        assert_eq!(v["released"], true);
        assert_eq!(v["previous_pic"], "op-b");

        // back to unclaimed.
        let v = dispatch(br#"{"op":"get_state"}"#, &arb).await;
        assert_eq!(v["state"], "unclaimed");
    }

    #[tokio::test]
    async fn force_takeover_and_heartbeat_410_for_non_holder() {
        let arb = fresh();
        dispatch(br#"{"op":"claim","client_id":"op-a"}"#, &arb).await;
        // force wins outright.
        let v = dispatch(br#"{"op":"claim","client_id":"op-b","force":true}"#, &arb).await;
        assert_eq!(v["mode"], "forced");
        assert_eq!(v["previous_pic"], "op-a");
        // op-a heartbeat now 410s (no active claim).
        let v = dispatch(br#"{"op":"heartbeat","client_id":"op-a"}"#, &arb).await;
        assert_eq!(v["ok_heartbeat"], false);
        assert_eq!(v["status"], 410);
        // op-b heartbeat ok.
        let v = dispatch(br#"{"op":"heartbeat","client_id":"op-b"}"#, &arb).await;
        assert_eq!(v["ok_heartbeat"], true);
    }

    #[tokio::test]
    async fn end_to_end_socket_claim_and_event_stream() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("pic.sock");
        let arbiter = fresh();
        let buttons = ButtonEventBus::new();
        let server = tokio::spawn({
            let arbiter = arbiter.clone();
            let buttons = buttons.clone();
            let sock = sock.clone();
            async move { serve(arbiter, buttons, &sock).await }
        });
        // Wait for the socket to appear.
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Open a subscribe connection FIRST so it sees the claim event.
        let sub = UnixStream::connect(&sock).await.unwrap();
        let mut sub = BufReader::new(sub);
        sub.get_mut()
            .write_all(b"{\"op\":\"subscribe\"}\n")
            .await
            .unwrap();
        // Give the server a moment to register the subscriber.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Claim over a second connection.
        let mut client = UnixStream::connect(&sock).await.unwrap();
        client
            .write_all(b"{\"op\":\"claim\",\"client_id\":\"op-a\"}\n")
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(&mut client)
            .read_line(&mut line)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["claimed"], true);
        assert_eq!(v["mode"], "fresh");

        // The subscriber sees the claimed event.
        let mut ev_line = String::new();
        sub.read_line(&mut ev_line).await.unwrap();
        let ev: Value = serde_json::from_str(ev_line.trim()).unwrap();
        assert_eq!(ev["event"], "claimed");
        assert_eq!(ev["client_id"], "op-a");
        assert_eq!(ev["claim_counter"], 1);

        server.abort();
    }

    #[tokio::test]
    async fn dispatch_gamepad_connected_binds_primary_and_auto_claims() {
        let arb = fresh();
        // A gamepad connect on an unclaimed arbiter auto-claims for the hint and
        // records the device as the PIC-bound primary.
        let v = dispatch(
            br#"{"op":"gamepad_connected","device_id":"usb:045e:028e:event3"}"#,
            &arb,
        )
        .await;
        assert_eq!(v["ok"], true);
        let v = dispatch(br#"{"op":"get_state"}"#, &arb).await;
        assert_eq!(v["state"], "claimed");
        assert_eq!(v["claimed_by"], "hdmi-kiosk");
        assert_eq!(v["primary_gamepad_id"], "usb:045e:028e:event3");
    }

    #[tokio::test]
    async fn dispatch_gamepad_connected_missing_device_id_errors() {
        let arb = fresh();
        let v = dispatch(br#"{"op":"gamepad_connected"}"#, &arb).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "E_MISSING_DEVICE_ID");
    }

    #[tokio::test]
    async fn dispatch_gamepad_connected_honours_custom_hint() {
        let arb = fresh();
        let v = dispatch(
            br#"{"op":"gamepad_connected","device_id":"usb:1","client_id_hint":"bench-op"}"#,
            &arb,
        )
        .await;
        assert_eq!(v["ok"], true);
        let v = dispatch(br#"{"op":"get_state"}"#, &arb).await;
        assert_eq!(v["claimed_by"], "bench-op");
    }

    #[tokio::test]
    async fn end_to_end_button_event_stream() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("pic.sock");
        let arbiter = fresh();
        let buttons = ButtonEventBus::new();
        let server = tokio::spawn({
            let arbiter = arbiter.clone();
            let buttons = buttons.clone();
            let sock = sock.clone();
            async move { serve(arbiter, buttons, &sock).await }
        });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // A consumer subscribes to the button stream.
        let sub = UnixStream::connect(&sock).await.unwrap();
        let mut sub = BufReader::new(sub);
        sub.get_mut()
            .write_all(b"{\"op\":\"subscribe_buttons\"}\n")
            .await
            .unwrap();
        // Wait until the server registered the subscriber before publishing.
        for _ in 0..50 {
            if buttons.receiver_count() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // The daemon's button reader publishes a press onto the bus.
        buttons.publish(crate::eventbus::ButtonBusEvent {
            button: 5,
            kind: "short",
            action: Some("cycle_screen".into()),
            timestamp_ms: 1500,
        });

        let mut ev_line = String::new();
        sub.read_line(&mut ev_line).await.unwrap();
        let ev: Value = serde_json::from_str(ev_line.trim()).unwrap();
        assert_eq!(ev["button"], 5);
        assert_eq!(ev["kind"], "short");
        assert_eq!(ev["action"], "cycle_screen");
        assert_eq!(ev["timestamp_ms"], 1500);

        server.abort();
    }
}
