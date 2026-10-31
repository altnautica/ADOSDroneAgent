//! Decoded key events and the two decoders that produce them: a raw byte-burst
//! parser (the `/dev/tty` backend) and a crossterm event mapper (the stdio
//! backend).

use ratatui::crossterm::event::{KeyCode, KeyEvent as CtKeyEvent, KeyEventKind, KeyModifiers};

/// A single decoded key event read off the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Enter / Return.
    Enter,
    /// Space bar.
    Space,
    /// Escape.
    Esc,
    /// Backspace / Delete-left.
    Backspace,
    /// Tab.
    Tab,
    /// Ctrl-C (or EOF on the terminal).
    CtrlC,
    /// A printable character.
    Char(char),
    /// The terminal was resized; the caller should repaint at the new size.
    Resize,
    /// A byte sequence we do not act on (an unrecognized escape, a stray
    /// control byte). Widgets ignore it.
    Unknown,
}

/// The outcome of a timed read: a decoded key, or a tick when the timeout
/// elapsed with no key (so an animated spinner can advance).
pub enum Input {
    /// A key arrived.
    Key(KeyEvent),
    /// The timeout elapsed with no key.
    Tick,
}

/// Decode a raw byte burst read off the terminal into a [`KeyEvent`]. Pure so it
/// is unit-tested without a terminal. In raw mode an arrow key or paste arrives
/// as a single `read()`, so the whole burst is available here: a lone `0x1B` is
/// `Esc`, `0x1B 0x5B 0x41` is `Up`, and so on. A printable byte (>= 0x20) is
/// decoded as UTF-8 so accented characters in a typed password survive.
pub fn parse_key(bytes: &[u8]) -> KeyEvent {
    let Some(&b0) = bytes.first() else {
        return KeyEvent::Unknown;
    };
    match b0 {
        0x0D | 0x0A => KeyEvent::Enter,
        0x09 => KeyEvent::Tab,
        0x7F | 0x08 => KeyEvent::Backspace,
        0x03 => KeyEvent::CtrlC,
        0x1B => {
            if bytes.len() >= 3 && bytes[1] == b'[' {
                match bytes[2] {
                    b'A' => KeyEvent::Up,
                    b'B' => KeyEvent::Down,
                    b'C' => KeyEvent::Right,
                    b'D' => KeyEvent::Left,
                    _ => KeyEvent::Unknown,
                }
            } else {
                // A bare ESC (no following bytes buffered) is the Escape key.
                KeyEvent::Esc
            }
        }
        0x20 => KeyEvent::Space,
        b if b >= 0x20 => match std::str::from_utf8(bytes) {
            Ok(s) => s
                .chars()
                .next()
                .map(KeyEvent::Char)
                .unwrap_or(KeyEvent::Unknown),
            Err(_) => KeyEvent::Unknown,
        },
        _ => KeyEvent::Unknown,
    }
}

/// Map a crossterm key event to a [`KeyEvent`]. `None` for key-release noise
/// (Windows delivers Press AND Release) so the caller keeps polling.
pub(super) fn map_ct_key(k: CtKeyEvent) -> Option<KeyEvent> {
    if k.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    Some(match k.code {
        KeyCode::Up => KeyEvent::Up,
        KeyCode::Down => KeyEvent::Down,
        KeyCode::Left => KeyEvent::Left,
        KeyCode::Right => KeyEvent::Right,
        KeyCode::Enter => KeyEvent::Enter,
        KeyCode::Esc => KeyEvent::Esc,
        KeyCode::Backspace => KeyEvent::Backspace,
        KeyCode::Tab => KeyEvent::Tab,
        KeyCode::Char(c) if ctrl && (c == 'c' || c == 'C') => KeyEvent::CtrlC,
        KeyCode::Char(' ') => KeyEvent::Space,
        KeyCode::Char(c) => KeyEvent::Char(c),
        _ => KeyEvent::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_space_tab_backspace_ctrlc() {
        assert_eq!(parse_key(&[0x0D]), KeyEvent::Enter);
        assert_eq!(parse_key(&[0x0A]), KeyEvent::Enter);
        assert_eq!(parse_key(&[0x20]), KeyEvent::Space);
        assert_eq!(parse_key(&[0x09]), KeyEvent::Tab);
        assert_eq!(parse_key(&[0x7F]), KeyEvent::Backspace);
        assert_eq!(parse_key(&[0x08]), KeyEvent::Backspace);
        assert_eq!(parse_key(&[0x03]), KeyEvent::CtrlC);
    }

    #[test]
    fn arrows_are_three_byte_csi_sequences() {
        assert_eq!(parse_key(&[0x1B, b'[', b'A']), KeyEvent::Up);
        assert_eq!(parse_key(&[0x1B, b'[', b'B']), KeyEvent::Down);
        assert_eq!(parse_key(&[0x1B, b'[', b'C']), KeyEvent::Right);
        assert_eq!(parse_key(&[0x1B, b'[', b'D']), KeyEvent::Left);
    }

    #[test]
    fn lone_escape_is_esc_and_unknown_csi_is_unknown() {
        assert_eq!(parse_key(&[0x1B]), KeyEvent::Esc);
        // An escape with a following byte that is not `[` is still Esc.
        assert_eq!(parse_key(&[0x1B, b'O']), KeyEvent::Esc);
        // A CSI we do not map (e.g. Home) is Unknown, not a stray char.
        assert_eq!(parse_key(&[0x1B, b'[', b'H']), KeyEvent::Unknown);
    }

    #[test]
    fn printable_ascii_and_utf8_decode_to_char() {
        assert_eq!(parse_key(b"a"), KeyEvent::Char('a'));
        assert_eq!(parse_key(b"Z"), KeyEvent::Char('Z'));
        assert_eq!(parse_key(b"7"), KeyEvent::Char('7'));
        // A 2-byte UTF-8 sequence (é) yields the char, not Unknown.
        assert_eq!(parse_key("é".as_bytes()), KeyEvent::Char('é'));
    }

    #[test]
    fn empty_and_stray_control_bytes_are_unknown() {
        assert_eq!(parse_key(&[]), KeyEvent::Unknown);
        assert_eq!(parse_key(&[0x01]), KeyEvent::Unknown);
    }

    #[test]
    fn crossterm_release_is_ignored_and_press_maps() {
        let press = CtKeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(map_ct_key(press), Some(KeyEvent::Enter));
        let ctrl_c = CtKeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(map_ct_key(ctrl_c), Some(KeyEvent::CtrlC));
        let space = CtKeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(map_ct_key(space), Some(KeyEvent::Space));
        let mut rel = CtKeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        rel.kind = KeyEventKind::Release;
        assert_eq!(map_ct_key(rel), None);
    }
}
