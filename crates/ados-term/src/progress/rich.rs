//! The rich renderer: a live, pinned status block with logs scrolling above it.
//!
//! Implemented as a manual crossterm "sticky block": it only ever *writes* to
//! stderr (move-up + clear-down + reprint), so it never queries the cursor
//! position or enables raw mode — the parts that break when stdin is a pipe
//! under `curl … | sudo bash`. A render-only design also means no event loop and
//! no stdin at all. The spinner animates on a recv timeout so a long step still
//! shows life.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use ratatui::crossterm::{
    cursor,
    style::Print,
    terminal::{Clear, ClearType},
};

use crate::fmt::fmt_dur;
use crate::progress::events::{ProgressEvent, ProgressSpec, SummaryView};
use crate::progress::model::{GStatus, Group, Model};
use crate::render::truncate;
use crate::theme::Theme;

/// Spinner cadence + log-ring depth.
const TICK_MS: u64 = 120;
const LOG_RING_CAP: usize = 12;

/// What one block repaint needs.
struct Block<'a> {
    model: &'a Model,
    theme: &'a Theme,
    spec: &'a ProgressSpec,
    spinner: usize,
}

/// Run the rich renderer to completion. Consumes events until `Finished`.
pub fn run(rx: Receiver<ProgressEvent>, theme: Theme, header: String, spec: ProgressSpec) {
    let stderr = io::stderr();
    let mut w = stderr.lock();
    let _ = ratatui::crossterm::execute!(w, cursor::Hide);
    let _ = writeln!(w, "{}", theme.accent(&header));
    let _ = w.flush();

    let mut model = Model::new(spec.groups);
    let mut spinner = 0usize;
    let mut height = 0usize;
    let mut logs: VecDeque<String> = VecDeque::with_capacity(LOG_RING_CAP);
    let mut summary: Option<Box<dyn SummaryView>> = None;
    let first = Block {
        model: &model,
        theme: &theme,
        spec: &spec,
        spinner,
    };
    redraw(&mut w, &first, None, &mut height);

    loop {
        let mut log_line: Option<String> = None;
        match rx.recv_timeout(Duration::from_millis(TICK_MS)) {
            Ok(ProgressEvent::StepStarted { id }) => {
                model.start(&id);
            }
            Ok(ProgressEvent::StepResult { id, outcome }) => {
                model.record(&id, &outcome);
            }
            Ok(ProgressEvent::SubProgress { id, done, total }) => {
                model.set_sub(&id, done, total);
            }
            Ok(ProgressEvent::Activity { message: line, .. })
            | Ok(ProgressEvent::SubLog { line, .. })
            | Ok(ProgressEvent::Log { line, .. }) => {
                push_log(&mut logs, line.clone());
                log_line = Some(line);
            }
            // The k-of-N `SubProgress` already drives this fallback tier's bar;
            // the byte-level detail is a full-screen-only refinement.
            Ok(ProgressEvent::ByteProgress { .. }) => continue,
            Ok(ProgressEvent::Summary(s)) => {
                summary = Some(s);
                continue;
            }
            Ok(ProgressEvent::Finished) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => spinner = spinner.wrapping_add(1),
        }
        let block = Block {
            model: &model,
            theme: &theme,
            spec: &spec,
            spinner,
        };
        redraw(&mut w, &block, log_line.as_deref(), &mut height);
    }

    // Erase the live block entirely before printing the final summary.
    if height > 0 {
        rewind(&mut w, height);
    }
    let _ = ratatui::crossterm::execute!(w, cursor::Show);
    if let Some(s) = summary {
        for line in s.rich_lines(&theme, &logs) {
            let _ = writeln!(w, "{line}");
        }
    }
    let _ = w.flush();
}

/// Box width: terminal columns, clamped to a tidy range.
fn box_width() -> usize {
    let cols = ratatui::crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80);
    cols.saturating_sub(2).clamp(40, 64)
}

/// Redraw the pinned block in place (move to its top, clear, reprint). A
/// `log` line first scrolls into the permanent history above the block.
fn redraw(w: &mut impl Write, block: &Block, log: Option<&str>, height: &mut usize) {
    let width = box_width();
    rewind(w, *height);
    if let Some(line) = log {
        let text = truncate(line, width.saturating_sub(2));
        let _ = ratatui::crossterm::queue!(
            w,
            Print(block.theme.dim(&format!("  {text}"))),
            Print("\r\n")
        );
    }
    let lines = block_lines(block, width);
    for l in &lines {
        let _ = ratatui::crossterm::queue!(w, Print(l), Print("\r\n"));
    }
    *height = lines.len();
    let _ = w.flush();
}

/// Move the cursor to the top of the current block and clear everything below.
fn rewind(w: &mut impl Write, height: usize) {
    if height > 0 {
        let _ = ratatui::crossterm::queue!(w, cursor::MoveToPreviousLine(height as u16));
    }
    let _ = ratatui::crossterm::queue!(w, Clear(ClearType::FromCursorDown));
    let _ = w.flush();
}

fn push_log(logs: &mut VecDeque<String>, line: String) {
    if logs.len() == LOG_RING_CAP {
        logs.pop_front();
    }
    logs.push_back(line);
}

/// The full block: top border, one row per group, bottom border.
fn block_lines(block: &Block, width: usize) -> Vec<String> {
    let theme = block.theme;
    let bc = theme.box_chars();
    let content_w = width.saturating_sub(2);
    let mut out = Vec::with_capacity(block.model.groups.len() + 2);
    out.push(top_border(block, content_w));
    for g in &block.model.groups {
        out.push(group_row(block, bc.v, content_w, g));
    }
    out.push(theme.accent(&format!("{}{}{}", bc.bl, bc.h.repeat(content_w), bc.br)));
    out
}

/// `╭─ <title> ──── step 4/10 ─╮`
fn top_border(block: &Block, content_w: usize) -> String {
    let bc = block.theme.box_chars();
    let counter = format!("step {}/{}", block.model.finalized(), block.model.total());
    let lead = format!("{} {} ", bc.h, block.spec.rich_title);
    let tail = format!(" {} {}", counter, bc.h);
    let dashes = content_w.saturating_sub(lead.chars().count() + tail.chars().count());
    let inner = format!("{}{}{}", lead, bc.h.repeat(dashes), tail);
    block.theme.accent(&format!("{}{}{}", bc.tl, inner, bc.tr))
}

/// One group row: `│ ✓ Building radio stack            0:42 │`.
fn group_row(block: &Block, vbar: &str, content_w: usize, g: &Group) -> String {
    let theme = block.theme;
    let body_w = content_w.saturating_sub(2); // the 1-space pad on each side
    let glyph_plain = glyph_for(theme, g.status, block.spinner);
    let glyph_colored = color_glyph(theme, g.status, glyph_plain);

    let detail_plain = detail_for(theme, g, block.spec.skipped);
    let detail_w = detail_plain.chars().count();
    // Reserve: glyph(1) + space(1) + at least one gap before the detail.
    let label_max = body_w.saturating_sub(2 + detail_w + 1);
    let label = truncate(g.label, label_max);
    let used = 2 + label.chars().count() + detail_w;
    let pad = body_w.saturating_sub(used);
    let detail_colored = if g.status == GStatus::Running && g.sub.is_some() {
        theme.accent(&detail_plain)
    } else {
        theme.dim(&detail_plain)
    };

    let body = format!("{glyph_colored} {label}{}{detail_colored}", " ".repeat(pad));
    let v = theme.accent(vbar);
    format!("{v} {body} {v}")
}

fn glyph_for(theme: &Theme, status: GStatus, spinner: usize) -> &'static str {
    match status {
        GStatus::Ok | GStatus::Skipped => theme.glyph_ok(),
        GStatus::Failed => theme.glyph_fail(),
        GStatus::Running => theme.spinner(spinner),
        GStatus::Pending => theme.glyph_pending(),
    }
}

fn color_glyph(theme: &Theme, status: GStatus, glyph: &str) -> String {
    match status {
        GStatus::Ok => theme.ok(glyph),
        GStatus::Failed => theme.fail(glyph),
        GStatus::Running => theme.accent(glyph),
        GStatus::Skipped | GStatus::Pending => theme.dim(glyph),
    }
}

/// The right-aligned detail (plain text; width is measured for padding).
fn detail_for(theme: &Theme, g: &Group, skipped: &str) -> String {
    match g.status {
        GStatus::Ok | GStatus::Failed => g.elapsed.map(fmt_dur).unwrap_or_default(),
        GStatus::Skipped => skipped.to_string(),
        GStatus::Running => match g.sub {
            Some((done, total)) => progress_bar(theme, done, total),
            None => g.elapsed_now().map(fmt_dur).unwrap_or_default(),
        },
        GStatus::Pending => String::new(),
    }
}

/// `▕██████░░▏ 63%` (or `[######..] 63%` in the ASCII tier).
fn progress_bar(theme: &Theme, done: u64, total: u64) -> String {
    const CELLS: u64 = 8;
    // checked_div yields None on a zero total (nothing known yet → empty bar).
    let frac = done
        .saturating_mul(CELLS)
        .checked_div(total)
        .unwrap_or(0)
        .min(CELLS);
    let pct = done
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100);
    let (l, r, fill, empty) = if theme.ascii {
        ("[", "]", "#", ".")
    } else {
        ("▕", "▏", "█", "░")
    };
    format!(
        "{l}{}{}{r} {pct}%",
        fill.repeat(frac as usize),
        empty.repeat((CELLS - frac) as usize)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::StepOutcome;
    use crate::progress::TEST_SPEC;
    use crate::theme::ColorTier;

    fn theme() -> Theme {
        Theme {
            ascii: false,
            tier: ColorTier::None,
        }
    }

    #[test]
    fn block_lines_count_is_groups_plus_borders() {
        let m = Model::new(TEST_SPEC.groups);
        let t = theme();
        let block = Block {
            model: &m,
            theme: &t,
            spec: &TEST_SPEC,
            spinner: 0,
        };
        let lines = block_lines(&block, 58);
        assert_eq!(lines.len(), m.groups.len() + 2);
        assert!(lines[0].contains(TEST_SPEC.rich_title));
    }

    #[test]
    fn rows_fit_the_width_when_color_is_off() {
        // With color off there are no SGR escapes, so chars() == display width.
        let mut m = Model::new(TEST_SPEC.groups);
        m.record("deps", &StepOutcome::Ok);
        m.record("mac_pin", &StepOutcome::Skipped);
        let t = theme();
        let block = Block {
            model: &m,
            theme: &t,
            spec: &TEST_SPEC,
            spinner: 0,
        };
        let width = 58;
        for line in block_lines(&block, width) {
            assert_eq!(
                line.chars().count(),
                width,
                "line not exactly width {width}: {line:?}"
            );
        }
    }

    #[test]
    fn skipped_group_shows_the_flow_wording() {
        let mut m = Model::new(TEST_SPEC.groups);
        m.record("deps", &StepOutcome::Skipped);
        let t = theme();
        let block = Block {
            model: &m,
            theme: &t,
            spec: &TEST_SPEC,
            spinner: 0,
        };
        let joined = block_lines(&block, 64).join("\n");
        assert!(joined.contains(TEST_SPEC.skipped), "{joined}");
    }

    #[test]
    fn progress_bar_clamps_and_formats() {
        let t = theme();
        assert!(progress_bar(&t, 0, 0).contains("0%"));
        assert!(progress_bar(&t, 5, 10).contains("50%"));
        let full = progress_bar(&t, 10, 10);
        assert!(full.contains("100%"));
    }
}
