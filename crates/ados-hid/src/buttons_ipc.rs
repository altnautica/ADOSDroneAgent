//! Button-event IPC seam — a dedicated fanout socket for front-panel presses.
//!
//! The `ados-pic` daemon owns the GPIO button reader: it classifies each press
//! as short / long / cancel through the shared [`crate::buttons::PressClassifier`]
//! (the single source of truth for those semantics and for the config action
//! mapping) and publishes a [`crate::eventbus::ButtonBusEvent`] to the in-process
//! [`ButtonEventBus`]. This seam binds a dedicated Unix socket at [`BUTTONS_SOCK`]
//! and streams those events as newline-JSON to every subscriber — the seam the
//! HDMI cockpit relay reads through so a browser can be driven by the ground
//! station's four front-panel buttons. It carries no button semantics of its own:
//! it is a pure fanout of already-classified events, so a consumer never
//! re-derives short/long/cancel or the mapping.
//!
//! Wire shape mirrors the PIC control socket ([`crate::pic_ipc`]): one
//! newline-JSON `{"op":"subscribe"}` request opens a stream of newline-JSON
//! events, one per press, until the subscriber disconnects. The event shape
//! matches the pic.sock `subscribe_buttons` stream and the Python button event so
//! either runtime reads it with `json.loads` / serde:
//!
//! ```text
//! {"op":"subscribe"}  -> streams one line per front-panel button press:
//!   {"button":N,"kind":"short|long","action":<str|null>,"timestamp_ms":M}
//!   until the client disconnects.
//! ```

use std::path::Path;

use ados_protocol::ipc::bind_command_socket;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::eventbus::ButtonEventBus;

/// Button-event fanout socket path (sibling to pic.sock / mavlink.sock).
pub const BUTTONS_SOCK: &str = "/run/ados/buttons.sock";

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Only the `op` field is read: this socket serves exactly one op.
#[derive(Debug, Deserialize)]
struct Request {
    op: String,
}

/// Bind the button fanout socket and stream events to each subscriber until the
/// listener errors. Run as its own task. The shared helper removes a stale
/// socket first and chmods it 0660 (root-owned; the front/api service runs as
/// root on target). Returns only on a bind error; the accept loop never exits on
/// the happy path.
///
/// `buttons` is a clone of the daemon's button bus — the same fanout the pic.sock
/// `subscribe_buttons` op reads — so the classifier stays the single source of
/// truth for short / long / cancel and the config mapping.
pub async fn serve(buttons: ButtonEventBus, sock_path: &Path) -> std::io::Result<()> {
    let listener = bind_command_socket(sock_path, 0o660)?;
    tracing::info!(path = %sock_path.display(), "button event socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let buttons = buttons.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, buttons).await {
                        tracing::debug!(error = %e, "button conn error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "button accept failed");
                // Brief backoff so a persistent accept error can't hot-spin.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Read one newline-terminated request and, when it is the `subscribe` op, stream
/// button events until the client disconnects. Any other op closes with a single
/// error line, matching the pic.sock unknown-op posture.
async fn handle_conn(mut stream: UnixStream, buttons: ButtonEventBus) -> std::io::Result<()> {
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

    match op_of(line).as_deref() {
        Some("subscribe") => stream_button_events(stream, buttons).await,
        other => {
            let mut body = serde_json::to_vec(&json!({
                "ok": false,
                "error": format!("E_UNKNOWN_OP: {}", other.unwrap_or("")),
            }))
            .unwrap_or_default();
            body.push(b'\n');
            stream.write_all(&body).await?;
            stream.flush().await?;
            Ok(())
        }
    }
}

/// The `op` field of a request line, if it parses.
fn op_of(line: &[u8]) -> Option<String> {
    serde_json::from_slice::<Request>(line).ok().map(|r| r.op)
}

/// Stream front-panel button presses to a subscriber as newline-JSON until the
/// client disconnects (the write fails) or the bus is dropped. Each subscriber
/// gets its own bounded receiver; a lagging client drops the oldest events rather
/// than stalling the publisher. The wire shape
/// (`button` / `kind` / `action` / `timestamp_ms`) matches the pic.sock button
/// stream and the Python button event.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eventbus::ButtonBusEvent;
    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[test]
    fn button_sock_default_is_the_literal_run_dir_path() {
        assert_eq!(BUTTONS_SOCK, "/run/ados/buttons.sock");
    }

    #[tokio::test]
    async fn subscribe_streams_a_published_button_press() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("buttons.sock");
        let buttons = ButtonEventBus::new();
        let server = tokio::spawn({
            let buttons = buttons.clone();
            let sock = sock.clone();
            async move { serve(buttons, &sock).await }
        });
        // Wait for the socket to appear.
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // A consumer subscribes.
        let sub = UnixStream::connect(&sock).await.unwrap();
        let mut sub = BufReader::new(sub);
        sub.get_mut()
            .write_all(b"{\"op\":\"subscribe\"}\n")
            .await
            .unwrap();
        // Wait until the server registered the subscriber before publishing.
        for _ in 0..50 {
            if buttons.receiver_count() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // The daemon's button reader publishes a classified press onto the bus.
        buttons.publish(ButtonBusEvent {
            button: 13,
            kind: "long",
            action: Some("pair_drone".into()),
            timestamp_ms: 4242,
        });

        let mut line = String::new();
        sub.read_line(&mut line).await.unwrap();
        let ev: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(ev["button"], 13);
        assert_eq!(ev["kind"], "long");
        assert_eq!(ev["action"], "pair_drone");
        assert_eq!(ev["timestamp_ms"], 4242);

        server.abort();
    }

    #[tokio::test]
    async fn unmapped_action_serializes_as_null() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("buttons.sock");
        let buttons = ButtonEventBus::new();
        let server = tokio::spawn({
            let buttons = buttons.clone();
            let sock = sock.clone();
            async move { serve(buttons, &sock).await }
        });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let sub = UnixStream::connect(&sock).await.unwrap();
        let mut sub = BufReader::new(sub);
        sub.get_mut()
            .write_all(b"{\"op\":\"subscribe\"}\n")
            .await
            .unwrap();
        for _ in 0..50 {
            if buttons.receiver_count() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // An unmapped (button, kind) carries no action — it must ride the wire as
        // an explicit JSON null so the consumer can default it.
        buttons.publish(ButtonBusEvent {
            button: 26,
            kind: "short",
            action: None,
            timestamp_ms: 7,
        });
        let mut line = String::new();
        sub.read_line(&mut line).await.unwrap();
        let ev: Value = serde_json::from_str(line.trim()).unwrap();
        assert!(ev["action"].is_null());

        server.abort();
    }

    #[tokio::test]
    async fn an_unknown_op_gets_an_error_line_not_a_stream() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("buttons.sock");
        let buttons = ButtonEventBus::new();
        let server = tokio::spawn({
            let buttons = buttons.clone();
            let sock = sock.clone();
            async move { serve(buttons, &sock).await }
        });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let stream = UnixStream::connect(&sock).await.unwrap();
        let mut stream = BufReader::new(stream);
        stream
            .get_mut()
            .write_all(b"{\"op\":\"frob\"}\n")
            .await
            .unwrap();
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("E_UNKNOWN_OP"));

        server.abort();
    }
}
