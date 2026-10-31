//! Compact human-readable formatting for progress detail.

use std::time::Duration;

/// Human-readable byte size: `4.2 MB`, `812 KB`, `900 B`.
pub fn fmt_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.0} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

/// Format a duration as `M:SS` or `S.s` for compact display.
pub fn fmt_dur(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{}.{}s", secs, d.subsec_millis() / 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_bytes_scales() {
        assert_eq!(fmt_bytes(900), "900 B");
        assert_eq!(fmt_bytes(2048), "2 KB");
        assert_eq!(fmt_bytes(4_404_019), "4.2 MB");
        assert_eq!(fmt_bytes(3_221_225_472), "3.0 GB");
    }

    #[test]
    fn fmt_dur_forms() {
        assert_eq!(fmt_dur(Duration::from_millis(400)), "0.4s");
        assert_eq!(fmt_dur(Duration::from_secs(42)), "42.0s");
        assert_eq!(fmt_dur(Duration::from_secs(75)), "1:15");
    }
}
