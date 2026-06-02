//! Memory and pressure-stall (PSI) readers.
//!
//! - `/proc/meminfo` — `MemTotal` / `MemAvailable` / `SwapFree` are reported in
//!   kibibytes; this reader returns them in bytes.
//! - `/proc/pressure/{cpu,memory,io}` — the kernel PSI interface, present only on
//!   kernels built with `CONFIG_PSI`. Each file has a `some` line and (for memory
//!   and io) a `full` line, each carrying `avg10` / `avg60` / `avg300` percentages
//!   and a `total` microsecond stall counter. A kernel without PSI simply has no
//!   files, so the reader returns `None` for that resource rather than an error.

use std::path::Path;

use super::reader::under;

/// Parsed `/proc/meminfo` values, all in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemInfo {
    /// `MemTotal` in bytes, when present.
    pub total: Option<u64>,
    /// `MemAvailable` in bytes, when present.
    pub available: Option<u64>,
    /// `SwapFree` in bytes, when present.
    pub swap_free: Option<u64>,
}

/// Read and parse `/proc/meminfo`. Returns a default (all-`None`) [`MemInfo`]
/// when the file is absent.
pub fn read_meminfo(root: &Path) -> MemInfo {
    let path = under(root, "/proc/meminfo");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_meminfo(&text),
        Err(_) => MemInfo::default(),
    }
}

/// Parse the text of `/proc/meminfo`. Each line is `Key: <value> kB`; the value
/// is in kibibytes and is converted to bytes. Unknown keys are ignored.
pub fn parse_meminfo(text: &str) -> MemInfo {
    let mut info = MemInfo::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        // The value is the first whitespace-separated token after the colon, in
        // kibibytes; convert to bytes.
        let kib = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok());
        let Some(kib) = kib else { continue };
        let bytes = kib.saturating_mul(1024);
        match key.trim() {
            "MemTotal" => info.total = Some(bytes),
            "MemAvailable" => info.available = Some(bytes),
            "SwapFree" => info.swap_free = Some(bytes),
            _ => {}
        }
    }
    info
}

/// One PSI resource's pressure: the `some` line averages plus the total stall.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pressure {
    /// `some avg10` percentage.
    pub avg10: f32,
    /// `some avg60` percentage.
    pub avg60: f32,
    /// `some avg300` percentage.
    pub avg300: f32,
    /// `some total` cumulative stall, microseconds.
    pub total_us: u64,
}

/// Read one PSI resource file (`cpu`, `memory`, or `io`) and parse its `some`
/// line. Returns `None` when the file is absent (no `CONFIG_PSI`) or the line is
/// unparseable.
pub fn read_pressure(root: &Path, resource: &str) -> Option<Pressure> {
    let path = under(root, &format!("/proc/pressure/{resource}"));
    let text = std::fs::read_to_string(&path).ok()?;
    parse_pressure_some(&text)
}

/// Parse the `some` line of a PSI file:
/// `some avg10=0.00 avg60=0.00 avg300=0.00 total=12345`. Returns `None` when no
/// `some` line is present.
pub fn parse_pressure_some(text: &str) -> Option<Pressure> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() != Some("some") {
            continue;
        }
        let mut avg10 = 0.0f32;
        let mut avg60 = 0.0f32;
        let mut avg300 = 0.0f32;
        let mut total_us = 0u64;
        for kv in parts {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            match k {
                "avg10" => avg10 = v.parse().unwrap_or(0.0),
                "avg60" => avg60 = v.parse().unwrap_or(0.0),
                "avg300" => avg300 = v.parse().unwrap_or(0.0),
                "total" => total_us = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        return Some(Pressure {
            avg10,
            avg60,
            avg300,
            total_us,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn parses_meminfo_in_bytes() {
        let text = "\
MemTotal:        8123456 kB
MemFree:          512000 kB
MemAvailable:    6543210 kB
Buffers:           12345 kB
SwapTotal:       1000000 kB
SwapFree:         999000 kB
";
        let info = parse_meminfo(text);
        assert_eq!(info.total, Some(8_123_456 * 1024));
        assert_eq!(info.available, Some(6_543_210 * 1024));
        assert_eq!(info.swap_free, Some(999_000 * 1024));
    }

    #[test]
    fn read_meminfo_from_a_fixture_root() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "proc/meminfo",
            "MemTotal: 1024 kB\nMemAvailable: 512 kB\n",
        );
        let info = read_meminfo(dir.path());
        assert_eq!(info.total, Some(1024 * 1024));
        assert_eq!(info.available, Some(512 * 1024));
        assert_eq!(info.swap_free, None);
    }

    #[test]
    fn read_meminfo_is_default_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_meminfo(dir.path()), MemInfo::default());
    }

    #[test]
    fn parses_psi_some_line() {
        let text = "\
some avg10=1.23 avg60=4.56 avg300=7.89 total=123456789
full avg10=0.10 avg60=0.20 avg300=0.30 total=42
";
        let p = parse_pressure_some(text).unwrap();
        assert!((p.avg10 - 1.23).abs() < 0.0001);
        assert!((p.avg60 - 4.56).abs() < 0.0001);
        assert!((p.avg300 - 7.89).abs() < 0.0001);
        assert_eq!(p.total_us, 123_456_789);
    }

    #[test]
    fn reads_psi_for_each_resource_from_a_fixture_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "proc/pressure/cpu",
            "some avg10=0.50 avg60=0.40 avg300=0.30 total=1000\n",
        );
        write(
            root,
            "proc/pressure/memory",
            "some avg10=2.00 avg60=1.00 avg300=0.50 total=2000\nfull avg10=1.0 avg60=0.5 avg300=0.2 total=900\n",
        );
        assert_eq!(read_pressure(root, "cpu").unwrap().total_us, 1000);
        assert_eq!(read_pressure(root, "memory").unwrap().total_us, 2000);
        // io is absent in this fixture.
        assert!(read_pressure(root, "io").is_none());
    }

    #[test]
    fn psi_is_none_on_a_kernel_without_the_interface() {
        // No /proc/pressure at all (CONFIG_PSI off).
        let dir = tempfile::tempdir().unwrap();
        assert!(read_pressure(dir.path(), "cpu").is_none());
        // A file with only a `full` line (no `some`) also yields None.
        assert!(parse_pressure_some("full avg10=0.0 total=0\n").is_none());
    }
}
