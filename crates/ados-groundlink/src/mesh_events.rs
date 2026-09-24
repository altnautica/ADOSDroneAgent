//! Cross-process mesh-event seam for the relay and receiver loops.
//!
//! The relay/receiver loops run in their own processes (`ados-wfb-relay` /
//! `ados-wfb-receiver`), so they publish their events onto the cross-process
//! mesh-event journal the native `/ws/mesh` stream tails. The journal writer and
//! its line envelope live in [`ados_supervisor::mesh_journal`], shared with the
//! supervisor's role transition, so every writer emits the same shape.

use std::path::Path;

/// The mesh-event kinds a relay or receiver emits across the seam. These are
/// the subset of the mesh kinds the FEC loops are the authority for;
/// neighbor/gateway/partition kinds stay with the mesh poll loop.
pub const KIND_RELAY_CONNECTED: &str = "relay_connected";
pub const KIND_RELAY_DISCONNECTED: &str = "relay_disconnected";
pub const KIND_RECEIVER_UNREACHABLE: &str = "receiver_unreachable";
pub const KIND_WFB_ADAPTER_MISSING: &str = "wfb_adapter_missing";

/// Append one mesh event to the journal under the run dir (honouring
/// `ADOS_RUN_DIR`), stamped now. Best-effort: an I/O error is logged and
/// swallowed.
pub fn emit(kind: &str, payload: serde_json::Value) {
    let journal = ados_supervisor::mesh_journal::journal_path(Path::new(&crate::paths::run_dir()));
    ados_supervisor::mesh_journal::append(&journal, kind, payload, now_ms());
}

/// Wall-clock unix milliseconds (the bus timestamp basis).
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
