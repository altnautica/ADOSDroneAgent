//! Reusable interactive widgets for full-screen wizards.
//!
//! Each widget builds a body (a `Vec<String>` of pre-styled lines) plus a
//! footer key-hint and hands them to [`crate::tty::Tty::render`], which centers
//! the body in a panel on the full screen with the header, progress rail, and
//! footer around it. The pure movement and edit logic is factored out so it is
//! unit-tested without a terminal; the widget functions themselves are thin
//! build + read loops. A terminal resize returns
//! [`crate::tty::KeyEvent::Resize`], which the loops ignore and so repaint at
//! the new size. The row helpers ([`hint`], [`gutter`], [`choice_row`], …) are
//! public so a tool can build its own widgets in the same style.

mod input;
mod list;

pub use input::{password_input, text_input};
pub use list::{checklist, confirm_card, select_list, summary_select};

use crate::render::{cursor_glyph, dot};
use crate::theme::Theme;

/// The outcome of one widget: a value, a request to go back a step, or an abort
/// (Ctrl-C / terminal closed) that unwinds the whole wizard.
pub enum Flow<T> {
    /// The operator answered.
    Value(T),
    /// Esc: go back one step.
    Back,
    /// Ctrl-C or the terminal closed.
    Abort,
}

/// A single-select option row.
pub struct Choice {
    /// A stable id for the caller.
    pub id: String,
    /// The visible label.
    pub label: String,
    /// An optional dim hint line under the label.
    pub hint: Option<String>,
}

impl Choice {
    /// A choice from borrowed strings.
    pub fn new(id: &str, label: &str, hint: Option<&str>) -> Choice {
        Choice {
            id: id.to_string(),
            label: label.to_string(),
            hint: hint.map(str::to_string),
        }
    }
}

/// A multi-select checklist row.
#[derive(Clone)]
pub struct CheckItem {
    /// A stable id for the caller.
    pub id: String,
    /// The visible label.
    pub label: String,
    /// A dim one-line benefit shown under the label.
    pub benefit: String,
    /// Whether the row is selected.
    pub checked: bool,
    /// A locked row is always on and cannot be toggled.
    pub locked: bool,
}

// ── pure movement + edit logic (unit-tested) ─────────────────────────────

/// Move a cursor up one row (saturating at the top).
pub fn nav_up(pos: usize) -> usize {
    pos.saturating_sub(1)
}

/// Move a cursor down one row (clamped to the last index).
pub fn nav_down(pos: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (pos + 1).min(len - 1)
    }
}

/// Toggle a checklist item unless it is locked. Returns whether it changed.
pub fn toggle_check(items: &mut [CheckItem], idx: usize) -> bool {
    match items.get_mut(idx) {
        Some(item) if !item.locked => {
            item.checked = !item.checked;
            true
        }
        _ => false,
    }
}

/// Delete the last char of the edit buffer.
pub fn backspace(raw: &mut String) {
    raw.pop();
}

// ── row + hint helpers ───────────────────────────────────────────────────

/// An ASCII-safe key hint joined by dot separators (the footer bar text).
pub fn hint(theme: &Theme, parts: &[&str]) -> String {
    parts.join(&format!(" {} ", dot(theme)))
}

/// The "move" hint for an up/down list.
pub fn arrows_move(theme: &Theme) -> String {
    if theme.ascii {
        "Up/Down move".to_string()
    } else {
        "↑ ↓ move".to_string()
    }
}

/// The "choose" hint for a left/right pair.
pub fn left_right(theme: &Theme) -> String {
    if theme.ascii {
        "Left/Right choose".to_string()
    } else {
        "← → choose".to_string()
    }
}

/// One selectable choice row. The selected row is a full-width solid-amber bar
/// with dark text and a filled dot; an unselected row is default text with an
/// empty dot. `inner` is the panel's inner text width.
pub fn choice_row(theme: &Theme, label: &str, selected: bool, inner: usize) -> String {
    if selected {
        theme.selection_bar(&format!(" {} {label}", theme.dot_filled()), inner)
    } else {
        format!(" {} {label}", theme.dim(theme.dot_empty()))
    }
}

/// Gutter for the cursor row (accent arrow) or a plain row (one space).
pub fn gutter(theme: &Theme, selected: bool) -> String {
    if selected {
        theme.accent(cursor_glyph(theme))
    } else {
        " ".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorTier;

    #[test]
    fn cursor_nav_clamps_at_both_ends() {
        assert_eq!(nav_up(0), 0);
        assert_eq!(nav_up(3), 2);
        assert_eq!(nav_down(0, 3), 1);
        assert_eq!(nav_down(2, 3), 2); // clamps at last index
        assert_eq!(nav_down(0, 0), 0); // empty list
    }

    fn items() -> Vec<CheckItem> {
        vec![
            CheckItem {
                id: "radio".into(),
                label: "Radio".into(),
                benefit: "far".into(),
                checked: true,
                locked: false,
            },
            CheckItem {
                id: "cam".into(),
                label: "Camera".into(),
                benefit: "see".into(),
                checked: false,
                locked: true,
            },
        ]
    }

    #[test]
    fn toggle_flips_unlocked_and_refuses_locked() {
        let mut it = items();
        assert!(toggle_check(&mut it, 0));
        assert!(!it[0].checked);
        // A locked row cannot be toggled.
        assert!(!toggle_check(&mut it, 1));
        assert!(!it[1].checked);
        // Out of range is a no-op.
        assert!(!toggle_check(&mut it, 9));
    }

    #[test]
    fn backspace_trims_the_tail() {
        let mut raw = "abc".to_string();
        backspace(&mut raw);
        assert_eq!(raw, "ab");
        backspace(&mut raw);
        backspace(&mut raw);
        backspace(&mut raw); // over-delete is safe
        assert_eq!(raw, "");
    }

    #[test]
    fn choice_row_selected_is_a_full_width_amber_bar() {
        let theme = Theme {
            ascii: false,
            tier: ColorTier::Truecolor,
        };
        let bar = choice_row(&theme, "This flies (Drone)", true, 40);
        assert!(
            bar.contains("\x1b[48;2;235;193;87m"),
            "selected row not an amber bar"
        );
        assert!(bar.contains('●'), "selected row missing the filled dot");
        let plain_row = choice_row(&theme, "This flies (Drone)", false, 40);
        assert!(
            !plain_row.contains("\x1b[48;2;235;193;87m"),
            "unselected row barred"
        );
        assert!(
            plain_row.contains('○'),
            "unselected row missing the empty dot"
        );
    }

    #[test]
    fn hint_joins_with_the_tier_separator() {
        let ascii = Theme {
            ascii: true,
            tier: ColorTier::None,
        };
        assert_eq!(hint(&ascii, &["a", "b"]), "a - b");
        assert_eq!(arrows_move(&ascii), "Up/Down move");
    }
}
