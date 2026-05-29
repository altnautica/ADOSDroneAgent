//! Atomic tmp-sibling + rename writers for Contract-E sidecar files.
//!
//! Modeled on `ados-video/src/camera_state.rs`: write the body to a tmp
//! sibling, then `rename` over the destination so a reader never sees a
//! half-written file. Two flavors:
//!
//! * [`write_atomic`] — write + rename, no fsync. Matches the Python
//!   `save_priority` (`tmp.write_text(...)` then `os.replace`), which does not
//!   fsync. Used for the priority list and the active-uplink flag.
//! * [`write_atomic_fsync`] — write + `sync_all` + rename. For callers that
//!   want the bytes durable before the rename (later chunks may opt in).
//!
//! The tmp-suffix policy mirrors Python's `Path.with_suffix(".json.tmp")`,
//! which replaces the final `.json` component, so
//! `ground-station-uplink.json` → `ground-station-uplink.json.tmp`.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Compute the tmp sibling for `path` the way Python `with_suffix(".json.tmp")`
/// does: replace a trailing `.json` extension with `.json.tmp`. For any other
/// (or absent) extension, append `.tmp` to the file name. The tmp file lives
/// in the same directory so the `rename` is same-filesystem and atomic.
pub fn tmp_sibling(path: &Path) -> PathBuf {
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => path.with_extension("json.tmp"),
        _ => {
            let mut name = path
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            name.push(".tmp");
            path.with_file_name(name)
        }
    }
}

/// Atomically write `body` to `path` (tmp sibling + rename), creating the
/// parent directory. No fsync, matching the Python `os.replace` path.
pub fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    write_inner(path, body, false)
}

/// Atomically write `body` to `path` with an `fsync` of the tmp file before
/// the rename, so the payload is durable on disk before it becomes visible.
pub fn write_atomic_fsync(path: &Path, body: &[u8]) -> std::io::Result<()> {
    write_inner(path, body, true)
}

fn write_inner(path: &Path, body: &[u8], fsync: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_sibling(path);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(body)?;
        if fsync {
            f.sync_all()?;
        }
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmp_sibling_matches_python_with_suffix() {
        // `.json` → `.json.tmp` (replaces the trailing extension, not appends).
        assert_eq!(
            tmp_sibling(Path::new("/etc/ados/ground-station-uplink.json")),
            PathBuf::from("/etc/ados/ground-station-uplink.json.tmp")
        );
        // No `.json` extension → append `.tmp`.
        assert_eq!(
            tmp_sibling(Path::new("/run/ados/uplink-active")),
            PathBuf::from("/run/ados/uplink-active.tmp")
        );
    }

    #[test]
    fn write_atomic_round_trips_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ground-station-uplink.json");
        write_atomic(&path, br#"{"priority":["eth0"]}"#).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            br#"{"priority":["eth0"]}"#.to_vec()
        );
        assert!(!dir.path().join("ground-station-uplink.json.tmp").exists());
    }

    #[test]
    fn write_atomic_fsync_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uplink-active");
        write_atomic_fsync(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello".to_vec());
    }
}
