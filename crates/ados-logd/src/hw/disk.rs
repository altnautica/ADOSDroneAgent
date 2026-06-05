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
mod tests {
    use super::*;
    use std::fs;

    // A realistic /proc/diskstats covering an eMMC whole device + partition, an
    // NVMe whole device + partition, an SD-style disk + partition, and a loop.
    const SAMPLE: &str = "\
 179       0 mmcblk0 12000 100 480000 5000 8000 50 320000 4000 0 6000 9000
 179       1 mmcblk0p1 5000 0 200000 2000 3000 0 120000 1500 0 3000 3500
 259       0 nvme0n1 90000 0 7200000 30000 60000 0 4800000 20000 0 40000 50000
 259       1 nvme0n1p1 1000 0 8000 100 500 0 4000 50 0 120 130
   8       0 sda 4000 0 160000 1800 2000 0 80000 900 0 2200 2500
   8       1 sda1 100 0 800 10 50 0 400 5 0 12 13
   7       0 loop0 0 0 0 0 0 0 0 0 0 0 0
";

    #[test]
    fn keeps_backing_devices_and_drops_partitions_and_loops() {
        let stats = parse_diskstats(SAMPLE);
        let names: Vec<&str> = stats.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["mmcblk0", "nvme0n1", "sda"]);
    }

    #[test]
    fn parses_the_right_counter_fields() {
        let stats = parse_diskstats(SAMPLE);
        let emmc = stats.iter().find(|s| s.name == "mmcblk0").unwrap();
        // field 5 (zero-based) = sectors read = 480000.
        assert_eq!(emmc.rd_sectors, 480_000);
        // field 9 = sectors written = 320000.
        assert_eq!(emmc.wr_sectors, 320_000);
        // field 12 = io_ms = 6000.
        assert_eq!(emmc.io_ms, 6000);
    }

    #[test]
    fn read_diskstats_from_a_fixture_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("proc")).unwrap();
        fs::write(dir.path().join("proc/diskstats"), SAMPLE).unwrap();
        let stats = read_diskstats(dir.path());
        assert_eq!(stats.len(), 3);
    }

    #[test]
    fn read_diskstats_is_empty_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_diskstats(dir.path()).is_empty());
    }

    #[test]
    fn short_lines_are_skipped() {
        // A truncated line (kernel without the extended fields) is dropped.
        assert!(parse_diskstats(" 8 0 sda 1 2 3\n").is_empty());
    }
}
