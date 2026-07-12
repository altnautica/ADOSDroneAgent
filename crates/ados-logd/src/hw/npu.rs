//! NPU utilization reader (Rockchip RKNPU debugfs).
//!
//! Rockchip exposes the NPU load at `/sys/kernel/debug/rknpu/load` as a line of
//! per-core percentages, e.g. `NPU load:  Core0: 15%, Core1:  0%, Core2:  0%,`
//! on a tri-core RK3588, or `NPU load:  43%,` on a single-core part. The node
//! utilization is the average across the cores. An absent file (no NPU, debugfs
//! not mounted, or no read permission) yields `None` — the honest "not
//! sampleable" reading, never a fabricated 0 (Rule 44). The Jetson `tegrastats`
//! path is a subprocess (this file-based collector's boundary), left as a
//! follow-up alongside the Pi-throttle subprocess exception.

use std::path::Path;

use super::reader::under;

/// The debugfs path the RKNPU driver publishes its per-core load at.
const RKNPU_LOAD: &str = "/sys/kernel/debug/rknpu/load";

/// The trailing run of ascii digits of `s` parsed as an f32 (the number that
/// precedes a `%`), or `None` when the segment has no trailing digits.
fn trailing_percent(s: &str) -> Option<f32> {
    let rev: String = s
        .bytes()
        .rev()
        .take_while(u8::is_ascii_digit)
        .map(|b| b as char)
        .collect();
    if rev.is_empty() {
        return None;
    }
    rev.chars().rev().collect::<String>().parse().ok()
}

/// Parse an RKNPU load line into an average utilization percent across its
/// per-core values, clamped to `0.0..=100.0`. Returns `None` when no percentage
/// parses (an empty / malformed file), so a missing reading stays absent rather
/// than reading as 0.
pub fn parse_rknpu_load(text: &str) -> Option<f32> {
    let mut sum = 0.0f32;
    let mut n = 0u32;
    // Each `%` is preceded by a core's percentage; the tail after the last `%`
    // (whitespace / a comma) has no trailing digits and is skipped.
    for seg in text.split('%') {
        if let Some(v) = trailing_percent(seg) {
            sum += v;
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    Some((sum / n as f32).clamp(0.0, 100.0))
}

/// Read the Rockchip NPU utilization percent from `root`'s RKNPU debugfs. `None`
/// when the file is absent / unreadable (no NPU, debugfs not mounted, or no
/// permission) or its contents do not parse.
pub fn read_npu_load_pct(root: &Path) -> Option<f32> {
    let path = under(root, RKNPU_LOAD);
    let text = std::fs::read_to_string(&path).ok()?;
    parse_rknpu_load(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_a_tri_core_line() {
        let s = "NPU load:  Core0: 15%, Core1:  0%, Core2: 30%,\n";
        // (15 + 0 + 30) / 3 = 15
        assert_eq!(parse_rknpu_load(s), Some(15.0));
    }

    #[test]
    fn parses_a_single_core_line() {
        assert_eq!(parse_rknpu_load("NPU load:  43%,\n"), Some(43.0));
    }

    #[test]
    fn clamps_and_averages() {
        // Two cores, one pegged.
        assert_eq!(parse_rknpu_load("Core0: 100%, Core1: 0%,"), Some(50.0));
    }

    #[test]
    fn a_line_with_no_percent_is_none() {
        assert_eq!(parse_rknpu_load("NPU load: unknown\n"), None);
        assert_eq!(parse_rknpu_load(""), None);
    }

    #[test]
    fn reads_load_under_an_injected_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join("sys/kernel/debug/rknpu/load");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "NPU load:  Core0: 20%, Core1: 40%,\n").unwrap();
        assert_eq!(read_npu_load_pct(root), Some(30.0));
    }

    #[test]
    fn absent_file_is_none_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_npu_load_pct(dir.path()), None);
    }
}
