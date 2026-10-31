//! Text entry widgets: a live-sanitized single-line input and a masked
//! passphrase input.

use super::{backspace, hint, Flow};
use crate::render::cursor_glyph;
use crate::theme::Theme;
use crate::tty::{KeyEvent, Tty};

/// The text cursor drawn after the typed value.
fn cursor_bar(theme: &Theme) -> &'static str {
    if theme.ascii {
        "_"
    } else {
        "▏"
    }
}

/// Live-sanitized single-line input. `sanitize` filters every keystroke so an
/// invalid value cannot be typed; `derive` renders a live preview line (e.g.
/// `<name>.local`); `validate` gates Enter (returns `Some(reason)` to refuse).
#[allow(clippy::too_many_arguments)]
pub fn text_input<S, D, V>(
    tty: &mut Tty,
    theme: &Theme,
    section: &str,
    prompt: &str,
    initial: &str,
    preview_label: &str,
    sanitize: S,
    derive: D,
    validate: V,
) -> Flow<String>
where
    S: Fn(&mut String, char),
    D: Fn(&str) -> Option<String>,
    V: Fn(&str) -> Option<String>,
{
    let mut raw = initial.to_string();
    let mut error: Option<String> = None;
    loop {
        let mut body = vec![
            theme.heading(prompt),
            String::new(),
            format!(
                " {} {}{}",
                theme.accent(cursor_glyph(theme)),
                theme.bold(&raw),
                theme.accent(cursor_bar(theme))
            ),
            String::new(),
        ];
        if let Some(derived) = derive(&raw) {
            body.push(format!(
                "   {}  {}",
                theme.dim(preview_label),
                theme.accent(&derived)
            ));
        }
        if let Some(e) = &error {
            body.push(theme.warn(e));
        }
        tty.render(
            theme,
            section,
            &body,
            &hint(theme, &["type a value", "Enter to confirm"]),
        );
        match tty.read_key() {
            Ok(KeyEvent::Char(c)) => {
                sanitize(&mut raw, c);
                error = None;
            }
            Ok(KeyEvent::Space) => {
                sanitize(&mut raw, ' ');
                error = None;
            }
            Ok(KeyEvent::Backspace) => {
                backspace(&mut raw);
                error = None;
            }
            Ok(KeyEvent::Enter) => match validate(&raw) {
                None => return Flow::Value(raw),
                Some(reason) => error = Some(reason),
            },
            Ok(KeyEvent::Esc) => return Flow::Back,
            Ok(KeyEvent::CtrlC) => return Flow::Abort,
            _ => {}
        }
    }
}

/// Masked passphrase input. Tab toggles reveal, Enter confirms once `min_len` is
/// met. Spaces are literal (passphrases may contain them).
pub fn password_input(
    tty: &mut Tty,
    theme: &Theme,
    section: &str,
    prompt: &str,
    min_len: usize,
) -> Flow<String> {
    let mut raw = String::new();
    let mut reveal = false;
    loop {
        let body = vec![
            theme.heading(prompt),
            String::new(),
            format!(
                " {} {}{}   {}",
                theme.accent(cursor_glyph(theme)),
                theme.bold(&masked(theme, &raw, reveal)),
                theme.accent(cursor_bar(theme)),
                theme.dim(if reveal {
                    "(Tab to hide)"
                } else {
                    "(Tab to show)"
                })
            ),
        ];
        tty.render(
            theme,
            section,
            &body,
            &hint(theme, &["type the password", "Enter to connect"]),
        );
        match tty.read_key() {
            Ok(KeyEvent::Char(c)) => raw.push(c),
            Ok(KeyEvent::Space) => raw.push(' '),
            Ok(KeyEvent::Backspace) => backspace(&mut raw),
            Ok(KeyEvent::Tab) => reveal = !reveal,
            Ok(KeyEvent::Enter) => {
                if raw.chars().count() >= min_len {
                    return Flow::Value(raw);
                }
            }
            Ok(KeyEvent::Esc) => return Flow::Back,
            Ok(KeyEvent::CtrlC) => return Flow::Abort,
            _ => {}
        }
    }
}

/// The shown form of a passphrase: itself when revealed, else one mask glyph
/// per character.
fn masked(theme: &Theme, raw: &str, reveal: bool) -> String {
    if reveal {
        raw.to_string()
    } else {
        let mask = if theme.ascii { "*" } else { "•" };
        mask.repeat(raw.chars().count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorTier;

    #[test]
    fn masked_hides_each_char_until_revealed() {
        let t = Theme {
            ascii: true,
            tier: ColorTier::None,
        };
        assert_eq!(masked(&t, "pa ss", false), "*****");
        assert_eq!(masked(&t, "pa ss", true), "pa ss");
    }
}
