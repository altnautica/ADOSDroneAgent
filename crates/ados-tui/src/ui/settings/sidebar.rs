//! The Settings sidebar: group headings and page items for this profile,
//! including one page per configurable extension.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{Core, FrameOut, Hit};
use crate::settings::plugin_rows;
use crate::settings::registry::{self, Group, PageBody, PAGES, PLUGIN_PREFIX};
use crate::ui::widgets::{scroll_offset, truncate};

/// The sidebar width, and the width below 100 terminal columns.
pub const WIDE: u16 = 22;
pub const NARROW: u16 = 16;

/// One page as the sidebar lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Route id: a registry id, or `plugin:<plugin id>`.
    pub id: String,
    pub title: String,
    pub group: Group,
    pub body: PageBody,
}

impl Entry {
    /// The extension id of a plugin page.
    pub fn plugin_id(&self) -> Option<&str> {
        self.id.strip_prefix(PLUGIN_PREFIX)
    }
}

/// Every page shown on this profile, in sidebar order. Extension pages sit
/// under EXTENSIONS, before ALL SETTINGS.
pub fn entries(core: &Core) -> Vec<Entry> {
    let profile = core.profile();
    let mut out: Vec<Entry> = Vec::with_capacity(PAGES.len() + 4);
    let plugins = plugin_rows::configurable(core);
    let mut plugins_added = false;
    for p in PAGES.iter().filter(|p| registry::visible(p, profile)) {
        if p.group == Group::AllSettings && !plugins_added {
            plugins_added = true;
            out.extend(plugins.iter().map(plugin_entry));
        }
        out.push(Entry {
            id: p.id.to_string(),
            title: p.title.to_string(),
            group: p.group,
            body: p.body,
        });
    }
    if !plugins_added {
        out.extend(plugins.iter().map(plugin_entry));
    }
    out
}

fn plugin_entry((id, name): &(String, String)) -> Entry {
    Entry {
        id: format!("{PLUGIN_PREFIX}{id}"),
        title: if name.is_empty() { id.clone() } else { name.clone() },
        group: Group::Extensions,
        body: PageBody::Plugin,
    }
}

/// One drawn sidebar line: a heading, or the page at `entries[i]`.
enum SideLine {
    Heading(Group),
    Page(usize),
}

fn lines(entries: &[Entry]) -> Vec<SideLine> {
    let mut out = Vec::with_capacity(entries.len() + Group::ALL.len());
    let mut group = None;
    for (i, e) in entries.iter().enumerate() {
        if group != Some(e.group) {
            group = Some(e.group);
            out.push(SideLine::Heading(e.group));
        }
        out.push(SideLine::Page(i));
    }
    out
}

/// Draw the sidebar. `selected` indexes `entries`; `offset` is the kept
/// scroll position. Each page line records `Hit::Sidebar(index)`.
pub fn render(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    entries: &[Entry],
    selected: usize,
    offset: &mut usize,
    out: &mut FrameOut,
) {
    let t = &core.theme;
    let lines = lines(entries);
    let sel_line = lines
        .iter()
        .position(|l| matches!(l, SideLine::Page(i) if *i == selected))
        .unwrap_or(0);
    let height = area.height as usize;
    // Keep the selected page's heading in view when the page is first in it.
    let anchor = if sel_line > 0 && matches!(lines[sel_line - 1], SideLine::Heading(_)) {
        sel_line - 1
    } else {
        sel_line
    };
    *offset = scroll_offset(anchor, *offset, height);
    if sel_line >= *offset + height {
        *offset = sel_line + 1 - height;
    }
    let width = area.width as usize;
    for (row, line) in lines.iter().enumerate().skip(*offset).take(height) {
        let rect = Rect::new(area.x, area.y + (row - *offset) as u16, area.width, 1);
        let text = match line {
            SideLine::Heading(g) => Line::from(Span::styled(truncate(g.label(), width), t.dim())),
            SideLine::Page(i) => {
                out.hit(rect, Hit::Sidebar(*i));
                let title = truncate(&entries[*i].title, width.saturating_sub(2));
                if *i == selected {
                    Line::from(vec![
                        Span::styled(format!("{} ", core.sym.page_marker), t.accent_bold()),
                        Span::styled(title, t.accent_bold()),
                    ])
                } else {
                    Line::from(vec![Span::raw("  "), Span::styled(title, t.text())])
                }
            }
        };
        f.render_widget(Paragraph::new(text), rect);
    }
}

/// A vertical rule one column wide.
pub fn rule(f: &mut Frame, area: Rect, core: &Core) {
    let glyph = if core.sym.name == "ascii" { "|" } else { "│" };
    let style = ratatui::style::Style::default().fg(core.theme.border());
    for y in area.y..area.bottom() {
        f.render_widget(
            Paragraph::new(Span::styled(glyph, style)),
            Rect::new(area.x, y, 1, 1),
        );
    }
}

/// The hit name of an All settings section.
pub const SECTION_HIT: &str = "settings.section";
/// Width of the All settings section list, and below 100 columns.
const SECTIONS_WIDE: u16 = 16;
const SECTIONS_NARROW: u16 = 12;

/// Draw the All settings section list (`sections`, `selected` highlighted)
/// left of the rows, recording `Hit::Item(SECTION_HIT, i)`; returns the rows
/// area.
pub fn sections(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    sections: &[&str],
    selected: usize,
    out: &mut FrameOut,
) -> Rect {
    let w = if f.area().width < 100 { SECTIONS_NARROW } else { SECTIONS_WIDE };
    let [list, rule_area, rows] = ratatui::layout::Layout::horizontal([
        ratatui::layout::Constraint::Length(w),
        ratatui::layout::Constraint::Length(1),
        ratatui::layout::Constraint::Min(0),
    ])
    .areas(area);
    let height = list.height as usize;
    let offset = selected.saturating_sub(height.saturating_sub(1));
    let t = &core.theme;
    for (i, name) in sections.iter().enumerate().skip(offset).take(height) {
        let rect = Rect::new(list.x, list.y + (i - offset) as u16, list.width, 1);
        out.hit(rect, Hit::Item(SECTION_HIT, i));
        let text = truncate(name, (w as usize).saturating_sub(2));
        let line = if i == selected {
            Line::from(vec![
                Span::styled(format!("{} ", core.sym.page_marker), t.accent_bold()),
                Span::styled(text, t.accent_bold()),
            ])
        } else {
            Line::from(vec![Span::raw("  "), Span::styled(text, t.dim())])
        };
        f.render_widget(Paragraph::new(line), rect);
    }
    rule(f, rule_area, core);
    Rect {
        x: rows.x + 1,
        width: rows.width.saturating_sub(1),
        ..rows
    }
}
