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
/// The canonical drone key shared with the ground station after bind.
pub const DRONE_KEY: &str = "/etc/drone.key";

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
