//! Live step-progress UI.
//!
//! A step engine emits [`ProgressEvent`]s through a [`ProgressSink`] (cheap to
//! clone, a no-op when no renderer is attached). A renderer thread consumes them
//! and draws one of: a full-screen split dashboard ([`RenderMode::Fullscreen`]),
//! an inline sticky block on stderr ([`RenderMode::Rich`]), or clean line
//! transitions ([`RenderMode::Plain`]). The durable log keeps flowing to the
//! tool's own subscriber; this UI is purely additive operator feedback.
//!
//! Mode is chosen from **stderr** (where the inline UI renders) plus the
//! environment, so it behaves correctly under `curl … | sudo bash` (stdin is a
//! pipe, stderr is the operator's terminal) and degrades to plain text in CI /
//! piped / `TERM=dumb` contexts.

pub mod events;
pub mod export;
pub mod fullscreen;
pub mod model;
pub mod plain;
pub mod rich;

use std::io::{IsTerminal, Write};
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::thread::JoinHandle;

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

pub use events::{group_index_for_step, GroupMap, ProgressEvent, ProgressSpec, SummaryView};

use crate::outcome::StepOutcome;
use crate::theme::Theme;
use crate::tty::Tty;

/// The renderer thread name.
const THREAD_NAME: &str = "ados-term-ui";

/// Set once a log-forwarding renderer is running. The tracing [`ChannelLayer`]
/// forwards log lines here; until it is set, forwarding is a no-op.
static LOG_TX: OnceLock<Sender<ProgressEvent>> = OnceLock::new();

/// Which renderer to drive, decided once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    /// Full-screen split dashboard on an open [`Tty`].
    Fullscreen,
    /// Inline sticky-block dashboard on stderr (stderr is a terminal but no
    /// full-screen terminal could be opened).
    Rich,
    /// Clean escape-free line transitions (non-tty / CI / `--plain`).
    Plain,
    /// Only the final summary (and errors).
    Quiet,
    /// No UI; the tool writes machine output itself.
    Json,
}

/// The output flags a tool's command line carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModeFlags {
    /// Machine output; no UI.
    pub json: bool,
    /// Only the final summary.
    pub quiet: bool,
    /// Force the plain line renderer.
    pub plain: bool,
}

/// Decide the render mode from the flags + environment. Gates on **stderr**
/// because that is where the inline UI draws.
pub fn detect_mode(flags: ModeFlags) -> RenderMode {
    if flags.json {
        return RenderMode::Json;
    }
    if flags.quiet {
        return RenderMode::Quiet;
    }
    let is_tty = std::io::stderr().is_terminal();
    let ci = std::env::var_os("CI").is_some();
    let dumb = std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false);
    if flags.plain || !is_tty || ci || dumb {
        return RenderMode::Plain;
    }
    RenderMode::Rich
}

/// Resolve the live render mode + terminal, upgrading `Rich` to `Fullscreen`
/// when a terminal can be opened. `carried` is a still-open [`Tty`] (a wizard →
/// progress handoff keeps one alternate-screen session); when it is `None` but
/// the base mode is `Rich`, `open` is tried (pass [`Tty::open_controlling`] or
/// [`Tty::open_stdio`]). Plain / Quiet / Json drop any carried `Tty` (leaving
/// the alternate screen) and keep the base renderer.
pub fn resolve_live_mode(
    base: RenderMode,
    carried: Option<Tty>,
    open: fn() -> std::io::Result<Option<Tty>>,
) -> (RenderMode, Option<Tty>) {
    match base {
        RenderMode::Rich => {
            if let Some(t) = carried {
                (RenderMode::Fullscreen, Some(t))
            } else if let Ok(Some(t)) = open() {
                (RenderMode::Fullscreen, Some(t))
            } else {
                (RenderMode::Rich, None)
            }
        }
        other => {
            drop(carried);
            (other, None)
        }
    }
}

/// A cheap, clonable handle the step engine emits events through. A sink with
/// no channel (the default, or [`RenderMode::Json`]) silently drops events.
#[derive(Debug, Clone, Default)]
pub struct ProgressSink {
    tx: Option<Sender<ProgressEvent>>,
}

impl ProgressSink {
    fn send(&self, ev: ProgressEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
    }

    /// A step's `run()` is about to execute.
    pub fn step_started(&self, id: &str) {
        self.send(ProgressEvent::StepStarted { id: id.to_string() });
    }

    /// A step finished (ran or skipped) with this outcome.
    pub fn step_result(&self, id: &str, outcome: &StepOutcome) {
        self.send(ProgressEvent::StepResult {
            id: id.to_string(),
            outcome: outcome.clone(),
        });
    }

    /// Incremental sub-progress for a step that reports a fraction.
    pub fn sub_progress(&self, id: &str, done: u64, total: u64) {
        self.send(ProgressEvent::SubProgress {
            id: id.to_string(),
            done,
            total,
        });
    }

    /// Set the curated headline for the running step's live-detail pane.
    pub fn activity(&self, id: &str, message: String) {
        self.send(ProgressEvent::Activity {
            id: id.to_string(),
            message,
        });
    }

    /// Emit one raw subprocess line into the running step's log tail.
    pub fn sub_log(&self, id: &str, line: &str) {
        self.send(ProgressEvent::SubLog {
            id: id.to_string(),
            line: line.to_string(),
        });
    }

    /// Byte-level download progress for the running step's current file.
    pub fn byte_progress(&self, id: &str, done: u64, total: u64, label: &str) {
        self.send(ProgressEvent::ByteProgress {
            id: id.to_string(),
            done,
            total,
            label: label.to_string(),
        });
    }

    /// The closing summary (success card / failure panel).
    pub fn summary(&self, s: Box<dyn SummaryView>) {
        self.send(ProgressEvent::Summary(s));
    }

    /// Stop the renderer and restore the terminal.
    pub fn finish(&self) {
        self.send(ProgressEvent::Finished);
    }
}

/// Owns the renderer thread; [`RenderHandle::finish`] joins it (call after
/// sending [`ProgressSink::summary`] + [`ProgressSink::finish`]).
pub struct RenderHandle {
    join: Option<JoinHandle<()>>,
}

impl RenderHandle {
    /// Wait for the renderer to draw the final summary and restore the terminal.
    pub fn finish(self) {
        if let Some(j) = self.join {
            let _ = j.join();
        }
    }
}

/// Create the progress sink + spawn the renderer thread for `mode`. `header` is
/// the one-line banner the renderer prints first. `tty` is the terminal for
/// `Fullscreen` (from [`resolve_live_mode`]); the other modes ignore it (it is
/// dropped, leaving the alternate screen if one was carried). `interactive`
/// holds a non-failed closing card full-screen until a key press.
pub fn start(
    mode: RenderMode,
    header: String,
    theme: Theme,
    tty: Option<Tty>,
    spec: ProgressSpec,
    interactive: bool,
) -> (ProgressSink, RenderHandle) {
    if mode == RenderMode::Json {
        return (ProgressSink::default(), RenderHandle { join: None });
    }
    let (tx, rx) = mpsc::channel::<ProgressEvent>();
    let sink = ProgressSink {
        tx: Some(tx.clone()),
    };
    let join = match (mode, tty) {
        (RenderMode::Fullscreen, Some(tty)) => {
            // The `Tty`'s own panic-reset hook leaves the alternate screen and
            // un-raws on a panic, so no stderr cursor hook here.
            let _ = LOG_TX.set(tx);
            spawn(move || fullscreen::run(tty, rx, theme, header, spec, interactive))
        }
        (RenderMode::Fullscreen | RenderMode::Rich, _) => {
            // The renderer hides the cursor; a panic (release builds abort) must
            // still restore it, so install the hook before drawing.
            install_panic_hook();
            // Forward log lines into the live block.
            let _ = LOG_TX.set(tx);
            spawn(move || rich::run(rx, theme, header, spec))
        }
        (RenderMode::Quiet, _) => spawn(move || plain::run(rx, true, header, spec)),
        (_, _) => spawn(move || plain::run(rx, false, header, spec)),
    };
    (sink, RenderHandle { join })
}

/// Spawn the renderer thread.
fn spawn(body: impl FnOnce() + Send + 'static) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(THREAD_NAME.to_string())
        .spawn(body)
        .ok()
}

/// Restore the cursor on panic. Release builds abort on panic, so the hook is
/// the only chance to undo `cursor::Hide`. Installed at most once.
fn install_panic_hook() {
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut err = std::io::stderr();
        let _ = ratatui::crossterm::execute!(err, ratatui::crossterm::cursor::Show);
        let _ = writeln!(err);
        prev(info);
    }));
}

/// A tracing layer that forwards each log line to the renderer (when a
/// log-forwarding renderer is attached). Add it alongside the tool's durable
/// log layer so that log keeps the full record regardless.
pub struct ChannelLayer;

#[derive(Default)]
struct MsgVisitor {
    message: Option<String>,
}

impl Visit for MsgVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }
}

impl<S: Subscriber> Layer<S> for ChannelLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let Some(tx) = LOG_TX.get() else {
            return;
        };
        let mut v = MsgVisitor::default();
        event.record(&mut v);
        if let Some(msg) = v.message {
            let _ = tx.send(ProgressEvent::Log {
                level: *event.metadata().level(),
                line: msg,
            });
        }
    }
}

/// A small flow shared by the renderer tests: a three-member group, a
/// one-member group, a two-member group.
#[cfg(test)]
pub(crate) const TEST_SPEC: ProgressSpec = ProgressSpec {
    groups: &[
        ("Configuring", &["identity", "mac_pin", "regulatory"]),
        ("Installing dependencies", &["deps"]),
        ("Registering services", &["watchdog", "units"]),
    ],
    footer: "Safe to leave running.",
    rich_title: "Example · installing",
    skipped: "cached",
    plain_skipped: "already done",
    interrupted: "Example interrupted.",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_and_quiet_flags_win() {
        let mut f = ModeFlags {
            json: true,
            ..ModeFlags::default()
        };
        assert_eq!(detect_mode(f), RenderMode::Json);
        f.json = false;
        f.quiet = true;
        assert_eq!(detect_mode(f), RenderMode::Quiet);
    }

    #[test]
    fn plain_flag_forces_plain() {
        let f = ModeFlags {
            plain: true,
            ..ModeFlags::default()
        };
        assert_eq!(detect_mode(f), RenderMode::Plain);
    }

    #[test]
    fn rich_without_a_terminal_stays_rich() {
        fn none() -> std::io::Result<Option<Tty>> {
            Ok(None)
        }
        let (mode, tty) = resolve_live_mode(RenderMode::Rich, None, none);
        assert_eq!(mode, RenderMode::Rich);
        assert!(tty.is_none());
        let (mode, _) = resolve_live_mode(RenderMode::Plain, None, none);
        assert_eq!(mode, RenderMode::Plain);
    }

    #[test]
    fn noop_sink_does_not_panic() {
        let sink = ProgressSink::default();
        sink.step_started("deps");
        sink.step_result("deps", &StepOutcome::Ok);
        sink.finish();
    }
}
