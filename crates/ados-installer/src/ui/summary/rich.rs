//! The rich (framed, colored) closing screen and its full-screen placement.

use std::collections::VecDeque;

use ados_term::render::{fit_to, truncate, visible_width};
use ados_term::theme::Theme;

use super::*;

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

/// The closing card centered full-screen on a `cols × rows` charcoal field,
/// with a dismiss hint below it. The full-screen renderer draws this inside the
/// alternate screen (the plain and rich renderers print [`rich_lines`] /
/// [`plain_lines`] inline instead). Every returned line is exactly `cols`
/// visible columns so the caller's background paint fills the whole screen.
pub fn fullscreen_grid(
    s: &SummaryData,
    theme: &Theme,
    journal: &VecDeque<String>,
    cols: usize,
    rows: usize,
) -> Vec<String> {
    let card = rich_lines(s, theme, journal);
    let hint = theme.dim("Press Enter to finish");

    // The centered block is the card, a blank spacer, and the hint line.
    let card_w = card.iter().map(|l| visible_width(l)).max().unwrap_or(0);
    let block_h = card.len() + 2;
    let top = rows.saturating_sub(block_h) / 2;
    let card_left = cols.saturating_sub(card_w) / 2;
    let hint_left = cols.saturating_sub(visible_width(&hint)) / 2;

    let mut grid = vec![" ".repeat(cols); rows];
    for (i, line) in card.iter().enumerate() {
        place(&mut grid, top + i, card_left, line, cols);
    }
    place(&mut grid, top + card.len() + 1, hint_left, &hint, cols);
    grid
}

/// Write `line` into `grid[row]`, left-padded by `left` and fitted to `cols`
/// visible columns. A row past the grid is a no-op (the card is clipped, not a
/// panic, on a short terminal).
fn place(grid: &mut [String], row: usize, left: usize, line: &str, cols: usize) {
    if let Some(slot) = grid.get_mut(row) {
        *slot = fit_to(&format!("{}{}", " ".repeat(left), line), cols);
    }
}

fn success_card(s: &SummaryData, theme: &Theme, width: usize) -> Vec<String> {
    let installed = if s.status == "degraded" {
        "installed with warnings"
    } else {
        "installed"
    };
    let title = format!("{} ADOS Drone Agent {installed}", theme.glyph_ok());

    let mut body = Vec::new();
    // Dim meta lines: version + profile, then board + device + pairing.
    body.push(theme.dim(&format!("v{} · profile: {}", s.version, s.profile)));
    body.push(theme.dim(&format!(
        "board: {} · device: {} · {}",
        s.board,
        s.device_id,
        if s.paired { "paired" } else { "not paired" }
    )));
    if s.status == "degraded" && !s.failed_steps.is_empty() {
        body.push(theme.warn(&format!("warnings: {}", s.failed_steps.join(", "))));
    }

    // Reach block: mDNS `.local` host first, then one LAN IP per line.
    body.push(String::new());
    body.push(section(theme, "Open the console"));
    for url in console_urls(s) {
        body.push(format!("{}  {}", theme.accent(arrow(theme)), url));
    }

    // The access point's own credentials, on a profile that runs one.
    let ap = ap_lines(s);
    if !ap.is_empty() {
        body.push(String::new());
        body.push(section(theme, "Wi-Fi access point"));
        for (label, value) in &ap {
            let padded = format!("{label:<11}");
            body.push(format!("  {}  {}", theme.dim(&padded), theme.accent(value)));
        }
        body.push(theme.dim("This passphrase is unique to this unit. Write it down."));
    }

    // A ground station on somebody's own computer: the same reassurance the
    // plain card carries, in place of the access-point block it does not have.
    if s.is_personal_host() {
        body.push(String::new());
        body.push(section(theme, PERSONAL_HEADING));
        body.push(format!(
            "  {}  {}",
            theme.accent(arrow(theme)),
            theme.bold(PERSONAL_REASSURANCE)
        ));
        body.push(theme.dim(PERSONAL_REACH));
        if !s.host_basis.is_empty() {
            let padded = "detected".to_string();
            body.push(format!(
                "  {}  {}",
                theme.dim(&format!("{padded:<11}")),
                theme.dim(&s.host_basis)
            ));
        }
    }

    // A staged overlay means the hardware it provisions does not work yet, so it
    // outranks the next-step commands. Warned, not merely noted: on an appliance
    // the installer finishes it, on a personal computer the owner has to.
    if !s.pending_reboot.is_empty() {
        body.push(String::new());
        body.push(section(theme, "Reboot needed to finish"));
        for reason in &s.pending_reboot {
            body.push(theme.warn(&format!("  {reason}")));
        }
    }
    if !s.deferred_reboot.is_empty() {
        body.push(String::new());
        body.push(section(theme, "Reboot when convenient"));
        for reason in &s.deferred_reboot {
            body.push(theme.warn(&format!("  {reason}")));
        }
        body.push(theme.dim("This install left your computer up; reboot it whenever suits you."));
    }

    // Curated primitive commands: command in accent, description dimmed.
    body.push(String::new());
    body.push(section(theme, "Next steps"));
    let cmd_w = next_step_cmd_width();
    for &(cmd, desc) in NEXT_STEPS {
        let padded = format!("{cmd:<cmd_w$}");
        body.push(format!("  {}  {}", theme.accent(&padded), theme.dim(desc)));
    }

    // Notes: local-mode caveat + docs pointer.
    body.push(String::new());
    body.push(section(theme, "Notes"));
    body.push(theme.dim(NOTE_LOCAL_MODE));
    body.push(theme.dim(NOTE_DIRECT_IP));
    body.push(theme.dim(NOTE_MDNS));
    body.push(theme.dim(NOTE_DOCS));

    frame(theme, Border::Ok, &title, &body, width)
}

/// An accent section marker + bold label: `▌ Open the console`. This minimal
/// `▌` word-mark is the only logo in the card — no block ASCII art.
fn section(theme: &Theme, label: &str) -> String {
    format!("{} {}", theme.accent(marker(theme)), theme.bold(label))
}

/// The section marker glyph (`▌`), with an ASCII fallback.
fn marker(theme: &Theme) -> &'static str {
    if theme.ascii {
        "#"
    } else {
        "▌"
    }
}

/// The reach arrow glyph (`➜`), with an ASCII fallback.
fn arrow(theme: &Theme) -> &'static str {
    if theme.ascii {
        "->"
    } else {
        "➜"
    }
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
    for reason in &s.failure_reasons {
        body.push(format!("reason:      {reason}"));
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
        out.push(format!("{v} {} {v}", fit_to(line, body_w)));
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
