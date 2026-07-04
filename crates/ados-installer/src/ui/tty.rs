//! Interactive input/output over the controlling terminal, robust under
//! `curl … | sudo bash` (fd 0 is the piped script, not the tty).
//!
//! The onboarding wizard needs keystrokes, but `stdin` under the install
//! one-liner is the shell script being piped in, not the keyboard. The fix is
//! to open `/dev/tty` directly (which still resolves to the operator's terminal
//! under `sudo`), put it in raw mode, and hand-roll the render + read loop. This
//! matches `rich.rs`'s render-only philosophy: write ANSI to a file descriptor,
//! reuse [`crate::ui::theme`] for every glyph and color, and never depend on a
//! crossterm event source that would read the piped stdin.
//!
//! Raw mode + the exact original terminal settings are restored on `Drop` for
//! the normal, error, and Ctrl-C paths. Release builds abort on panic (so `Drop`
//! does not run); a panic hook resets the terminal to a sane cooked mode as a
//! backstop. On a non-Linux host there is no SBC to install, so [`Tty::open`]
//! returns `Ok(None)` and the caller proceeds with the silent, flag-driven path.

use std::fs::File;
#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
use std::io::{Read, Write};

/// A single decoded key event read off the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Space,
    Esc,
    Backspace,
    Tab,
    CtrlC,
    Char(char),
    /// A byte sequence we do not act on (an unrecognized escape, a stray
    /// control byte). Widgets ignore it.
    Unknown,
}

/// Decode a raw byte burst read off the terminal into a [`KeyEvent`]. Pure so it
/// is unit-tested without a terminal. In raw mode an arrow key or paste arrives
/// as a single `read()`, so the whole burst is available here: a lone `0x1B` is
/// `Esc`, `0x1B 0x5B 0x41` is `Up`, and so on. A printable byte (>= 0x20) is
/// decoded as UTF-8 so accented characters in a Wi-Fi password survive.
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

/// The terminal box width the wizard cards draw to: terminal columns clamped to
/// the same tidy range as the install progress board (`rich.rs`). Pure over the
/// probed column count.
pub fn box_width_from(cols: u16) -> usize {
    (cols as usize).saturating_sub(2).clamp(40, 64)
}

/// Probe the terminal column count (via the stdout ioctl, which is the operator
/// terminal under `curl … | sudo bash`), falling back to 80.
fn probe_cols() -> u16 {
    ratatui::crossterm::terminal::size()
        .map(|(c, _)| c)
        .unwrap_or(80)
}

/// A raw, restore-on-drop handle to the controlling terminal (`/dev/tty`).
pub struct Tty {
    /// Read + write handle to `/dev/tty`.
    file: File,
    /// Lines drawn in the current in-place frame (for the rewind on repaint).
    painted: usize,
    /// The terminal settings captured before raw mode, restored on `Drop`.
    #[cfg(target_os = "linux")]
    original: nix::sys::termios::Termios,
}

impl Tty {
    /// Open `/dev/tty`, snapshot its settings, and put it in raw mode. Returns
    /// `Ok(None)` when there is no usable controlling terminal (CI, a fully
    /// piped run, or a non-Linux host) so the caller falls back to the silent,
    /// flag-driven install.
    #[cfg(target_os = "linux")]
    pub fn open() -> std::io::Result<Option<Tty>> {
        use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
        use std::os::fd::AsFd;

        let file = match OpenOptions::new().read(true).write(true).open("/dev/tty") {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        // `tcgetattr` succeeding is our proof that this is a real terminal.
        let original = match tcgetattr(file.as_fd()) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        if tcsetattr(file.as_fd(), SetArg::TCSANOW, &raw).is_err() {
            return Ok(None);
        }
        install_panic_reset();
        let mut tty = Tty {
            file,
            painted: 0,
            original,
        };
        tty.hide_cursor();
        Ok(Some(tty))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn open() -> std::io::Result<Option<Tty>> {
        // No SBC to onboard on a non-Linux dev host; the wizard stays out.
        Ok(None)
    }

    /// Whether an interactive terminal is available (the wizard-gate proof).
    #[cfg(target_os = "linux")]
    pub fn is_available() -> bool {
        use nix::sys::termios::tcgetattr;
        use std::os::fd::AsFd;
        match OpenOptions::new().read(true).write(true).open("/dev/tty") {
            Ok(f) => tcgetattr(f.as_fd()).is_ok(),
            Err(_) => false,
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn is_available() -> bool {
        false
    }

    /// The card box width for the current terminal size.
    pub fn cols(&self) -> usize {
        box_width_from(probe_cols())
    }

    /// Repaint a frame in place: rewind over the previous frame, clear below,
    /// then print each line. Mirrors `rich.rs`'s move-up + clear-down mechanic,
    /// so a card redraws with no flicker and no alternate screen.
    pub fn paint(&mut self, lines: &[String]) {
        let mut out = String::new();
        if self.painted > 0 {
            // Cursor Previous Line: move to column 0 of the top block line.
            out.push_str(&format!("\x1b[{}F", self.painted));
        }
        // Clear from the cursor to the end of the screen.
        out.push_str("\x1b[0J");
        for l in lines {
            out.push_str(l);
            out.push_str("\r\n");
        }
        self.painted = lines.len();
        let _ = self.file.write_all(out.as_bytes());
        let _ = self.file.flush();
    }

    /// Freeze the current frame into the scrollback: the next `paint` starts a
    /// fresh block below it instead of rewinding over it. Called when a step is
    /// confirmed so the operator keeps a clean transcript of their answers.
    pub fn commit(&mut self) {
        self.painted = 0;
    }

    /// Block for one key press and decode it.
    pub fn read_key(&mut self) -> std::io::Result<KeyEvent> {
        let mut buf = [0u8; 8];
        let n = self.file.read(&mut buf)?;
        if n == 0 {
            // EOF on the terminal (the operator closed it): treat as an abort.
            return Ok(KeyEvent::CtrlC);
        }
        Ok(parse_key(&buf[..n]))
    }

    /// Hide the cursor for a stable card render.
    pub fn hide_cursor(&mut self) {
        let _ = self.file.write_all(b"\x1b[?25l");
        let _ = self.file.flush();
    }

    /// Show the cursor again.
    pub fn show_cursor(&mut self) {
        let _ = self.file.write_all(b"\x1b[?25h");
        let _ = self.file.flush();
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        self.show_cursor();
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsFd;
            let _ = nix::sys::termios::tcsetattr(
                self.file.as_fd(),
                nix::sys::termios::SetArg::TCSANOW,
                &self.original,
            );
        }
        let _ = self.file.flush();
    }
}

/// Install a one-shot panic hook that resets the terminal to a sane cooked mode.
/// Release builds abort on panic (so `Drop` never runs); this is the only chance
/// to un-raw the terminal, and it computes a sane mode from a fresh `/dev/tty`
/// open so it needs no captured state. The previous hook is chained.
#[cfg(target_os = "linux")]
fn install_panic_reset() {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        sane_reset();
        prev(info);
    }));
}

/// Re-enable canonical mode + echo on the controlling terminal and show the
/// cursor. Best-effort — a failure here only leaves the operator to run `reset`.
#[cfg(target_os = "linux")]
fn sane_reset() {
    use nix::sys::termios::{tcgetattr, tcsetattr, InputFlags, LocalFlags, OutputFlags, SetArg};
    use std::os::fd::AsFd;
    if let Ok(f) = OpenOptions::new().read(true).write(true).open("/dev/tty") {
        if let Ok(mut t) = tcgetattr(f.as_fd()) {
            t.local_flags |=
                LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG | LocalFlags::IEXTEN;
            t.input_flags |= InputFlags::ICRNL | InputFlags::BRKINT;
            t.output_flags |= OutputFlags::OPOST | OutputFlags::ONLCR;
            let _ = tcsetattr(f.as_fd(), SetArg::TCSANOW, &t);
        }
        let mut w = f;
        let _ = w.write_all(b"\x1b[?25h\r\n");
    }
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
    fn box_width_clamps_to_the_tidy_range() {
        assert_eq!(box_width_from(20), 40); // tiny terminal → floor
        assert_eq!(box_width_from(200), 64); // huge terminal → ceiling
        assert_eq!(box_width_from(60), 58); // 60 - 2, inside the range
    }
}
