//! Progress bars and the elapsed clock for the full-screen view.

use std::time::Duration;

use crate::fmt::fmt_bytes;
use crate::progress::model::Group;
use crate::render;
use crate::theme::Theme;

/// The detail pane's progress bar line: byte progress for a download, else the
/// sub-progress bar, else blank.
pub(super) fn bar_line(theme: &Theme, g: &Group, width: usize) -> String {
    if let Some((done, total, label)) = &g.bytes {
        let size = if *total > 0 {
            format!("{} / {}", fmt_bytes(*done), fmt_bytes(*total))
        } else {
            fmt_bytes(*done)
        };
        let count = g
            .sub
            .map(|(d, t)| format!("  {}", theme.dim(&format!("({d}/{t})"))))
            .unwrap_or_default();
        let bar = theme.accent(&wide_bar(theme, *done, (*total).max(1)));
        return render::fit_to(
            &format!("{bar} {} {}{count}", theme.heading(label), theme.dim(&size)),
            width,
        );
    }
    if let Some((done, total)) = g.sub {
        return render::fit_to(&theme.accent(&wide_bar(theme, done, total)), width);
    }
    " ".repeat(width)
}

/// A compact 8-cell bar for the checklist token: `▕████░░░░▏ 50%`.
pub(super) fn short_bar(done: u64, total: u64) -> String {
    bar_glyphs(done, total, 8, ["▕", "▏", "█", "░"])
}

/// A wide 16-cell bar for the detail pane.
fn wide_bar(theme: &Theme, done: u64, total: u64) -> String {
    if theme.ascii {
        bar_glyphs(done, total, 16, ["[", "]", "#", "."])
    } else {
        bar_glyphs(done, total, 16, ["▕", "▏", "█", "░"])
    }
}

/// `cells` wide, with `[left, right, fill, empty]` glyphs, then the percentage.
fn bar_glyphs(done: u64, total: u64, cells: u64, [l, r, fill, empty]: [&str; 4]) -> String {
    let frac = done
        .saturating_mul(cells)
        .checked_div(total)
        .unwrap_or(0)
        .min(cells);
    let pct = done
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100);
    format!(
        "{l}{}{}{r} {pct}%",
        fill.repeat(frac as usize),
        empty.repeat((cells - frac) as usize)
    )
}

/// A total-elapsed `M:SS` clock.
pub(super) fn fmt_clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_clock_is_mm_ss() {
        assert_eq!(fmt_clock(Duration::from_secs(5)), "0:05");
        assert_eq!(fmt_clock(Duration::from_secs(252)), "4:12");
    }

    #[test]
    fn bars_clamp_and_report_percent() {
        assert_eq!(short_bar(0, 0), "▕░░░░░░░░▏ 0%");
        assert_eq!(short_bar(4, 8), "▕████░░░░▏ 50%");
        assert_eq!(short_bar(20, 8), "▕████████▏ 100%");
    }
}
