//! The closing screen: success card + first-run guide, or failure panel.
//!
//! Plain renderers print [`plain_lines`] verbatim. The rich renderer wraps the
//! same information in a styled box (see `rich.rs`), so the *content* is
//! identical whether or not the terminal supports color.

use std::collections::VecDeque;

use crate::ui::events::SummaryData;
use crate::ui::theme::Theme;

/// The next-step commands shown after a successful install. Product-facing copy
/// only — no internal tags.
pub fn next_steps(s: &SummaryData) -> Vec<String> {
    vec![
        "ados status".to_string(),
        "journalctl -t ados-installer".to_string(),
        format!(
            "Add this node in Mission Control by host: {}.local",
            s.hostname
        ),
        format!("Setup: {}", s.setup_url),
        "Docs:  https://docs.altnautica.com".to_string(),
    ]
}

/// The headline for the summary, e.g. `ADOS Drone Agent 0.51.4 installed`.
pub fn headline(s: &SummaryData) -> String {
    match s.status.as_str() {
        "failed" => "ADOS Drone Agent install failed".to_string(),
        "degraded" => format!("ADOS Drone Agent {} installed with warnings", s.version),
        _ => format!("ADOS Drone Agent {} installed", s.version),
    }
}

/// Plain, escape-free summary lines (machine-grep-friendly). Used by the plain
/// and quiet renderers, and as the fallback content of the rich card.
pub fn plain_lines(s: &SummaryData) -> Vec<String> {
    let mut out = Vec::new();
    out.push(headline(s));

    if s.status == "failed" {
        if !s.required_failures.is_empty() {
            out.push(format!("  failed step: {}", s.required_failures.join(", ")));
        }
        out.push("  full log:    journalctl -t ados-installer".to_string());
        out.push("  retry:       re-run the install one-liner".to_string());
        return out;
    }

    out.push(format!("  profile: {}    board: {}", s.profile, s.board));
    out.push(format!(
        "  device:  {}    {}",
        s.device_id,
        if s.paired { "paired" } else { "not paired" }
    ));
    if s.status == "degraded" && !s.failed_steps.is_empty() {
        out.push(format!("  warnings: {}", s.failed_steps.join(", ")));
    }
    out.push(String::new());
    out.push("Next steps:".to_string());
    for cmd in next_steps(s) {
        out.push(format!("  {cmd}"));
    }
    out
}

/// Which border color a frame uses.
enum Border {
    Ok,
    Fail,
}

/// Rich (framed, colored) closing screen for the rich renderer. `logs` is the
/// recent log ring, surfaced in the failure panel. The information matches
/// [`plain_lines`]; only the styling differs.
pub fn rich_lines(s: &SummaryData, theme: &Theme, logs: &VecDeque<String>) -> Vec<String> {
    let width = frame_width();
    if s.status == "failed" {
        failure_panel(s, theme, logs, width)
    } else {
        success_card(s, theme, width)
    }
}

fn success_card(s: &SummaryData, theme: &Theme, width: usize) -> Vec<String> {
    let title = format!("{} {}", theme.glyph_ok(), headline(s));
    let mut body = vec![
        format!("profile: {}    board: {}", s.profile, s.board),
        format!(
            "device:  {}    {}",
            s.device_id,
            if s.paired { "paired" } else { "not paired" }
        ),
    ];
    if s.status == "degraded" && !s.failed_steps.is_empty() {
        body.push(format!("warnings: {}", s.failed_steps.join(", ")));
    }
    body.push(String::new());
    body.push("Next steps".to_string());
    for cmd in next_steps(s) {
        body.push(format!("  {cmd}"));
    }
    frame(theme, Border::Ok, &title, &body, width)
}

fn failure_panel(
    s: &SummaryData,
    theme: &Theme,
    logs: &VecDeque<String>,
    width: usize,
) -> Vec<String> {
    let title = format!("{} {}", theme.glyph_fail(), headline(s));
    let mut body = Vec::new();
    if !s.required_failures.is_empty() {
        body.push(format!("failed step: {}", s.required_failures.join(", ")));
    }
    if !logs.is_empty() {
        body.push(String::new());
        body.push("recent log".to_string());
        let tail: Vec<&String> = logs.iter().rev().take(8).collect();
        for l in tail.into_iter().rev() {
            body.push(format!("  {l}"));
        }
    }
    body.push(String::new());
    body.push("full log: journalctl -t ados-installer".to_string());
    body.push("retry:    re-run the install one-liner".to_string());
    frame(theme, Border::Fail, &title, &body, width)
}

/// Wrap plain `body` lines in a colored rounded box with a titled top border.
fn frame(theme: &Theme, border: Border, title: &str, body: &[String], width: usize) -> Vec<String> {
    let bc = theme.box_chars();
    let content_w = width.saturating_sub(2);
    let body_w = content_w.saturating_sub(2);

    let lead = format!("{} {} ", bc.h, truncate(title, content_w.saturating_sub(4)));
    let dashes = content_w.saturating_sub(lead.chars().count() + 1);
    let top = format!("{}{}{}{}", bc.tl, lead, bc.h.repeat(dashes), bc.tr);

    let mut out = Vec::with_capacity(body.len() + 2);
    out.push(paint_border(theme, &border, &top));
    let v = paint_border(theme, &border, bc.v);
    for line in body {
        out.push(format!("{v} {} {v}", pad_to(line, body_w)));
    }
    out.push(paint_border(
        theme,
        &border,
        &format!("{}{}{}", bc.bl, bc.h.repeat(content_w), bc.br),
    ));
    out
}

fn paint_border(theme: &Theme, border: &Border, s: &str) -> String {
    match border {
        Border::Ok => theme.ok(s),
        Border::Fail => theme.fail(s),
    }
}

/// Card width: terminal columns clamped to a tidy range.
fn frame_width() -> usize {
    ratatui::crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .saturating_sub(2)
        .clamp(40, 64)
}

/// Truncate to `max` columns with an ellipsis when clipped.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Truncate then right-pad to exactly `w` columns (plain text).
fn pad_to(s: &str, w: usize) -> String {
    let t = truncate(s, w);
    let pad = w.saturating_sub(t.chars().count());
    format!("{t}{}", " ".repeat(pad))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(status: &str) -> SummaryData {
        SummaryData {
            status: status.to_string(),
            version: "0.51.4".to_string(),
            profile: "drone".to_string(),
            board: "Raspberry Pi 4 Model B".to_string(),
            device_id: "17bf646b".to_string(),
            hostname: "skynode".to_string(),
            setup_url: "http://skynode.local:8080/setup".to_string(),
            paired: true,
            failed_steps: vec![],
            required_failures: vec![],
        }
    }

    #[test]
    fn ok_summary_has_next_steps_and_host_hint() {
        let lines = plain_lines(&sample("ok"));
        assert!(lines[0].contains("0.51.4 installed"));
        assert!(lines.iter().any(|l| l.contains("Next steps:")));
        assert!(lines.iter().any(|l| l.contains("skynode.local")));
        assert!(lines.iter().any(|l| l.contains("ados status")));
    }

    #[test]
    fn failed_summary_points_at_the_log_and_failed_step() {
        let mut s = sample("failed");
        s.required_failures = vec!["systemd".to_string()];
        s.failed_steps = vec!["systemd".to_string()];
        let lines = plain_lines(&s);
        assert!(lines[0].contains("failed"));
        assert!(lines.iter().any(|l| l.contains("systemd")));
        assert!(lines
            .iter()
            .any(|l| l.contains("journalctl -t ados-installer")));
    }
}
