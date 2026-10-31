//! The pair-event journal: one newline-JSON line per field-pairing event,
//! `{"bus":"pair","kind","timestamp_ms","payload"}`, appended to
//! `pair-events.jsonl` in the run dir. The native mesh-event stream tails it
//! next to the mesh journal. Append-only and best-effort: a failed write is
//! logged and dropped so it never breaks pairing.

use std::io::Write;
use std::path::Path;

use serde_json::{json, Value};

/// Event kinds the pairing service emits.
pub const ACCEPT_WINDOW_OPENED: &str = "accept_window_opened";
pub const ACCEPT_WINDOW_CLOSED: &str = "accept_window_closed";
pub const ACCEPT_WINDOW_TICK: &str = "accept_window_tick";
pub const JOIN_REQUEST_RECEIVED: &str = "join_request_received";
pub const JOIN_APPROVED: &str = "join_approved";
pub const JOIN_COMPLETED: &str = "join_completed";
pub const REVOKED: &str = "revoked";

/// Append one event stamped `timestamp_ms`.
pub fn append(path: &Path, kind: &str, timestamp_ms: i64, payload: Value) {
    let line = json!({
        "bus": "pair",
        "kind": kind,
        "timestamp_ms": timestamp_ms,
        "payload": payload,
    });
    if let Err(e) = append_line(path, &line) {
        tracing::debug!(error = %e, kind, "pair_event_journal_write_failed");
    }
}

fn append_line(path: &Path, line: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = serde_json::to_vec(line).map_err(std::io::Error::other)?;
    body.push(b'\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&body)
}
