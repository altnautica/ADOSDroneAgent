//! Interactive full-screen terminal I/O over one of two backends.
//!
//! - **Controlling terminal** ([`Tty::open_controlling`], Linux): opens
//!   `/dev/tty` directly, so it still reaches the operator's keyboard under
//!   `curl … | sudo bash`, where fd 0 is the piped script. Raw mode via termios,
//!   a poll-based read loop, and `SIGWINCH` / `SIGINT` handlers. On other hosts
//!   it is unavailable and the open returns `Ok(None)`.
//! - **Stdio** ([`Tty::open_stdio`], every OS): crossterm raw mode, the
//!   alternate screen and crossterm events on stdin/stdout, for tools that are
//!   run from an interactive shell on macOS, Windows or Linux.
//!
//! Both draw the same way: [`Tty::render`] centers a body panel between a
//! header, a progress rail and a footer, building the whole frame with
//! [`crate::frame`] (a pure, host-independent compositor) and writing it in one
//! syscall with absolute cursor placement; [`Tty::present`] writes a
//! caller-composed grid. Raw mode and the alternate screen are restored on
//! `Drop`, and a panic hook restores them as a backstop for release builds that
//! abort on panic.

#[cfg(target_os = "linux")]
mod dev_tty;
mod keys;
mod stdio;

use std::sync::atomic::{AtomicBool, Ordering};

pub use keys::{parse_key, Input, KeyEvent};

use crate::frame::{self, Chrome, Screen, TermSize};
use crate::render;
use crate::theme::Theme;

/// The pending-interrupt flag. The `/dev/tty` backend's `SIGINT` handler sets
/// it; the stdio backend sets it when a Ctrl-C key arrives. A render loop
/// checks it each tick through [`take_interrupt`] and exits cleanly (leaving
/// the alternate screen via `Drop`) rather than dying mid-frame.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Read and clear the pending-interrupt flag.
pub fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

/// Probe the terminal size (via the stdout ioctl, which is the operator
/// terminal under `curl … | sudo bash`), falling back to a sane 80x24.
pub fn probe_size() -> TermSize {
    match ratatui::crossterm::terminal::size() {
        Ok((c, r)) => TermSize {
            cols: usize::from(c.max(1)),
            rows: usize::from(r.max(1)),
        },
        Err(_) => TermSize { cols: 80, rows: 24 },
    }
}

/// The terminal a [`Tty`] reads from and writes to.
enum Backend {
    #[cfg(target_os = "linux")]
    DevTty(dev_tty::DevTty),
    Stdio(stdio::Stdio),
}

/// A raw, restore-on-drop handle to the terminal, drawn to as a full-screen
/// alternate-screen app.
pub struct Tty {
    backend: Backend,
    /// The chrome (step of steps + label) drawn around the current screen.
    chrome: Chrome,
    /// The size the last frame was drawn for; a change triggers a full clear.
    last_size: Option<TermSize>,
}

impl Tty {
    fn with(backend: Backend) -> Tty {
        Tty {
            backend,
            chrome: Chrome::default(),
            last_size: None,
        }
    }

    /// Open `/dev/tty`, snapshot its settings, put it in raw mode, and switch to
    /// the alternate screen. Returns `Ok(None)` when there is no usable
    /// controlling terminal (CI, a fully piped run, or a non-Linux host) so the
    /// caller falls back to a non-interactive path.
    #[cfg(target_os = "linux")]
    pub fn open_controlling() -> std::io::Result<Option<Tty>> {
        Ok(dev_tty::DevTty::open().map(|b| Tty::with(Backend::DevTty(b))))
    }

    /// Open `/dev/tty` (Linux only): on this host there is none, so `Ok(None)`.
    #[cfg(not(target_os = "linux"))]
    pub fn open_controlling() -> std::io::Result<Option<Tty>> {
        Ok(None)
    }

    /// Whether a controlling terminal can be opened (the interactive-gate proof).
    #[cfg(target_os = "linux")]
    pub fn is_available_controlling() -> bool {
        dev_tty::DevTty::is_available()
    }

    /// Whether a controlling terminal can be opened: never off Linux.
    #[cfg(not(target_os = "linux"))]
    pub fn is_available_controlling() -> bool {
        false
    }

    /// Enter crossterm raw mode + the alternate screen on stdin/stdout. Returns
    /// `Ok(None)` when stdout or stdin is not a terminal (CI / piped), so the
    /// caller falls back to a line-based renderer.
    pub fn open_stdio() -> std::io::Result<Option<Tty>> {
        Ok(stdio::Stdio::open()?.map(|b| Tty::with(Backend::Stdio(b))))
    }

    /// Whether stdin and stdout are both terminals.
    pub fn is_available_stdio() -> bool {
        stdio::Stdio::is_available()
    }

    /// The current terminal size.
    pub fn size(&self) -> TermSize {
        probe_size()
    }

    /// The inner text width a widget's body lines have (so a widget can build a
    /// full-width selection bar). Derived from the current terminal size.
    pub fn body_width(&self) -> usize {
        render::panel_body_width(frame::panel_width(probe_size().cols))
    }

    /// Set the chrome (progress rail) for the screens that follow. A `total` of
    /// zero hides the rail (a welcome screen).
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
        self.write(&frame::to_ansi(&grid, cleared, theme));
        self.last_size = Some(size);
    }

    /// Present a pre-composed full-screen grid (one full-width line per row),
    /// clearing on the first paint and after a resize. Unlike [`Tty::render`]
    /// (the centered-panel layout) this writes a caller-built grid, so a
    /// progress view can own its own split layout while reusing the same
    /// single-syscall ANSI writer.
    pub fn present(&mut self, grid: &[String], theme: &Theme) {
        let size = probe_size();
        let cleared = self.last_size != Some(size);
        self.write(&frame::to_ansi(grid, cleared, theme));
        self.last_size = Some(size);
    }

    /// Force the next [`Tty::present`] / [`Tty::render`] to fully clear the
    /// screen (a wizard → progress handoff in one alternate-screen session).
    pub fn force_clear_next(&mut self) {
        self.last_size = None;
    }

    /// Discard any queued (type-ahead) terminal input, so a keystroke made
    /// during a write-only phase does not instantly dismiss the next card.
    pub fn flush_input(&mut self) {
        match &mut self.backend {
            #[cfg(target_os = "linux")]
            Backend::DevTty(t) => t.flush_input(),
            Backend::Stdio(s) => s.flush_input(),
        }
    }

    /// Let Ctrl-C raise `SIGINT` during a write-only phase while keeping the
    /// rest of raw mode (`/dev/tty` backend; the handler sets the interrupt
    /// flag read by [`take_interrupt`]). A no-op on the stdio backend, where
    /// Ctrl-C arrives as a key and [`Tty::poll_interrupt`] handles it.
    pub fn enable_signals(&mut self) {
        match &mut self.backend {
            #[cfg(target_os = "linux")]
            Backend::DevTty(t) => t.enable_signals(),
            Backend::Stdio(_) => {}
        }
    }

    /// Non-blocking: drain pending key events and, if a Ctrl-C is among them,
    /// set the interrupt flag (stdio backend). A render loop that otherwise
    /// blocks on a channel calls this each tick. A no-op on the `/dev/tty`
    /// backend, where Ctrl-C raises `SIGINT` after [`Tty::enable_signals`].
    pub fn poll_interrupt(&mut self) {
        match &mut self.backend {
            #[cfg(target_os = "linux")]
            Backend::DevTty(_) => {}
            Backend::Stdio(s) => s.poll_interrupt(),
        }
    }

    /// Block for one key press and decode it. A terminal resize wakes the wait
    /// and returns [`KeyEvent::Resize`] so the caller repaints at the new size.
    pub fn read_key(&mut self) -> std::io::Result<KeyEvent> {
        match &mut self.backend {
            #[cfg(target_os = "linux")]
            Backend::DevTty(t) => t.read_key(),
            Backend::Stdio(s) => s.read_key(),
        }
    }

    /// Block up to `ms` for a key press. On timeout returns [`Input::Tick`] so an
    /// animated spinner can advance; a resize returns [`Input::Key`] with
    /// [`KeyEvent::Resize`].
    pub fn read_input(&mut self, ms: u64) -> Input {
        match &mut self.backend {
            #[cfg(target_os = "linux")]
            Backend::DevTty(t) => t.read_input(ms),
            Backend::Stdio(s) => s.read_input(ms),
        }
    }

    fn write(&mut self, out: &str) {
        match &mut self.backend {
            #[cfg(target_os = "linux")]
            Backend::DevTty(t) => t.write(out),
            Backend::Stdio(s) => s.write(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_size_is_never_zero() {
        let s = probe_size();
        assert!(s.cols >= 1 && s.rows >= 1, "probed size must be usable");
    }

    #[test]
    fn interrupt_flag_is_consumed_once() {
        INTERRUPTED.store(true, Ordering::SeqCst);
        assert!(take_interrupt());
        assert!(!take_interrupt());
    }
}
