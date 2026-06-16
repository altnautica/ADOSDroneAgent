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
//! The write is atomic (tmp sibling + fsync + rename), reusing the same pattern
//! as [`crate::sidecar`], so a power loss mid-save never half-writes the file.

use std::path::Path;

use serde_json::{json, Value};

use crate::pic::{PicState, PicStateSnapshot};

/// Render a [`PicStateSnapshot`] as the sidecar/`get_state` JSON value. The field
/// set + order match `PicArbiter::get_state` (the IPC `state_to_json`, minus the
/// transport `ok` flag) and the FastAPI `/pic` read: `state` as the lowercase
/// wire string, then the holder / since / counter / primary-gamepad fields.
pub fn snapshot_to_json(snapshot: &PicStateSnapshot) -> Value {
    let state = match snapshot.state {
        PicState::Unclaimed => "unclaimed",
        PicState::Claimed => "claimed",
    };
    json!({
        "state": state,
        "claimed_by": snapshot.claimed_by,
        "claimed_since": snapshot.claimed_since,
        "claim_counter": snapshot.claim_counter,
        "primary_gamepad_id": snapshot.primary_gamepad_id,
    })
}

/// Atomically persist the arbiter snapshot to `path` (tmp sibling + fsync +
/// rename), creating the parent dir. Best-effort by contract: the daemon logs
/// and continues on a write fault rather than failing a state transition over a
/// sidecar it only mirrors.
pub fn write_snapshot(path: &Path, snapshot: &PicStateSnapshot) -> std::io::Result<()> {
    let body = serde_json::to_vec(&snapshot_to_json(snapshot)).map_err(std::io::Error::other)?;
    atomic_write(path, &body)
}

/// Read the persisted PIC snapshot JSON from `path`, or `None` when the file is
/// absent / unreadable / malformed. The value is the `get_state` shape a reader
/// projects directly (no struct round-trip needed by the consumers).
pub fn read_snapshot(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Atomic tmp-sibling write, disambiguated by pid so two writers in the same
/// directory never collide. Mirrors [`crate::sidecar`]'s persister.
fn atomic_write(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pic-state.json");
    let tmp = parent.join(format!("{}.{}.tmp", file_name, std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(body)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write_result;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pic::PicArbiter;

    #[test]
    fn unclaimed_snapshot_is_the_get_state_default() {
        let arb = PicArbiter::new();
        let v = snapshot_to_json(&arb.get_state());
        assert_eq!(
            v,
            json!({
                "state": "unclaimed",
                "claimed_by": null,
                "claimed_since": null,
                "claim_counter": 0,
                "primary_gamepad_id": null,
            })
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
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
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
