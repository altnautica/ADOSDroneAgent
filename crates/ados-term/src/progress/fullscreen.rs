//! The full-screen progress renderer.
//!
//! A split-view alternate-screen dashboard: the checklist of display groups on
//! the left, and a live-detail pane on the right showing the running step's
//! activity headline, a download / sub-progress bar, and a scrolling tail of its
//! real subprocess output. It draws through a [`Tty`] and the ANSI-width helpers
//! in [`crate::render`], so the compositor is pure and width-exact and the same
//! graceful-degradation rules apply (NO_COLOR, ASCII glyphs, resize handling).
//!
//! The render thread OWNS the `Tty` and is its only writer; the step engine and
//! the tracing bridge are producers on the event channel, so no other thread
//! ever touches the terminal. The compositor is a pure function of the model +
//! terminal size, which is what [`sample_grid`] exposes for snapshots and asset
//! export.

mod bars;
mod compose;

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::frame::TermSize;
use crate::progress::events::{GroupMap, ProgressEvent, ProgressSpec, SummaryView};
use crate::progress::model::Model;
use crate::theme::Theme;
use crate::tty::{self, Input, KeyEvent, Tty};

use compose::{compose, View};

/// Spinner cadence.
const TICK_MS: u64 = 120;
/// Live-detail log-tail ring depth (kept small; only the tail is shown).
const LOG_CAP: usize = 64;
/// Journal ring depth retained for the failure panel.
const JOURNAL_CAP: usize = 12;
/// How long an interactive closing card waits for a key before returning.
const AUTO_DISMISS: Duration = Duration::from_secs(120);

/// Mutable render state, driven by the event stream.
struct State {
    model: Model,
    /// The group whose detail pane is shown (the running step).
    active: Option<usize>,
    /// The running step's raw output tail.
    logs: VecDeque<String>,
    /// Forwarded tracing lines, retained for the failure panel only.
    journal: VecDeque<String>,
    /// When the first step started (drives the total-elapsed clock).
    started: Option<Instant>,
    summary: Option<Box<dyn SummaryView>>,
    finished: bool,
}

impl State {
    fn new(groups: GroupMap) -> Self {
        State {
            model: Model::new(groups),
            active: None,
            logs: VecDeque::with_capacity(LOG_CAP),
            journal: VecDeque::with_capacity(JOURNAL_CAP),
            started: None,
            summary: None,
            finished: false,
        }
    }

    /// Switch the detail pane to `group`, clearing the previous step's log tail.
    fn focus(&mut self, group: Option<usize>) {
        if group.is_some() && group != self.active {
            self.active = group;
            self.logs.clear();
        }
    }

    fn apply(&mut self, ev: ProgressEvent) {
        match ev {
            ProgressEvent::StepStarted { id } => {
                if self.started.is_none() {
                    self.started = Some(Instant::now());
                }
                self.focus(self.model.group_index(&id));
                self.model.start(&id);
            }
            ProgressEvent::StepResult { id, outcome } => {
                self.model.record(&id, &outcome);
            }
            ProgressEvent::SubProgress { id, done, total } => {
                self.model.set_sub(&id, done, total);
            }
            ProgressEvent::Activity { id, message } => {
                self.model.set_activity(&id, message);
            }
            ProgressEvent::SubLog { id, line } => {
                self.focus(self.model.group_index(&id));
                push(&mut self.logs, line, LOG_CAP);
            }
            ProgressEvent::ByteProgress {
                id,
                done,
                total,
                label,
            } => {
                self.model.set_bytes(&id, done, total, label);
            }
            ProgressEvent::Log { line, .. } => {
                push(&mut self.journal, line, JOURNAL_CAP);
            }
            ProgressEvent::Summary(s) => self.summary = Some(s),
            ProgressEvent::Finished => self.finished = true,
        }
    }

    fn view<'a>(&'a self, title: &'a str, spec: &'a ProgressSpec, spinner: usize) -> View<'a> {
        View {
            title,
            footer: spec.footer,
            skipped: spec.skipped,
            model: &self.model,
            active: self.active,
            logs: &self.logs,
            spinner,
            elapsed: self.started.map(|s| s.elapsed()),
        }
    }
}

/// Run the full-screen renderer to completion, then leave the alternate screen
/// and print the summary (if one was sent) on the primary screen. A Ctrl-C
/// leaves the alternate screen, prints `spec.interrupted` and exits 130.
pub fn run(
    mut tty: Tty,
    rx: Receiver<ProgressEvent>,
    theme: Theme,
    header: String,
    spec: ProgressSpec,
    interactive: bool,
) {
    let title = title_from_header(&header);
    let mut st = State::new(spec.groups);
    let mut spinner = 0usize;
    let mut interrupted = false;

    tty.force_clear_next();
    // The run is write-only: on the /dev/tty backend let Ctrl-C raise SIGINT
    // (a wizard ran with it off); the stdio backend polls for the key below.
    tty.enable_signals();
    paint(&mut tty, &theme, &st.view(&title, &spec, spinner));

    while !st.finished {
        match rx.recv_timeout(Duration::from_millis(TICK_MS)) {
            Ok(ev) => {
                st.apply(ev);
                // Drain-coalesce: apply everything queued, then paint one frame
                // (decouples the frame rate from a flood of streamed log lines).
                while let Ok(ev) = rx.try_recv() {
                    st.apply(ev);
                }
            }
            Err(RecvTimeoutError::Timeout) => spinner = spinner.wrapping_add(1),
            Err(RecvTimeoutError::Disconnected) => break,
        }
        tty.poll_interrupt();
        if tty::take_interrupt() {
            interrupted = true;
            break;
        }
        paint(&mut tty, &theme, &st.view(&title, &spec, spinner));
    }

    // On an interactive success, show the closing summary full-screen inside the
    // alt screen and wait for the operator to dismiss it, before leaving.
    if interactive && !interrupted {
        if let Some(s) = st.summary.as_deref() {
            if !s.failed() {
                present_completion(&mut tty, &theme, s, &st.journal);
            }
        }
    }

    // Leave the alt screen + restore the terminal (RAII), then print the summary
    // on the primary buffer so it persists in scrollback after the shell returns.
    drop(tty);
    if interrupted {
        eprintln!("\n{}", spec.interrupted);
        std::process::exit(130);
    }
    if let Some(s) = st.summary {
        for line in s.rich_lines(&theme, &st.journal) {
            eprintln!("{line}");
        }
    }
}

/// Render a representative split-view frame from a sequence of progress events,
/// for snapshot tests and asset export. Deterministic (fixed elapsed clock), so
/// it never touches a terminal.
pub fn sample_grid(
    theme: &Theme,
    size: TermSize,
    spec: &ProgressSpec,
    title: &str,
    events: Vec<ProgressEvent>,
    spinner: usize,
    elapsed_secs: u64,
) -> Vec<String> {
    let mut st = State::new(spec.groups);
    for ev in events {
        st.apply(ev);
    }
    let mut view = st.view(title, spec, spinner);
    view.elapsed = Some(Duration::from_secs(elapsed_secs));
    compose(theme, &view, size)
}

/// Paint the closing summary as a full-screen frame on the still-open alt screen.
fn paint_completion(tty: &mut Tty, theme: &Theme, s: &dyn SummaryView, journal: &VecDeque<String>) {
    let size = tty.size();
    let grid = s.fullscreen_grid(theme, journal, size.cols, size.rows);
    tty.present(&grid, theme);
}

/// Show the closing summary full-screen and block until the operator dismisses
/// it (any key / Ctrl-C), repainting on a resize. A generous auto-dismiss
/// deadline means an unattended-but-interactive session still returns to a shell.
fn present_completion(
    tty: &mut Tty,
    theme: &Theme,
    s: &dyn SummaryView,
    journal: &VecDeque<String>,
) {
    let deadline = Instant::now() + AUTO_DISMISS;
    // Drop any keystrokes typed during the run so the card holds until a fresh
    // keypress instead of a buffered byte dismissing it instantly.
    tty.flush_input();
    paint_completion(tty, theme, s, journal);
    loop {
        match tty.read_input(200) {
            Input::Key(KeyEvent::Resize) => paint_completion(tty, theme, s, journal),
            Input::Key(_) => break,
            Input::Tick => {
                if tty::take_interrupt() || Instant::now() >= deadline {
                    break;
                }
            }
        }
    }
}

/// The banner text: the header minus any trailing ellipsis/dots.
fn title_from_header(header: &str) -> String {
    header.trim_end_matches(['.', '…', ' ']).to_string()
}

fn push(ring: &mut VecDeque<String>, line: String, cap: usize) {
    if ring.len() == cap {
        ring.pop_front();
    }
    ring.push_back(line);
}

fn paint(tty: &mut Tty, theme: &Theme, view: &View) {
    let grid = compose(theme, view, tty.size());
    tty.present(&grid, theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::StepOutcome;
    use crate::progress::TEST_SPEC;
    use crate::theme::ColorTier;

    #[test]
    fn sample_grid_replays_events_into_a_full_frame() {
        let theme = Theme {
            ascii: false,
            tier: ColorTier::None,
        };
        let events = vec![
            ProgressEvent::StepStarted { id: "deps".into() },
            ProgressEvent::StepResult {
                id: "deps".into(),
                outcome: StepOutcome::Ok,
            },
            ProgressEvent::StepStarted { id: "units".into() },
            ProgressEvent::Activity {
                id: "units".into(),
                message: "writing unit files".into(),
            },
        ];
        let size = TermSize {
            cols: 110,
            rows: 28,
        };
        let grid = sample_grid(&theme, size, &TEST_SPEC, "Installing", events, 0, 252);
        assert_eq!(grid.len(), 28);
        let joined = grid.join("\n");
        assert!(joined.contains("Registering services"), "checklist label");
        assert!(joined.contains("writing unit files"), "activity headline");
        assert!(joined.contains("4:12"), "fixed elapsed clock");
        assert!(joined.contains(TEST_SPEC.footer), "footer");
    }

    #[test]
    fn title_drops_trailing_ellipsis() {
        assert_eq!(
            title_from_header("Installing the agent…"),
            "Installing the agent"
        );
        assert_eq!(title_from_header("Deploying..."), "Deploying");
    }
}
