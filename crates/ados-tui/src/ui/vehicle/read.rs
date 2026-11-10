//! How a polled resource's reads went, for the stale note of a block whose
//! last read failed.

use std::time::Instant;

/// The outcome of one resource's reads: when the last one succeeded, and the
/// error of a failed one since.
#[derive(Debug, Default)]
pub(super) struct Read {
    pub(super) ok_at: Option<Instant>,
    pub(super) failed: Option<String>,
}

impl Read {
    pub(super) fn ok(now: Instant) -> Self {
        Self {
            ok_at: Some(now),
            failed: None,
        }
    }

    /// The note for a failed read: how long ago the last good one was, or
    /// the error when there never was one.
    pub(super) fn failed_note(&self, now: Instant) -> Option<String> {
        let err = self.failed.as_ref()?;
        Some(match self.ok_at {
            Some(at) => format!(
                "read failed · last good read {} s ago",
                now.saturating_duration_since(at).as_secs()
            ),
            None => format!("unavailable: {err}"),
        })
    }
}
