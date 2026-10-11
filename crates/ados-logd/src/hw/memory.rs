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
    /// `Buffers` + `Cached` in bytes (the reclaimable page cache), when present.
    pub cache: Option<u64>,
    /// `SwapTotal` in bytes, when present.
    pub swap_total: Option<u64>,
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
    // `Buffers` and `Cached` together are the reclaimable page cache the operator
    // sees as "cache"; accumulate both into the one `cache` field.
    let (mut buffers, mut cached) = (None, None);
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
            "SwapTotal" => info.swap_total = Some(bytes),
            "Buffers" => buffers = Some(bytes),
            "Cached" => cached = Some(bytes),
            _ => {}
        }
    }
    if buffers.is_some() || cached.is_some() {
        info.cache = Some(buffers.unwrap_or(0).saturating_add(cached.unwrap_or(0)));
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
mod tests;
