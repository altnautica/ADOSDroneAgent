//! Runtime file paths for the WFB radio service, mirroring the Python
//! constants in `core/paths.py`. All paths are in `/run/ados` (tmpfs) or
//! `/etc/ados/wfb` (persistent). Use the functions rather than the string
//! literals directly so the `ADOS_RUN_DIR` env override is honoured.

/// Contract E sidecar JSON files written by this service.
pub const WFB_STATS_JSON: &str = "/run/ados/wfb-stats.json";
pub const HOP_SUPERVISOR_JSON: &str = "/run/ados/hop-supervisor.json";
pub const PEER_PRESENCE_JSON: &str = "/run/ados/peer-presence.json";
/// In-memory channel hint (no file, but this is the path if we ever write one).
pub const WFB_LOCKED_CHANNEL: &str = "/run/ados/wfb-locked-channel";

/// Persistent WFB key directory.
pub const WFB_KEY_DIR: &str = "/etc/ados/wfb";
/// Drone TX keypair (present ⟺ this rig is WFB-paired as a drone).
pub const WFB_TX_KEY: &str = "/etc/ados/wfb/tx.key";
/// Drone RX key (decrypts the GS uplink). Present ⟺ the stats RX can run.
pub const WFB_RX_KEY: &str = "/etc/ados/wfb/rx.key";
/// The canonical drone key shared with the ground station after bind.
pub const DRONE_KEY: &str = "/etc/drone.key";

/// Cross-process bind-liveness sentinel written by the supervisor while a bind
/// session owns the radio adapter. `{"active": <bool>}`.
pub const BIND_STATE_SENTINEL: &str = "/run/ados/bind-state.json";

/// Return the run directory, honouring the `ADOS_RUN_DIR` env override.
pub fn run_dir() -> String {
    std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string())
}

/// Return the path to a run-dir file, honouring the env override.
pub fn run_path(name: &str) -> String {
    format!("{}/{}", run_dir(), name)
}

/// Atomic JSON write: write to `.tmp` then rename, matching the Python
/// `tmp.write + tmp.replace` pattern so a crash mid-write never leaves a
/// truncated sidecar file.
pub fn write_sidecar(path: &str, value: &serde_json::Value) -> std::io::Result<()> {
    let tmp = format!("{}.tmp", path);
    let body = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read the cross-process bind-liveness sentinel. Synchronous and cheap enough
/// to call from a hot loop without a socket round-trip.
///
/// Returns `obj["active"]` (coerced to bool) from `bind-state.json`, or `false`
/// on any error: file missing, unreadable, not valid JSON, not an object, or the
/// `active` key absent. The supervisor writes this file while a bind session
/// owns the radio adapter; the hop loop reads it to suppress channel changes
/// that would corrupt the bind key exchange.
pub fn read_bind_sentinel_active() -> bool {
    let path = run_path("bind-state.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    value
        .as_object()
        .and_then(|obj| obj.get("active"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize tests that mutate the `ADOS_RUN_DIR` process-global env var.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn bind_sentinel_missing_file_is_false() {
        let _g = env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        // No bind-state.json written → false.
        assert!(!read_bind_sentinel_active());
        std::env::remove_var("ADOS_RUN_DIR");
    }

    #[test]
    fn bind_sentinel_active_true_reads_true() {
        let _g = env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        std::fs::write(dir.path().join("bind-state.json"), r#"{"active": true}"#).unwrap();
        assert!(read_bind_sentinel_active());
        std::env::remove_var("ADOS_RUN_DIR");
    }

    #[test]
    fn bind_sentinel_active_false_reads_false() {
        let _g = env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        std::fs::write(dir.path().join("bind-state.json"), r#"{"active": false}"#).unwrap();
        assert!(!read_bind_sentinel_active());
        std::env::remove_var("ADOS_RUN_DIR");
    }

    #[test]
    fn bind_sentinel_garbled_json_is_false() {
        let _g = env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        std::fs::write(dir.path().join("bind-state.json"), "not json at all").unwrap();
        assert!(!read_bind_sentinel_active());
        // A non-object (a bare array) is also false.
        std::fs::write(dir.path().join("bind-state.json"), "[1, 2, 3]").unwrap();
        assert!(!read_bind_sentinel_active());
        // An object missing the active key is false.
        std::fs::write(dir.path().join("bind-state.json"), r#"{"other": 1}"#).unwrap();
        assert!(!read_bind_sentinel_active());
        std::env::remove_var("ADOS_RUN_DIR");
    }
}
