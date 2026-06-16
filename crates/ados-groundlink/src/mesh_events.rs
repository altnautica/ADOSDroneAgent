//! Cross-process mesh-event seam.
//!
//! The relay/receiver loops run in their own processes (`ados-wfb-relay` /
//! `ados-wfb-receiver`), so they cannot reach the in-process asyncio event bus
//! the REST WebSocket (`/ws/mesh`) and the OLED screens subscribe to. This
//! module bridges the gap with a newline-delimited JSON journal under
//! `/run/ados`: the loop appends one event object per line; a small Python
//! tailer follows the file and republishes each line onto the process-local
//! bus so the existing consumers light up unchanged.
//!
//! The line shape is byte-compatible with the in-process `MeshEvent`:
//!
//! ```json
//! {"bus":"mesh","kind":"relay_connected","timestamp_ms":1234,"payload":{...}}
//! ```
//!
//! Append-only and best-effort: a write error is logged and dropped (the
//! authoritative state always lives in the relay/receiver state files), never
//! fatal to the loop. The file lives on tmpfs and is truncated on boot by the
//! tmpfiles rule, so it never grows without bound across reboots; within a
//! session the tailer seeks to end so a long-lived journal does not replay.

use std::io::Write;
use std::path::PathBuf;

use serde_json::json;

/// The mesh-event kinds a relay or receiver emits across the seam. These are
/// the subset of the in-process bus kinds the FEC loops are the authority for;
/// neighbor/gateway/partition kinds stay with the mesh poll loop.
pub const KIND_RELAY_CONNECTED: &str = "relay_connected";
pub const KIND_RELAY_DISCONNECTED: &str = "relay_disconnected";
pub const KIND_RECEIVER_UNREACHABLE: &str = "receiver_unreachable";
pub const KIND_WFB_ADAPTER_MISSING: &str = "wfb_adapter_missing";
/// A mesh role transition. Emitted by the role-apply path so the GCS Hardware
/// tab, OLED status row, and logs all see the change. Mirrors the in-process
/// `MeshEvent(kind="role_changed", ...)` the Python `role_manager` published.
pub const KIND_ROLE_CHANGED: &str = "role_changed";

/// Resolve the journal path, honouring the `ADOS_RUN_DIR` test override.
fn journal_path() -> PathBuf {
    PathBuf::from(crate::paths::run_path("mesh-events.jsonl"))
}

/// Append a `role_changed` event with the given payload at an explicit
/// timestamp. The role-apply path calls this last so subscribers see the new
/// role only after the unit transition has run; the timestamp is the
/// transition's `ts_ms` so the event + the route's response carry the same value.
pub fn emit_role_changed(payload: serde_json::Value, timestamp_ms: i64) {
    emit_to(&journal_path(), KIND_ROLE_CHANGED, payload, timestamp_ms);
}

/// Append one mesh event to the cross-process journal. `payload` is any JSON
/// object value; the `bus`/`kind`/`timestamp_ms` envelope is added here so the
/// Python tailer can deserialize straight into a `MeshEvent`. Best-effort: an
/// I/O error is logged and swallowed.
pub fn emit(kind: &str, payload: serde_json::Value) {
    emit_to(&journal_path(), kind, payload, now_ms());
}

/// Testable core: append the event line to an explicit path with an explicit
/// timestamp. Creates the parent dir and opens the file append-only so
/// concurrent processes never truncate each other's lines.
pub fn emit_to(path: &std::path::Path, kind: &str, payload: serde_json::Value, timestamp_ms: i64) {
    let line = json!({
        "bus": "mesh",
        "kind": kind,
        "timestamp_ms": timestamp_ms,
        "payload": payload,
    });
    if let Err(e) = append_line(path, &line) {
        tracing::debug!(error = %e, kind, "mesh_event_emit_failed");
    }
}

fn append_line(path: &std::path::Path, value: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    body.push(b'\n');
    f.write_all(&body)?;
    Ok(())
}

/// Wall-clock unix milliseconds (the bus timestamp basis).
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn emits_one_json_line_per_event() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mesh-events.jsonl");
        emit_to(&p, KIND_RELAY_CONNECTED, json!({"relay_mac": "aa:bb"}), 100);
        emit_to(
            &p,
            KIND_RECEIVER_UNREACHABLE,
            json!({"last_receiver": "10.0.0.5", "stale_ms": 16000}),
            200,
        );
        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);

        let e0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(e0["bus"], "mesh");
        assert_eq!(e0["kind"], "relay_connected");
        assert_eq!(e0["timestamp_ms"], 100);
        assert_eq!(e0["payload"]["relay_mac"], "aa:bb");

        let e1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(e1["kind"], "receiver_unreachable");
        assert_eq!(e1["payload"]["stale_ms"], 16000);
    }

    #[test]
    fn append_does_not_truncate_existing_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nested/mesh-events.jsonl");
        for i in 0..5 {
            emit_to(&p, KIND_RELAY_CONNECTED, json!({"i": i}), i);
        }
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 5);
    }

    #[test]
    fn adapter_missing_carries_side_and_reason() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mesh-events.jsonl");
        emit_to(
            &p,
            KIND_WFB_ADAPTER_MISSING,
            json!({"side": "relay", "reason": "adapter_not_found", "detail": "no monitor adapter"}),
            7,
        );
        let line = std::fs::read_to_string(&p).unwrap();
        let e: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(e["payload"]["side"], "relay");
        assert_eq!(e["payload"]["reason"], "adapter_not_found");
    }
}
