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

use std::sync::{Arc, Mutex};

use crate::buttons::{Edge, PressClassifier};
use crate::eventbus::{ButtonBusEvent, ButtonEventBus};

/// Button-event fanout socket path (sibling to pic.sock / mavlink.sock).
pub const BUTTONS_SOCK: &str = "/run/ados/buttons.sock";

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Env flag that turns the `inject` op on. Default off: injection lets a caller
/// synthesize presses, which on a production node would be a way to drive the
/// UI (and any button-subscribing plugin) remotely. It is a sim / bench affordance
/// — the virtual ground panel uses it so a clicked button travels the same path a
/// real GPIO press would — so it stays behind an explicit opt-in.
pub const INJECT_ENABLE_ENV: &str = "ADOS_BUTTONS_ALLOW_INJECT";

/// `op` selects the request; `subscribe` reads no more, `inject` reads the rest.
#[derive(Debug, Deserialize)]
struct Request {
    op: String,
}

/// The `inject` op body: one synthetic edge for the shared classifier.
#[derive(Debug, Deserialize)]
struct InjectRequest {
    /// BCM pin of the button being driven.
    pin: u32,
    /// `"press"` (falling) or `"release"` (rising).
    edge: String,
    /// Monotonic-millisecond timestamp of the edge. The classifier measures
    /// short vs long from the press→release delta, so the caller must supply a
    /// real clock here — a press and release at the same ts would always read
    /// short.
    #[serde(default)]
    ts_ms: u64,
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
    serve_with_inject(buttons, None, sock_path).await
}

/// [`serve`] plus an optional injection classifier.
///
/// When `inject` is `Some`, the `inject` op is served (subject to the
/// [`INJECT_ENABLE_ENV`] gate): a synthetic edge is fed to that classifier and
/// any press it classifies is published to the bus, so an injected press is
/// byte-identical downstream to a real one. `inject` shares the SAME
/// [`PressClassifier`] type the GPIO reader uses — it is a second instance, not a
/// second implementation, so short/long/cancel and the action mapping cannot
/// drift. On a node with no GPIO (a VM) the reader is skipped entirely, so this
/// is then the only classifier and there is no second instance at all.
pub async fn serve_with_inject(
    buttons: ButtonEventBus,
    inject: Option<Arc<Mutex<PressClassifier>>>,
    sock_path: &Path,
) -> std::io::Result<()> {
    let listener = bind_command_socket(sock_path, 0o660)?;
    tracing::info!(path = %sock_path.display(), "button event socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let buttons = buttons.clone();
                let inject = inject.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, buttons, inject).await {
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
async fn handle_conn(
    mut stream: UnixStream,
    buttons: ButtonEventBus,
    inject: Option<Arc<Mutex<PressClassifier>>>,
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

    match op_of(line).as_deref() {
        Some("subscribe") => stream_button_events(stream, buttons).await,
        Some("inject") => handle_inject(stream, line, buttons, inject).await,
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

/// Serve one `inject` request: feed the edge to the shared classifier and
/// publish any resulting press. Refuses when injection is disabled (the default)
/// or unwired, so a production node cannot be driven through this op.
async fn handle_inject(
    mut stream: UnixStream,
    line: &[u8],
    buttons: ButtonEventBus,
    inject: Option<Arc<Mutex<PressClassifier>>>,
) -> std::io::Result<()> {
    let reply = classify_injected(line, &buttons, inject.as_deref());
    let mut body = serde_json::to_vec(&reply).unwrap_or_default();
    body.push(b'\n');
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

/// The pure core of an inject: parse, gate, feed the classifier, publish. Split
/// out with no I/O so the gate and the classify→publish path are unit-tested
/// against the real classifier rather than through a socket.
fn classify_injected(
    line: &[u8],
    buttons: &ButtonEventBus,
    inject: Option<&Mutex<PressClassifier>>,
) -> serde_json::Value {
    if std::env::var(INJECT_ENABLE_ENV).ok().as_deref() != Some("1") {
        return json!({"ok": false, "error": "E_INJECT_DISABLED"});
    }
    let Some(classifier) = inject else {
        return json!({"ok": false, "error": "E_INJECT_UNAVAILABLE"});
    };
    let req: InjectRequest = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": format!("E_BAD_REQUEST: {e}")}),
    };
    let edge = match req.edge.as_str() {
        "press" => Edge::Falling,
        "release" => Edge::Rising,
        other => return json!({"ok": false, "error": format!("E_BAD_EDGE: {other}")}),
    };

    // The classifier emits a press only on the release edge; a press edge returns
    // None and is acknowledged without a published event.
    let event = {
        // Recover a poisoned lock rather than propagating the panic: a prior
        // panic while classifying one injected edge must not take the whole
        // inject path down, and on_edge holds no invariant a poisoned state
        // corrupts (a stale press timestamp at worst yields one mis-timed press).
        let mut guard = classifier.lock().unwrap_or_else(|e| e.into_inner());
        guard.on_edge(req.pin, edge, req.ts_ms)
    };
    match event {
        Some(ev) => {
            buttons.publish(ButtonBusEvent {
                button: ev.pin,
                kind: ev.kind.as_str(),
                action: ev.action.clone(),
                timestamp_ms: ev.timestamp_ms,
            });
            json!({"ok": true, "published": true, "kind": ev.kind.as_str()})
        }
        None => json!({"ok": true, "published": false}),
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
                    // The stable friendly id (B1..B4) the cockpit and plugin
                    // bindings key on; additive so no existing reader breaks.
                    "label": crate::buttons::pin_to_label(ev.button).to_lowercase(),
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

    /// Serializes the inject tests: they toggle the process-global
    /// [`INJECT_ENABLE_ENV`], and Rust runs tests on several threads in one
    /// process, so one test's env would otherwise leak into another's gate.
    static INJECT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn line(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    fn short_mapping() -> Arc<Mutex<PressClassifier>> {
        use std::collections::HashMap;
        use std::sync::RwLock;
        // Pin 5 maps to label B1 (see buttons::pin_to_label); the classifier's
        // mapping key is "<label>_<kind>".
        let mut m = HashMap::new();
        m.insert("B1_short".to_string(), "commit".to_string());
        Arc::new(Mutex::new(PressClassifier::with_mapping(Arc::new(
            RwLock::new(m),
        ))))
    }

    #[test]
    fn a_press_then_release_publishes_a_classified_event() {
        let _guard = INJECT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(INJECT_ENABLE_ENV, "1");
        let bus = ButtonEventBus::new();
        let mut rx = bus.subscribe();
        let classifier = short_mapping();

        // Press at t=1000: no event yet (the classifier emits on release).
        let r1 = classify_injected(
            &line(r#"{"op":"inject","pin":5,"edge":"press","ts_ms":1000}"#),
            &bus,
            Some(&classifier),
        );
        assert_eq!(r1["ok"], serde_json::json!(true));
        assert_eq!(r1["published"], serde_json::json!(false));

        // Release at t=1200: a short press, published to the bus with the
        // mapped action — proof the real classifier ran, not a re-implementation.
        let r2 = classify_injected(
            &line(r#"{"op":"inject","pin":5,"edge":"release","ts_ms":1200}"#),
            &bus,
            Some(&classifier),
        );
        assert_eq!(r2["ok"], serde_json::json!(true));
        assert_eq!(r2["published"], serde_json::json!(true));
        assert_eq!(r2["kind"], serde_json::json!("short"));

        let ev = rx.try_recv().expect("a press was published");
        assert_eq!(ev.button, 5);
        assert_eq!(ev.kind, "short");
        assert_eq!(ev.action.as_deref(), Some("commit"));
        std::env::remove_var(INJECT_ENABLE_ENV);
    }

    #[test]
    fn injection_is_refused_when_the_env_gate_is_off() {
        let _guard = INJECT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(INJECT_ENABLE_ENV);
        let bus = ButtonEventBus::new();
        let classifier = short_mapping();
        let r = classify_injected(
            &line(r#"{"op":"inject","pin":5,"edge":"press","ts_ms":1}"#),
            &bus,
            Some(&classifier),
        );
        assert_eq!(r["ok"], serde_json::json!(false));
        assert_eq!(r["error"], serde_json::json!("E_INJECT_DISABLED"));
    }

    #[test]
    fn injection_is_refused_when_no_classifier_is_wired() {
        let _guard = INJECT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(INJECT_ENABLE_ENV, "1");
        let bus = ButtonEventBus::new();
        let r = classify_injected(
            &line(r#"{"op":"inject","pin":5,"edge":"press","ts_ms":1}"#),
            &bus,
            None,
        );
        assert_eq!(r["error"], serde_json::json!("E_INJECT_UNAVAILABLE"));
        std::env::remove_var(INJECT_ENABLE_ENV);
    }

    #[test]
    fn a_malformed_edge_is_rejected_not_guessed() {
        let _guard = INJECT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(INJECT_ENABLE_ENV, "1");
        let bus = ButtonEventBus::new();
        let classifier = short_mapping();
        let r = classify_injected(
            &line(r#"{"op":"inject","pin":5,"edge":"sideways","ts_ms":1}"#),
            &bus,
            Some(&classifier),
        );
        assert_eq!(r["ok"], serde_json::json!(false));
        assert!(r["error"].as_str().unwrap().starts_with("E_BAD_EDGE"));
        std::env::remove_var(INJECT_ENABLE_ENV);
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
