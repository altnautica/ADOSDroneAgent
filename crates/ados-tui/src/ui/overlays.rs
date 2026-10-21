//! Overlay rendering: every [`Overlay`] kind, drawn centred over the screen.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;
use tui_input::Input;

use super::widgets::{centered_fixed, panel, scroll_offset, truncate};
use crate::action::ACTIONS;
use crate::app::{Core, FrameOut};
use crate::keymap::{self, Ctx};
use crate::overlay::{AuthMode, Overlay};
use crate::toast::ToastKind;

/// Draw every open overlay, bottom to top. `under` is the key-context stack of
/// the screen below (for the help overlay).
pub fn render(f: &mut Frame, core: &Core, under: &[Ctx], out: &mut FrameOut) {
    for overlay in &core.overlays {
        render_one(f, core, overlay, under, out);
    }
}

fn frame_box(f: &mut Frame, core: &Core, title: &str, w: u16, h: u16, out: &mut FrameOut) -> Rect {
    let area = centered_fixed(w, h, f.area());
    f.render_widget(Clear, area);
    let mut block = panel(&core.theme, core.sym, title);
    if let Some(bg) = core.theme.canvas() {
        block = block.style(Style::default().bg(bg));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    out.overlay = Some(area);
    inner
}

/// Rows `text` takes when wrapped at `width` columns (at least one).
fn wrapped_rows(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let chars = text.chars().count().max(1);
    chars.div_ceil(width) as u16
}

/// The visible text of an input: masked as bullets when secret.
fn shown(input: &Input, masked: bool) -> String {
    if masked {
        "•".repeat(input.value().chars().count())
    } else {
        input.value().to_string()
    }
}

/// Draw a one-line input field at `rect` and place the cursor in it.
fn field(f: &mut Frame, core: &Core, rect: Rect, input: &Input, masked: bool, out: &mut FrameOut) {
    let t = &core.theme;
    let width = rect.width.saturating_sub(3) as usize;
    let scroll = input.visual_scroll(width);
    let text: String = shown(input, masked).chars().skip(scroll).collect();
    let line = Line::from(vec![
        Span::styled("› ", t.accented()),
        Span::styled(text, t.bright()),
    ]);
    f.render_widget(Paragraph::new(line), rect);
    let cx = rect.x + 2 + (input.visual_cursor().saturating_sub(scroll)) as u16;
    out.cursor = Some((cx.min(rect.right().saturating_sub(1)), rect.y));
}

fn kind_style(core: &Core, kind: ToastKind) -> (&'static str, Style) {
    let t = &core.theme;
    match kind {
        ToastKind::Info => (core.sym.info, t.text()),
        ToastKind::Success => (core.sym.ok, Style::default().fg(t.success())),
        ToastKind::Warn => (core.sym.warn, Style::default().fg(t.warning())),
        ToastKind::Error => (core.sym.err, Style::default().fg(t.error())),
    }
}

fn hint(core: &Core, text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), core.theme.dim()))
}

fn list_lines(core: &Core, items: &[String], selected: usize, height: usize) -> Vec<Line<'static>> {
    let t = &core.theme;
    let off = scroll_offset(selected, 0, height);
    items
        .iter()
        .enumerate()
        .skip(off)
        .take(height)
        .map(|(i, item)| {
            if i == selected {
                Line::from(vec![
                    Span::styled(format!("{} ", core.sym.row_marker), t.accented()),
                    Span::styled(item.clone(), t.accent_bold()),
                ])
            } else {
                Line::from(vec![Span::raw("  "), Span::styled(item.clone(), t.text())])
            }
        })
        .collect()
}

fn render_one(f: &mut Frame, core: &Core, overlay: &Overlay, under: &[Ctx], out: &mut FrameOut) {
    let t = &core.theme;
    let screen = f.area();
    match overlay {
        Overlay::Help => {
            let rows = keymap::help(under);
            let h = (rows.len() as u16 + 3).min(screen.height.saturating_sub(2));
            let inner = frame_box(f, core, "Keys", 64, h, out);
            let mut lines: Vec<Line> = rows
                .iter()
                .map(|(k, d)| {
                    Line::from(vec![
                        Span::styled(format!("{k:>8}  "), t.accented()),
                        Span::styled(d.to_string(), t.text()),
                    ])
                })
                .collect();
            lines.push(hint(core, "Esc close"));
            f.render_widget(Paragraph::new(lines), inner);
        }
        Overlay::Palette(p) => {
            let h = (p.ranked.len() as u16 + 4).clamp(6, 18);
            let inner = frame_box(f, core, "Command palette", 72, h, out);
            field(
                f,
                core,
                Rect::new(inner.x, inner.y, inner.width, 1),
                &p.input,
                false,
                out,
            );
            let mut lines = vec![Line::from("")];
            for (row, idx) in p.ranked.iter().enumerate() {
                let item = &p.items[*idx];
                let label = truncate(&item.label, inner.width.saturating_sub(14) as usize);
                let sel = row == p.selected;
                let style = if sel { t.accent_bold() } else { t.text() };
                let marker = if sel { core.sym.row_marker } else { " " };
                let pad = (inner.width as usize)
                    .saturating_sub(label.chars().count() + item.kind.len() + 3);
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker} "), t.accented()),
                    Span::styled(label, style),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(item.kind, t.dim()),
                ]));
            }
            if p.ranked.is_empty() {
                lines.push(hint(core, "  no match"));
            }
            let rest = Rect::new(
                inner.x,
                inner.y + 1,
                inner.width,
                inner.height.saturating_sub(1),
            );
            f.render_widget(Paragraph::new(lines), rest);
        }
        Overlay::Confirm(c) => {
            let w = 72.min(screen.width);
            let body_h: u16 = c
                .body
                .iter()
                .map(|l| wrapped_rows(l, w.saturating_sub(2)))
                .sum();
            let inner = frame_box(f, core, &c.title, w, (body_h + 6).min(screen.height), out);
            let mut lines: Vec<Line> = c
                .body
                .iter()
                .map(|l| Line::from(Span::styled(l.clone(), t.text())))
                .collect();
            lines.push(Line::from(""));
            match c.typed {
                Some(word) => {
                    lines.push(Line::from(vec![
                        Span::styled("Type ", t.dim()),
                        Span::styled(
                            word,
                            Style::default().fg(t.error()).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" and press Enter to confirm · Esc cancel", t.dim()),
                    ]));
                    let y = inner.y + lines.len() as u16;
                    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
                    if y < inner.bottom() {
                        field(
                            f,
                            core,
                            Rect::new(inner.x, y, inner.width, 1),
                            &c.input,
                            false,
                            out,
                        );
                    }
                }
                None => {
                    let yes = if c.danger { t.error() } else { t.accent() };
                    lines.push(Line::from(vec![
                        Span::styled("y", Style::default().fg(yes).add_modifier(Modifier::BOLD)),
                        Span::styled(" confirm   ", t.dim()),
                        Span::styled("n", t.accent_bold()),
                        Span::styled(" cancel", t.dim()),
                    ]));
                    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
                }
            }
        }
        Overlay::Input(s) if s.inline && out.focus_value.is_some() => {
            // Edit in place: the field replaces the value cell of the focused
            // row; an error or the bounds hint sits on the line below it.
            let rect = out.focus_value.unwrap_or_default();
            f.render_widget(Clear, rect);
            let width = usize::from(rect.width.saturating_sub(1));
            let scroll = s.input.visual_scroll(width);
            let text: String = shown(&s.input, s.masked).chars().skip(scroll).collect();
            let style = t.accent_bold().add_modifier(Modifier::UNDERLINED);
            f.render_widget(Paragraph::new(Span::styled(text, style)), rect);
            let cx = rect.x + (s.input.visual_cursor().saturating_sub(scroll)) as u16;
            out.cursor = Some((cx.min(rect.right().saturating_sub(1)), rect.y));
            let mut note = match (&s.error, &s.hint) {
                (Some(e), _) => Some((e.clone(), Style::default().fg(t.error()))),
                (None, Some(h)) => Some((h.clone(), t.dim())),
                (None, None) => None,
            };
            if s.nullable {
                let clear = "Backspace on empty clears".to_string();
                note = Some(match note {
                    Some((text, style)) => (format!("{text} · {clear}"), style),
                    None => (clear, t.dim()),
                });
            }
            if let Some((text, style)) = note {
                if rect.y + 1 < screen.bottom() {
                    let below = Rect::new(rect.x, rect.y + 1, rect.width, 1);
                    f.render_widget(Clear, below);
                    f.render_widget(
                        Paragraph::new(Span::styled(format!("  {text}"), style)),
                        below,
                    );
                }
            }
            out.overlay = Some(rect);
        }
        Overlay::Input(s) => {
            let extra = u16::from(s.error.is_some()) + u16::from(s.hint.is_some());
            let inner = frame_box(f, core, &s.title, 64, 6 + extra, out);
            let mut lines = vec![Line::from(Span::styled(s.prompt.clone(), t.text()))];
            lines.push(Line::from(""));
            lines.push(Line::from(""));
            if let Some(e) = &s.error {
                lines.push(Line::from(Span::styled(
                    e.clone(),
                    Style::default().fg(t.error()),
                )));
            }
            if let Some(h) = &s.hint {
                lines.push(hint(core, h));
            }
            let mut tail = String::from("Enter ok · Esc cancel");
            if s.nullable {
                tail.push_str(" · Backspace on empty clears");
            }
            lines.push(hint(core, &tail));
            f.render_widget(Paragraph::new(lines), inner);
            field(
                f,
                core,
                Rect::new(inner.x, inner.y + 1, inner.width, 1),
                &s.input,
                s.masked,
                out,
            );
        }
        Overlay::Picker(p) => {
            let h = (p.items.len() as u16 + 3).clamp(5, 20);
            let inner = frame_box(f, core, &p.title, 56, h, out);
            let mut lines = list_lines(
                core,
                &p.items,
                p.selected,
                inner.height.saturating_sub(1) as usize,
            );
            lines.push(hint(core, "↑↓ move · Enter select · Esc cancel"));
            f.render_widget(Paragraph::new(lines), inner);
        }
        Overlay::Checklist(c) => {
            let footer_rows = c.footer.as_deref().map_or(0, |f| wrapped_rows(f, 62));
            let h = (c.items.len() as u16 + 4 + footer_rows).clamp(6, 22);
            let inner = frame_box(f, core, &c.title, 64, h, out);
            let height = inner.height.saturating_sub(1 + footer_rows) as usize;
            let off = scroll_offset(c.selected, 0, height);
            let mut lines: Vec<Line> = c
                .items
                .iter()
                .enumerate()
                .skip(off)
                .take(height)
                .map(|(i, item)| {
                    let mark = match (item.checked, core.sym.name == "ascii") {
                        (true, true) => "[x]",
                        (false, true) => "[ ]",
                        (true, false) => "☑",
                        (false, false) => "☐",
                    };
                    let style = if i == c.selected {
                        t.accent_bold()
                    } else {
                        t.text()
                    };
                    let lock = if item.locked {
                        format!("  {} required", core.sym.read_only)
                    } else {
                        String::new()
                    };
                    Line::from(vec![
                        Span::styled(
                            if i == c.selected {
                                format!("{} ", core.sym.row_marker)
                            } else {
                                "  ".into()
                            },
                            t.accented(),
                        ),
                        Span::styled(format!("{mark} "), style),
                        Span::styled(item.label.clone(), style),
                        Span::styled(lock, t.dim()),
                    ])
                })
                .collect();
            let footer = match (&c.footer, c.items.iter().any(|i| i.bit.is_some())) {
                (Some(f), _) => f.clone(),
                (None, true) => format!("value {}", c.bit_sum()),
                (None, false) => String::new(),
            };
            if !footer.is_empty() {
                lines.push(Line::from(Span::styled(footer, t.accent_bold())));
            }
            lines.push(hint(core, "Space toggle · Enter done · Esc cancel"));
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        Overlay::Result(r) => {
            let h = (r.lines.len() as u16 + 3).clamp(5, screen.height.saturating_sub(4));
            let inner = frame_box(f, core, &r.title, 84, h, out);
            let mut lines: Vec<Line> = r
                .lines
                .iter()
                .skip(r.scroll)
                .map(|(kind, text)| {
                    let (glyph, style) = kind_style(core, *kind);
                    Line::from(vec![
                        Span::styled(format!("{glyph} "), style),
                        Span::styled(text.clone(), t.text()),
                    ])
                })
                .collect();
            let copy = if r.copy.is_some() { " · y copy" } else { "" };
            let body = Rect::new(
                inner.x,
                inner.y,
                inner.width,
                inner.height.saturating_sub(1),
            );
            f.render_widget(
                Paragraph::new(std::mem::take(&mut lines)).wrap(Wrap { trim: false }),
                body,
            );
            let foot = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
            f.render_widget(
                Paragraph::new(hint(core, &format!("↑↓ scroll{copy} · Esc close"))),
                foot,
            );
        }
        Overlay::Secret(s) => {
            let w = 76.min(screen.width);
            let inner_w = w.saturating_sub(2);
            let h = 5 + wrapped_rows(&s.note, inner_w) + wrapped_rows(&s.secret, inner_w);
            let inner = frame_box(f, core, &s.title, w, h, out);
            let lines = vec![
                Line::from(Span::styled(
                    s.note.clone(),
                    Style::default().fg(t.warning()),
                )),
                Line::from(""),
                Line::from(Span::styled(s.secret.clone(), t.accent_bold())),
                Line::from(""),
                hint(core, "y copy · Esc close (it will not be shown again)"),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        Overlay::Actions(sel) => {
            let inner = frame_box(f, core, "Actions", 76, ACTIONS.len() as u16 + 4, out);
            let mut lines: Vec<Line> = Vec::new();
            for (i, action) in ACTIONS.iter().enumerate() {
                let selected = i == *sel;
                let (marker, style) = if selected {
                    (core.sym.row_marker, t.accent_bold())
                } else {
                    (" ", t.bright())
                };
                let mut spans = vec![
                    Span::styled(format!("{marker} "), t.accented()),
                    Span::styled(format!("{:<22}", action.label), style),
                    Span::styled(action.desc, t.dim()),
                ];
                if action.confirm {
                    spans.push(Span::styled("  (confirm)", t.dim()));
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(""));
            lines.push(hint(core, "↑↓ select · Enter run · Esc close"));
            f.render_widget(Paragraph::new(lines), inner);
        }
        Overlay::UpdateSplash => {
            let installed = core.dash().map(|d| d.version.clone()).unwrap_or_default();
            let latest = core.latest_version().unwrap_or_default();
            let inner = frame_box(f, core, "Update available", 56, 7, out);
            let lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(format!("v{installed}"), t.dim()),
                    Span::styled("  →  ", t.accented()),
                    Span::styled(format!("v{latest}"), t.bright()),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("[u] ", t.accented()),
                    Span::styled("update now", t.bright()),
                    Span::styled("      any key: later", t.dim()),
                ]),
            ];
            f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
        }
        Overlay::Auth(a) => {
            let title = match a.mode {
                AuthMode::Key => "API key required",
                AuthMode::Pin => "Dashboard PIN",
                AuthMode::Blocked => "Access",
            };
            let inner = frame_box(f, core, title, 68, 8, out);
            let mut lines = vec![Line::from(Span::styled(
                format!("Agent {}", core.target.base),
                t.dim(),
            ))];
            if a.offer_remember {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!(
                        "Key accepted. Remember it for {}? (y/n)",
                        core.target.label()
                    ),
                    Style::default().fg(t.success()),
                )));
                f.render_widget(Paragraph::new(lines), inner);
                return;
            }
            let prompt = match a.mode {
                AuthMode::Key => "Paste this agent's pairing key:",
                AuthMode::Pin => "Enter the dashboard PIN (4-12 digits):",
                AuthMode::Blocked => "",
            };
            lines.push(Line::from(Span::styled(prompt, t.text())));
            lines.push(Line::from(""));
            lines.push(match &a.message {
                Some(m) => Line::from(Span::styled(m.clone(), Style::default().fg(t.warning()))),
                None => Line::from(""),
            });
            lines.push(hint(core, "Enter submit · Esc close (data stays locked)"));
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            if a.mode != AuthMode::Blocked {
                field(
                    f,
                    core,
                    Rect::new(inner.x, inner.y + 2, inner.width, 1),
                    &a.input,
                    true,
                    out,
                );
            }
        }
        Overlay::Notifications(back) => {
            let h = screen.height.saturating_sub(6).max(6);
            let inner = frame_box(f, core, "Notifications", 90, h, out);
            let rows = inner.height.saturating_sub(1) as usize;
            let all: Vec<_> = core.toasts.history().rev().skip(*back).take(rows).collect();
            let mut lines: Vec<Line> = all
                .into_iter()
                .map(|toast| {
                    let (glyph, style) = kind_style(core, toast.kind);
                    Line::from(vec![
                        Span::styled(format!("{} ", toast.clock), t.dim()),
                        Span::styled(format!("{glyph} "), style),
                        Span::styled(toast.text.clone(), t.text()),
                    ])
                })
                .collect();
            if lines.is_empty() {
                lines.push(hint(core, "No notifications yet"));
            }
            let body = Rect::new(
                inner.x,
                inner.y,
                inner.width,
                inner.height.saturating_sub(1),
            );
            f.render_widget(Paragraph::new(lines), body);
            let foot = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
            f.render_widget(Paragraph::new(hint(core, "↑↓ scroll · Esc close")), foot);
        }
    }
}
