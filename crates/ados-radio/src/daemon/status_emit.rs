//! Dedup gate for the `link.wfb_status` snapshot event.
//!
//! The sidecar file is rewritten every heartbeat (the REST staleness check
//! needs it fresh), but shipping the full body to the logging store every 2 s
//! is pure write volume when nothing changed. The event goes out when a field
//! an operator would query on changes, and at least every
//! [`STATUS_EMIT_MAX_GAP`] so the durable series never goes quiet.

use std::time::{Duration, Instant};

/// Longest gap between two `link.wfb_status` events while nothing changes.
/// Kept below the 10 s age past which the status readers treat the latest
/// event as stale, with one 2 s heartbeat tick of margin.
pub(crate) const STATUS_EMIT_MAX_GAP: Duration = Duration::from_secs(8);

/// The fields whose change forces an immediate event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StatusKey {
    pub(crate) state: String,
    pub(crate) actual_channel: u8,
    pub(crate) operating_channel: u8,
    pub(crate) reg_domain: Option<String>,
    pub(crate) reg_verified: bool,
    pub(crate) tx_power_dbm: i8,
    pub(crate) tx_power_readback_dbm: Option<f32>,
}

/// Remembers the last emitted key and when it went out.
#[derive(Debug, Default)]
pub(crate) struct StatusEmitGate {
    last: Option<(StatusKey, Instant)>,
}

impl StatusEmitGate {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// True when the event should be emitted for `key` at `now`: the first
    /// call, any change of a key field, or [`STATUS_EMIT_MAX_GAP`] since the
    /// last emit. Records the emit when it returns true.
    pub(crate) fn should_emit(&mut self, key: StatusKey, now: Instant) -> bool {
        let due = match &self.last {
            None => true,
            Some((prev, at)) => {
                *prev != key || now.saturating_duration_since(*at) >= STATUS_EMIT_MAX_GAP
            }
        };
        if due {
            self.last = Some((key, now));
        }
        due
    }
}

#[cfg(test)]
mod tests;
