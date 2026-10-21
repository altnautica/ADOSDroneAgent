//! A braille line chart over one or more sample rings, shared by the Vehicle
//! and Link tabs.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::Span;
use ratatui::widgets::{Axis, Chart, Dataset, GraphType, Paragraph};
use ratatui::Frame;

use crate::app::Core;
use crate::model::ring::Ring;
use crate::ui::widgets::panel;

/// One trace: legend name, samples, colour.
pub struct Trace<'a> {
    pub name: &'a str,
    pub ring: &'a Ring,
    pub color: Color,
}

/// Draw a titled line chart of `traces`. The x axis spans the ring capacity
/// (newest at the right, labelled `span_label` ago … `now`); the y axis spans
/// every trace's samples. An empty chart says so instead of drawing axes.
pub fn line_chart(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    title: &str,
    span_label: &str,
    traces: &[Trace],
) {
    let t = &core.theme;
    let block = panel(t, core.sym, title);
    let Some((lo, hi)) = traces
        .iter()
        .filter_map(|tr| tr.ring.bounds())
        .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
    else {
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(Span::styled("no samples yet", t.dim())),
            inner,
        );
        return;
    };
    let points: Vec<Vec<(f64, f64)>> = traces.iter().map(|tr| tr.ring.points()).collect();
    let datasets = traces
        .iter()
        .zip(&points)
        .map(|(tr, pts)| {
            Dataset::default()
                .name(tr.name.to_string())
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(tr.color))
                .data(pts)
        })
        .collect();
    let cap = traces.first().map(|tr| tr.ring.cap()).unwrap_or(1);
    let label = |v: f64| Span::styled(fmt_axis(v), t.dim());
    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(
            Axis::default()
                .bounds([0.0, (cap.saturating_sub(1)) as f64])
                .style(Style::default().fg(t.border()))
                .labels(vec![
                    Span::styled(span_label.to_string(), t.dim()),
                    Span::styled("now", t.dim()),
                ]),
        )
        .y_axis(
            Axis::default()
                .bounds([lo, hi])
                .style(Style::default().fg(t.border()))
                .labels(vec![label(lo), label(hi)]),
        )
        .hidden_legend_constraints((
            ratatui::layout::Constraint::Ratio(1, 1),
            ratatui::layout::Constraint::Ratio(1, 1),
        ));
    let chart = if traces.len() < 2 {
        chart.legend_position(None)
    } else {
        chart
    };
    f.render_widget(chart, area);
}

/// A compact axis label: no decimals past 100, one below.
fn fmt_axis(v: f64) -> String {
    if v.abs() >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}
