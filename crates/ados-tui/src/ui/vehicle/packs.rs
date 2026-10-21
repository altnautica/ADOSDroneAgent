//! Battery packs from `/api/v1/battery`: per-pack summary, a cell bar chart,
//! divergence, the time-to-reserve prediction, and live anomalies.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Bar, BarChart, BarGroup, Paragraph};
use ratatui::Frame;

use crate::app::Core;
use crate::model::vehicle::{eta_text, Batteries, Pack};
use crate::ui::widgets::{chip, panel};

/// Cell bars are drawn above this floor so a few hundred mV of spread is
/// visible (2.5 V is the plausibility floor).
const FLOOR_MV: f64 = 2500.0;
const SPAN_MV: u64 = 2000;

pub fn render(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    bat: Option<&Batteries>,
    hidden: Option<&str>,
) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Battery packs");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let note = |text: String| Paragraph::new(Span::styled(text, t.dim()));
    if let Some(why) = hidden {
        f.render_widget(note(why.to_string()), inner);
        return;
    }
    let Some(bat) = bat else {
        f.render_widget(note("Waiting for the battery monitor…".into()), inner);
        return;
    };
    if !bat.enabled {
        f.render_widget(
            note("Battery monitoring is off (battery.enabled)".into()),
            inner,
        );
        return;
    }
    if bat.packs.is_empty() {
        f.render_widget(note("No battery samples yet".into()), inner);
        return;
    }
    // Each pack: summary line, cell bars (when cells are known), anomalies.
    let heights: Vec<u16> = bat
        .packs
        .iter()
        .map(|p| 1 + if p.cells.is_empty() { 0 } else { 4 } + p.anomalies.len().min(3) as u16)
        .collect();
    let mut constraints: Vec<Constraint> = heights.iter().map(|h| Constraint::Length(*h)).collect();
    constraints.push(Constraint::Min(0));
    let rects = Layout::vertical(constraints).split(inner);
    for (p, rect) in bat.packs.iter().zip(rects.iter()) {
        pack(f, *rect, core, p, bat.reserve_percent);
    }
}

fn pack(f: &mut Frame, area: Rect, core: &Core, p: &Pack, reserve: Option<i64>) {
    if area.height == 0 {
        return;
    }
    let t = &core.theme;
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if p.cells.is_empty() { 0 } else { 4 }),
        Constraint::Min(0),
    ])
    .split(area);
    let name = if p.id == 255 {
        "Node gauge".to_string()
    } else {
        format!("Pack {}", p.id)
    };
    let mut spans = vec![
        Span::styled(format!("{name} "), t.accent_bold()),
        Span::styled(format!("{} ", p.source), t.dim()),
    ];
    if p.stale {
        spans.push(chip(t, "STALE", t.warning()));
        spans.push(Span::raw(" "));
    }
    let mut push = |label: &str, value: Option<String>| {
        if let Some(v) = value {
            spans.push(Span::styled(format!(" {label}"), t.dim()));
            spans.push(Span::styled(v, t.bright()));
        }
    };
    push("", p.voltage.map(|v| format!("{v:.2} V")));
    push("", p.current.map(|v| format!("{v:.1} A")));
    push("", p.remaining.map(|v| format!("{v:.0} %")));
    push("", p.temperature.map(|v| format!("{v:.0} °C")));
    push("div ", p.divergence_mv.map(|v| format!("{v:.0} mV")));
    if let Some(eta) = p.eta_s {
        let to = reserve.map(|r| format!(" to {r} %")).unwrap_or_default();
        let state = p.prediction_state.as_deref().unwrap_or("");
        push("ETA ", Some(format!("{}{to} ({state})", eta_text(eta))));
    } else if let Some(state) = &p.prediction_state {
        push("prediction ", Some(state.clone()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), rows[0]);

    if !p.cells.is_empty() {
        let bars: Vec<Bar> = p
            .cells
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mv = v * 1000.0;
                let weak = p.weakest == Some(i) && p.cells.len() > 1;
                let color = if !p.cells_plausible {
                    t.muted()
                } else if weak {
                    t.warning()
                } else {
                    t.success()
                };
                Bar::default()
                    .value((mv - FLOOR_MV).clamp(0.0, SPAN_MV as f64) as u64)
                    .text_value(format!("{v:.2}"))
                    .label(Line::from(format!(
                        "c{}{}",
                        i + 1,
                        if weak { "▼" } else { "" }
                    )))
                    .style(Style::default().fg(color))
                    .value_style(t.chip(color))
            })
            .collect();
        let chart = BarChart::default()
            .data(BarGroup::default().bars(&bars))
            .bar_width(5)
            .bar_gap(1)
            .max(SPAN_MV)
            .label_style(t.dim());
        f.render_widget(chart, rows[1]);
    }

    let lines: Vec<Line> = p
        .anomalies
        .iter()
        .take(3)
        .map(|a| {
            let (glyph, color) = if a.critical {
                (core.sym.err, t.error())
            } else {
                (core.sym.warn, t.warning())
            };
            let detail = match (a.value, a.threshold) {
                (Some(v), Some(th)) => format!(" {v:.2} (limit {th:.2})"),
                (Some(v), None) => format!(" {v:.2}"),
                _ => String::new(),
            };
            Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(a.rule.replace('_', " "), Style::default().fg(color)),
                Span::styled(detail, t.dim()),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), rows[2]);
}
