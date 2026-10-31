//! The onboarding wizard's own widgets: the Wi-Fi picker, the acknowledgement
//! and welcome cards, the live status board with its spinner, and the
//! keystroke rules for the device-name, pairing-code, SSID and region fields.
//!
//! They are built in the same style as the shared [`ados_term::widgets`]
//! (a styled body handed to [`Tty::render`], a [`Flow`] result, pure edit
//! logic unit-tested without a terminal).

use ados_term::render;
use ados_term::theme::Theme;
use ados_term::tty::{Input, KeyEvent, Tty};
use ados_term::widgets::{arrows_move, gutter, hint, nav_down, nav_up, Flow};

/// A Wi-Fi network row.
#[derive(Clone)]
pub struct WifiRow {
    pub ssid: String,
    pub signal: u8,
    pub secured: bool,
    pub in_use: bool,
}

/// What the operator picked in the Wi-Fi list.
pub enum WifiPick {
    Network { ssid: String, secured: bool },
    Hidden,
    Rescan,
}

/// An acknowledgement screen's action.
pub enum Ack {
    Continue,
    Rescan,
}

/// The per-item state of a live status board row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    Queued,
    Active,
    Ok,
    Failed,
}

/// One row on the live status board.
pub struct BoardItem {
    pub label: String,
    pub state: ItemState,
    pub detail: Option<String>,
}

/// The result of a spinner-animated background task.
pub enum Spin<T> {
    Done(T),
    Aborted,
}

// ── keystroke rules (unit-tested) ────────────────────────────────────────

/// Insert-rule for a device name: lowercase `[a-z0-9]` pass through; any other
/// character collapses to a single `-`, never leading and never doubled. This
/// makes an invalid hostname impossible to type, so the raw field and the live
/// `<name>.local` preview can never disagree.
pub fn insert_hostname_char(raw: &mut String, ch: char) {
    let c = ch.to_ascii_lowercase();
    if c.is_ascii_lowercase() || c.is_ascii_digit() {
        raw.push(c);
    } else if !raw.is_empty() && !raw.ends_with('-') {
        raw.push('-');
    }
}

/// Insert-rule for a pairing code: uppercase `[A-Z0-9-]` only; everything else
/// is dropped so the code stays in its canonical shape as it is typed.
pub fn insert_pair_char(raw: &mut String, ch: char) {
    let c = ch.to_ascii_uppercase();
    if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-' {
        raw.push(c);
    }
}

/// Insert-rule for a Wi-Fi network name: any visible character passes through
/// (an SSID may contain almost anything); control characters are dropped.
pub fn insert_ssid_char(raw: &mut String, ch: char) {
    if !ch.is_control() {
        raw.push(ch);
    }
}

/// Insert-rule for a 2-letter region code: uppercase letters only, capped at
/// two, so the field can only ever hold a well-formed ISO country code.
pub fn insert_region_char(raw: &mut String, ch: char) {
    if raw.chars().count() >= 2 {
        return;
    }
    if ch.is_ascii_alphabetic() {
        raw.push(ch.to_ascii_uppercase());
    }
}

// ── interactive widgets ──────────────────────────────────────────────────

/// Scanned Wi-Fi networks, strongest first, plus a "hidden network" row. Up/Down
/// move, Enter picks, `r` rescans.
pub fn wifi_picker(tty: &mut Tty, theme: &Theme, rows: &[WifiRow]) -> Flow<WifiPick> {
    // The list is the networks plus a trailing "hidden network" entry.
    let total = rows.len() + 1;
    let mut cursor = 0usize;
    loop {
        let mut body = vec![theme.heading("Choose a Wi-Fi network"), String::new()];
        for (i, row) in rows.iter().enumerate() {
            let sel = i == cursor;
            // Pad the name to a fixed column so the signal bars and the
            // security / current markers line up down the list. All markers are
            // plain, single-column text so the box-width math stays exact (a
            // wide emoji would push the right border out of alignment).
            let name_plain = render::truncate(&row.ssid, 20);
            let name_padded = format!("{name_plain:<20}");
            let name = if sel {
                theme.bold(&name_padded)
            } else {
                name_padded
            };
            let bars = theme.accent(&signal_bars(row.signal, theme.ascii));
            let sec = theme.dim(&format!(
                "{:<6}",
                if row.secured { "locked" } else { "open" }
            ));
            let cur = if row.in_use {
                theme.accent("current")
            } else {
                "       ".to_string()
            };
            body.push(format!(
                " {} {name}  {bars}  {sec} {cur}",
                gutter(theme, sel)
            ));
        }
        let hidden_sel = cursor == rows.len();
        let hidden_label = "Enter a hidden network…";
        let hidden = if hidden_sel {
            theme.bold(hidden_label)
        } else {
            theme.dim(hidden_label)
        };
        body.push(format!(" {} {hidden}", gutter(theme, hidden_sel)));
        tty.render(
            theme,
            "Wi-Fi",
            &body,
            &hint(theme, &[&arrows_move(theme), "Enter select", "r rescan"]),
        );
        match tty.read_key() {
            Ok(KeyEvent::Up) => cursor = nav_up(cursor),
            Ok(KeyEvent::Down) => cursor = nav_down(cursor, total),
            Ok(KeyEvent::Char('r')) | Ok(KeyEvent::Char('R')) => {
                return Flow::Value(WifiPick::Rescan)
            }
            Ok(KeyEvent::Enter) => {
                if cursor < rows.len() {
                    let row = &rows[cursor];
                    return Flow::Value(WifiPick::Network {
                        ssid: row.ssid.clone(),
                        secured: row.secured,
                    });
                }
                return Flow::Value(WifiPick::Hidden);
            }
            Ok(KeyEvent::Esc) => return Flow::Back,
            Ok(KeyEvent::CtrlC) => return Flow::Abort,
            _ => {}
        }
    }
}

/// A 4-cell signal bar for a 0..100 signal, filled by quartile.
pub fn signal_bars(signal: u8, ascii: bool) -> String {
    let filled = (usize::from(signal) * 4 / 100).clamp(1, 4);
    let (full, empty) = if ascii { ('#', '.') } else { ('▇', '▁') };
    let mut s = String::new();
    for i in 0..4 {
        s.push(if i < filled { full } else { empty });
    }
    s
}

/// An informational acknowledgement card (green ticks, optional rescan). Enter
/// continues, `r` rescans, Esc goes back.
pub fn ack_card(
    tty: &mut Tty,
    theme: &Theme,
    section: &str,
    prompt: &str,
    body_lines: &[String],
    allow_rescan: bool,
) -> Flow<Ack> {
    let hint_parts: Vec<&str> = if allow_rescan {
        vec!["Enter continue", "r rescan"]
    } else {
        vec!["Enter continue"]
    };
    loop {
        let mut body = vec![theme.heading(prompt), String::new()];
        body.extend(body_lines.iter().cloned());
        tty.render(theme, section, &body, &hint(theme, &hint_parts));
        match tty.read_key() {
            Ok(KeyEvent::Enter) => return Flow::Value(Ack::Continue),
            Ok(KeyEvent::Char('r')) | Ok(KeyEvent::Char('R')) if allow_rescan => {
                return Flow::Value(Ack::Rescan)
            }
            Ok(KeyEvent::Esc) => return Flow::Back,
            Ok(KeyEvent::CtrlC) => return Flow::Abort,
            _ => {}
        }
    }
}

/// A single-key welcome splash (no progress rail). Any key or Enter proceeds;
/// Ctrl-C aborts.
pub fn welcome(tty: &mut Tty, theme: &Theme, body_lines: &[String]) -> Flow<()> {
    loop {
        tty.render(
            theme,
            "welcome",
            body_lines,
            &hint(theme, &["Enter to begin", "Ctrl-C to cancel"]),
        );
        match tty.read_key() {
            Ok(KeyEvent::CtrlC) => return Flow::Abort,
            Ok(KeyEvent::Resize) | Ok(KeyEvent::Unknown) => {}
            Ok(_) => return Flow::Value(()),
            Err(_) => return Flow::Abort,
        }
    }
}

/// Build the live status board body for the current item states (`spin` selects
/// the spinner frame on the active row).
pub fn board_body(theme: &Theme, items: &[BoardItem], spin: usize) -> Vec<String> {
    let mut body = Vec::new();
    for item in items {
        let (glyph, label) = match item.state {
            ItemState::Ok => (theme.ok(theme.glyph_ok()), item.label.clone()),
            ItemState::Failed => (theme.fail(theme.glyph_fail()), theme.fail(&item.label)),
            ItemState::Active => (theme.accent(theme.spinner(spin)), theme.bold(&item.label)),
            ItemState::Queued => (theme.dim(theme.glyph_pending()), theme.dim(&item.label)),
        };
        let detail = item
            .detail
            .as_ref()
            .map(|d| format!("  {}", theme.dim(d)))
            .unwrap_or_default();
        body.push(format!(" {glyph} {label}{detail}"));
    }
    body
}

/// Paint one frame of a live status board on the tty.
pub fn paint_board(tty: &mut Tty, theme: &Theme, section: &str, items: &[BoardItem], spin: usize) {
    let body = board_body(theme, items, spin);
    tty.render(theme, section, &body, "");
}

/// Run `worker` on a background thread while animating the board's active row,
/// returning the worker's result. Ctrl-C aborts (after the worker finishes). A
/// resize repaints. This is what makes the braille spinner actually animate
/// while a blocking probe (a Wi-Fi scan, a network join) runs.
pub fn run_with_spinner<T, F>(
    tty: &mut Tty,
    theme: &Theme,
    section: &str,
    items: &[BoardItem],
    worker: F,
) -> Spin<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let handle = std::thread::spawn(worker);
    let mut spin = 0usize;
    loop {
        paint_board(tty, theme, section, items, spin);
        if handle.is_finished() {
            return match handle.join() {
                Ok(v) => Spin::Done(v),
                Err(_) => Spin::Aborted,
            };
        }
        match tty.read_input(90) {
            Input::Key(KeyEvent::CtrlC) => {
                let _ = handle.join();
                return Spin::Aborted;
            }
            Input::Key(KeyEvent::Resize) => {}
            Input::Tick | Input::Key(_) => spin = spin.wrapping_add(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_term::theme::ColorTier;

    #[test]
    fn hostname_sanitizer_makes_invalid_impossible() {
        let mut raw = String::new();
        for c in "My Drone 01".chars() {
            insert_hostname_char(&mut raw, c);
        }
        // Uppercase lowercases; spaces collapse to single dashes; no leading dash.
        assert_eq!(raw, "my-drone-01");
        // A leading disallowed char never produces a leading dash.
        let mut lead = String::new();
        insert_hostname_char(&mut lead, ' ');
        insert_hostname_char(&mut lead, '@');
        assert_eq!(lead, "");
        // Consecutive disallowed chars never double the dash.
        let mut runs = String::new();
        for c in "a  b".chars() {
            insert_hostname_char(&mut runs, c);
        }
        assert_eq!(runs, "a-b");
        // A trailing separator leaves a single transient dash (never doubled);
        // the downstream slugify trims it before the hostname is set.
        let mut trail = String::new();
        for c in "node ".chars() {
            insert_hostname_char(&mut trail, c);
        }
        assert_eq!(trail, "node-");
        assert_eq!(
            crate::steps::config_identity::slugify_hostname(&trail),
            "node"
        );
    }

    #[test]
    fn pair_sanitizer_uppercases_and_drops_junk() {
        let mut raw = String::new();
        for c in "ab9-x z!".chars() {
            insert_pair_char(&mut raw, c);
        }
        // Uppercased, keeps digits + dash, drops the space and the bang.
        assert_eq!(raw, "AB9-XZ");
    }

    #[test]
    fn region_sanitizer_caps_at_two_letters() {
        let mut raw = String::new();
        for c in "u5sa".chars() {
            insert_region_char(&mut raw, c);
        }
        assert_eq!(raw, "US");
    }

    #[test]
    fn signal_bars_fill_by_quartile_and_never_empty() {
        assert_eq!(signal_bars(0, true), "#...");
        assert_eq!(signal_bars(100, true), "####");
        assert_eq!(signal_bars(50, true), "##..");
        // Unicode tier renders exactly four cells.
        assert_eq!(signal_bars(75, false).chars().count(), 4);
    }

    #[test]
    fn board_body_renders_each_state() {
        let theme = Theme {
            ascii: true,
            tier: ColorTier::None,
        };
        let items = vec![
            BoardItem {
                label: "Connected".into(),
                state: ItemState::Ok,
                detail: None,
            },
            BoardItem {
                label: "Checking".into(),
                state: ItemState::Active,
                detail: Some("gateway".into()),
            },
            BoardItem {
                label: "Failed".into(),
                state: ItemState::Failed,
                detail: None,
            },
            BoardItem {
                label: "Saving".into(),
                state: ItemState::Queued,
                detail: None,
            },
        ];
        let body = board_body(&theme, &items, 0);
        assert_eq!(body.len(), items.len());
        let joined = body.join("\n");
        assert!(joined.contains("Connected"));
        assert!(joined.contains("gateway"));
    }
}
