//! Atomic JSON sidecar writer (Contract E).
//!
//! The ground-station data-plane publishes several live-state JSON files under
//! `/run/ados` that the API layer and on-box UI read cross-process. The Python
//! predecessors all follow the same shape: serialize, write to a tmp sibling,
//! `os.chmod(tmp, mode)`, then `os.replace(tmp, path)` so a crash mid-write
//! never leaves a truncated file and a reader never sees a half-written sidecar.
//! This module is the single Rust helper for that pattern; the per-snapshot
//! types call it with their own path + permission bits.

use std::path::Path;

/// Atomically write `value` as JSON to `path` with the given Unix `mode`.
///
/// Mirrors the Python `tmp.write_text(...); os.chmod(tmp, mode); os.replace(tmp,
/// path)` pattern: serialize to a tmp sibling, set the mode, `fsync`, then
/// rename over the target. The parent directory is created if absent. The
/// rename is atomic on the same filesystem, so a reader sees either the old
/// file or the fully-written new one, never a partial. Best-effort: an I/O
/// error is returned for the caller to log and discard, never fatal.
pub fn write_json_atomic<T: serde::Serialize>(
    path: &Path,
    value: &T,
    mode: u32,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // tmp sibling: `<path>.tmp`, matching the Python `with_suffix(".tmp")`
    // sidecar convention (the suffix swap, e.g. `wfb-stats.json` →
    // `wfb-stats.tmp`).
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    // Re-assert the mode: an existing tmp from a prior crash may carry the old
    // permissions, and the create `mode` is masked by the process umask.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn write_is_atomic_round_trips_and_sets_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb-stats.json");

        let value = serde_json::json!({ "rssi": -48, "valid_pps": 630 });
        write_json_atomic(&path, &value, 0o644).unwrap();

        // Round-trips through the file.
        let reloaded: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(reloaded["rssi"], -48);
        assert_eq!(reloaded["valid_pps"], 630);

        // Mode applied (mask the file-type bits).
        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o644);

        // No leftover tmp sibling (`wfb-stats.tmp`, matching with_extension).
        assert!(!dir.path().join("wfb-stats.tmp").exists());
    }

    #[test]
    fn write_creates_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/state.json");
        write_json_atomic(&path, &serde_json::json!({ "ok": true }), 0o600).unwrap();
        assert!(path.exists());
        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }
}
