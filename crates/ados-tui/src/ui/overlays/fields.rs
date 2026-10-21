//! Text fields drawn by overlays: the boxed one-line field and the in-place
//! editor that replaces a focused row's value cell.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;
use tui_input::Input;

use crate::app::{Core, FrameOut};
use crate::overlay::InputState;

/// Rows `text` takes when wrapped at `width` columns (at least one).
pub(super) fn wrapped_rows(text: &str, width: u16) -> u16 {
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
pub(super) fn field(
    f: &mut Frame,
    core: &Core,
    rect: Rect,
    input: &Input,
    masked: bool,
    out: &mut FrameOut,
) {
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

/// Edit in place: the field replaces the value cell of the focused row; an
/// error or the bounds hint sits on the line below it.
pub(super) fn inline_input(f: &mut Frame, core: &Core, s: &InputState, out: &mut FrameOut) {
    let t = &core.theme;
    let screen = f.area();
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
