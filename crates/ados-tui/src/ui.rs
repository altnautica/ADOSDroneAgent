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
    refreshed: &str,
    stale: bool,
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
    verdict(frame, rows[1], dash);
    footer(frame, rows[3], dash, update_latest);

    match dash {
        Some(d) => body(frame, rows[2], d, history),
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

fn header(frame: &mut Frame, area: Rect, dash: Option<&Dashboard>, refreshed: &str, stale: bool) {
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
    right.push(Span::styled(format!("refreshed {refreshed} UTC"), dim()));
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
/// launch when the background check finds a newer version. `[u]` runs the
/// update (reusing the installer's full-screen UI via `ados update`); any other
/// key drops into the cockpit, where the footer badge stays as a reminder.
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

fn body(frame: &mut Frame, area: Rect, dash: &Dashboard, history: &History) {
    // Left: every reachable link (wide enough for a full URL). Right: live state.
    let cols =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(area);
    links_panel(frame, cols[0], dash);

    let cockpit = Layout::vertical([
        Constraint::Length(9),
        Constraint::Length(3),
        Constraint::Length(5),
        Constraint::Min(4),
    ])
    .split(cols[1]);
    autopilot_panel(frame, cockpit[0], dash, history);
    video_panel(frame, cockpit[1], dash);
    link_panel(frame, cockpit[2], dash);
    services_panel(frame, cockpit[3], dash);
}

fn links_panel(frame: &mut Frame, area: Rect, dash: &Dashboard) {
    let block = panel("Links");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let groups = dash.reach_links();
    if groups.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("no links advertised yet", dim())),
            inner,
        );
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        if gi > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            group.title,
            Style::default().fg(theme::heading()),
        )));
        if !group.desc.is_empty() {
            lines.push(Line::from(Span::styled(group.desc, dim())));
        }
        for row in &group.rows {
            let (arrow, url_style) = if row.loopback {
                (dim(), dim())
            } else {
                (Style::default().fg(theme::accent()), bright())
            };
            let mut spans = vec![
                Span::styled("➜ ", arrow),
                Span::styled(row.url.clone(), url_style),
            ];
            if row.primary && !row.loopback {
                spans.push(Span::styled("  ●", Style::default().fg(theme::accent())));
            }
            lines.push(Line::from(spans));
        }
    }
    // Reachability guidance: the LAN IP always works (incl. from the hosted GCS),
    // while the .local mDNS name resolves only from a desktop / localhost app.
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Direct IP is most reliable. It works from command.altnautica.com too.",
        dim(),
    )));
    lines.push(Line::from(Span::styled(
        "The .local (mDNS) name needs a desktop app on the LAN.",
        dim(),
    )));
    // Full URLs wrap instead of truncating, so they stay copyable.
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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
    let (dot_color, dot_text) = match dash.fc_link() {
        FcLink::Connected => (theme::success(), "FC connected".to_string()),
        FcLink::Msp => (
            theme::accent(),
            format!(
                "FC: {} (MSP)",
                dash.fc_variant_label().unwrap_or_else(|| "MSP".to_string())
            ),
        ),
        FcLink::PortOpen => (theme::warning(), "port open · no MAVLink".to_string()),
        FcLink::Down => (theme::danger(), "FC not connected".to_string()),
    };
    let mut fc = vec![
        Span::styled("● ", Style::default().fg(dot_color)),
        Span::styled(dot_text, bright()),
    ];
    if let Some(mode) = &dash.mode {
        fc.push(Span::raw("  "));
        fc.push(pill(mode, theme::accent()));
    }
    if let Some(armed) = dash.armed {
        fc.push(Span::raw(" "));
        fc.push(if armed {
            pill("ARMED", theme::danger())
        } else {
            pill("DISARMED", theme::success())
        });
    }
    cells.push(Cell::Line(Line::from(fc)));

    // FC serial endpoint + an honest sub-line (the link hint, or — for an MSP
    // FC — that MAVLink telemetry does not apply to it).
    let mut detail: Vec<Span> = Vec::new();
    if let Some(ep) = dash.fc_endpoint() {
        detail.push(Span::styled(ep, dim()));
    }
    let sub = match dash.fc_link() {
        FcLink::Msp => Some("MSP flight controller · MAVLink telemetry N/A".to_string()),
        // No serial FC device found at all — say so, and what to check, instead
        // of a bare "FC not connected" with no explanation.
        FcLink::Down => Some(
            "No flight controller detected on USB/serial — connect the FC over USB, then [r] refresh (or [l] logs)."
                .to_string(),
        ),
        _ => dash.fc_hint(),
    };
    if let Some(sub) = sub {
        if !detail.is_empty() {
            detail.push(Span::raw("   "));
        }
        detail.push(Span::styled(sub, dim()));
    }
    if !detail.is_empty() {
        cells.push(Cell::Line(Line::from(detail)));
    }

    // Battery gauge + trend.
    if let Some(battery) = dash.battery {
        let color = if battery > 50.0 {
            theme::success()
        } else if battery > 20.0 {
            theme::warning()
        } else {
            theme::danger()
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
                color: theme::accent(),
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
        theme::success()
    } else if matches!(dash.video_state.as_str(), "" | "unknown") {
        theme::muted()
    } else {
        theme::warning()
    };
    let state = if dash.video_state.is_empty() {
        "unknown".to_string()
    } else {
        dash.video_state.clone()
    };
    let line = Line::from(vec![
        Span::styled("● ", Style::default().fg(dot_color)),
        Span::styled(state, bright()),
    ]);
    // The viewer URL lives in the Links panel; this panel shows live state only.
    frame.render_widget(Paragraph::new(line), inner);
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
            Style::default().fg(theme::success()),
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
            // A dim sub-line explaining a step that still needs attention (the
            // agent's per-step detail — why it needs action / what it is).
            if step.state != "complete" && !step.detail.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("    {}", step.detail),
                    dim(),
                )));
            }
        }
        // The "next action" summary, unless it just repeats a step's detail
        // already shown above.
        if !dash.next_action.is_empty()
            && !dash
                .steps
                .iter()
                .any(|st| st.state != "complete" && st.detail == dash.next_action)
        {
            lines.push(Line::from(Span::styled(dash.next_action.clone(), dim())));
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Colour the cloud-relay display string by its state.
fn cloud_span(value: &str) -> Span<'static> {
    let color = if value.starts_with("paired") {
        theme::success()
    } else if value.starts_with("configured") {
        theme::warning()
    } else {
        theme::muted()
    };
    Span::styled(value.to_string(), Style::default().fg(color))
}

/// Colour the remote-access status string by its state.
fn remote_span(value: &str) -> Span<'static> {
    let color = match value {
        "running" | "connected" | "enabled" | "up" => theme::success(),
        "" | "disabled" | "off" | "stopped" | "unknown" => theme::muted(),
        _ => theme::warning(),
    };
    Span::styled(value.to_string(), Style::default().fg(color))
}

/// Glyph and colour for a setup-step state.
fn step_dot(state: &str) -> (&'static str, Color) {
    match state {
        "complete" => ("●", theme::success()),
        "in_progress" => ("◐", theme::accent()),
        "needs_action" => ("▲", theme::warning()),
        _ => ("○", theme::muted()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Dashboard;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    /// Render one frame to an off-screen backend and return every cell's symbol,
    /// row by row, so a test can assert a rendered string is present.
    fn buffer_text(dash: Option<&Dashboard>, update_latest: Option<&str>, splash: bool) -> String {
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let history = History::default();
        terminal
            .draw(|f| {
                render(
                    f,
                    dash,
                    &history,
                    "12:00:00",
                    false,
                    None,
                    None,
                    update_latest,
                    splash,
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// A drone with no FC and an incomplete step, exercising every new surface.
    fn sample() -> Dashboard {
        let data = json!({
            "version": "0.99.108",
            "device_name": "skynodepi",
            "profile": "drone",
            "paired": true,
            "access_urls": [
                {"kind": "setup", "label": "mDNS setup", "url": "http://ados-x.local:8080/setup", "primary": true},
                {"kind": "setup", "label": "LAN setup", "url": "http://192.168.1.5:8080/setup"},
                {"kind": "mission_control", "label": "Mission Control", "url": "https://command.altnautica.com"},
                {"kind": "video", "label": "viewer", "url": "http://ados-x.local:8889/main/"},
                {"kind": "mavlink", "label": "MAVLink WS", "url": "ws://ados-x.local:8765/"}
            ],
            "mavlink": {"connected": false},
            "steps": [
                {"label": "Flight controller", "state": "needs_action", "detail": "Connect the FC and re-check."},
                {"label": "Profile", "state": "complete", "detail": "Confirmed as drone"}
            ],
            "services": [{"state": "running"}]
        });
        Dashboard::from_status(&data)
    }

    #[test]
    fn renders_links_fc_and_update_badge() {
        let text = buffer_text(Some(&sample()), Some("0.99.200"), false);
        // The new Connect-to-GCS section + its bare base address (the `  ●`
        // primary marker distinguishes it from the Console `…/setup` row).
        assert!(text.contains("Connect to GCS"), "{text}");
        assert!(text.contains("http://ados-x.local:8080  ●"), "{text}");
        // Reworded Ground control + hosted Mission Control.
        assert!(text.contains("Conventional MAVLink"), "{text}");
        assert!(text.contains("command.altnautica.com"), "{text}");
        // FC-down diagnostic sub-line (Autopilot cell).
        assert!(
            text.contains("No flight controller detected on USB/serial"),
            "{text}"
        );
        // The agent's per-step detail is surfaced in the Services cell.
        assert!(text.contains("Connect the FC and re-check."), "{text}");
        // Footer bottom-right update badge.
        assert!(text.contains("update available"), "{text}");
    }

    #[test]
    fn renders_update_splash() {
        let text = buffer_text(Some(&sample()), Some("0.99.200"), true);
        assert!(text.contains("Update available"), "{text}");
        assert!(text.contains("update now"), "{text}");
    }

    #[test]
    fn no_update_badge_when_up_to_date() {
        let text = buffer_text(Some(&sample()), Some("0.99.108"), false);
        assert!(!text.contains("update available"), "{text}");
        assert!(text.contains("up to date"), "{text}");
    }
}
