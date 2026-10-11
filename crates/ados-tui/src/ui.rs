//! ratatui rendering of the terminal dashboard.
//!
//! Layout: a word-mark header, a one-line health verdict with a dim sub-line, a
//! two-column body (a "Reach this agent" panel on the left, a cockpit grid on
//! the right), and a dim footer. Every value shown is a verified field from the
//! status payload; trend sparklines are drawn only from recorded telemetry.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Gauge, Paragraph, Sparkline, Wrap};
use ratatui::Frame;

use crate::action::ACTIONS;
use crate::model::{state_label, Dashboard, FcLink, Health, History};
use crate::theme;

fn dim() -> Style {
    Style::default().fg(theme::muted())
}

fn bright() -> Style {
    Style::default()
        .fg(theme::heading())
        .add_modifier(Modifier::BOLD)
}

/// A rounded, dim-bordered panel titled with the `▌ ` accent word-mark marker.
fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Span::styled(
            format!(" ▌ {title} "),
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ))
}

/// A `label   value` row: dim label padded for alignment, bright value.
fn kv(label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{label:<8} "), dim()), value])
}

/// A short accent/coloured chip, e.g. ` STABILIZE ` or ` ARMED `.
fn pill(text: &str, color: Color) -> Span<'static> {
    Span::styled(
        format!(" {text} "),
        Style::default().fg(theme::on_amber()).bg(color),
    )
}

/// One row inside a text-and-widget panel.
enum Cell {
    Line(Line<'static>),
    Gauge {
        ratio: f64,
        label: String,
        color: Color,
    },
    Spark {
        data: Vec<u64>,
        max: Option<u64>,
        color: Color,
    },
}

/// Lay `cells` out as stacked one-row widgets inside `inner`. A trailing filler
/// keeps them top-aligned. Splitting yields one rect per cell regardless of the
/// available height (extra rects get zero height), so this never panics on a
/// short terminal.
fn render_cells(frame: &mut Frame, inner: Rect, cells: Vec<Cell>) {
    if inner.height == 0 || cells.is_empty() {
        return;
    }
    let mut constraints: Vec<Constraint> = cells.iter().map(|_| Constraint::Length(1)).collect();
    constraints.push(Constraint::Min(0));
    let rects = Layout::vertical(constraints).split(inner);
    for (i, cell) in cells.into_iter().enumerate() {
        let rect = rects[i];
        match cell {
            Cell::Line(line) => frame.render_widget(Paragraph::new(line), rect),
            Cell::Gauge {
                ratio,
                label,
                color,
            } => frame.render_widget(
                Gauge::default()
                    .gauge_style(Style::default().fg(color))
                    .ratio(ratio.clamp(0.0, 1.0))
                    .label(Span::styled(label, Style::default().fg(theme::on_amber()))),
                rect,
            ),
            Cell::Spark { data, max, color } => {
                let mut spark = Sparkline::default()
                    .data(data)
                    .style(Style::default().fg(color));
                if let Some(m) = max {
                    spark = spark.max(m);
                }
                frame.render_widget(spark, rect);
            }
        }
    }
}

/// Battery history mapped to 0..=100 bars.
fn battery_bars(history: &[f64]) -> Vec<u64> {
    history
        .iter()
        .map(|v| v.clamp(0.0, 100.0).round() as u64)
        .collect()
}

/// Altitude history offset by its window minimum so the trend shape survives
/// (bars are unsigned) without inventing a floor at zero.
fn alt_bars(history: &[f64]) -> Vec<u64> {
    let min = history.iter().copied().fold(f64::INFINITY, f64::min);
    if !min.is_finite() {
        return Vec::new();
    }
    history
        .iter()
        .map(|v| (v - min).max(0.0).round() as u64)
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    dash: Option<&Dashboard>,
    history: &History,
    // Wall-clock time of the last successful poll; `None` before the first one.
    refreshed: Option<&str>,
    // The last success is old enough that its readings no longer count as live.
    stale: bool,
    // Why the latest poll failed, if it did.
    error: Option<&str>,
    actions_selected: Option<usize>,
    // The latest agent version fetched from GitHub, once known (drives the footer
    // badge + the launch splash). `None` while the check is pending / offline.
    update_latest: Option<&str>,
    show_update_splash: bool,
) {
    // Paint the charcoal base once per frame on tiers that can show it; widgets
    // rendered on top keep it unless they set their own background (only the
    // chips and gauge do), because a default `Style` leaves `bg` unset.
    if let Some(bg) = theme::background() {
        let area = frame.area();
        frame.buffer_mut().set_style(area, Style::default().bg(bg));
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(frame.area());

    header(frame, rows[0], dash, refreshed, stale);
    verdict(frame, rows[1], dash, error, stale);
    footer(frame, rows[3], dash, update_latest);

    match dash {
        Some(d) => body(frame, rows[2], d, history, stale),
        None => {
            let block = panel("Status");
            let inner = block.inner(rows[2]);
            frame.render_widget(block, rows[2]);
            let msg = error.unwrap_or("Connecting to the agent…");
            frame.render_widget(
                Paragraph::new(msg.to_string())
                    .style(Style::default().fg(theme::warning()))
                    .wrap(Wrap { trim: false }),
                inner,
            );
        }
    }

    if let Some(sel) = actions_selected {
        actions_overlay(frame, sel);
    }
    if show_update_splash {
        if let (Some(d), Some(latest)) = (dash, update_latest) {
            update_splash_overlay(frame, &d.version, latest);
        }
    }
}

fn header(
    frame: &mut Frame,
    area: Rect,
    dash: Option<&Dashboard>,
    refreshed: Option<&str>,
    stale: bool,
) {
    let ident = match dash {
        Some(d) => Span::styled(d.ident(), bright()),
        None => Span::styled("connecting", dim()),
    };
    let left = Line::from(vec![
        Span::styled("▌ ", Style::default().fg(theme::accent())),
        Span::styled(
            "ADOS",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        ident,
    ]);

    let mut right: Vec<Span> = Vec::new();
    if let Some(d) = dash {
        right.push(Span::styled(format!("v{}  ·  ", d.version), dim()));
    }
    right.push(Span::styled(
        match refreshed {
            Some(at) => format!("refreshed {at} UTC"),
            None => "not refreshed yet".to_string(),
        },
        dim(),
    ));
    if stale {
        right.push(Span::styled(
            "  stale",
            Style::default().fg(theme::warning()),
        ));
    }
    let right_w: usize = right.iter().map(|s| s.content.chars().count()).sum();

    let cols = Layout::horizontal([Constraint::Fill(1), Constraint::Length(right_w as u16 + 1)])
        .split(area);
    frame.render_widget(Paragraph::new(left), cols[0]);
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        cols[1],
    );
}

fn verdict(
    frame: &mut Frame,
    area: Rect,
    dash: Option<&Dashboard>,
    error: Option<&str>,
    stale: bool,
) {
    let Some(d) = dash else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("· · ·", dim()))),
            area,
        );
        return;
    };
    // A snapshot the agent is no longer confirming is never shown as a health
    // verdict: the failure takes the verdict line, so the last good snapshot
    // cannot read as a live HEALTHY.
    if error.is_some() || stale {
        let word = if error.is_some() {
            "UNREACHABLE"
        } else {
            "STALE"
        };
        let why = error.unwrap_or("No answer from the agent.");
        let line1 = Line::from(vec![
            Span::styled("▲ ", Style::default().fg(theme::danger())),
            Span::styled(
                word,
                Style::default()
                    .fg(theme::danger())
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        let line2 = Line::from(Span::styled(
            format!("{why} · showing the last snapshot"),
            Style::default().fg(theme::warning()),
        ));
        frame.render_widget(Paragraph::new(vec![line1, line2]), area);
        return;
    }
    let health = d.health();
    let color = match health {
        Health::Healthy => theme::success(),
        Health::Degraded | Health::Setup => theme::warning(),
    };
    let word_style = match health {
        Health::Setup => Style::default().fg(color),
        _ => Style::default().fg(color).add_modifier(Modifier::BOLD),
    };
    let line1 = Line::from(vec![
        Span::styled(format!("{} ", health.dot()), Style::default().fg(color)),
        Span::styled(health.label(), word_style),
    ]);
    let line2 = Line::from(Span::styled(d.status_summary(), dim()));
    frame.render_widget(Paragraph::new(vec![line1, line2]), area);
}

/// An amber `[key] label` hint pair for the bottom action bar.
fn key_hint(key: &str, label: &str) -> [Span<'static>; 2] {
    [
        Span::styled(format!("[{key}] "), Style::default().fg(theme::accent())),
        Span::styled(format!("{label}  "), dim()),
    ]
}

fn footer(frame: &mut Frame, area: Rect, dash: Option<&Dashboard>, update_latest: Option<&str>) {
    // Is a newer agent version available? (installed = the polled version).
    let update_available = matches!(
        (dash, update_latest),
        (Some(d), Some(l)) if crate::update::is_newer(l, &d.version)
    );

    // Left: the key hints. `[u] update` appears only when an update is available.
    let mut left: Vec<Span> = Vec::new();
    for action in ACTIONS.iter() {
        if let Some(key) = action.key {
            left.extend(key_hint(&key.to_string(), action.short));
        }
    }
    if update_available {
        left.extend(key_hint("u", "update"));
    }
    left.extend(key_hint("a", "actions"));
    left.extend(key_hint("r", "refresh"));
    left.extend(key_hint("q", "quit"));

    // Right: the version + update state (bottom-right, right-aligned like the
    // header). Silent until the background check reports a latest version.
    let mut right: Vec<Span> = Vec::new();
    if let Some(d) = dash {
        right.push(Span::styled(format!("v{}", d.version), dim()));
        if update_latest.is_some() {
            if update_available {
                right.push(Span::styled(
                    "  ·  update available → ados update",
                    Style::default().fg(theme::warning()),
                ));
            } else {
                right.push(Span::styled("  ·  up to date", dim()));
            }
        }
    }
    let right_w: usize = right.iter().map(|s| s.content.chars().count()).sum();

    let cols = Layout::horizontal([Constraint::Fill(1), Constraint::Length(right_w as u16 + 1)])
        .split(area);
    frame.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        cols[1],
    );
}

/// A rect `pct_x`% wide and `height` tall, centered in `area`.
fn centered_rect(pct_x: u16, height: u16, area: Rect) -> Rect {
    let w = (area.width * pct_x / 100).min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

fn actions_overlay(frame: &mut Frame, selected: usize) {
    let height = ACTIONS.len() as u16 + 4; // list + border + blank + hint
    let area = centered_rect(60, height, frame.area());
    frame.render_widget(Clear, area);
    let block = panel("Actions");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, action) in ACTIONS.iter().enumerate() {
        let is_selected = i == selected;
        let (marker, label_style) = if is_selected {
            (
                "› ",
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("  ", bright())
        };
        let mut spans = vec![
            Span::styled(marker, Style::default().fg(theme::accent())),
            Span::styled(format!("{:<27}", action.label), label_style),
            Span::styled(action.desc, dim()),
        ];
        if action.confirm {
            spans.push(Span::styled("  (confirm)", dim()));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑↓ select · Enter run · Esc close",
        dim(),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A centered, dismissible splash prompting an agent update, shown once on
/// launch when the background check finds a newer version. `[u]` asks for the
/// update through the y/N-confirmed action (refused while the vehicle reports
/// armed); any other key drops into the cockpit, where the footer badge stays as
/// a reminder.
fn update_splash_overlay(frame: &mut Frame, installed: &str, latest: &str) {
    let area = centered_rect(60, 7, frame.area());
    frame.render_widget(Clear, area);
    let block = panel("Update available");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("v{installed}"), dim()),
            Span::styled("  →  ", Style::default().fg(theme::accent())),
            Span::styled(format!("v{latest}"), bright()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("[u] ", Style::default().fg(theme::accent())),
            Span::styled("update now", bright()),
            Span::styled("      any key: later", dim()),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}

mod panels;
use panels::*;

#[cfg(test)]
mod tests;
