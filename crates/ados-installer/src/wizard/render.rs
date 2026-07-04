//! Card framing + ANSI-width helpers for the onboarding wizard.
//!
//! Every wizard screen is a rounded box with an accent title (the `▌ ADOS`
//! word-mark, the only logo) and a dim key-hint on the bottom border. The body
//! lines carry their own color from [`crate::ui::theme`]; the framing here is
//! ANSI-width-aware so a colored body line still aligns to the right border.

use crate::ui::theme::Theme;

/// The accent cursor gutter shown on the highlighted row.
pub fn cursor_glyph(theme: &Theme) -> &'static str {
    if theme.ascii {
        ">"
    } else {
        "❯"
    }
}

/// The `▌` word-mark used in the card title, with an ASCII fallback.
pub fn wordmark(theme: &Theme) -> &'static str {
    if theme.ascii {
        "#"
    } else {
        "▌"
    }
}

/// The `·` dot separator used in title + hint text, with an ASCII fallback.
pub fn dot(theme: &Theme) -> &'static str {
    if theme.ascii {
        "-"
    } else {
        "·"
    }
}

/// Build a full card: an accent top border with the title, the body lines, and
/// an accent bottom border carrying the dim key-hint. `width` is the box's outer
/// column count (see [`crate::ui::tty::Tty::cols`]).
pub fn card(theme: &Theme, title: &str, body: &[String], hint: &str, width: usize) -> Vec<String> {
    let bc = theme.box_chars();
    let content_w = width.saturating_sub(2);
    let body_w = content_w.saturating_sub(2);

    let mut out = Vec::with_capacity(body.len() + 2);
    out.push(theme.accent(&titled_border(bc.tl, bc.tr, bc.h, title, content_w)));
    let v = theme.accent(bc.v);
    for line in body {
        out.push(format!("{v} {} {v}", fit_to(line, body_w)));
    }
    out.push(theme.accent(&titled_border(bc.bl, bc.br, bc.h, hint, content_w)));
    out
}

/// Build one titled border line: `╭─ <title> ────────╮`. `content_w` is the
/// interior column count (box width minus the two corner glyphs), so the
/// returned line is exactly `content_w + 2` columns, matching the body rows.
fn titled_border(left: &str, right: &str, h: &str, title: &str, content_w: usize) -> String {
    let lead = format!("{} {} ", h, truncate(title, content_w.saturating_sub(4)));
    let dashes = content_w.saturating_sub(lead.chars().count());
    format!("{left}{lead}{}{right}", h.repeat(dashes))
}

/// Truncate to `max` display columns, appending an ellipsis when it would clip.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Right-pad `s` to `w` *visible* columns, ignoring embedded ANSI SGR escapes so
/// a colored body line still aligns to the box border. A line whose visible
/// content overflows `w` is stripped of styling and hard-truncated.
pub fn fit_to(s: &str, w: usize) -> String {
    let vis = visible_width(s);
    if vis <= w {
        format!("{s}{}", " ".repeat(w - vis))
    } else {
        let plain = truncate(&strip_ansi(s), w);
        let pad = w.saturating_sub(plain.chars().count());
        format!("{plain}{}", " ".repeat(pad))
    }
}

/// The number of display columns `s` occupies, skipping ANSI SGR escapes.
pub fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for nc in chars.by_ref() {
                if nc == 'm' {
                    break;
                }
            }
        } else {
            width += 1;
        }
    }
    width
}

/// Drop ANSI SGR escape sequences, leaving the visible text.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for nc in chars.by_ref() {
                if nc == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme {
            color: false,
            ascii: false,
        }
    }

    #[test]
    fn card_rows_are_exactly_the_box_width_with_color_off() {
        let t = theme();
        let body = vec!["hello".to_string(), "world".to_string()];
        let width = 50;
        for line in card(&t, "ADOS · setup", &body, "Enter to continue", width) {
            assert_eq!(
                line.chars().count(),
                width,
                "card line not exactly {width} cols: {line:?}"
            );
        }
    }

    #[test]
    fn card_carries_title_and_hint() {
        let t = theme();
        let lines = card(&t, "ADOS · setup", &["body".to_string()], "Enter", 50);
        assert!(lines.first().unwrap().contains("ADOS"));
        assert!(lines.last().unwrap().contains("Enter"));
    }

    #[test]
    fn fit_to_pads_visible_width_ignoring_ansi() {
        // A colored 1-column glyph pads to the requested visible width.
        let colored = "\u{1b}[36m x\u{1b}[39m"; // visible " x" = 2 cols
        let out = fit_to(colored, 6);
        assert_eq!(visible_width(&out), 6);
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
    }

    #[test]
    fn visible_width_and_strip_ignore_escapes() {
        let colored = "\u{1b}[36m❯\u{1b}[39m";
        assert_eq!(visible_width(colored), 1);
        assert_eq!(strip_ansi(colored), "❯");
    }
}
