//! Body panels: links, autopilot, video, link and services.

use super::*;

pub(super) fn body(
    frame: &mut Frame,
    area: Rect,
    dash: &Dashboard,
    history: &History,
    stale: bool,
) {
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
    autopilot_panel(frame, cockpit[0], dash, history, stale);
    video_panel(frame, cockpit[1], dash);
    link_panel(frame, cockpit[2], dash);
    services_panel(frame, cockpit[3], dash);
}

pub(super) fn links_panel(frame: &mut Frame, area: Rect, dash: &Dashboard) {
    let block = panel("Links");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let groups = &dash.links;
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

pub(super) fn autopilot_panel(
    frame: &mut Frame,
    area: Rect,
    dash: &Dashboard,
    history: &History,
    stale: bool,
) {
    let block = panel("Autopilot");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Flight readings from a snapshot the agent stopped confirming are not
    // live: hide them rather than show a frozen mode, arm state or battery.
    if stale {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no fresh data from the agent · flight readings hidden",
                Style::default().fg(theme::warning()),
            )))
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }

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
        FcLink::Connected => (
            theme::success(),
            // Name the MAVLink family (ArduPilot / PX4) when it is verified;
            // fall back to a generic label when the family is unknown.
            format!(
                "{} connected",
                dash.fc_family_label().unwrap_or_else(|| "FC".to_string())
            ),
        ),
        FcLink::Msp => (
            // An identified, reachable MSP FC is a good state — render it green
            // like a connected FC, not the amber accent (which reads as caution
            // next to the warning colour).
            theme::success(),
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
    // FC — where its live telemetry actually flows). Rendered dim (neutral), not
    // a warning colour: an MSP FC is a byte-pipe through the daemon by design.
    let mut detail: Vec<Span> = Vec::new();
    if let Some(ep) = dash.fc_endpoint() {
        detail.push(Span::styled(ep, dim()));
    }
    let sub = match dash.fc_link() {
        // Honest per the daemon's role: it does not decode MSP telemetry (it is a
        // transparent byte-pipe), so live attitude/battery/GPS show in the
        // dashboard / Mission Control over MSP, not here. Not a fault.
        FcLink::Msp => Some("reachable over MSP · live telemetry in the dashboard / GCS".to_string()),
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

pub(super) fn video_panel(frame: &mut Frame, area: Rect, dash: &Dashboard) {
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

pub(super) fn link_panel(frame: &mut Frame, area: Rect, dash: &Dashboard) {
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
    let mut lines = vec![
        kv("cloud", cloud),
        kv("remote", remote),
        kv("hotspot", hotspot),
    ];
    // The passphrase is generated per unit, so this view and the installer's
    // closing card are how an operator learns it. Only rendered when there is
    // a real value to show.
    if !dash.hotspot.is_empty() && !dash.hotspot_passphrase.is_empty() {
        lines.push(kv(
            "wifi key",
            Span::styled(dash.hotspot_passphrase.clone(), bright()),
        ));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

pub(super) fn services_panel(frame: &mut Frame, area: Rect, dash: &Dashboard) {
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
pub(super) fn cloud_span(value: &str) -> Span<'static> {
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
pub(super) fn remote_span(value: &str) -> Span<'static> {
    let color = match value {
        "running" | "connected" | "enabled" | "up" => theme::success(),
        "" | "disabled" | "off" | "stopped" | "unknown" => theme::muted(),
        _ => theme::warning(),
    };
    Span::styled(value.to_string(), Style::default().fg(color))
}

/// Glyph and colour for a setup-step state.
pub(super) fn step_dot(state: &str) -> (&'static str, Color) {
    match state {
        "complete" => ("●", theme::success()),
        "in_progress" => ("◐", theme::accent()),
        "needs_action" => ("▲", theme::warning()),
        _ => ("○", theme::muted()),
    }
}
