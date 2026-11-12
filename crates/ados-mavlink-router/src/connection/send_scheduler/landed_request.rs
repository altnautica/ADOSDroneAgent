//! The EXTENDED_SYS_STATE interval request while the message is absent.
//!
//! The landed state the snapshot publishes comes from EXTENDED_SYS_STATE,
//! which not every firmware streams by default. A stream table that already
//! asks for it owns its rate; otherwise, when the FC link is up and the
//! message has been absent for [`LANDED_STATE_GRACE`], the router asks for it
//! at 1 Hz, and asks again every [`LANDED_STATE_REREQUEST`] while it stays
//! absent. That covers an FC that reboots inside the heartbeat timeout (its
//! intervals reset without a link drop), a request the FC dropped, and a
//! message that arrived at link-up and then stopped. The request is
//! fire-and-forget: a NACK or a missing COMMAND_ACK leaves `landed_state`
//! null and nothing else.

use std::time::{Duration, Instant};

use super::{stream_rates, FcConnection};
use crate::state::MSG_EXTENDED_SYS_STATE;

/// How long EXTENDED_SYS_STATE may stay unseen on a live link before it is
/// requested: an FC that streams it by default has sent one by then.
pub(crate) const LANDED_STATE_GRACE: Duration = Duration::from_secs(3);

/// How often the request repeats while the message stays absent.
pub(crate) const LANDED_STATE_REREQUEST: Duration = Duration::from_secs(30);

/// 1 Hz, as a SET_MESSAGE_INTERVAL interval in microseconds.
const LANDED_STATE_INTERVAL_US: f32 = 1_000_000.0;

/// Request state for the current link-up.
#[derive(Debug, Default)]
pub(crate) struct LandedStateRequest {
    /// Since when the message has been absent on a live link; `None` while
    /// the link is down or the message arrives.
    absent_since: Option<Instant>,
    /// When it was last requested on this link-up.
    requested_at: Option<Instant>,
}

impl LandedStateRequest {
    /// Whether to request EXTENDED_SYS_STATE now. `linked` is the FC link
    /// state, `arriving` whether a fresh report is held. A link drop resets
    /// the state, so the next link-up starts a fresh grace.
    pub(crate) fn due(&mut self, linked: bool, arriving: bool, now: Instant) -> bool {
        if !linked {
            *self = Self::default();
            return false;
        }
        if arriving {
            self.absent_since = None;
            return false;
        }
        let since = *self.absent_since.get_or_insert(now);
        if now.saturating_duration_since(since) < LANDED_STATE_GRACE {
            return false;
        }
        if self
            .requested_at
            .is_some_and(|at| now.saturating_duration_since(at) < LANDED_STATE_REREQUEST)
        {
            return false;
        }
        self.requested_at = Some(now);
        true
    }

    #[cfg(test)]
    pub(crate) fn absent_since(&mut self, at: Instant) {
        self.absent_since = Some(at);
    }
}

impl FcConnection {
    /// Ask the FC for EXTENDED_SYS_STATE at 1 Hz while it is not arriving and
    /// the active stream table does not already request it, at most once per
    /// [`LANDED_STATE_REREQUEST`].
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
            tracing::info!("requesting EXTENDED_SYS_STATE at 1 Hz: not arriving on a live link");
            self.send_message_interval(MSG_EXTENDED_SYS_STATE, LANDED_STATE_INTERVAL_US)
                .await;
        }
    }
}

#[cfg(test)]
mod tests;
