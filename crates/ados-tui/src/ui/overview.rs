//! Tab 1, Overview: the health verdict and the cockpit panels.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{Core, FrameOut, Tab};
use crate::keymap::{Command, Ctx};
use crate::model::Health;

/// The Overview tab holds no state of its own: it renders the global poll.
#[derive(Default)]
pub struct OverviewTab;

impl Tab for OverviewTab {
    fn ctx(&self, _core: &Core) -> Vec<Ctx> {
        Vec::new()
    }

    fn on_command(&mut self, _core: &mut Core, _cmd: Command) -> bool {
        false
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, _out: &mut FrameOut) {
        let rows = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).split(area);
        verdict(f, rows[0], core);
        match core.dash() {
            Some(d) => super::panels::body(f, rows[1], core, d),
            None => {
                let msg = core.view.error.clone().unwrap_or_else(|| "Connecting to the agent…".into());
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
    let Some(d) = core.dash() else {
        f.render_widget(Paragraph::new(Line::from(Span::styled("· · ·", t.dim()))), area);
        return;
    };
    let error = core.view.error.as_deref();
    let stale = core.stale();
    if error.is_some() || stale {
        let word = if core.view.locked() {
            "LOCKED"
        } else if error.is_some() {
            "UNREACHABLE"
        } else {
            "STALE"
        };
        let why = error.unwrap_or("No answer from the agent.");
        let line1 = Line::from(vec![
            Span::styled(format!("{} ", core.sym.warn), Style::default().fg(t.error())),
            Span::styled(word, Style::default().fg(t.error()).add_modifier(Modifier::BOLD)),
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
    let line2 = Line::from(Span::styled(d.status_summary(), t.dim()));
    f.render_widget(Paragraph::new(vec![line1, line2]), area);
}
