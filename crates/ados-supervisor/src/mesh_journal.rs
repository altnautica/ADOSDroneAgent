//! The cross-process mesh-event journal writer.
//!
//! Ground-station processes that are not the REST front (the role transition in
//! this supervisor, the relay and receiver loops in `ados-groundlink`) publish
//! mesh events by appending one JSON object per line to `mesh-events.jsonl`
//! under the run dir; the native `/ws/mesh` stream tails that file and forwards
//! each line verbatim. The line envelope is the contract:
//!
//! ```json
//! {"bus":"mesh","kind":"relay_connected","timestamp_ms":1234,"payload":{...}}
//! ```
//!
//! Append-only and best-effort: a write error is logged and dropped (the
//! authoritative state lives in the role sentinel and the per-role state
//! files), never fatal to the caller. The file lives on tmpfs, so it never grows
//! across reboots; a reader seeks to its end on start, so it never replays.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The journal's file name under the run dir.
pub const MESH_EVENTS_FILE: &str = "mesh-events.jsonl";

/// The journal path under `run_dir`.
pub fn journal_path(run_dir: &Path) -> PathBuf {
    run_dir.join(MESH_EVENTS_FILE)
}

/// Append one event line to the journal at `path`. Creates the parent dir and
/// opens the file append-only, so concurrent writers in different processes
/// never truncate each other's lines.
pub fn append(path: &Path, kind: &str, payload: serde_json::Value, timestamp_ms: i64) {
    let line = serde_json::json!({
        "bus": "mesh",
        "kind": kind,
        "timestamp_ms": timestamp_ms,
        "payload": payload,
    });
    if let Err(e) = append_line(path, &line) {
        tracing::debug!(error = %e, kind, "mesh_event_emit_failed");
    }
}

fn append_line(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    body.push(b'\n');
    f.write_all(&body)
}

#[cfg(test)]
mod tests;
