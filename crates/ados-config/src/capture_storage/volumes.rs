//! Volume discovery: the mount table, sysfs block devices, and the capture
//! root `auto` resolves to.

use std::path::{Path, PathBuf};

use super::{CaptureStorageConfig, Volume, AUTO_MIN_FREE_BYTES, FALLBACK_ROOT};

/// Size and free space of one filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsSpace {
    pub total_bytes: u64,
    /// Space available to an unprivileged writer.
    pub free_bytes: u64,
}

/// The system paths discovery reads, injectable so a test can point every one
/// of them at a temporary tree.
#[derive(Debug, Clone)]
pub struct SysRoots {
    /// `/sys/block`.
    pub sys_block: PathBuf,
    /// `/proc/self/mounts`.
    pub mounts: PathBuf,
    /// The capture root when `auto` finds no NVMe volume.
    pub fallback_root: PathBuf,
    /// Filesystem space reader, `statvfs` on a node.
    pub stat: fn(&Path) -> Option<FsSpace>,
}

impl Default for SysRoots {
    fn default() -> Self {
        SysRoots {
            sys_block: PathBuf::from("/sys/block"),
            mounts: PathBuf::from("/proc/self/mounts"),
            fallback_root: PathBuf::from(FALLBACK_ROOT),
            stat: statvfs_space,
        }
    }
}

/// `statvfs` of the filesystem holding `path`. `None` when it cannot be read.
#[cfg(target_os = "linux")]
pub fn statvfs_space(path: &Path) -> Option<FsSpace> {
    let st = nix::sys::statvfs::statvfs(path).ok()?;
    let block = st.fragment_size() as u64;
    Some(FsSpace {
        total_bytes: (st.blocks() as u64).saturating_mul(block),
        free_bytes: (st.blocks_available() as u64).saturating_mul(block),
    })
}

#[cfg(not(target_os = "linux"))]
pub fn statvfs_space(_path: &Path) -> Option<FsSpace> {
    None
}

/// Filesystem types that never hold capture data even when backed by a device.
const SKIP_FS: &[&str] = &[
    "tmpfs",
    "devtmpfs",
    "ramfs",
    "proc",
    "sysfs",
    "overlay",
    "squashfs",
    "cgroup",
    "cgroup2",
    "devpts",
    "mqueue",
    "debugfs",
    "tracefs",
    "securityfs",
    "pstore",
    "bpf",
    "configfs",
    "fusectl",
    "hugetlbfs",
    "autofs",
    "binfmt_misc",
    "efivarfs",
    "nsfs",
    "rpc_pipefs",
];

/// Device-name prefixes that are not persistent storage.
const SKIP_DEVICES: &[&str] = &["loop", "zram", "ram"];

struct MountEntry {
    device: String,
    mount: String,
    fs: String,
}

/// Undo the octal escapes the kernel applies to spaces, tabs, newlines and
/// backslashes in mount-table fields.
pub(super) fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if let Some(v) = field
                .get(i + 1..i + 4)
                .and_then(|oct| u8::from_str_radix(oct, 8).ok())
            {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_mounts(path: &Path) -> Vec<MountEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let device = unescape(f.next()?);
            let mount = unescape(f.next()?);
            let fs = f.next()?.to_string();
            Some(MountEntry { device, mount, fs })
        })
        .collect()
}

/// Kernel name of a `/dev/...` device, following a symlink such as
/// `/dev/root` or `/dev/disk/by-uuid/...` when it resolves.
fn device_name(device: &str) -> Option<String> {
    let path = Path::new(device);
    if !device.starts_with("/dev/") {
        return None;
    }
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolved
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// The whole disk under `/sys/block` that `name` is, or is a partition of.
fn disk_of(sys_block: &Path, name: &str) -> Option<String> {
    if sys_block.join(name).is_dir() {
        return Some(name.to_string());
    }
    std::fs::read_dir(sys_block)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|disk| sys_block.join(disk).join(name).is_dir())
}

/// `queue/rotational` of a disk; unreadable reads as `None`.
fn rotational(sys_block: &Path, disk: &str) -> Option<bool> {
    let text = std::fs::read_to_string(sys_block.join(disk).join("queue/rotational")).ok()?;
    match text.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// Every mounted block-device filesystem, in mount-table order, one entry per
/// device (a bind mount of an already-listed device is skipped). Pseudo,
/// in-memory and loop-backed filesystems are skipped, and so is a mount whose
/// space cannot be read.
pub fn list_volumes(roots: &SysRoots) -> Vec<Volume> {
    let mut out: Vec<Volume> = Vec::new();
    for entry in read_mounts(&roots.mounts) {
        if SKIP_FS.contains(&entry.fs.as_str()) {
            continue;
        }
        let Some(device) = device_name(&entry.device) else {
            continue;
        };
        if SKIP_DEVICES.iter().any(|p| device.starts_with(p)) {
            continue;
        }
        if out.iter().any(|v| v.device == device) {
            continue;
        }
        let Some(space) = (roots.stat)(Path::new(&entry.mount)) else {
            continue;
        };
        let rotational = disk_of(&roots.sys_block, &device)
            .and_then(|disk| rotational(&roots.sys_block, &disk))
            .unwrap_or(false);
        out.push(Volume {
            mount: entry.mount,
            device,
            fs: entry.fs,
            total_bytes: space.total_bytes,
            free_bytes: space.free_bytes,
            rotational,
        });
    }
    out
}

/// [`list_volumes`] over this host's real mount table, sysfs and `statvfs`.
pub fn list_volumes_live() -> Vec<Volume> {
    list_volumes(&SysRoots::default())
}

/// The volume holding `path`: the listed mount that is its longest prefix.
pub fn volume_for<'a>(path: &Path, volumes: &'a [Volume]) -> Option<&'a Volume> {
    volumes
        .iter()
        .filter(|v| path.starts_with(&v.mount))
        .max_by_key(|v| v.mount.len())
}

/// The capture root for `cfg`. An absolute `capture_root` is taken as given.
/// `auto` (or anything that is not an absolute path) picks the first mounted,
/// non-rotational NVMe disk under `/sys/block` with at least 64 GiB free and
/// writes to `<mount>/ados/capture`; a disk mounted at `/` uses the fallback
/// root, which then lives on it anyway. No such disk: the fallback root.
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
    auto_root(roots).unwrap_or_else(|| roots.fallback_root.clone())
}

fn auto_root(roots: &SysRoots) -> Option<PathBuf> {
    let mut disks: Vec<String> = std::fs::read_dir(&roots.sys_block)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("nvme"))
        .filter(|name| rotational(&roots.sys_block, name) == Some(false))
        .collect();
    disks.sort();
    let volumes = list_volumes(roots);
    disks.iter().find_map(|disk| {
        volumes
            .iter()
            .filter(|v| disk_of(&roots.sys_block, &v.device).as_deref() == Some(disk.as_str()))
            .find(|v| v.free_bytes >= AUTO_MIN_FREE_BYTES)
            .map(|v| {
                if v.mount == "/" {
                    roots.fallback_root.clone()
                } else {
                    Path::new(&v.mount).join("ados/capture")
                }
            })
    })
}
