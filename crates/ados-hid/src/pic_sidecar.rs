//! The PIC arbiter state sidecar (`/run/ados/pic-state.json`).
//!
//! The `ados-pic` daemon is the single owner of the arbiter state. So other
//! processes can read the current holder without a socket round-trip — and can
//! still read it when the daemon is momentarily busy — the daemon mirrors the
//! arbiter snapshot to this on-disk sidecar on every transition (claim, release,
//! disconnect, watchdog auto-release) and on each watchdog tick.
//!
//! The on-disk shape is byte-identical to the arbiter's `get_state` dict (the
//! same field set + order the IPC `get_state` op and the FastAPI `/pic` read
//! report): `{"state", "claimed_by", "claimed_since", "claim_counter",
//! "primary_gamepad_id"}`. A reader that prefers the sidecar over a socket
//! round-trip therefore sees exactly what the socket would have reported.
//!
//! The write is atomic (`ados_protocol::sidecar::write_json_atomic`: unique temp
//! sibling + fsync + rename), so a power loss mid-save never half-writes the file.

use std::path::Path;

use serde_json::{json, Value};

use ados_hmi::pic::{PicState, PicStateSnapshot};

/// The schema version this build reads and writes for the PIC-state sidecar. Sits
/// alongside the arbiter fields as an additive `version` key. Must match the
/// `pic-state` entry in the shared contract registry (asserted by a test).
pub const PIC_STATE_SIDECAR_VERSION: u16 = 1;

/// Render a [`PicStateSnapshot`] as the sidecar/`get_state` JSON value. The field
/// set + order match `PicArbiter::get_state` (the IPC `state_to_json`, minus the
/// transport `ok` flag) and the FastAPI `/pic` read: `state` as the lowercase
/// wire string, then the holder / since / counter / primary-gamepad fields. A
/// leading `version` key carries the sidecar schema version additively; readers
/// that project specific fields ignore it.
pub fn snapshot_to_json(snapshot: &PicStateSnapshot) -> Value {
    let state = match snapshot.state {
        PicState::Unclaimed => "unclaimed",
        PicState::Claimed => "claimed",
    };
    json!({
        "version": PIC_STATE_SIDECAR_VERSION,
        "state": state,
        "claimed_by": snapshot.claimed_by,
        "claimed_since": snapshot.claimed_since,
        "claim_counter": snapshot.claim_counter,
        "primary_gamepad_id": snapshot.primary_gamepad_id,
    })
}

/// Atomically persist the arbiter snapshot to `path` (temp sibling + fsync +
/// rename), creating the parent dir. Best-effort by contract: the daemon logs
/// and continues on a write fault rather than failing a state transition over a
/// sidecar it only mirrors.
pub fn write_snapshot(path: &Path, snapshot: &PicStateSnapshot) -> std::io::Result<()> {
    ados_protocol::sidecar::write_json_atomic(path, &snapshot_to_json(snapshot), 0o644)
}

/// Read the persisted PIC snapshot JSON from `path`, or `None` when the file is
/// absent / unreadable / malformed. The value is the `get_state` shape a reader
/// projects directly (no struct round-trip needed by the consumers).
pub fn read_snapshot(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    // Best-effort schema-drift signal: an older agent's file has no `version`
    // key and reads back as 0, which warns but is still used. Never a reject.
    let got = value.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    ados_protocol::sidecar::check_sidecar_version("pic-state", got, PIC_STATE_SIDECAR_VERSION);
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_hmi::pic::PicArbiter;

    #[test]
    fn unclaimed_snapshot_is_the_get_state_default() {
        let arb = PicArbiter::new();
        let v = snapshot_to_json(&arb.get_state());
        assert_eq!(
            v,
            json!({
                "version": PIC_STATE_SIDECAR_VERSION,
                "state": "unclaimed",
                "claimed_by": null,
                "claimed_since": null,
                "claim_counter": 0,
                "primary_gamepad_id": null,
            })
        );
    }

    #[test]
    fn version_matches_registry() {
        assert_eq!(
            PIC_STATE_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("pic-state").unwrap()
        );
    }

    #[test]
    fn claimed_snapshot_carries_the_holder_and_counter() {
        let mut arb = PicArbiter::new();
        arb.claim("op-a", None, false);
        let v = snapshot_to_json(&arb.get_state());
        assert_eq!(v["state"], "claimed");
        assert_eq!(v["claimed_by"], "op-a");
        assert_eq!(v["claim_counter"], 1);
        // claimed_since is a float seconds value, not null, once claimed.
        assert!(v["claimed_since"].is_number());
    }

    #[test]
    fn sidecar_round_trips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pic-state.json");
        let mut arb = PicArbiter::new();
        arb.claim("op-a", None, false);
        write_snapshot(&path, &arb.get_state()).unwrap();

        let v = read_snapshot(&path).unwrap();
        assert_eq!(v["state"], "claimed");
        assert_eq!(v["claimed_by"], "op-a");
        assert_eq!(v["claim_counter"], 1);

        // No stray tmp left behind.
        let stray = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".tmp"));
        assert!(!stray);
    }

    #[test]
    fn read_missing_or_malformed_is_none() {
        assert!(read_snapshot(Path::new("/nonexistent/pic-state.json")).is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pic-state.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(read_snapshot(&path).is_none());
    }
}
