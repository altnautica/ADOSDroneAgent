//! Transient notifications and their session history.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// What a toast reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

impl ToastKind {
    /// How long a toast of this kind stays on screen.
    pub fn duration(self) -> Duration {
        match self {
            ToastKind::Info | ToastKind::Success => Duration::from_secs(4),
            ToastKind::Warn => Duration::from_secs(6),
            ToastKind::Error => Duration::from_secs(8),
        }
    }
}

/// One toast.
#[derive(Debug, Clone)]
pub struct Toast {
    pub id: u64,
    pub kind: ToastKind,
    pub text: String,
    pub at: Instant,
    /// Wall-clock `HH:MM:SS` when it was raised, for the history list.
    pub clock: String,
}

impl Toast {
    /// When the toast leaves the screen.
    pub fn expires(&self) -> Instant {
        self.at + self.kind.duration()
    }
}

/// At most this many toasts are on screen at once.
pub const MAX_VISIBLE: usize = 3;
/// The notification history keeps this many entries.
pub const HISTORY_CAP: usize = 200;

/// The toast queue and the notification history.
#[derive(Debug, Default)]
pub struct Toasts {
    active: VecDeque<Toast>,
    history: VecDeque<Toast>,
    next_id: u64,
}

impl Toasts {
    /// Raise a toast. Every toast also lands in the history.
    pub fn push(&mut self, kind: ToastKind, text: impl Into<String>, now: Instant, clock: String) {
        self.next_id += 1;
        let toast = Toast {
            id: self.next_id,
            kind,
            text: text.into(),
            at: now,
            clock,
        };
        self.history.push_back(toast.clone());
        while self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
        self.active.push_back(toast);
    }

    /// Drop expired toasts. Returns whether anything changed.
    pub fn prune(&mut self, now: Instant) -> bool {
        let before = self.active.len();
        self.active.retain(|t| t.expires() > now);
        before != self.active.len()
    }

    /// The toasts on screen at `now`, newest last, at most [`MAX_VISIBLE`].
    pub fn visible(&self, now: Instant) -> Vec<&Toast> {
        let live: Vec<&Toast> = self.active.iter().filter(|t| t.expires() > now).collect();
        let skip = live.len().saturating_sub(MAX_VISIBLE);
        live.into_iter().skip(skip).collect()
    }

    /// The earliest moment a visible toast expires (drives a redraw).
    pub fn next_expiry(&self) -> Option<Instant> {
        self.active.iter().map(Toast::expires).min()
    }

    /// Every toast raised this session, oldest first.
    pub fn history(&self) -> impl DoubleEndedIterator<Item = &Toast> + ExactSizeIterator {
        self.history.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_most_three_show_and_errors_outlive_info() {
        let t0 = Instant::now();
        let mut toasts = Toasts::default();
        for i in 0..4 {
            toasts.push(ToastKind::Info, format!("t{i}"), t0, String::new());
        }
        toasts.push(ToastKind::Error, "boom", t0, String::new());
        let shown: Vec<_> = toasts.visible(t0).iter().map(|t| t.text.clone()).collect();
        assert_eq!(shown, ["t2", "t3", "boom"]);
        let later = t0 + Duration::from_secs(5);
        let shown: Vec<_> = toasts
            .visible(later)
            .iter()
            .map(|t| t.text.clone())
            .collect();
        assert_eq!(shown, ["boom"]);
        assert_eq!(toasts.history().len(), 5);
    }
}
