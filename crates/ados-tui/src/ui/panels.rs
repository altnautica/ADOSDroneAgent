//! Overview body panels: links, autopilot, video, link and services.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use super::widgets::{chip, kv, panel, render_cells, Cell};
use crate::app::Core;
use crate::model::{state_label, Dashboard, FcLink, History};
use crate::theme::Theme;

/// Battery history mapped to 0..=100 bars.
pub(super) fn battery_bars(history: &[f64]) -> Vec<u64> {
    history
        .iter()
        .map(|v| v.clamp(0.0, 100.0).round() as u64)
        .collect()
}

/// Altitude history offset by its window minimum so the trend shape survives
/// (bars are unsigned) without inventing a floor at zero.
pub(super) fn alt_bars(history: &[f64]) -> Vec<u64> {
    let min = history.iter().copied().fold(f64::INFINITY, f64::min);
    if !min.is_finite() {
        return Vec::new();
    }
    history
        .iter()
        .map(|v| (v - min).max(0.0).round() as u64)
        .collect()
}

pub(super) fn body(frame: &mut Frame, area: Rect, core: &Core, dash: &Dashboard) {
    let stale = core.stale();
    // Left: every reachable link (wide enough for a full URL). Right: live state.
    let cols =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(area);
    links_panel(frame, cols[0], core, dash);

    let mut constraints = vec![
        Constraint::Length(9),
        Constraint::Length(3),
        Constraint::Length(5),
        Constraint::Min(4),
    ];
    if !dash.profile_row.is_empty() {
        let rows = super::profile_row::packed_lines(&core.theme, dash, cols[1].width.saturating_sub(2)).len();
        constraints.push(Constraint::Length(rows as u16 + 2));
    }
    let cockpit = Layout::vertical(constraints).split(cols[1]);
    autopilot_panel(frame, cockpit[0], core, dash, &core.view.history, stale);
    video_panel(frame, cockpit[1], core, dash);
    link_panel(frame, cockpit[2], core, dash);
    services_panel(frame, cockpit[3], core, dash);
    if let Some(rect) = cockpit.get(4) {
        super::profile_row::profile_panel(frame, *rect, core, dash, stale);
    }
}

pub(super) fn links_panel(frame: &mut Frame, area: Rect, core: &Core, dash: &Dashboard) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Links");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let groups = &dash.links;
    if groups.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("no links advertised yet", t.dim())),
            inner,
        );
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        if gi > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(group.title, t.text())));
        if !group.desc.is_empty() {
            lines.push(Line::from(Span::styled(group.desc, t.dim())));
        }
        for row in &group.rows {
            let (arrow, url_style) = if row.loopback {
                (t.dim(), t.dim())
            } else {
                (t.accented(), t.bright())
            };
            let mut spans = vec![
                Span::styled("➜ ", arrow),
                Span::styled(row.url.clone(), url_style),
            ];
            if row.primary && !row.loopback {
                spans.push(Span::styled("  ●", t.accented()));
            }
            lines.push(Line::from(spans));
        }
    }
    // Reachability guidance: the LAN IP always works (incl. from the hosted GCS),
    // while the .local mDNS name resolves only from a desktop / localhost app.
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Direct IP is most reliable. It works from command.altnautica.com too.",
        t.dim(),
    )));
    lines.push(Line::from(Span::styled(
        "The .local (mDNS) name needs a desktop app on the LAN.",
        t.dim(),
    )));
    // Full URLs wrap instead of truncating, so they stay copyable.
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

pub(super) fn autopilot_panel(
    frame: &mut Frame,
    area: Rect,
    core: &Core,
    dash: &Dashboard,
    history: &History,
    stale: bool,
) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Autopilot");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Flight readings from a snapshot the agent stopped confirming are not
    // live: hide them rather than show a frozen mode, arm state or battery.
    if stale {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no fresh data from the agent · flight readings hidden",
                Style::default().fg(t.warning()),
            )))
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }

    // A ground station or a Visor with no flight controller has no autopilot
    // to show.
    let no_fc_label = match dash.profile.as_str() {
        "ground_station" => Some("no autopilot (ground station)"),
        "visor" => Some("no autopilot (visor)"),
        _ => None,
    };
    if let (Some(label), false) = (no_fc_label, dash.mavlink_connected) {
        frame.render_widget(Paragraph::new(Line::from(Span::styled(label, t.dim()))), inner);
        return;
    }

    let mut cells: Vec<Cell> = Vec::new();

    // Flight-controller link + mode/armed chips.
    let (dot_color, dot_text) = match dash.fc_link() {
        FcLink::Connected => (
            t.success(),
            // Name the MAVLink family (ArduPilot / PX4) when it is verified;
            // fall back to a generic label when the family is unknown.
            format!(
                "{} connected",
                dash.fc_family_label().unwrap_or_else(|| "FC".to_string())
            ),
        ),
        FcLink::Msp => (
            // An identified, reachable MSP FC is a good state: render it like
            // a connected FC, not the accent (which reads as caution).
            t.success(),
            format!(
                "FC: {} (MSP)",
                dash.fc_variant_label().unwrap_or_else(|| "MSP".to_string())
            ),
        ),
        FcLink::PortOpen => (t.warning(), "port open · no MAVLink".to_string()),
        FcLink::Down => (t.error(), "FC not connected".to_string()),
    };
    let mut fc = vec![
        Span::styled("● ", Style::default().fg(dot_color)),
        Span::styled(dot_text, t.bright()),
    ];
    if let Some(mode) = &dash.mode {
        fc.push(Span::raw("  "));
        fc.push(chip(t, mode, t.accent()));
    }
    if let Some(armed) = dash.armed {
        fc.push(Span::raw(" "));
        fc.push(if armed {
            chip(t, "ARMED", t.error())
        } else {
            chip(t, "DISARMED", t.success())
        });
    }
    cells.push(Cell::Line(Line::from(fc)));

    // FC serial endpoint + an honest sub-line (the link hint, or, for an MSP
    // FC, where its live telemetry actually flows). Rendered dim (neutral): an
    // MSP FC is a byte-pipe through the daemon by design.
    let mut detail: Vec<Span> = Vec::new();
    if let Some(ep) = dash.fc_endpoint() {
        detail.push(Span::styled(ep, t.dim()));
    }
    let sub = match dash.fc_link() {
        // The daemon does not decode MSP telemetry (it is a transparent
        // byte-pipe), so live attitude/battery/GPS show in the dashboard /
        // Mission Control over MSP, not here. Not a fault.
        FcLink::Msp => Some("reachable over MSP · live telemetry in the dashboard / GCS".to_string()),
        // No serial FC device found at all: say so, and what to check.
        FcLink::Down => Some(
            "No flight controller detected on USB/serial — connect the FC over USB, then [r] refresh (or the Logs tab)."
                .to_string(),
        ),
        _ => dash.fc_hint(),
    };
    if let Some(sub) = sub {
        if !detail.is_empty() {
            detail.push(Span::raw("   "));
        }
        detail.push(Span::styled(sub, t.dim()));
    }
    if !detail.is_empty() {
        cells.push(Cell::Line(Line::from(detail)));
    }

    // Battery gauge + trend.
    if let Some(battery) = dash.battery {
        let color = if battery > 50.0 {
            t.success()
        } else if battery > 20.0 {
            t.warning()
        } else {
            t.error()
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
        nav.push(Span::styled("GPS ", t.dim()));
        nav.push(Span::styled(fix.clone(), t.bright()));
        nav.push(Span::raw("   "));
    }
    if let Some(sats) = dash.satellites {
        nav.push(Span::styled("sats ", t.dim()));
        nav.push(Span::styled(sats.to_string(), t.bright()));
        nav.push(Span::raw("   "));
    }
    if let Some(alt) = dash.alt {
        nav.push(Span::styled("alt ", t.dim()));
        nav.push(Span::styled(format!("{alt:.1} m"), t.bright()));
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
                color: t.accent(),
            });
        }
    }

    render_cells(frame, t, inner, cells);
}

pub(super) fn video_panel(frame: &mut Frame, area: Rect, core: &Core, dash: &Dashboard) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Video");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let running = matches!(dash.video_state.as_str(), "running" | "streaming");
    let dot_color = if running {
        t.success()
    } else if matches!(dash.video_state.as_str(), "" | "unknown") {
        t.muted()
    } else {
        t.warning()
    };
    let state = if dash.video_state.is_empty() {
        "unknown".to_string()
    } else {
        dash.video_state.clone()
    };
    let line = Line::from(vec![
        Span::styled("● ", Style::default().fg(dot_color)),
        Span::styled(state, t.bright()),
    ]);
    // The viewer URL lives in the Links panel; this panel shows live state only.
    frame.render_widget(Paragraph::new(line), inner);
}

pub(super) fn link_panel(frame: &mut Frame, area: Rect, core: &Core, dash: &Dashboard) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Link");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let cloud = cloud_span(t, &dash.cloud_relay);
    let remote = remote_span(t, &dash.remote_status);
    let hotspot = if dash.hotspot.is_empty() {
        Span::styled("off", t.dim())
    } else {
        Span::styled(dash.hotspot.clone(), t.bright())
    };
    let mut lines = vec![
        kv(t, "cloud", 8, cloud),
        kv(t, "remote", 8, remote),
        kv(t, "hotspot", 8, hotspot),
    ];
    // The passphrase is generated per unit, so this view and the installer's
    // closing card are how an operator learns it. Only rendered when there is
    // a real value to show.
    if !dash.hotspot.is_empty() && !dash.hotspot_passphrase.is_empty() {
        lines.push(kv(
            t,
            "wifi key",
            8,
            Span::styled(dash.hotspot_passphrase.clone(), t.bright()),
        ));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

pub(super) fn services_panel(frame: &mut Frame, area: Rect, core: &Core, dash: &Dashboard) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Services");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let stopped = dash.services_total.saturating_sub(dash.services_running);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("● {} running", dash.services_running),
            Style::default().fg(t.success()),
        ),
        Span::raw("   "),
        Span::styled(format!("○ {stopped} stopped"), t.dim()),
    ])];

    if dash.has_steps && !dash.steps_all_complete {
        lines.push(Line::from(Span::styled("setup steps", t.dim())));
        for step in &dash.steps {
            let (glyph, color) = step_dot(t, &step.state);
            lines.push(Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(step.label.clone(), t.bright()),
                Span::raw("  "),
                Span::styled(state_label(&step.state), t.dim()),
            ]));
            // A dim sub-line explaining a step that still needs attention.
            if step.state != "complete" && !step.detail.is_empty() {
                lines.push(Line::from(Span::styled(format!("    {}", step.detail), t.dim())));
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
            lines.push(Line::from(Span::styled(dash.next_action.clone(), t.dim())));
        }
    }
    if let Some(summary) = hardware_summary(core) {
        lines.push(Line::from(summary));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// The hardware-check summary line: item counts per state, from the setup
/// status `hardware_check.items`.
fn hardware_summary(core: &Core) -> Option<Vec<Span<'static>>> {
    let t = &core.theme;
    let counts = core.dash()?.hardware_counts.as_ref()?;
    let mut spans = vec![Span::styled("hardware ", t.dim())];
    for (state, n) in counts {
        let color = match state.as_str() {
            "ok" | "pass" | "present" => t.success(),
            "warn" | "warning" | "degraded" => t.warning(),
            "fail" | "error" | "missing" => t.error(),
            _ => t.muted(),
        };
        spans.push(Span::styled(format!("{n} {state}  "), Style::default().fg(color)));
    }
    Some(spans)
}

/// Colour the cloud-relay display string by its state.
pub(super) fn cloud_span(t: &Theme, value: &str) -> Span<'static> {
    let color = if value.starts_with("paired") {
        t.success()
    } else if value.starts_with("configured") {
        t.warning()
    } else {
        t.muted()
    };
    Span::styled(value.to_string(), Style::default().fg(color))
}

/// Colour the remote-access status string by its state.
pub(super) fn remote_span(t: &Theme, value: &str) -> Span<'static> {
    let color = match value {
        "running" | "connected" | "enabled" | "up" => t.success(),
        "" | "disabled" | "off" | "stopped" | "unknown" => t.muted(),
        _ => t.warning(),
    };
    Span::styled(value.to_string(), Style::default().fg(color))
}

/// Glyph and colour for a setup-step state.
pub(super) fn step_dot(t: &Theme, state: &str) -> (&'static str, Color) {
    match state {
        "complete" => ("●", t.success()),
        "in_progress" => ("◐", t.accent()),
        "needs_action" => ("▲", t.warning()),
        _ => ("○", t.muted()),
    }
}
