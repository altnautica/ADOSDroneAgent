//! Choice widgets: a two-option confirm, one-of-N lists and a multi-select
//! checklist.

use super::{
    arrows_move, choice_row, gutter, hint, left_right, nav_down, nav_up, toggle_check, CheckItem,
    Choice, Flow,
};
use crate::render::cursor_glyph;
use crate::theme::Theme;
use crate::tty::{KeyEvent, Tty};

/// Two-choice confirm with a pre-highlighted default. Left/Right/Tab move,
/// Enter accepts. Returns `true` for the first (yes) option.
#[allow(clippy::too_many_arguments)]
pub fn confirm_card(
    tty: &mut Tty,
    theme: &Theme,
    section: &str,
    question: &str,
    detail: &[String],
    yes_label: &str,
    no_label: &str,
    default_yes: bool,
) -> Flow<bool> {
    let mut yes = default_yes;
    loop {
        let mut body = Vec::new();
        for d in detail {
            body.push(theme.dim(d));
        }
        if !detail.is_empty() {
            body.push(String::new());
        }
        body.push(theme.heading(question));
        body.push(String::new());
        body.push(options_row(theme, yes_label, no_label, yes));
        tty.render(
            theme,
            section,
            &body,
            &hint(theme, &[&left_right(theme), "Enter to confirm"]),
        );
        match tty.read_key() {
            Ok(KeyEvent::Left) | Ok(KeyEvent::Right) | Ok(KeyEvent::Tab) => yes = !yes,
            Ok(KeyEvent::Enter) => return Flow::Value(yes),
            Ok(KeyEvent::Esc) => return Flow::Back,
            Ok(KeyEvent::CtrlC) => return Flow::Abort,
            _ => {}
        }
    }
}

/// Render the two side-by-side confirm options with the selected one accented.
fn options_row(theme: &Theme, yes_label: &str, no_label: &str, yes: bool) -> String {
    let pick = |label: &str| {
        format!(
            "{} {}",
            theme.accent(cursor_glyph(theme)),
            theme.bold(label)
        )
    };
    let (y, n) = if yes {
        (pick(yes_label), format!("  {}", theme.dim(no_label)))
    } else {
        (format!("  {}", theme.dim(yes_label)), pick(no_label))
    };
    format!("  {y}      {n}")
}

/// One-of-N list. Up/Down move, Enter selects, Esc goes back.
pub fn select_list(
    tty: &mut Tty,
    theme: &Theme,
    section: &str,
    prompt: &str,
    choices: &[Choice],
    default_idx: usize,
) -> Flow<usize> {
    summary_select(tty, theme, section, prompt, &[], choices, default_idx)
}

/// One-of-N with a leading read-only summary block: a heading, the summary
/// lines (each already styled), then the selectable choices. A review screen
/// uses it so the operator sees every answer above the finish/change actions.
pub fn summary_select(
    tty: &mut Tty,
    theme: &Theme,
    section: &str,
    header: &str,
    summary: &[String],
    choices: &[Choice],
    default_idx: usize,
) -> Flow<usize> {
    let mut cursor = default_idx.min(choices.len().saturating_sub(1));
    loop {
        let inner = tty.body_width();
        let mut body = vec![theme.heading(header), String::new()];
        if !summary.is_empty() {
            body.extend(summary.iter().cloned());
            body.push(String::new());
        }
        for (i, c) in choices.iter().enumerate() {
            body.push(choice_row(theme, &c.label, i == cursor, inner));
            if let Some(h) = &c.hint {
                body.push(format!("     {}", theme.dim(h)));
            }
        }
        tty.render(
            theme,
            section,
            &body,
            &hint(theme, &[&arrows_move(theme), "Enter to choose"]),
        );
        match tty.read_key() {
            Ok(KeyEvent::Up) => cursor = nav_up(cursor),
            Ok(KeyEvent::Down) => cursor = nav_down(cursor, choices.len()),
            Ok(KeyEvent::Enter) => return Flow::Value(cursor),
            Ok(KeyEvent::Esc) => return Flow::Back,
            Ok(KeyEvent::CtrlC) => return Flow::Abort,
            _ => {}
        }
    }
}

/// Multi-of-N. Up/Down move, Space toggles, Enter confirms. Recommended rows
/// arrive checked so an Enter-through accepts them. A checked box is a filled
/// amber square; the cursor row is accented.
pub fn checklist(
    tty: &mut Tty,
    theme: &Theme,
    section: &str,
    prompt: &str,
    mut items: Vec<CheckItem>,
) -> Flow<Vec<CheckItem>> {
    let mut cursor = 0usize;
    loop {
        let mut body = vec![theme.heading(prompt), String::new()];
        for (i, item) in items.iter().enumerate() {
            let sel = i == cursor;
            let checkbox = if item.locked {
                // A locked capability is not a choice: show a fixed filled dot,
                // never a checkbox, so it does not read as a toggleable option.
                theme.accent(theme.dot_filled())
            } else if item.checked {
                theme.accent(theme.box_checked())
            } else {
                theme.dim(theme.box_unchecked())
            };
            let mut label = if item.checked {
                theme.heading(&item.label)
            } else if sel {
                theme.bold(&item.label)
            } else {
                item.label.clone()
            };
            if item.locked {
                label = format!("{label} {}", theme.dim("· needed for this profile"));
            }
            body.push(format!(" {} {checkbox} {label}", gutter(theme, sel)));
            body.push(format!("       {}", theme.dim(&item.benefit)));
        }
        tty.render(
            theme,
            section,
            &body,
            &hint(
                theme,
                &[&arrows_move(theme), "Space toggle", "Enter continue"],
            ),
        );
        match tty.read_key() {
            Ok(KeyEvent::Up) => cursor = nav_up(cursor),
            Ok(KeyEvent::Down) => cursor = nav_down(cursor, items.len()),
            Ok(KeyEvent::Space) => {
                toggle_check(&mut items, cursor);
            }
            Ok(KeyEvent::Enter) => return Flow::Value(items),
            Ok(KeyEvent::Esc) => return Flow::Back,
            Ok(KeyEvent::CtrlC) => return Flow::Abort,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorTier;

    #[test]
    fn options_row_marks_only_the_selected_side() {
        let t = Theme {
            ascii: true,
            tier: ColorTier::None,
        };
        assert_eq!(options_row(&t, "Yes", "No", true), "  > Yes        No");
        assert_eq!(options_row(&t, "Yes", "No", false), "    Yes      > No");
    }
}
