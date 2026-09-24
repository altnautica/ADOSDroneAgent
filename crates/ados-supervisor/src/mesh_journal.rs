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
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_event_is_one_enveloped_line_and_appends_never_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let p = journal_path(&dir.path().join("nested"));
        append(&p, "relay_connected", json!({"relay_mac": "aa:bb"}), 100);
        append(&p, "role_changed", json!({"role": "relay"}), 200);

        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            json!({"bus": "mesh", "kind": "relay_connected", "timestamp_ms": 100,
                   "payload": {"relay_mac": "aa:bb"}})
        );
        assert_eq!(lines[1]["kind"], "role_changed");
        assert_eq!(lines[1]["timestamp_ms"], 200);
    }
}
