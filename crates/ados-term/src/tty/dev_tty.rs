//! The controlling-terminal backend: `/dev/tty` in termios raw mode.
//!
//! Under `curl … | sudo bash` stdin is the piped script, not the keyboard, so
//! this backend opens `/dev/tty` directly (it still resolves to the operator's
//! terminal under `sudo`), puts it in raw mode, switches to the alternate
//! screen, and hand-rolls the read loop with `poll`. It never depends on a
//! crossterm event source that would read the piped stdin.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::termios::{
    cfmakeraw, tcflush, tcgetattr, tcsetattr, FlushArg, InputFlags, LocalFlags, OutputFlags,
    SetArg, Termios,
};

use super::keys::{parse_key, Input, KeyEvent};
use super::INTERRUPTED;

/// Enter the alternate screen, clear it, and hide the cursor.
const ENTER_ALT: &str = "\x1b[?1049h\x1b[2J\x1b[?25l";
/// Reset attributes, show the cursor, and leave the alternate screen. Emitted
/// on every restore path so a full-screen session never strands the terminal.
const LEAVE_ALT: &str = "\x1b[0m\x1b[?25h\x1b[?1049l";

/// An open, raw `/dev/tty`. Restores the captured settings on `Drop`.
pub(super) struct DevTty {
    /// Read + write handle to `/dev/tty`.
    file: File,
    /// The terminal settings captured before raw mode.
    original: Termios,
}

impl DevTty {
    /// Open `/dev/tty` in raw mode on the alternate screen; `None` when there is
    /// no usable controlling terminal.
    pub(super) fn open() -> Option<DevTty> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        // `tcgetattr` succeeding is our proof that this is a real terminal.
        let original = tcgetattr(file.as_fd()).ok()?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(file.as_fd(), SetArg::TCSANOW, &raw).ok()?;
        install_panic_reset();
        install_winch_handler();
        install_int_handler();
        let _ = file.write_all(ENTER_ALT.as_bytes());
        let _ = file.flush();
        Some(DevTty { file, original })
    }

    /// Whether `/dev/tty` opens and is a real terminal.
    pub(super) fn is_available() -> bool {
        match OpenOptions::new().read(true).write(true).open("/dev/tty") {
            Ok(f) => tcgetattr(f.as_fd()).is_ok(),
            Err(_) => false,
        }
    }

    pub(super) fn write(&mut self, out: &str) {
        let _ = self.file.write_all(out.as_bytes());
        let _ = self.file.flush();
    }

    /// Discard queued input. A write-only phase never reads `/dev/tty`, so
    /// keystrokes pile up in the kernel input queue until flushed.
    pub(super) fn flush_input(&mut self) {
        let _ = tcflush(self.file.as_fd(), FlushArg::TCIFLUSH);
    }

    /// Re-enable the terminal signal keys (`ISIG`) while keeping the rest of raw
    /// mode, so Ctrl-C raises `SIGINT` and the installed handler records it.
    pub(super) fn enable_signals(&mut self) {
        if let Ok(mut t) = tcgetattr(self.file.as_fd()) {
            t.local_flags |= LocalFlags::ISIG;
            let _ = tcsetattr(self.file.as_fd(), SetArg::TCSANOW, &t);
        }
    }

    pub(super) fn read_key(&mut self) -> std::io::Result<KeyEvent> {
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
                // spawned child exits) it is spurious, so re-poll rather than
                // returning a phantom resize.
                Err(Errno::EINTR) => {
                    if take_resized() {
                        return Ok(KeyEvent::Resize);
                    }
                }
                Err(e) => return Err(std::io::Error::from_raw_os_error(e as i32)),
            }
        }
    }

    pub(super) fn read_input(&mut self, ms: u64) -> Input {
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
    fn read_decoded(&mut self) -> std::io::Result<KeyEvent> {
        let mut buf = [0u8; 8];
        let n = self.file.read(&mut buf)?;
        if n == 0 {
            // EOF on the terminal (the operator closed it): treat as an abort.
            return Ok(KeyEvent::CtrlC);
        }
        Ok(parse_key(&buf[..n]))
    }
}

impl Drop for DevTty {
    fn drop(&mut self) {
        let _ = self.file.write_all(LEAVE_ALT.as_bytes());
        let _ = tcsetattr(self.file.as_fd(), SetArg::TCSANOW, &self.original);
        let _ = self.file.flush();
    }
}

/// The pending-resize flag, set by the `SIGWINCH` handler (async-signal-safe:
/// it only stores to an atomic) and consumed by the read loop.
static RESIZED: AtomicBool = AtomicBool::new(false);

/// Read and clear the pending-resize flag.
fn take_resized() -> bool {
    RESIZED.swap(false, Ordering::SeqCst)
}

/// The `SIGWINCH` handler: record that the terminal was resized.
extern "C" fn handle_winch(_: i32) {
    RESIZED.store(true, Ordering::SeqCst);
}

/// The `SIGINT` handler: record the interrupt. Async-signal-safe (only an atomic
/// store); the render loop does the terminal cleanup off the signal path.
extern "C" fn handle_int(_: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Install `handler` for `signal` once, WITHOUT `SA_RESTART` so the signal
/// interrupts a blocking `poll` (returns `EINTR`) and the read loop reacts.
/// Best-effort: on failure the UI simply repaints on the next keystroke.
fn install_handler(once: &OnceLock<()>, signal: Signal, handler: extern "C" fn(i32)) {
    if once.set(()).is_err() {
        return;
    }
    let action = SigAction::new(
        SigHandler::Handler(handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: the handlers only store to an atomic, which is async-signal-safe.
    unsafe {
        let _ = sigaction(signal, &action);
    }
}

fn install_int_handler() {
    static ONCE: OnceLock<()> = OnceLock::new();
    install_handler(&ONCE, Signal::SIGINT, handle_int);
}

fn install_winch_handler() {
    static ONCE: OnceLock<()> = OnceLock::new();
    install_handler(&ONCE, Signal::SIGWINCH, handle_winch);
}

/// Install a one-shot panic hook that resets the terminal to a sane cooked mode,
/// leaves the alternate screen, and shows the cursor. Release builds abort on
/// panic (so `Drop` never runs); this is the only chance to un-raw the terminal,
/// and it computes a sane mode from a fresh `/dev/tty` open so it needs no
/// captured state. The previous hook is chained.
fn install_panic_reset() {
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
/// alternate screen, and show the cursor. Best-effort: a failure here only
/// leaves the operator to run `reset`.
fn sane_reset() {
    if let Ok(mut f) = OpenOptions::new().read(true).write(true).open("/dev/tty") {
        if let Ok(mut t) = tcgetattr(f.as_fd()) {
            t.local_flags |=
                LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG | LocalFlags::IEXTEN;
            t.input_flags |= InputFlags::ICRNL | InputFlags::BRKINT;
            t.output_flags |= OutputFlags::OPOST | OutputFlags::ONLCR;
            let _ = tcsetattr(f.as_fd(), SetArg::TCSANOW, &t);
        }
        let _ = f.write_all(LEAVE_ALT.as_bytes());
        let _ = f.write_all(b"\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
