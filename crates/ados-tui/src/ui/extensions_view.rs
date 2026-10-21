//! Drawing the Extensions tab: the left list and the right detail.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use super::{entries, name_of, Entry, ExtensionsTab, Focus};
use crate::app::{Core, FrameOut, Hit};
use crate::ext::detail;
use crate::poll::ResourceId;
use crate::ui::widgets::{scroll_offset, truncate};

/// One drawn list line: a section header, or an entry by list index.
enum ListLine {
    Header(String),
    Note(String),
    Entry(usize, Line<'static>),
}

impl ExtensionsTab {
    pub(super) fn render_list(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let t = &core.theme;
        let (installs, catalog) = entries(core);
        let w = area.width as usize;
        let mut lines = vec![ListLine::Header(format!("INSTALLED ({})", installs.len()))];
        match core.slot(&ResourceId::Plugins) {
            Some(s) if s.value.is_none() => {
                lines.push(ListLine::Note(s.error.clone().unwrap_or_else(|| "Reading…".into())));
            }
            None => lines.push(ListLine::Note("Reading…".into())),
            _ if installs.is_empty() => lines.push(ListLine::Note("none".into())),
            _ => {}
        }
        for (i, inst) in installs.iter().enumerate() {
            let (color, glyph) = detail::status_style(core, &inst.status);
            let version = format!(" {}", inst.version);
            let name_w = w.saturating_sub(4 + version.chars().count());
            let line = Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(format!("{:<name_w$}", truncate(&name_of(core, &inst.id), name_w)), t.text()),
                Span::styled(version, t.dim()),
            ]);
            lines.push(ListLine::Entry(i, line));
        }
        lines.push(ListLine::Note(String::new()));
        lines.push(ListLine::Header(format!("CATALOG ({})", catalog.len())));
        match core.slot(&ResourceId::PluginsCatalog) {
            Some(s) if s.value.is_none() => {
                lines.push(ListLine::Note(s.error.clone().unwrap_or_else(|| "Reading…".into())));
            }
            None => lines.push(ListLine::Note("Reading…".into())),
            _ if catalog.is_empty() => lines.push(ListLine::Note("everything is installed".into())),
            _ => {}
        }
        for (j, c) in catalog.iter().enumerate() {
            let tail = format!(" {} {}", c.version, c.risk);
            let name_w = w.saturating_sub(2 + tail.chars().count());
            let line = Line::from(vec![
                Span::styled(format!("{:<name_w$}", truncate(&c.name, name_w)), t.text()),
                Span::styled(format!(" {}", c.version), t.dim()),
                Span::styled(format!(" {}", c.risk), Style::default().fg(detail::risk_color(t, &c.risk))),
            ]);
            lines.push(ListLine::Entry(installs.len() + j, line));
        }

        let focus_line = lines
            .iter()
            .position(|l| matches!(l, ListLine::Entry(i, _) if *i == self.sel))
            .unwrap_or(0);
        self.list_offset = scroll_offset(focus_line, self.list_offset, area.height as usize);
        let focused = self.focus == super::Focus::List;
        for (row, line) in lines.into_iter().skip(self.list_offset).take(area.height as usize).enumerate() {
            let rect = Rect::new(area.x, area.y + row as u16, area.width, 1);
            let drawn = match line {
                ListLine::Header(h) => Line::from(Span::styled(h, t.dim().add_modifier(Modifier::BOLD))),
                ListLine::Note(n) => Line::from(Span::styled(format!("  {n}"), t.dim())),
                ListLine::Entry(i, line) => {
                    out.hit(rect, Hit::Sidebar(i));
                    let sel = i == self.sel;
                    let marker = if sel {
                        Span::styled(
                            format!("{} ", core.sym.page_marker),
                            if focused { t.accent_bold() } else { t.dim() },
                        )
                    } else {
                        Span::raw("  ")
                    };
                    let mut spans = vec![marker];
                    spans.extend(line.spans.into_iter().map(|s| {
                        if sel && focused {
                            let st = s.style.add_modifier(Modifier::BOLD);
                            s.style(st)
                        } else {
                            s
                        }
                    }));
                    Line::from(spans)
                }
            };
            f.render_widget(Paragraph::new(drawn), rect);
        }
    }

    pub(super) fn render_detail(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let t = &core.theme;
        let mut area = area;
        if let Some(status) = self.installer.status() {
            let line = Line::from(vec![
                Span::styled(format!("{} ", core.sym.spinner_frame(core.frame as usize)), t.accented()),
                Span::styled(status, t.text()),
            ]);
            f.render_widget(Paragraph::new(line), Rect::new(area.x, area.y, area.width, 1));
            area = Rect::new(area.x, area.y + 2.min(area.height), area.width, area.height.saturating_sub(2));
        }
        let lines = match self.selected(core) {
            Some(Entry::Installed(inst)) => detail::installed(core, &inst),
            Some(Entry::Catalog(c)) => detail::catalog(core, &c),
            None => vec![Line::from(Span::styled(
                "No extension selected. Installed extensions and the catalog are listed on the left.",
                t.dim(),
            ))],
        };
        // The config rows take the lower part when the plugin declares any.
        let config_rows = match (&self.pane, self.selected(core)) {
            (Some(p), Some(Entry::Installed(_))) if p.has_rows(core) => p.items(core).len() as u16 + 1,
            _ => 0,
        };
        let (doc, cfg) = if config_rows > 0 && area.height > 8 {
            let h = (config_rows + 2).min(area.height / 2);
            let doc_h = area.height - h;
            (
                Rect::new(area.x, area.y, area.width, doc_h),
                Some(Rect::new(area.x, area.y + doc_h, area.width, h)),
            )
        } else {
            (area, None)
        };
        self.detail_len = lines.len() as u16;
        self.detail_scroll = self.detail_scroll.min(self.detail_len.saturating_sub(1));
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)),
            doc,
        );
        if let (Some(cfg), Some(pane)) = (cfg, self.pane.as_mut()) {
            let style = if self.focus == Focus::Config { t.accent_bold() } else { t.text() };
            let head = Line::from(Span::styled("Config", style.add_modifier(Modifier::UNDERLINED)));
            f.render_widget(Paragraph::new(head), Rect::new(cfg.x, cfg.y, cfg.width, 1));
            let rows = Rect::new(cfg.x, cfg.y + 1, cfg.width, cfg.height.saturating_sub(1));
            pane.draw(f, rows, core, self.focus == Focus::Config, out);
        }
    }
}
