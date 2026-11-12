//! The one EXTENDED_SYS_STATE interval request per FC link-up.
//!
//! The landed state the snapshot publishes comes from EXTENDED_SYS_STATE,
//! which not every firmware streams by default. A stream table that already
//! asks for it owns its rate; otherwise, when the FC link comes up and the
//! message has not arrived within [`LANDED_STATE_GRACE`], the router asks for
//! it once at 1 Hz. The request is fire-and-forget: a NACK or a missing
//! COMMAND_ACK leaves `landed_state` null and nothing else.

use std::time::{Duration, Instant};

use super::{stream_rates, FcConnection};
use crate::state::MSG_EXTENDED_SYS_STATE;

/// How long after link-up EXTENDED_SYS_STATE may stay unseen before it is
/// requested: an FC that streams it by default has sent one by then.
pub(crate) const LANDED_STATE_GRACE: Duration = Duration::from_secs(3);

/// 1 Hz, as a SET_MESSAGE_INTERVAL interval in microseconds.
const LANDED_STATE_INTERVAL_US: f32 = 1_000_000.0;

/// Per-link-up decision state.
#[derive(Debug, Default)]
pub(crate) struct LandedStateRequest {
    /// When the current link-up was first seen; `None` while the link is down.
    linked_at: Option<Instant>,
    /// The decision for this link-up is made (requested, or seen arriving).
    settled: bool,
}

impl LandedStateRequest {
    /// Whether to request EXTENDED_SYS_STATE now. `linked` is the FC link
    /// state, `arriving` whether a fresh report is held. A link drop resets
    /// the decision, so the next link-up (an FC reboot drops its intervals)
    /// decides again.
    pub(crate) fn due(&mut self, linked: bool, arriving: bool, now: Instant) -> bool {
        if !linked {
            *self = Self::default();
            return false;
        }
        if self.settled {
            return false;
        }
        if arriving {
            self.settled = true;
            return false;
        }
        let since = *self.linked_at.get_or_insert(now);
        if now.saturating_duration_since(since) < LANDED_STATE_GRACE {
            return false;
        }
        self.settled = true;
        true
    }

    #[cfg(test)]
    pub(crate) fn linked_since(&mut self, at: Instant) {
        self.linked_at = Some(at);
    }
}

impl FcConnection {
    /// Ask the FC for EXTENDED_SYS_STATE at 1 Hz once per link-up when it is
    /// not arriving and the active stream table does not already request it.
    pub async fn tick_landed_state_request(&self) {
        if !self.transport_open() {
            return;
        }
        let table = stream_rates(&self.cfg.profile, self.sensor_streams.capture_active());
        if table.iter().any(|&(id, _)| id == MSG_EXTENDED_SYS_STATE) {
            return;
        }
        let linked = self.mavlink_alive().await;
        let now = Instant::now();
        let arriving = self.state.lock().await.landed_state_at(now).is_some();
        let due = self
            .landed_state_req
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .due(linked, arriving, now);
        if due {
            tracing::info!("requesting EXTENDED_SYS_STATE at 1 Hz: not arriving after link-up");
            self.send_message_interval(MSG_EXTENDED_SYS_STATE, LANDED_STATE_INTERVAL_US)
                .await;
        }
    }
}

#[cfg(test)]
mod tests;
