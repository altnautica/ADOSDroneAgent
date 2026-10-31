//! Progress events, the step→group map, and the per-flow wording.
//!
//! A step engine emits [`ProgressEvent`]s onto a channel; a renderer thread
//! consumes them and draws the live progress. Technical steps are collapsed
//! into human-readable groups so the checklist reads at a glance; a group
//! lights up when any of its member steps starts and finalizes when all of them
//! have a result.

use std::collections::VecDeque;

use crate::outcome::StepOutcome;
use crate::theme::Theme;

/// A checklist group set: friendly labels (in display order) each mapping to one
/// or more technical step ids. The renderers are generic over this.
pub type GroupMap = &'static [(&'static str, &'static [&'static str])];

/// The display group index a step belongs to within `groups`, if any.
pub fn group_index_for_step(groups: GroupMap, step_id: &str) -> Option<usize> {
    groups
        .iter()
        .position(|(_, steps)| steps.contains(&step_id))
}

/// One flow's groups and wording (an install, an uninstall, a deploy).
#[derive(Debug, Clone, Copy)]
pub struct ProgressSpec {
    /// The checklist groups.
    pub groups: GroupMap,
    /// The full-screen footer reassurance line.
    pub footer: &'static str,
    /// The title on the inline renderer's top border.
    pub rich_title: &'static str,
    /// The checklist token for an all-skipped group (inline + full-screen).
    pub skipped: &'static str,
    /// The detail for an all-skipped group on the plain renderer.
    pub plain_skipped: &'static str,
    /// The line printed after Ctrl-C leaves the full-screen view (exit 130).
    pub interrupted: &'static str,
}

/// The closing summary a renderer draws once the run ends: a success card or a
/// failure panel. The tool owns its content; the renderers only place it.
pub trait SummaryView: Send + std::fmt::Debug {
    /// Escape-free lines for the plain and quiet renderers.
    fn plain_lines(&self) -> Vec<String>;
    /// Styled lines printed on the primary screen after the live view ends.
    /// `logs` is the renderer's ring of recent log lines (for a failure panel).
    fn rich_lines(&self, theme: &Theme, logs: &VecDeque<String>) -> Vec<String>;
    /// A full-screen grid (`rows` lines of `cols` visible columns) shown inside
    /// the alternate screen at the end of an interactive run.
    fn fullscreen_grid(
        &self,
        theme: &Theme,
        journal: &VecDeque<String>,
        cols: usize,
        rows: usize,
    ) -> Vec<String>;
    /// Whether the run failed. An interactive full-screen run holds a closing
    /// card for a key press only when it did not.
    fn failed(&self) -> bool;
}

/// An event emitted from the step engine to the renderer thread.
#[derive(Debug)]
pub enum ProgressEvent {
    /// A step's `run()` is about to execute (lights up its group's spinner).
    StepStarted {
        /// The step id.
        id: String,
    },
    /// A step finished (ran or was skipped) with the given outcome.
    StepResult {
        /// The step id.
        id: String,
        /// What happened.
        outcome: StepOutcome,
    },
    /// Incremental sub-progress for a step that reports a fraction.
    /// `done`/`total` are in the step's own units.
    SubProgress {
        /// The step id.
        id: String,
        /// Units done.
        done: u64,
        /// Units in total.
        total: u64,
    },
    /// A curated one-line headline for the running step ("installing ffmpeg")
    /// — the accent line in the live-detail pane.
    Activity {
        /// The step id.
        id: String,
        /// The headline.
        message: String,
    },
    /// One raw subprocess line for the running step's dim scrolling log tail.
    SubLog {
        /// The step id.
        id: String,
        /// The raw line.
        line: String,
    },
    /// Byte-level download progress for the running step's current file.
    /// `total` is 0 when the size is unknown. `label` names the file so the
    /// detail pane reads "service-name 4.2/8.1 MB".
    ByteProgress {
        /// The step id.
        id: String,
        /// Bytes done.
        done: u64,
        /// Bytes in total, or 0 when unknown.
        total: u64,
        /// The file being fetched.
        label: String,
    },
    /// A forwarded log line (from the tracing layer).
    Log {
        /// The log level.
        level: tracing::Level,
        /// The message.
        line: String,
    },
    /// The closing summary: render the success card / failure panel.
    Summary(Box<dyn SummaryView>),
    /// Stop the renderer loop and restore the terminal.
    Finished,
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUPS: GroupMap = &[("Prepare", &["a", "b"]), ("Finish", &["c"])];

    #[test]
    fn group_lookup_resolves() {
        assert_eq!(group_index_for_step(GROUPS, "a"), Some(0));
        assert_eq!(group_index_for_step(GROUPS, "b"), Some(0));
        assert_eq!(group_index_for_step(GROUPS, "c"), Some(1));
        assert_eq!(group_index_for_step(GROUPS, "nope"), None);
    }
}
