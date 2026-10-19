//! The capture root: an explicit directory, or the `auto` choice of an NVMe
//! volume, which sticks once made.
//!
//! **Sticky.** Once `auto` has put captures on an NVMe volume, the choice is
//! persisted and kept for as long as that mount is present, even after its
//! free space drops under the 64 GiB selection threshold. A full NVMe volume
//! then reads `near_full` / `full` and new sessions are refused with
//! `E_STORAGE_FULL`; captures never silently move to the SD card's fallback
//! root, where they would fill the card the system boots from. Only when the
//! persisted mount is gone does `auto` choose again.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::volumes::{disk_of, list_volumes, rotational};
use super::{CaptureStorageConfig, SysRoots, AUTO_MIN_FREE_BYTES};

/// The persisted `auto` choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StickyRoot {
    /// Mount point of the chosen volume.
    pub mount: String,
    /// The capture root on it.
    pub root: String,
}

/// The persisted choice, when the file parses.
pub fn read_sticky(path: &Path) -> Option<StickyRoot> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_sticky(path: &Path, sticky: &StickyRoot) {
    let written = serde_json::to_vec(sticky)
        .map_err(std::io::Error::other)
        .and_then(|body| ados_protocol::sidecar::write_atomic(path, &body, 0o644));
    if let Err(e) = written {
        tracing::warn!(path = %path.display(), error = %e, "could not persist the capture root choice");
    }
}

/// The capture root for `cfg`. An absolute `capture_root` is taken as given.
/// `auto` (or anything that is not an absolute path) keeps the persisted NVMe
/// choice while its mount is present; otherwise it picks the first mounted,
/// non-rotational NVMe disk under `/sys/block` with at least 64 GiB free,
/// writes to `<mount>/ados/capture` and persists that choice. A disk mounted at
/// `/` uses the fallback root, which then lives on it anyway, and is not
/// persisted. No such disk: the fallback root.
///
/// Persisting is best-effort: a caller that cannot write the state file still
/// gets the right root for this call.
pub fn resolve_capture_root(cfg: &CaptureStorageConfig, roots: &SysRoots) -> PathBuf {
    let raw = cfg.capture_root.trim();
    if raw != "auto" {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            return path;
        }
        tracing::warn!(
            capture_root = raw,
            "storage.capture_root is neither auto nor an absolute path; resolving as auto"
        );
    }
    let volumes = list_volumes(roots);
    let sticky = read_sticky(&roots.sticky_file);
    if let Some(s) = &sticky {
        if volumes.iter().any(|v| v.mount == s.mount) {
            return PathBuf::from(&s.root);
        }
    }
    match auto_choice(roots, &volumes) {
        Some(choice) if choice.mount == "/" => roots.fallback_root.clone(),
        Some(choice) => {
            if sticky.as_ref() != Some(&choice) {
                write_sticky(&roots.sticky_file, &choice);
            }
            PathBuf::from(choice.root)
        }
        None => roots.fallback_root.clone(),
    }
}

fn auto_choice(roots: &SysRoots, volumes: &[super::Volume]) -> Option<StickyRoot> {
    let mut disks: Vec<String> = std::fs::read_dir(&roots.sys_block)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("nvme"))
        .filter(|name| rotational(&roots.sys_block, name) == Some(false))
        .collect();
    disks.sort();
    disks.iter().find_map(|disk| {
        volumes
            .iter()
            .filter(|v| disk_of(&roots.sys_block, &v.device).as_deref() == Some(disk.as_str()))
            .find(|v| v.free_bytes >= AUTO_MIN_FREE_BYTES)
            .map(|v| StickyRoot {
                mount: v.mount.clone(),
                root: Path::new(&v.mount)
                    .join("ados/capture")
                    .display()
                    .to_string(),
            })
    })
}
