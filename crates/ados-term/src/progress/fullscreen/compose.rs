//! The pure full-screen compositor: model + terminal size in, `rows` lines of
//! exactly `cols` visible columns out.

use std::collections::VecDeque;
use std::time::Duration;

use super::bars::{bar_line, fmt_clock, short_bar};
use crate::fmt::fmt_dur;
use crate::frame::{self, TermSize};
use crate::progress::model::{GStatus, Group, Model};
use crate::render;
use crate::theme::Theme;

/// Below this width the split collapses to a single column.
const SPLIT_MIN_COLS: usize = 90;

/// Everything the compositor needs for one frame.
pub(super) struct View<'a> {
    pub(super) title: &'a str,
    pub(super) footer: &'a str,
    /// The checklist token for an all-skipped group.
    pub(super) skipped: &'a str,
    pub(super) model: &'a Model,
    pub(super) active: Option<usize>,
    pub(super) logs: &'a VecDeque<String>,
    pub(super) spinner: usize,
    pub(super) elapsed: Option<Duration>,
}

/// Compose the whole screen as `size.rows` full-width lines. Pure.
pub(super) fn compose(theme: &Theme, v: &View, size: TermSize) -> Vec<String> {
    let cols = size.cols.max(1);
    let rows = size.rows.max(1);
    if cols < frame::MIN_COLS || rows < frame::MIN_ROWS {
        return too_small(theme, cols, rows);
    }

    let mut grid = vec![" ".repeat(cols); rows];
    grid[0] = header_line(theme, v.title, v.elapsed, cols);
    grid[1] = render::fit_to(&theme.dim(&theme.box_chars().h.repeat(cols)), cols);
    let footer_row = rows - 1;
    grid[footer_row] = render::center(&theme.dim(v.footer), cols);

    let body_top = 2;
    let body_count = footer_row.saturating_sub(body_top);

    if cols >= SPLIT_MIN_COLS {
        let left_w = (cols * 42 / 100).clamp(28, 44).min(cols.saturating_sub(24));
        let sep_w = 3; // " │ "
        let right_w = cols - left_w - sep_w;
        let left = checklist_cells(theme, v, left_w, body_count);
        let right = detail_cells(theme, v, right_w, body_count);
        let vbar = theme.dim(theme.box_chars().v);
        for i in 0..body_count {
            let lc = left.get(i).cloned().unwrap_or_else(|| " ".repeat(left_w));
            let rc = right.get(i).cloned().unwrap_or_else(|| " ".repeat(right_w));
            grid[body_top + i] = render::fit_to(&format!("{lc} {vbar} {rc}"), cols);
        }
    } else {
        // Narrow tier: checklist stacked, then the active headline + bar under it.
        let mut body = checklist_cells(theme, v, cols, body_count);
        if let Some(idx) = v.active {
            let g = &v.model.groups[idx];
            let mut extra: Vec<String> = Vec::new();
            extra.push(" ".repeat(cols));
            if let Some(a) = &g.activity {
                extra.push(render::fit_to(&theme.accent(&format!(" → {a}")), cols));
            }
            extra.push(bar_line(theme, g, cols));
            // Overwrite the tail rows with the detail lines.
            for (k, line) in extra.into_iter().enumerate() {
                let slot = body_count.saturating_sub(3) + k;
                if slot < body.len() {
                    body[slot] = line;
                }
            }
        }
        for (i, line) in body.into_iter().enumerate().take(body_count) {
            grid[body_top + i] = line;
        }
    }
    grid
}

/// `▌ ADOS  ·  Installing …            04:12`
fn header_line(theme: &Theme, title: &str, elapsed: Option<Duration>, cols: usize) -> String {
    let left = format!(
        "{} {}  {}  {}",
        theme.accent(render::wordmark(theme)),
        theme.heading("ADOS"),
        theme.dim(render::dot(theme)),
        theme.accent(title),
    );
    let right = elapsed
        .map(|d| theme.dim(&fmt_clock(d)))
        .unwrap_or_default();
    let lw = render::visible_width(&left);
    let rw = render::visible_width(&right);
    if lw + rw + 1 >= cols {
        return render::fit_to(&left, cols);
    }
    render::fit_to(
        &format!("{left}{}{right}", " ".repeat(cols - lw - rw)),
        cols,
    )
}

/// The checklist rows, each fit to `width`, padded to `height`.
fn checklist_cells(theme: &Theme, v: &View, width: usize, height: usize) -> Vec<String> {
    let mut cells: Vec<String> = v
        .model
        .groups
        .iter()
        .map(|g| checklist_row(theme, g, v, width))
        .collect();
    while cells.len() < height {
        cells.push(" ".repeat(width));
    }
    cells.truncate(height);
    cells
}

/// One checklist row: `✓ Building radio stack      28.8s` fit to `width`.
fn checklist_row(theme: &Theme, g: &Group, v: &View, width: usize) -> String {
    let glyph = glyph_colored(theme, g, v.spinner);
    let detail = detail_token(g, v.skipped);
    let detail_w = render::visible_width(&detail);
    let label_max = width.saturating_sub(2 + detail_w + 1);
    let label = render::truncate(g.label, label_max);
    let used = 2 + render::visible_width(&label) + detail_w;
    let pad = width.saturating_sub(used);
    let label_styled = match g.status {
        GStatus::Running => theme.heading(&label),
        GStatus::Pending | GStatus::Skipped => theme.dim(&label),
        _ => label.clone(),
    };
    let detail_styled = if g.status == GStatus::Running && g.sub.is_some() {
        theme.accent(&detail)
    } else {
        theme.dim(&detail)
    };
    render::fit_to(
        &format!("{glyph} {label_styled}{}{detail_styled}", " ".repeat(pad)),
        width,
    )
}

fn glyph_colored(theme: &Theme, g: &Group, spinner: usize) -> String {
    match g.status {
        GStatus::Ok => theme.ok(theme.glyph_ok()),
        GStatus::Skipped => theme.dim(theme.glyph_ok()),
        GStatus::Failed => theme.fail(theme.glyph_fail()),
        GStatus::Running => theme.accent(theme.spinner(spinner)),
        GStatus::Pending => theme.dim(theme.glyph_pending()),
    }
}

/// The right-aligned checklist token (plain text; caller measures for padding).
fn detail_token(g: &Group, skipped: &str) -> String {
    match g.status {
        GStatus::Ok | GStatus::Failed => g.elapsed.map(fmt_dur).unwrap_or_default(),
        GStatus::Skipped => skipped.to_string(),
        GStatus::Running => match g.sub {
            Some((done, total)) => short_bar(done, total),
            None => g.elapsed_now().map(fmt_dur).unwrap_or_default(),
        },
        GStatus::Pending => String::new(),
    }
}

/// The live-detail pane: title, activity headline, a bar, then the log tail.
fn detail_cells(theme: &Theme, v: &View, width: usize, height: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::with_capacity(height);
    match v.active {
        Some(idx) => {
            let g = &v.model.groups[idx];
            let sp = theme.accent(theme.spinner(v.spinner));
            lines.push(render::fit_to(
                &format!("{sp} {}", theme.heading(g.label)),
                width,
            ));
            lines.push(match &g.activity {
                Some(a) => render::fit_to(&theme.accent(&format!("→ {a}")), width),
                None => " ".repeat(width),
            });
            lines.push(bar_line(theme, g, width));
            lines.push(render::fit_to(&theme.dim("live"), width));
            // Fill the rest with the tail of the raw output, most recent last.
            let room = height.saturating_sub(lines.len());
            let skip = v.logs.len().saturating_sub(room);
            for raw in v.logs.iter().skip(skip) {
                let text = render::truncate(&render::strip_ansi(raw), width.saturating_sub(2));
                lines.push(render::fit_to(&theme.dim(&format!("  {text}")), width));
            }
        }
        None => lines.push(render::fit_to(&theme.dim("preparing…"), width)),
    }
    while lines.len() < height {
        lines.push(" ".repeat(width));
    }
    lines.truncate(height);
    lines
}

/// The below-minimum-size resize prompt.
fn too_small(theme: &Theme, cols: usize, rows: usize) -> Vec<String> {
    let mut grid = vec![" ".repeat(cols); rows];
    let msg = format!(
        "Terminal too small. Please resize to at least {} x {}.",
        frame::MIN_COLS,
        frame::MIN_ROWS
    );
    grid[rows / 2] = render::center(&theme.warn(&msg), cols);
    grid
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::StepOutcome;
    use crate::progress::events::group_index_for_step;
    use crate::progress::TEST_SPEC;
    use crate::theme::ColorTier;

    fn plain() -> Theme {
        Theme {
            ascii: false,
            tier: ColorTier::None,
        }
    }

    fn view<'a>(m: &'a Model, logs: &'a VecDeque<String>, active: Option<usize>) -> View<'a> {
        View {
            title: "Installing · Example (drone)",
            footer: TEST_SPEC.footer,
            skipped: TEST_SPEC.skipped,
            model: m,
            active,
            logs,
            spinner: 0,
            elapsed: Some(Duration::from_secs(252)),
        }
    }

    #[test]
    fn every_line_is_exactly_cols_wide_across_sizes_and_tiers() {
        let mut m = Model::new(TEST_SPEC.groups);
        m.record("deps", &StepOutcome::Ok);
        m.set_bytes("units", 4_404_019, 8_388_608, "example-service".into());
        m.set_activity("units", "installing example-service".into());
        let mut logs = VecDeque::new();
        logs.push_back("✓ example-agent 6.1 MB".to_string());
        logs.push_back("verifying example-service sha256".to_string());
        let active = group_index_for_step(TEST_SPEC.groups, "units");

        for tier in [ColorTier::None, ColorTier::Truecolor, ColorTier::Basic] {
            for ascii in [false, true] {
                let theme = Theme { ascii, tier };
                for (cols, rows) in [(120usize, 30usize), (90, 24), (80, 20), (72, 20)] {
                    let v = view(&m, &logs, active);
                    let grid = compose(&theme, &v, TermSize { cols, rows });
                    assert_eq!(grid.len(), rows);
                    for (i, line) in grid.iter().enumerate() {
                        assert_eq!(
                            render::visible_width(line),
                            cols,
                            "tier {tier:?} ascii {ascii} {cols}x{rows} row {i} width"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn below_floor_shows_resize_message() {
        let m = Model::new(TEST_SPEC.groups);
        let logs = VecDeque::new();
        let v = view(&m, &logs, None);
        let grid = compose(&plain(), &v, TermSize { cols: 70, rows: 18 });
        assert!(grid.join("\n").contains("Terminal too small"));
    }

    #[test]
    fn split_shows_checklist_detail_and_skipped_wording() {
        let mut m = Model::new(TEST_SPEC.groups);
        m.record("deps", &StepOutcome::Skipped);
        m.set_activity("units", "writing unit files".into());
        let logs = VecDeque::new();
        let v = view(&m, &logs, group_index_for_step(TEST_SPEC.groups, "units"));
        let joined = compose(
            &plain(),
            &v,
            TermSize {
                cols: 110,
                rows: 28,
            },
        )
        .join("\n");
        assert!(joined.contains("ADOS"), "header");
        assert!(joined.contains("Registering services"), "checklist label");
        assert!(joined.contains("writing unit files"), "activity headline");
        assert!(joined.contains(TEST_SPEC.skipped), "skipped token");
        assert!(joined.contains("4:12"), "total elapsed clock");
    }
}
