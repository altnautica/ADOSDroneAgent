//! The plugin event-bus export socket (contract `plugin.events`, seam K).
//!
//! The event bus is an in-process fanout inside this daemon, so a host service
//! in another process (the native control front, which serves the operator's
//! navigation, mapping and follow streams) cannot subscribe to it directly. This
//! module is that reach: a daemon-lifetime Unix socket,
//! `<control dir>/_events.sock`, beside the control socket in the root-only
//! `0700` control dir, so no plugin process can reach it.
//!
//! The wire is newline-delimited JSON:
//!
//! 1. The client writes one request line within [`REQUEST_READ_DEADLINE`]:
//!    `{"v": 1, "op": "subscribe", "topics": ["<exact topic>", ...]}`. Topics are
//!    exact names (no glob), at most [`EXPORT_MAX_TOPICS`].
//! 2. The host answers `{"v": 1, "ok": true}` or `{"v": 1, "error": "<reason>"}`
//!    (and closes on an error).
//! 3. Then one line per bus event on a subscribed topic:
//!    `{"v": 1, "topic", "publisher", "ts_ms", "payload"}`.
//!
//! Every exported event already passed the publish check
//! ([`crate::handlers::is_publish_allowed`]), and `publisher` is the
//! host-stamped plugin id, never a value the plugin chose, so a consumer can pin
//! a topic to the plugin it expects. A consumer that falls behind the bus loses
//! the events it lagged past (the bus is drop-on-full, never blocking a
//! publisher); a consumer that stops reading is dropped after
//! [`WRITE_DEADLINE`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::handlers::{topic_well_formed, Event, EventBus};

/// The `plugin.events` wire version, carried as `v` on every line.
pub const EVENT_EXPORT_VERSION: u32 = 1;

/// Most topics one export connection may subscribe to.
pub const EXPORT_MAX_TOPICS: usize = 32;

/// Most export connections served at once. The consumers are host services,
/// one long-lived connection each.
const EXPORT_MAX_CONNECTIONS: usize = 8;

/// Longest request line. A subscribe request names at most
/// [`EXPORT_MAX_TOPICS`] short topics.
const REQUEST_MAX_BYTES: usize = 16 * 1024;

/// How long a client has to deliver its subscribe request.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(5);

/// Upper bound on one event write. A consumer that stops reading fills the
/// socket buffer; past this the connection is dropped rather than pinned.
const WRITE_DEADLINE: Duration = Duration::from_secs(2);

/// The export socket path under a control dir.
pub fn export_socket_path(control_dir: &Path) -> PathBuf {
    control_dir.join(ados_protocol::paths::PLUGIN_EVENTS_SOCK)
}

/// The subscribe request a client sends first.
#[derive(Debug, Deserialize)]
struct SubscribeRequest {
    v: u32,
    op: String,
    topics: Vec<String>,
}

/// Parse and validate a subscribe request line into its topic set.
fn parse_request(line: &[u8]) -> Result<BTreeSet<String>, String> {
    let req: SubscribeRequest =
        serde_json::from_slice(line).map_err(|e| format!("bad request: {e}"))?;
    if req.v != EVENT_EXPORT_VERSION {
        return Err(format!("unsupported version {}", req.v));
    }
    if req.op != "subscribe" {
        return Err(format!("unknown op {:?}", req.op));
    }
    if req.topics.is_empty() || req.topics.len() > EXPORT_MAX_TOPICS {
        return Err(format!("topics must hold 1-{EXPORT_MAX_TOPICS} names"));
    }
    if let Some(bad) = req.topics.iter().find(|t| !topic_well_formed(t)) {
        return Err(format!("malformed topic {bad:?}"));
    }
    Ok(req.topics.into_iter().collect())
}

/// One exported event as its JSON line (newline included).
fn event_line(event: &Event) -> Vec<u8> {
    let mut line = serde_json::to_vec(&json!({
        "v": EVENT_EXPORT_VERSION,
        "topic": event.topic,
        "publisher": event.publisher_plugin_id,
        "ts_ms": event.timestamp_ms,
        "payload": crate::state_sidecar::msgpack_to_json(&event.payload),
    }))
    .unwrap_or_default();
    line.push(b'\n');
    line
}

async fn write_line(stream: &mut tokio::net::unix::OwnedWriteHalf, line: &[u8]) -> bool {
    matches!(
        tokio::time::timeout(WRITE_DEADLINE, stream.write_all(line)).await,
        Ok(Ok(()))
    )
}

/// Serve one export connection: read the request, answer it, then forward
/// every matching bus event until either side goes away.
async fn serve_connection(bus: Arc<EventBus>, stream: UnixStream) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let line = match tokio::time::timeout(
        REQUEST_READ_DEADLINE,
        ados_protocol::ipc::read_newline_line(&mut reader, REQUEST_MAX_BYTES),
    )
    .await
    {
        Ok(Ok(Some(line))) => line,
        _ => return,
    };
    let topics = match parse_request(&line) {
        Ok(topics) => topics,
        Err(reason) => {
            let mut body = serde_json::to_vec(&json!({"v": EVENT_EXPORT_VERSION, "error": reason}))
                .unwrap_or_default();
            body.push(b'\n');
            let _ = write_line(&mut write_half, &body).await;
            return;
        }
    };
    // Subscribe before acknowledging, so no event published after the client
    // reads the ack is missed.
    let mut rx = bus.subscribe();
    let mut ack =
        serde_json::to_vec(&json!({"v": EVENT_EXPORT_VERSION, "ok": true})).unwrap_or_default();
    ack.push(b'\n');
    if !write_line(&mut write_half, &ack).await {
        return;
    }
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(event) => {
                    if topics.contains(&event.topic)
                        && !write_line(&mut write_half, &event_line(&event)).await
                    {
                        return;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "event export consumer lagged");
                }
                Err(RecvError::Closed) => return,
            },
            // The client sends nothing after its request; a read returning
            // means it closed (or broke) its end.
            _ = tokio::io::AsyncReadExt::read(&mut reader, &mut probe) => return,
        }
    }
}

/// Bind the export socket in `control_dir` and spawn its accept loop. The dir
/// is created `0700` (the same root-only dir the control socket lives in) and
/// the socket bound `0600`, admitting root and operator-group peers only.
/// Returns the bound path and the accept task so the daemon can unlink and
/// abort on shutdown.
pub fn serve_event_export(
    bus: Arc<EventBus>,
    control_dir: PathBuf,
) -> std::io::Result<(PathBuf, JoinHandle<()>)> {
    crate::control::prepare_control_dir(&control_dir)?;
    let path = export_socket_path(&control_dir);
    let listener = ados_protocol::ipc::bind_command_socket(&path, 0o600)?;
    let permits = Arc::new(tokio::sync::Semaphore::new(EXPORT_MAX_CONNECTIONS));
    let task = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _addr)) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "event export socket accept failed");
                    tokio::time::sleep(crate::server::ACCEPT_RETRY_INTERVAL).await;
                    continue;
                }
            };
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                tracing::warn!("event export socket at its connection cap; dropping a connection");
                continue;
            };
            let bus = bus.clone();
            tokio::spawn(async move {
                serve_connection(bus, stream).await;
                drop(permit);
            });
        }
    });
    Ok((path, task))
}

#[cfg(test)]
mod tests;
