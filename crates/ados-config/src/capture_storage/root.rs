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

use std::path::{Component, Path, PathBuf};

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

/// Trees no capture root may be in: the root is handed to the capture group
/// (`root:ados-capture`, group writable) and bound read-write into every
/// capture plugin, so any of these would hand a plugin the system or the
/// agent itself. `/var/ados` is the agent's state; only its `capture` subtree
/// may hold captures (see [`capture_root_refusal`]).
const REFUSED_TREES: &[&str] = &[
    "/etc",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/boot",
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/root",
    "/opt/ados",
    "/var/lib",
    "/var/log",
    "/var/cache",
    "/var/spool",
];

/// Shared parents that may hold a capture root but never be one.
const REFUSED_EXACT: &[&str] = &[
    "/", "/var", "/tmp", "/var/tmp", "/home", "/mnt", "/media", "/srv", "/opt",
];

/// The agent's state directory, and the one subtree of it a capture root may
/// be in.
const AGENT_STATE_DIR: &str = "/var/ados";
const AGENT_CAPTURE_TREE: &str = "/var/ados/capture";

/// Why `path` may not be a capture root, or `None` when it may: it must be
/// absolute, carry no `.` or `..` component, and stay out of the system and
/// agent directories (`REFUSED_TREES`, `REFUSED_EXACT`, and `/var/ados`
/// outside `/var/ados/capture`). The resolver, the janitor's owner and mode
/// fix and the plugin sandbox's capture-root bind all apply it.
pub fn capture_root_refusal(path: &Path) -> Option<&'static str> {
    if !path.is_absolute() {
        return Some("not an absolute path");
    }
    if path
        .components()
        .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Some("has a . or .. component");
    }
    if REFUSED_EXACT.iter().any(|p| path == Path::new(p)) {
        return Some("is a system directory");
    }
    if REFUSED_TREES.iter().any(|p| path.starts_with(p)) {
        return Some("is inside a system or agent directory");
    }
    if path.starts_with(AGENT_STATE_DIR) && !path.starts_with(AGENT_CAPTURE_TREE) {
        return Some("is inside the agent state directory");
    }
    None
}

/// The capture root for `cfg`. An explicit `capture_root` is taken as given
/// unless [`capture_root_refusal`] refuses it. `auto` (or a refused value,
/// which is logged) keeps the persisted NVMe choice while its mount is
/// present; otherwise it picks the first mounted, non-rotational NVMe disk
/// under `/sys/block` with at least 64 GiB free, writes to
/// `<mount>/ados/capture` and persists that choice. A disk mounted at `/` uses
/// the fallback root, which then lives on it anyway, and is not persisted. No
/// such disk: the fallback root. A persisted or chosen root the check refuses
/// is passed over.
///
/// Persisting is best-effort: a caller that cannot write the state file still
/// gets the right root for this call.
pub fn resolve_capture_root(cfg: &CaptureStorageConfig, roots: &SysRoots) -> PathBuf {
    let raw = cfg.capture_root.trim();
    if raw != "auto" {
        let path = PathBuf::from(raw);
        match capture_root_refusal(&path) {
            None => return path,
            Some(reason) => tracing::warn!(
                capture_root = raw,
                reason,
                "storage.capture_root is refused; resolving as auto"
            ),
        }
    }
    let volumes = list_volumes(roots);
    let sticky = read_sticky(&roots.sticky_file);
    if let Some(s) = &sticky {
        let allowed = capture_root_refusal(Path::new(&s.root)).is_none();
        if allowed && volumes.iter().any(|v| v.mount == s.mount) {
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
            .filter(|v| v.free_bytes >= AUTO_MIN_FREE_BYTES)
            .map(|v| StickyRoot {
                mount: v.mount.clone(),
                root: Path::new(&v.mount)
                    .join("ados/capture")
                    .display()
                    .to_string(),
            })
            .find(|choice| capture_root_refusal(Path::new(&choice.root)).is_none())
    })
}
