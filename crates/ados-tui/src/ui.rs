//! ratatui rendering of the dashboard, laid out like the Python `rich`
//! dashboard: a header, a left "Open Setup And Access" panel, a right column
//! split into "Status" and "Telemetry", and a footer.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::model::Dashboard;

fn bold(s: &str) -> Span<'static> {
    Span::styled(s.to_string(), Style::default().add_modifier(Modifier::BOLD))
}

fn panel<'a>(title: &'a str, color: Color) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(color))
}

fn kv_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        bold(label),
        Span::raw("  "),
        Span::raw(value.to_string()),
    ])
}

pub fn render(frame: &mut Frame, dash: Option<&Dashboard>, refreshed: &str, error: Option<&str>) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    // Header.
    let header_text = match dash {
        Some(d) => format!("{}   refreshed {}", d.header_line(), refreshed),
        None => format!("ADOS Drone Agent   refreshed {refreshed}"),
    };
    frame.render_widget(
        Paragraph::new(Line::from(bold(&header_text))).block(panel("", Color::Cyan)),
        rows[0],
    );

    // Footer.
    frame.render_widget(
        Paragraph::new("Open the URL above in a browser | ados status --json | q quit | Ctrl-C")
            .style(Style::default().fg(Color::DarkGray))
            .block(panel("", Color::DarkGray)),
        rows[2],
    );

    // Body: if there is no data yet, show the error/connecting message.
    let Some(dash) = dash else {
        let msg = error.unwrap_or("Connecting to the agent on http://localhost:8080 ...");
        frame.render_widget(
            Paragraph::new(msg.to_string())
                .style(Style::default().fg(Color::Yellow))
                .block(panel("Status", Color::Blue))
                .wrap(Wrap { trim: false }),
            rows[1],
        );
        return;
    };

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body[1]);

    // Left: access URLs.
    let mut url_lines: Vec<Line> = Vec::new();
    for item in &dash.access_urls {
        let marker = if item.primary { "*" } else { " " };
        url_lines.push(Line::from(vec![
            bold(&format!("{marker} {}", item.label)),
            Span::raw("  "),
            Span::raw(item.url.clone()),
        ]));
    }
    frame.render_widget(
        Paragraph::new(url_lines)
            .block(panel("Open Setup And Access", Color::Green))
            .wrap(Wrap { trim: false }),
        body[0],
    );

    // Status: setup steps then the link/video/cloud rows.
    let mut status_lines: Vec<Line> = Vec::new();
    for step in &dash.steps {
        status_lines.push(kv_line(&step.label, &step.value));
    }
    for row in &dash.status_rows {
        status_lines.push(kv_line(&row.label, &row.value));
    }
    frame.render_widget(
        Paragraph::new(status_lines)
            .block(panel("Status", Color::Blue))
            .wrap(Wrap { trim: false }),
        right[0],
    );

    // Telemetry.
    let mut telem_lines: Vec<Line> = Vec::new();
    if dash.telemetry_empty {
        telem_lines.push(kv_line("Telemetry", "waiting for MAVLink"));
    } else {
        for row in &dash.telemetry {
            telem_lines.push(kv_line(&row.label, &row.value));
        }
    }
    telem_lines.push(kv_line(
        "Services",
        &format!("{}/{} running", dash.services_running, dash.services_total),
    ));
    if !dash.next_action.is_empty() {
        telem_lines.push(Line::from(Span::styled(
            dash.next_action.clone(),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(telem_lines)
            .block(panel("Telemetry", Color::White))
            .wrap(Wrap { trim: false }),
        right[1],
    );
}
