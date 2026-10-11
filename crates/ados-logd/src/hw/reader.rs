//! Shared filesystem read helpers for the hardware collector.
//!
//! Every signal reader takes an injectable filesystem root so the same code
//! reads the real `/sys` and `/proc` on a board and a fixture tree under a
//! tempdir in a unit test. The root defaults to `/` in production; a test points
//! it at a directory containing a sample `sys/...` and `proc/...` subtree.
//!
//! The helpers are deliberately small and allocation-light: each value file in
//! `/sys` is a few ASCII bytes, and the collector reads many of them per tick.
//! All reads are best-effort: a missing or unreadable node returns `None` (the
//! signal is recorded as absent for that tick) rather than an error, so a board
//! that lacks a node never aborts a sample.

use std::path::{Path, PathBuf};

/// Join an absolute-style sysfs/proc path onto the injectable root.
///
/// The collector knows canonical paths as absolute strings
/// (`/sys/class/thermal`, `/proc/stat`). Under a root of `/` they resolve to the
/// real kernel paths; under a test root they resolve inside the fixture. The
/// leading slash is stripped before joining so `root.join("sys/...")` lands
/// inside the root rather than escaping to the filesystem root.
pub fn under(root: &Path, abs: &str) -> PathBuf {
    root.join(abs.trim_start_matches('/'))
}

/// Read a small text value file and return its trimmed contents, or `None` when
/// the file is absent or unreadable. The trim drops the trailing newline sysfs
/// always appends.
pub fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Read a value file and parse it as an `i64`, or `None` on absence / a
/// non-integer body. Used for the raw integer files sysfs exposes
/// (millidegrees, kHz, byte counters).
pub fn read_i64(path: &Path) -> Option<i64> {
    read_trimmed(path).and_then(|s| s.parse::<i64>().ok())
}

/// Read a value file and parse it as a `u64`, or `None` on absence / a
/// non-integer body.
pub fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path).and_then(|s| s.parse::<u64>().ok())
}

/// Read a value file and parse it as a `u32`, or `None`.
pub fn read_u32(path: &Path) -> Option<u32> {
    read_trimmed(path).and_then(|s| s.parse::<u32>().ok())
}

/// List the immediate entry names under a directory, sorted for deterministic
/// iteration order. Returns an empty vector when the directory is absent or
/// unreadable, so a board without a given sysfs class is simply skipped.
pub fn list_dir(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(path) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

#[cfg(test)]
mod tests;
