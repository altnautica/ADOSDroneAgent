//! Owner, group and mode of the capture root.
//!
//! A capture plugin runs as its own system user and joins
//! [`ados_protocol::ipc::CAPTURE_GROUP`] only while it holds
//! `capture.storage.write`. The root is `2775 root:ados-capture`: group
//! writable so that plugin can create its session directories, setgid so every
//! session created under it stays in the capture group. Only the root itself is
//! corrected; the sessions under it keep what their writer gave them.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Mode of the capture root: setgid, rwx for owner and group, r-x for others.
pub const CAPTURE_ROOT_MODE: u32 = 0o2775;

/// Hands a directory to `root:<gid>`. Production passes [`chown_to_root`];
/// tests pass a recorder so they run unprivileged.
pub type ChownFn<'a> = &'a dyn Fn(&Path, u32) -> io::Result<()>;

/// What [`prepare_capture_root`] changed on the root.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RootFix {
    /// The owner or group was not `root:<capture group>` and was handed over.
    pub chowned: bool,
    /// The mode was not [`CAPTURE_ROOT_MODE`] and was set.
    pub chmodded: bool,
}

/// Create `root` (`mkdir -p`) and bring it to `2775 root:<gid>`. With `gid`
/// `None` (the capture group is absent, a development host) the directory is
/// only created: widening it to group write for a group that does not exist
/// would hand it to whatever group it happens to carry. The owner is fixed
/// before the mode, so the setgid bit is set last and nothing clears it.
pub fn prepare_capture_root(root: &Path, gid: Option<u32>, chown: ChownFn) -> io::Result<RootFix> {
    std::fs::create_dir_all(root)?;
    let mut fix = RootFix::default();
    let Some(gid) = gid else {
        return Ok(fix);
    };
    let meta = std::fs::metadata(root)?;
    if meta.uid() != 0 || meta.gid() != gid {
        chown(root, gid)?;
        fix.chowned = true;
    }
    let mode = std::fs::metadata(root)?.permissions().mode() & 0o7777;
    if mode != CAPTURE_ROOT_MODE {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(CAPTURE_ROOT_MODE))?;
        fix.chmodded = true;
    }
    Ok(fix)
}

/// The capture group's gid, when the group exists on this host.
#[cfg(target_os = "linux")]
pub fn capture_group_gid() -> Option<u32> {
    nix::unistd::Group::from_name(ados_protocol::ipc::CAPTURE_GROUP)
        .ok()
        .flatten()
        .map(|g| g.gid.as_raw())
}

/// The capture group's gid. Never present off Linux.
#[cfg(not(target_os = "linux"))]
pub fn capture_group_gid() -> Option<u32> {
    None
}

/// The production [`ChownFn`]: `chown root:<gid>` (needs root).
#[cfg(target_os = "linux")]
pub fn chown_to_root(path: &Path, gid: u32) -> io::Result<()> {
    nix::unistd::chown(
        path,
        Some(nix::unistd::Uid::from_raw(0)),
        Some(nix::unistd::Gid::from_raw(gid)),
    )
    .map_err(io::Error::from)
}

/// The production [`ChownFn`]. Off Linux there is no capture group to hand
/// the root to.
#[cfg(not(target_os = "linux"))]
pub fn chown_to_root(_path: &Path, _gid: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "capture root ownership is Linux only",
    ))
}

/// [`prepare_capture_root`] with the host's capture group and the real chown.
pub fn prepare_capture_root_on_host(root: &Path) -> io::Result<RootFix> {
    prepare_capture_root(root, capture_group_gid(), &chown_to_root)
}

#[cfg(test)]
#[path = "root_access_tests.rs"]
mod tests;
