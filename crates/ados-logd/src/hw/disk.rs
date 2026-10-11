//! Per-device disk I/O reader from `/proc/diskstats`.
//!
//! `/proc/diskstats` carries one line per block device with cumulative counters.
//! The fields used here are (1-based field index after the major/minor/name):
//! field 3 = reads completed, field 5 = sectors read, field 7 = writes
//! completed, field 9 = sectors written, field 10 = milliseconds spent doing
//! I/O. These are monotonic counters; rates are derived at read time from
//! successive snapshots, so the raw cumulative values are recorded.
//!
//! Partition lines and zero-traffic pseudo-devices (loop, ram) are filtered so
//! the snapshot carries the real backing devices (`mmcblk0`, `nvme0n1`, `sda`).

use std::path::Path;

use super::reader::under;

/// Cumulative I/O counters for one block device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskStats {
    /// Device name (e.g. `mmcblk0`).
    pub name: String,
    /// Cumulative sectors read (512-byte sectors).
    pub rd_sectors: u64,
    /// Cumulative sectors written (512-byte sectors).
    pub wr_sectors: u64,
    /// Cumulative milliseconds spent doing I/O.
    pub io_ms: u64,
}

/// Read and parse `/proc/diskstats`, keeping the whole-device backing stores and
/// dropping partitions and pseudo-devices. Returns an empty vector when the file
/// is absent.
pub fn read_diskstats(root: &Path) -> Vec<DiskStats> {
    let path = under(root, "/proc/diskstats");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_diskstats(&text),
        Err(_) => Vec::new(),
    }
}

/// Parse the text of `/proc/diskstats`. Each line is
/// `major minor name reads_completed reads_merged sectors_read read_ms
/// writes_completed writes_merged sectors_written write_ms io_in_progress io_ms
/// weighted_io_ms ...`.
pub fn parse_diskstats(text: &str) -> Vec<DiskStats> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // Need at least through the io_ms field (index 12, zero-based).
        if f.len() < 13 {
            continue;
        }
        let name = f[2];
        if !is_backing_device(name) {
            continue;
        }
        let rd_sectors = f[5].parse::<u64>().unwrap_or(0);
        let wr_sectors = f[9].parse::<u64>().unwrap_or(0);
        let io_ms = f[12].parse::<u64>().unwrap_or(0);
        out.push(DiskStats {
            name: name.to_string(),
            rd_sectors,
            wr_sectors,
            io_ms,
        });
    }
    out
}

/// Filesystem used-capacity percentage of the mount containing `path`.
///
/// Unlike the per-device I/O counters above (which come from `/proc/diskstats`),
/// filesystem capacity is not exposed as a sysfs/proc text file, so this queries
/// the live mount via `statvfs`. It therefore reflects the real filesystem at
/// `path` (`/` in production), not a fixture tree. Returns `None` when the
/// syscall fails or the filesystem reports zero total blocks.
///
/// The percentage uses the `df`-style definition: `used / (used + available)`
/// where `used = total - free`, so reserved (root-only) blocks are excluded from
/// the denominator and a near-full unprivileged filesystem reads close to 100%.
#[cfg(target_os = "linux")]
pub fn read_fs_used_pct(path: &Path) -> Option<f64> {
    let st = nix::sys::statvfs::statvfs(path).ok()?;
    // Block counts stay in their native `fsblkcnt_t` type (no intermediate cast)
    // and only cross to f64 for the final ratio, so the width is portable across
    // 32- and 64-bit targets.
    let total = st.blocks();
    if total == 0 {
        return None;
    }
    let free = st.blocks_free();
    let avail = st.blocks_available();
    let used = total.saturating_sub(free);
    let denom = used.saturating_add(avail);
    if denom == 0 {
        return None;
    }
    Some(used as f64 / denom as f64 * 100.0)
}

/// Non-Linux stub: filesystem capacity is read via `statvfs` (a Linux-only
/// dependency for this crate), so the metric is gracefully absent off Linux.
#[cfg(not(target_os = "linux"))]
pub fn read_fs_used_pct(_path: &Path) -> Option<f64> {
    None
}

/// Filesystem total and used bytes of the mount containing `path`, via `statvfs`.
/// Block counts are in fragment-size units, so bytes is `blocks * f_frsize`. Used
/// matches the `df`/`psutil` definition (`total - free`, so reserved blocks count
/// as used). Returns `(total_bytes, used_bytes)`, or `None` when the syscall fails
/// or the filesystem reports zero total blocks. Live mount, not a fixture tree.
#[cfg(target_os = "linux")]
pub fn read_fs_usage(path: &Path) -> Option<(u64, u64)> {
    let st = nix::sys::statvfs::statvfs(path).ok()?;
    let frsize = st.fragment_size() as u64;
    let total_blocks = st.blocks() as u64;
    if total_blocks == 0 || frsize == 0 {
        return None;
    }
    let free = st.blocks_free() as u64;
    let total_bytes = total_blocks.saturating_mul(frsize);
    let used_bytes = total_blocks.saturating_sub(free).saturating_mul(frsize);
    Some((total_bytes, used_bytes))
}

/// Non-Linux stub: see [`read_fs_used_pct`].
#[cfg(not(target_os = "linux"))]
pub fn read_fs_usage(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Filesystem total and *available* bytes of the mount containing `path`, via
/// `statvfs`. Available is `f_bavail`: what an unprivileged writer can still
/// allocate, which is the honest headroom for the store (reserved root blocks
/// are not space the daemon can count on). Returns `(total_bytes,
/// available_bytes)`, or `None` when the syscall fails or the filesystem reports
/// zero total blocks. Live mount, not a fixture tree.
#[cfg(target_os = "linux")]
pub fn read_fs_space(path: &Path) -> Option<(u64, u64)> {
    let st = nix::sys::statvfs::statvfs(path).ok()?;
    let frsize = st.fragment_size() as u64;
    let total_blocks = st.blocks() as u64;
    if total_blocks == 0 || frsize == 0 {
        return None;
    }
    let avail = st.blocks_available() as u64;
    Some((
        total_blocks.saturating_mul(frsize),
        avail.saturating_mul(frsize),
    ))
}

/// Non-Linux stub: see [`read_fs_used_pct`].
#[cfg(not(target_os = "linux"))]
pub fn read_fs_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Keep whole backing devices; drop partitions and pseudo-devices.
///
/// Partitions end in a digit on top of a name that ends in a non-digit
/// (`sda1`, `nvme0n1p1`, `mmcblk0p1`); the eMMC/SD naming `mmcblk0` /
/// `nvme0n1` themselves end in a digit but are whole devices, so the rule keys
/// off the `p<part>` / trailing-partition convention rather than a bare trailing
/// digit. Loop, ram, and zram pseudo-devices are dropped outright.
fn is_backing_device(name: &str) -> bool {
    if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") {
        return false;
    }
    // `mmcblkNpM` / `nvmeNnMpK` partitions carry a `p<digits>` tail.
    if let Some(idx) = name.rfind('p') {
        let tail = &name[idx + 1..];
        if !tail.is_empty()
            && tail.bytes().all(|b| b.is_ascii_digit())
            && (name.contains("mmcblk") || name.contains("nvme"))
        {
            return false;
        }
    }
    // `sdaN` / `vdaN` style partitions: a trailing digit on a name whose
    // device stem is alphabetic (`sd`, `vd`, `hd`).
    let stem_is_alpha_disk =
        name.starts_with("sd") || name.starts_with("vd") || name.starts_with("hd");
    if stem_is_alpha_disk && name.bytes().last().is_some_and(|b| b.is_ascii_digit()) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests;
