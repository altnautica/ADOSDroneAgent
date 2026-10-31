//! The stdio backend: crossterm raw mode + the alternate screen on stdout,
//! keys from crossterm events on stdin. Works the same on macOS, Windows and
//! Linux for a tool started from an interactive shell.

use std::io::{IsTerminal, Write};
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::{cursor, execute, terminal};

use super::keys::{map_ct_key, Input, KeyEvent};
use super::INTERRUPTED;

/// Raw mode + the alternate screen on stdout. Restores both on `Drop`.
pub(super) struct Stdio(());

impl Stdio {
    /// Enter raw mode + the alternate screen; `None` when stdin or stdout is
    /// not a terminal.
    pub(super) fn open() -> std::io::Result<Option<Stdio>> {
        if !Stdio::is_available() {
            return Ok(None);
        }
        terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        if execute!(out, terminal::EnterAlternateScreen, cursor::Hide).is_err() {
            let _ = terminal::disable_raw_mode();
            return Ok(None);
        }
        install_panic_reset();
        Ok(Some(Stdio(())))
    }

    pub(super) fn is_available() -> bool {
        std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
    }

    pub(super) fn write(&mut self, s: &str) {
        let mut out = std::io::stdout();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }

    pub(super) fn flush_input(&mut self) {
        while matches!(event::poll(Duration::ZERO), Ok(true)) {
            let _ = event::read();
        }
    }

    /// Drain pending key events; a Ctrl-C among them sets the interrupt flag.
    pub(super) fn poll_interrupt(&mut self) {
        while matches!(event::poll(Duration::ZERO), Ok(true)) {
            if let Ok(Event::Key(k)) = event::read() {
                if map_ct_key(k) == Some(KeyEvent::CtrlC) {
                    INTERRUPTED.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    pub(super) fn read_key(&mut self) -> std::io::Result<KeyEvent> {
        loop {
            match event::read()? {
                Event::Key(k) => {
                    if let Some(ev) = map_ct_key(k) {
                        return Ok(ev);
                    }
                }
                Event::Resize(_, _) => return Ok(KeyEvent::Resize),
                _ => {}
            }
        }
    }

    /// A Ctrl-C also sets the interrupt flag so a render loop polling ticks can
    /// exit via [`super::take_interrupt`].
    pub(super) fn read_input(&mut self, ms: u64) -> Input {
        match event::poll(Duration::from_millis(ms)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(k)) => match map_ct_key(k) {
                    Some(KeyEvent::CtrlC) => {
                        INTERRUPTED.store(true, Ordering::SeqCst);
                        Input::Key(KeyEvent::CtrlC)
                    }
                    Some(ev) => Input::Key(ev),
                    None => Input::Tick,
                },
                Ok(Event::Resize(_, _)) => Input::Key(KeyEvent::Resize),
                _ => Input::Tick,
            },
            Ok(false) | Err(_) => Input::Tick,
        }
    }
}

impl Drop for Stdio {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Leave the alternate screen, show the cursor, and un-raw the terminal.
/// Best-effort; safe to call more than once.
fn restore_terminal() {
    let mut out = std::io::stdout();
    let _ = execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
}

/// Install a one-shot panic hook that restores the terminal (release builds
/// abort on panic, so `Drop` never runs). The previous hook is chained.
fn install_panic_reset() {
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev(info);
    }));
}
