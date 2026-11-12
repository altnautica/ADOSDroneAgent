//! Tab 1, Overview: the health verdict and the cockpit panels.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{Core, FrameOut, Tab};
use crate::keymap::{Command, Ctx};
use crate::model::autonomy::NAV_STATUS_PATH;
use crate::model::Health;
use crate::poll::{ResourceId, Sub};
use crate::ui::vehicle::VehicleTab;

/// The Overview renders the global poll, and on a drone the flight-state
/// chip and alert count from its own copy of the Vehicle tab's readings.
#[derive(Default)]
pub struct OverviewTab {
    flight: VehicleTab,
}

impl Tab for OverviewTab {
    fn ctx(&self, _core: &Core) -> Vec<Ctx> {
        Vec::new()
    }

    fn subs(&self, core: &Core) -> Vec<Sub> {
        if core.profile() != "drone" {
            return Vec::new();
        }
        vec![
            Sub::every_ms(ResourceId::Telemetry, "/api/telemetry", 1000),
            Sub::every_ms(ResourceId::Battery, "/api/v1/battery", 2000),
            Sub::every_ms(ResourceId::NavStatus, NAV_STATUS_PATH, 1000),
        ]
    }

    /// Readings from before the Overview was last shown are not counted:
    /// the copy starts empty and fills from the reads that follow.
    fn on_activate(&mut self, _core: &mut Core) {
        self.flight = VehicleTab::default();
    }

    fn on_resource(&mut self, core: &mut Core, id: &ResourceId) {
        self.flight.on_resource(core, id);
    }

    fn on_command(&mut self, _core: &mut Core, _cmd: Command) -> bool {
        false
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, _out: &mut FrameOut) {
        let flight = if core.profile() == "drone" && core.dash().is_some() {
            self.flight.summary(core)
        } else {
            Vec::new()
        };
        let rows = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(flight.len() as u16),
            Constraint::Min(0),
        ])
        .split(area);
        verdict(f, rows[0], core);
        f.render_widget(Paragraph::new(flight), rows[1]);
        let rows = [rows[0], rows[2]];
        match core.dash() {
            Some(d) => super::panels::body(f, rows[1], core, d),
            None => {
                let msg = match core.view.error.as_deref() {
                    Some(e) => why_text(core, e),
                    None => "Connecting to the agent…".into(),
                };
                f.render_widget(
                    Paragraph::new(msg)
                        .style(Style::default().fg(core.theme.warning()))
                        .wrap(Wrap { trim: false }),
                    rows[1],
                );
            }
        }
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        core.dash().map(|d| d.status_summary())
    }
}

/// The one-word health verdict and its sub-line. A snapshot the agent is no
/// longer confirming never shows as a verdict: the failure takes the line, so
/// the last good snapshot cannot read as a live HEALTHY.
fn verdict(f: &mut Frame, area: Rect, core: &Core) {
    let t = &core.theme;
    let error = core.view.error.as_deref();
    let Some(d) = core.dash() else {
        let line = match error {
            Some(e) => Line::from(Span::styled(why_text(core, e), t.warning())),
            None => Line::from(Span::styled("· · ·", t.dim())),
        };
        f.render_widget(Paragraph::new(line), area);
        return;
    };
    let stale = core.stale();
    if error.is_some() || stale {
        let word = if core.view.locked() {
            "LOCKED"
        } else if error.is_some() {
            "UNREACHABLE"
        } else {
            "STALE"
        };
        let why = error.map_or_else(
            || "No answer from the agent.".to_string(),
            |e| why_text(core, e),
        );
        let line1 = Line::from(vec![
            Span::styled(
                format!("{} ", core.sym.warn),
                Style::default().fg(t.error()),
            ),
            Span::styled(
                word,
                Style::default().fg(t.error()).add_modifier(Modifier::BOLD),
            ),
        ]);
        let line2 = Line::from(Span::styled(
            format!("{why} · showing the last snapshot"),
            Style::default().fg(t.warning()),
        ));
        f.render_widget(Paragraph::new(vec![line1, line2]), area);
        return;
    }
    let health = d.health();
    let color = match health {
        Health::Healthy => t.success(),
        Health::Degraded | Health::Setup => t.warning(),
    };
    let word_style = match health {
        Health::Setup => Style::default().fg(color),
        _ => Style::default().fg(color).add_modifier(Modifier::BOLD),
    };
    let line1 = Line::from(vec![
        Span::styled(format!("{} ", health.dot()), Style::default().fg(color)),
        Span::styled(health.label(), word_style),
    ]);
    let summary = match d.health_cause() {
        Some(cause) => format!("{cause} · {}", d.status_summary()),
        None => d.status_summary(),
    };
    let line2 = Line::from(Span::styled(summary, t.dim()));
    f.render_widget(Paragraph::new(vec![line1, line2]), area);
}

/// The failed poll in words: a missing agent names the host and the retry key.
fn why_text(core: &Core, error: &str) -> String {
    if error == crate::poll::NO_AGENT {
        format!("{error} at {}. r retry", core.target.host)
    } else {
        error.to_string()
    }
}
