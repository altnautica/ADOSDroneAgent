//! ratatui rendering of the terminal dashboard.
//!
//! Layout: a word-mark header, a one-line health verdict with a dim sub-line, a
//! two-column body (a "Reach this agent" panel on the left, a cockpit grid on
//! the right), and a dim footer. Every value shown is a verified field from the
//! status payload; trend sparklines are drawn only from recorded telemetry.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Gauge, Paragraph, Sparkline, Wrap};
use ratatui::Frame;

use crate::model::{state_label, Dashboard, Health, History};

/// The single screen accent.
const ACCENT: Color = Color::Cyan;

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn bright() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// A rounded, dim-bordered panel titled with the `▌ ` accent word-mark marker.
fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Span::styled(
            format!(" ▌ {title} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
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
        Style::default().fg(Color::Black).bg(color),
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
                    .label(Span::styled(label, Style::default().fg(Color::Black))),
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

pub fn render(
    frame: &mut Frame,
    dash: Option<&Dashboard>,
    history: &History,
    refreshed: &str,
    error: Option<&str>,
) {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(frame.area());

    header(frame, rows[0], dash, refreshed);
    verdict(frame, rows[1], dash);
    footer(frame, rows[3]);

    match dash {
        Some(d) => body(frame, rows[2], d, history),
        None => {
            let block = panel("Status");
            let inner = block.inner(rows[2]);
            frame.render_widget(block, rows[2]);
            let msg = error.unwrap_or("Connecting to the agent…");
            frame.render_widget(
                Paragraph::new(msg.to_string())
                    .style(Style::default().fg(Color::Yellow))
                    .wrap(Wrap { trim: false }),
                inner,
            );
        }
    }
}

fn header(frame: &mut Frame, area: Rect, dash: Option<&Dashboard>, refreshed: &str) {
    let ident = match dash {
        Some(d) => Span::styled(d.ident(), bright()),
        None => Span::styled("connecting", dim()),
    };
    let left = Line::from(vec![
        Span::styled("▌ ", Style::default().fg(ACCENT)),
        Span::styled(
            "ADOS",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        ident,
    ]);
    let right = match dash {
        Some(d) => format!("v{}  ·  refreshed {}", d.version, refreshed),
        None => format!("refreshed {refreshed}"),
    };

    let cols = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(right.chars().count() as u16 + 1),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(left), cols[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(right, dim()))).alignment(Alignment::Right),
        cols[1],
    );
}

fn verdict(frame: &mut Frame, area: Rect, dash: Option<&Dashboard>) {
    let Some(d) = dash else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("· · ·", dim()))),
            area,
        );
        return;
    };
    let health = d.health();
    let color = match health {
        Health::Healthy => Color::Green,
        Health::Degraded | Health::Setup => Color::Yellow,
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

fn footer(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "ados help · ados pair · ados logs · q quit",
            dim(),
        ))),
        area,
    );
}

fn body(frame: &mut Frame, area: Rect, dash: &Dashboard, history: &History) {
    let cols =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(area);
    reach_panel(frame, cols[0], dash);

    let cockpit = Layout::vertical([
        Constraint::Length(9),
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Min(4),
    ])
    .split(cols[1]);
    autopilot_panel(frame, cockpit[0], dash, history);
    video_panel(frame, cockpit[1], dash);
    link_panel(frame, cockpit[2], dash);
    services_panel(frame, cockpit[3], dash);
}

fn reach_panel(frame: &mut Frame, area: Rect, dash: &Dashboard) {
    let block = panel("Reach this agent");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let hosts = dash.console_reach();
    let mut lines: Vec<Line> = Vec::new();
    if hosts.is_empty() {
        lines.push(Line::from(Span::styled(
            "no access URLs advertised yet",
            dim(),
        )));
    } else {
        lines.push(Line::from(Span::styled("open in a browser", dim())));
        // Each host sits on one line, truncated to fit — never wrapped mid-URL.
        let host_w = (inner.width as usize).saturating_sub(6);
        for host in &hosts {
            let (arrow_style, host_style) = if host.loopback {
                (dim(), dim())
            } else {
                (Style::default().fg(ACCENT), bright())
            };
            let mut spans = vec![
                Span::styled("➜  ", arrow_style),
                Span::styled(truncate(&host.host_port, host_w), host_style),
            ];
            if host.primary && !host.loopback {
                spans.push(Span::styled("  ●", Style::default().fg(ACCENT)));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("add in Mission Control", dim())));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Truncate to `max` columns with a trailing ellipsis when clipped.
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

fn autopilot_panel(frame: &mut Frame, area: Rect, dash: &Dashboard, history: &History) {
    let block = panel("Autopilot");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // A ground station with no flight controller has no autopilot to show.
    if dash.profile == "ground_station" && !dash.mavlink_connected {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no autopilot (ground station)",
                dim(),
            ))),
            inner,
        );
        return;
    }

    let mut cells: Vec<Cell> = Vec::new();

    // Flight-controller link + mode/armed chips.
    let (dot_color, dot_text) = if dash.mavlink_connected {
        (Color::Green, "FC connected")
    } else {
        (Color::Red, "FC not connected")
    };
    let mut fc = vec![
        Span::styled("● ", Style::default().fg(dot_color)),
        Span::styled(dot_text, bright()),
    ];
    if let Some(mode) = &dash.mode {
        fc.push(Span::raw("  "));
        fc.push(pill(mode, ACCENT));
    }
    if let Some(armed) = dash.armed {
        fc.push(Span::raw(" "));
        fc.push(if armed {
            pill("ARMED", Color::Red)
        } else {
            pill("DISARMED", Color::Green)
        });
    }
    cells.push(Cell::Line(Line::from(fc)));

    // Battery gauge + trend.
    if let Some(battery) = dash.battery {
        let color = if battery > 50.0 {
            Color::Green
        } else if battery > 20.0 {
            Color::Yellow
        } else {
            Color::Red
        };
        cells.push(Cell::Gauge {
            ratio: battery / 100.0,
            label: format!("battery {battery:.0}%"),
            color,
        });
        let bars = battery_bars(&history.battery);
        if bars.len() >= 2 {
            cells.push(Cell::Spark {
                data: bars,
                max: Some(100),
                color,
            });
        }
    }

    // GPS / satellites / altitude.
    let mut nav: Vec<Span> = Vec::new();
    if let Some(fix) = &dash.gps_fix {
        nav.push(Span::styled("GPS ", dim()));
        nav.push(Span::styled(fix.clone(), bright()));
        nav.push(Span::raw("   "));
    }
    if let Some(sats) = dash.satellites {
        nav.push(Span::styled("sats ", dim()));
        nav.push(Span::styled(sats.to_string(), bright()));
        nav.push(Span::raw("   "));
    }
    if let Some(alt) = dash.alt {
        nav.push(Span::styled("alt ", dim()));
        nav.push(Span::styled(format!("{alt:.1} m"), bright()));
    }
    if !nav.is_empty() {
        cells.push(Cell::Line(Line::from(nav)));
    }

    // Altitude trend.
    if dash.alt.is_some() {
        let bars = alt_bars(&history.alt);
        if bars.len() >= 2 {
            cells.push(Cell::Spark {
                data: bars,
                max: None,
                color: ACCENT,
            });
        }
    }

    render_cells(frame, inner, cells);
}

fn video_panel(frame: &mut Frame, area: Rect, dash: &Dashboard) {
    let block = panel("Video");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let running = matches!(dash.video_state.as_str(), "running" | "streaming");
    let dot_color = if running {
        Color::Green
    } else if matches!(dash.video_state.as_str(), "" | "unknown") {
        Color::DarkGray
    } else {
        Color::Yellow
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("● ", Style::default().fg(dot_color)),
        Span::styled(dash.video_state.clone(), bright()),
    ])];
    match &dash.video_viewer {
        Some(url) => lines.push(Line::from(vec![
            Span::styled("➜  ", Style::default().fg(ACCENT)),
            Span::styled(url.clone(), dim()),
        ])),
        None => lines.push(Line::from(Span::styled("no viewer URL", dim()))),
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn link_panel(frame: &mut Frame, area: Rect, dash: &Dashboard) {
    let block = panel("Link");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let cloud = cloud_span(&dash.cloud_relay);
    let remote = remote_span(&dash.remote_status);
    let hotspot = if dash.hotspot.is_empty() {
        Span::styled("off", dim())
    } else {
        Span::styled(dash.hotspot.clone(), bright())
    };
    let lines = vec![
        kv("cloud", cloud),
        kv("remote", remote),
        kv("hotspot", hotspot),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn services_panel(frame: &mut Frame, area: Rect, dash: &Dashboard) {
    let block = panel("Services");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let stopped = dash.services_total.saturating_sub(dash.services_running);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("● {} running", dash.services_running),
            Style::default().fg(Color::Green),
        ),
        Span::raw("   "),
        Span::styled(format!("○ {stopped} stopped"), dim()),
    ])];

    if dash.has_steps && !dash.steps_all_complete {
        lines.push(Line::from(Span::styled("setup steps", dim())));
        for step in &dash.steps {
            let (glyph, color) = step_dot(&step.state);
            lines.push(Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(step.label.clone(), bright()),
                Span::raw("  "),
                Span::styled(state_label(&step.state), dim()),
            ]));
        }
        if !dash.next_action.is_empty() {
            lines.push(Line::from(Span::styled(dash.next_action.clone(), dim())));
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Colour the cloud-relay display string by its state.
fn cloud_span(value: &str) -> Span<'static> {
    let color = if value.starts_with("paired") {
        Color::Green
    } else if value.starts_with("configured") {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    Span::styled(value.to_string(), Style::default().fg(color))
}

/// Colour the remote-access status string by its state.
fn remote_span(value: &str) -> Span<'static> {
    let color = match value {
        "running" | "connected" | "enabled" | "up" => Color::Green,
        "" | "disabled" | "off" | "stopped" | "unknown" => Color::DarkGray,
        _ => Color::Yellow,
    };
    Span::styled(value.to_string(), Style::default().fg(color))
}

/// Glyph and colour for a setup-step state.
fn step_dot(state: &str) -> (&'static str, Color) {
    match state {
        "complete" => ("●", Color::Green),
        "in_progress" => ("◐", ACCENT),
        "needs_action" => ("▲", Color::Yellow),
        _ => ("○", Color::DarkGray),
    }
}
