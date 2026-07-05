//! Interactive input/output over the controlling terminal, robust under
//! `curl … | sudo bash` (fd 0 is the piped script, not the tty).
//!
//! The onboarding wizard needs keystrokes, but `stdin` under the install
//! one-liner is the shell script being piped in, not the keyboard. The fix is
//! to open `/dev/tty` directly (which still resolves to the operator's terminal
//! under `sudo`), put it in raw mode, switch to the alternate screen, and
//! hand-roll the render + read loop. It never depends on a crossterm event
//! source that would read the piped stdin.
//!
//! Rendering is full-screen: [`Tty::render`] centers a body panel between a
//! header, a progress rail, and a footer, building the whole frame with
//! [`crate::wizard::frame`] (a pure, host-independent compositor) and writing
//! it in one syscall with absolute cursor placement. Only the actual
//! `/dev/tty` open, raw mode, and the poll-based read loop are Linux-gated, so
//! the frame builder is snapshot-tested on any host.
//!
//! Raw mode, the alternate screen, and the exact original terminal settings are
//! all restored on `Drop`. Release builds abort on panic (so `Drop` does not
//! run); a panic hook resets the terminal, leaves the alternate screen, and
//! shows the cursor as a backstop. On a non-Linux host there is no SBC to
//! install, so [`Tty::open`] returns `Ok(None)` and the caller proceeds with
//! the silent, flag-driven path.

use std::fs::File;
#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
use std::io::{Read, Write};

use crate::ui::theme::Theme;
use crate::wizard::frame::{self, Chrome, Screen, TermSize};

/// Enter the alternate screen, clear it, and hide the cursor. Written only by
/// the Linux `open`; the non-Linux build never drives a terminal.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const ENTER_ALT: &str = "\x1b[?1049h\x1b[2J\x1b[?25l";
/// Reset attributes, show the cursor, and leave the alternate screen. Emitted
/// on every restore path so a full-screen session never strands the terminal.
const LEAVE_ALT: &str = "\x1b[0m\x1b[?25h\x1b[?1049l";

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
    /// The terminal was resized; the caller should repaint at the new size.
    Resize,
    /// A byte sequence we do not act on (an unrecognized escape, a stray
    /// control byte). Widgets ignore it.
    Unknown,
}

/// The outcome of a timed read: a decoded key, or a tick when the timeout
/// elapsed with no key (so an animated spinner can advance).
pub enum Input {
    Key(KeyEvent),
    Tick,
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

/// Probe the terminal size (via the stdout ioctl, which is the operator terminal
/// under `curl … | sudo bash`), falling back to a sane 80x24.
pub fn probe_size() -> TermSize {
    match ratatui::crossterm::terminal::size() {
        Ok((c, r)) => TermSize {
            cols: usize::from(c.max(1)),
            rows: usize::from(r.max(1)),
        },
        Err(_) => TermSize { cols: 80, rows: 24 },
    }
}

/// A raw, restore-on-drop handle to the controlling terminal (`/dev/tty`),
/// drawn to as a full-screen alternate-screen app.
pub struct Tty {
    /// Read + write handle to `/dev/tty`.
    file: File,
    /// The chrome (step of steps + label) drawn around the current screen.
    chrome: Chrome,
    /// The size the last frame was drawn for; a change triggers a full clear.
    last_size: Option<TermSize>,
    /// The terminal settings captured before raw mode, restored on `Drop`.
    #[cfg(target_os = "linux")]
    original: nix::sys::termios::Termios,
}

impl Tty {
    /// Open `/dev/tty`, snapshot its settings, put it in raw mode, and switch to
    /// the alternate screen. Returns `Ok(None)` when there is no usable
    /// controlling terminal (CI, a fully piped run, or a non-Linux host) so the
    /// caller falls back to the silent, flag-driven install.
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
        install_winch_handler();
        install_int_handler();
        let mut tty = Tty {
            file,
            chrome: Chrome::default(),
            last_size: None,
            original,
        };
        let _ = tty.file.write_all(ENTER_ALT.as_bytes());
        let _ = tty.file.flush();
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

    /// The current terminal size.
    pub fn size(&self) -> TermSize {
        probe_size()
    }

    /// The inner text width a widget's body lines have (so a widget can build a
    /// full-width selection bar). Derived from the current terminal size.
    pub fn body_width(&self) -> usize {
        crate::wizard::render::panel_body_width(frame::panel_width(probe_size().cols))
    }

    /// Set the chrome (progress rail) for the screens that follow. A `total` of
    /// zero hides the rail (the welcome screen).
    pub fn set_chrome(&mut self, step: usize, total: usize, label: &str) {
        self.chrome = Chrome {
            step,
            total,
            label: label.to_string(),
        };
    }

    /// Draw a full-screen frame: the header + progress rail + the centered body
    /// panel (labeled `section`) + the footer key-hint. The whole frame is built
    /// once and written in a single syscall; the cursor stays hidden. The screen
    /// is cleared on the first paint and after a resize, and otherwise
    /// overwritten in place so an idle repaint does not flicker.
    pub fn render(&mut self, theme: &Theme, section: &str, body: &[String], footer: &str) {
        let size = probe_size();
        let cleared = self.last_size != Some(size);
        let screen = Screen {
            section,
            body,
            footer,
        };
        let grid = frame::compose(theme, &self.chrome, &screen, size);
        let out = frame::to_ansi(&grid, cleared, theme);
        let _ = self.file.write_all(out.as_bytes());
        let _ = self.file.flush();
        self.last_size = Some(size);
    }

    /// Present a pre-composed full-screen grid (one full-width line per row),
    /// clearing on the first paint and after a resize. Unlike [`Tty::render`]
    /// (the wizard's centered-panel layout) this writes a caller-built grid, so
    /// the install progress can own its own split layout while reusing the same
    /// single-syscall ANSI writer and `/dev/tty` handle.
    pub fn present(&mut self, grid: &[String], theme: &Theme) {
        let size = probe_size();
        let cleared = self.last_size != Some(size);
        let out = frame::to_ansi(grid, cleared, theme);
        let _ = self.file.write_all(out.as_bytes());
        let _ = self.file.flush();
        self.last_size = Some(size);
    }

    /// Force the next [`Tty::present`] / [`Tty::render`] to fully clear the
    /// screen. Called once at the wizard→install handoff so the first install
    /// frame overwrites the wizard's last frame in the shared alternate screen.
    pub fn force_clear_next(&mut self) {
        self.last_size = None;
    }

    /// Discard any queued (type-ahead) terminal input. The write-only install
    /// phase never reads `/dev/tty`, so keystrokes the operator makes during the
    /// install pile up in the kernel input queue; flushing them before the
    /// completion card is presented stops a stray byte from dismissing it
    /// instantly (it should hold until a fresh keypress).
    #[cfg(target_os = "linux")]
    pub fn flush_input(&mut self) {
        use nix::sys::termios::{tcflush, FlushArg};
        use std::os::fd::AsFd;
        let _ = tcflush(self.file.as_fd(), FlushArg::TCIFLUSH);
    }

    #[cfg(not(target_os = "linux"))]
    pub fn flush_input(&mut self) {}

    /// Re-enable the terminal signal keys (`ISIG`) while keeping the rest of raw
    /// mode. The wizard reads keys itself so it runs with `ISIG` off; the
    /// write-only install phase wants Ctrl-C to raise `SIGINT` instead, which
    /// the handler installed by [`Tty::open`] turns into a clean interrupt the
    /// render loop observes via [`take_interrupt`].
    #[cfg(target_os = "linux")]
    pub fn enable_signals(&mut self) {
        use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, SetArg};
        use std::os::fd::AsFd;
        if let Ok(mut t) = tcgetattr(self.file.as_fd()) {
            t.local_flags |= LocalFlags::ISIG;
            let _ = tcsetattr(self.file.as_fd(), SetArg::TCSANOW, &t);
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn enable_signals(&mut self) {}

    /// Block for one key press and decode it. A terminal resize wakes the wait
    /// and returns [`KeyEvent::Resize`] so the caller repaints at the new size.
    #[cfg(target_os = "linux")]
    pub fn read_key(&mut self) -> std::io::Result<KeyEvent> {
        use nix::errno::Errno;
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
        use std::os::fd::AsFd;
        loop {
            let mut fds = [PollFd::new(self.file.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(_) => {
                    if take_resized() {
                        return Ok(KeyEvent::Resize);
                    }
                    return self.read_decoded();
                }
                // A signal interrupted the wait. If it was a SIGWINCH resize,
                // surface it so the caller repaints; otherwise (a SIGCHLD when a
                // spawned child — nmcli, lsusb, the hardware probe — exits) it is
                // spurious, so re-poll rather than returning a phantom resize.
                Err(Errno::EINTR) => {
                    if take_resized() {
                        return Ok(KeyEvent::Resize);
                    }
                }
                Err(e) => return Err(std::io::Error::from_raw_os_error(e as i32)),
            }
        }
    }

    /// Block up to `ms` for a key press. On timeout returns [`Input::Tick`] so an
    /// animated spinner can advance; a resize returns [`Input::Key`] with
    /// [`KeyEvent::Resize`].
    #[cfg(target_os = "linux")]
    pub fn read_input(&mut self, ms: u64) -> Input {
        use nix::errno::Errno;
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
        use std::os::fd::AsFd;
        let timeout = PollTimeout::try_from(std::time::Duration::from_millis(ms))
            .unwrap_or(PollTimeout::ZERO);
        let mut fds = [PollFd::new(self.file.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, timeout) {
            Ok(0) => {
                if take_resized() {
                    Input::Key(KeyEvent::Resize)
                } else {
                    Input::Tick
                }
            }
            Ok(_) => {
                if take_resized() {
                    return Input::Key(KeyEvent::Resize);
                }
                match self.read_decoded() {
                    Ok(k) => Input::Key(k),
                    Err(_) => Input::Tick,
                }
            }
            Err(Errno::EINTR) => {
                take_resized();
                Input::Key(KeyEvent::Resize)
            }
            Err(_) => Input::Tick,
        }
    }

    /// Read one already-ready burst off the terminal and decode it.
    #[cfg(target_os = "linux")]
    fn read_decoded(&mut self) -> std::io::Result<KeyEvent> {
        let mut buf = [0u8; 8];
        let n = self.file.read(&mut buf)?;
        if n == 0 {
            // EOF on the terminal (the operator closed it): treat as an abort.
            return Ok(KeyEvent::CtrlC);
        }
        Ok(parse_key(&buf[..n]))
    }

    /// A plain blocking read for non-Linux builds (never reached: the wizard
    /// only opens a `Tty` on Linux, but the method must compile).
    #[cfg(not(target_os = "linux"))]
    pub fn read_key(&mut self) -> std::io::Result<KeyEvent> {
        let mut buf = [0u8; 8];
        let n = self.file.read(&mut buf)?;
        if n == 0 {
            return Ok(KeyEvent::CtrlC);
        }
        Ok(parse_key(&buf[..n]))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn read_input(&mut self, _ms: u64) -> Input {
        Input::Tick
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        let _ = self.file.write_all(LEAVE_ALT.as_bytes());
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

/// The pending-resize flag, set by the `SIGWINCH` handler (async-signal-safe:
/// it only stores to an atomic) and consumed by the read loop.
#[cfg(target_os = "linux")]
static RESIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Read and clear the pending-resize flag.
#[cfg(target_os = "linux")]
fn take_resized() -> bool {
    RESIZED.swap(false, std::sync::atomic::Ordering::SeqCst)
}

/// The `SIGWINCH` signal handler: record that the terminal was resized. Only an
/// atomic store, which is async-signal-safe.
#[cfg(target_os = "linux")]
extern "C" fn handle_winch(_: i32) {
    RESIZED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The pending-interrupt flag, set by the `SIGINT` handler. The render loop
/// checks it each tick and exits cleanly (leaving the alt screen via `Drop`)
/// rather than the process being killed mid-frame with the terminal stranded.
#[cfg(target_os = "linux")]
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Read and clear the pending-interrupt flag. `false` on non-Linux hosts.
#[cfg(target_os = "linux")]
pub fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, std::sync::atomic::Ordering::SeqCst)
}

#[cfg(not(target_os = "linux"))]
pub fn take_interrupt() -> bool {
    false
}

/// The `SIGINT` handler: record the interrupt. Async-signal-safe (only an atomic
/// store); the render loop does the terminal cleanup off the signal path.
#[cfg(target_os = "linux")]
extern "C" fn handle_int(_: i32) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Install the `SIGINT` handler once (no `SA_RESTART`, so a blocking wait wakes).
#[cfg(target_os = "linux")]
fn install_int_handler() {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    let action = SigAction::new(
        SigHandler::Handler(handle_int),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe {
        let _ = sigaction(Signal::SIGINT, &action);
    }
}

/// Install the `SIGWINCH` handler once, WITHOUT `SA_RESTART` so that a resize
/// interrupts a blocking `poll` (returns `EINTR`) and the read loop can repaint.
#[cfg(target_os = "linux")]
fn install_winch_handler() {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    let action = SigAction::new(
        SigHandler::Handler(handle_winch),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // Best-effort: if this fails the UI simply repaints on the next keystroke.
    unsafe {
        let _ = sigaction(Signal::SIGWINCH, &action);
    }
}

/// Install a one-shot panic hook that resets the terminal to a sane cooked mode,
/// leaves the alternate screen, and shows the cursor. Release builds abort on
/// panic (so `Drop` never runs); this is the only chance to un-raw the terminal
/// and leave the alternate screen, and it computes a sane mode from a fresh
/// `/dev/tty` open so it needs no captured state. The previous hook is chained.
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

/// Re-enable canonical mode + echo on the controlling terminal, leave the
/// alternate screen, and show the cursor. Best-effort — a failure here only
/// leaves the operator to run `reset`.
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
        // Leave the alternate screen and show the cursor as well, so a panic in
        // a full-screen frame never strands the operator in a blank buffer.
        let _ = w.write_all(LEAVE_ALT.as_bytes());
        let _ = w.write_all(b"\r\n");
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
    fn probe_size_is_never_zero() {
        let s = probe_size();
        assert!(s.cols >= 1 && s.rows >= 1, "probed size must be usable");
    }

    #[test]
    fn leave_alt_screen_shows_cursor_and_resets() {
        // The restore string emitted on both Drop and the panic hook must leave
        // the alternate screen, show the cursor, and reset attributes, so a
        // release-build panic never strands the terminal in a blank buffer.
        assert!(
            LEAVE_ALT.contains("\x1b[?1049l"),
            "must leave the alt screen"
        );
        assert!(LEAVE_ALT.contains("\x1b[?25h"), "must show the cursor");
        assert!(LEAVE_ALT.contains("\x1b[0m"), "must reset attributes");
        assert!(
            ENTER_ALT.contains("\x1b[?1049h"),
            "open must enter the alt screen"
        );
    }
}
