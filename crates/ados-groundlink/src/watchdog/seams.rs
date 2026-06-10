//! Watchdog tunables + the injected seams (clock, receive process, peer-presence
//! cache, locked-channel hint).
//!
//! The thresholds are the timing contract the FSM in the module root reads; the
//! traits are the seams tests inject fakes for. Pure declarations — no OS edges.

/// Valid-packet watchdog tunables. The valid-decode counter is the trustworthy
/// receive signal; a flat delta while the process is alive means we are tuned to
/// the wrong channel or the transmitter went away.
pub const VALID_RX_POLL_INTERVAL_S: f64 = 5.0;
pub const VALID_RX_SILENCE_THRESHOLD_S: f64 = 12.0;

/// Peer-presence freshness window. A paired peer emits a presence beacon on the
/// control plane every ~10 s. If we heard one within this window the link is up
/// and the peer is simply not sending video (idle-but-paired), so the watchdog
/// must NOT sweep or kill. Sized to tolerate one missed beacon.
pub const PEER_PRESENCE_FRESH_S: f64 = 30.0;

/// Cold-start home-hold budget. On a cold boot the receiver homes on the
/// configured rendezvous channel and waits there, because the transmitter
/// broadcasts on that same home channel until linked. But if the home channels
/// are mismatched an indefinite hold would deadlock forever, so after this long
/// unlinked at cold start with zero valid RX and no peer presence the receiver
/// performs ONE acquire sweep to self-heal, then returns to holding home if the
/// sweep finds nothing.
pub const COLD_START_HOME_HOLD_S: f64 = 75.0;

/// Peer-presence loss window. Between the fresh window and this one the peer was
/// seen recently but a marginal control-plane link is dropping beacons for tens
/// of seconds at a time. That is still a paired, idle link: hold the home
/// channel, do NOT sweep or restart. Only once presence has been absent longer
/// than this is the link treated as genuinely lost and the reacquisition sweep
/// allowed to run.
pub const PEER_PRESENCE_LOST_S: f64 = 120.0;

/// Secondary stdout-zombie net: the receive process is considered wedged if its
/// stats stream is silent this long, independent of the valid-decode path.
pub const RX_HEALTH_SILENCE_THRESHOLD_S: f64 = 30.0;

/// Monotonic clock seam. Tests inject a fake that returns scripted instants.
pub trait Clock: Send + Sync {
    /// Seconds on a monotonic timeline (only deltas are meaningful).
    fn monotonic(&self) -> f64;
}

/// The receive subprocess seam: liveness + terminate. Tests inject a fake.
pub trait RxProcess: Send + Sync {
    /// `true` while the subprocess is alive (mirrors `returncode is None`).
    fn is_running(&self) -> bool;
    /// Request termination; the run loop respawns the process. Best-effort.
    fn terminate(&self);
    /// Count of terminate requests, used by the genuine-loss kill path.
    fn terminate_count(&self) -> u32;
}

/// The peer-presence cache seam: presence age, freshness, and announced channel.
pub trait PresenceCache: Send + Sync {
    /// Seconds since the last presence beacon, or `None` when none decoded.
    fn presence_age_s(&self) -> Option<f64>;
    /// The channel the peer most recently advertised, if known.
    fn announced_channel(&self) -> Option<u8>;
    /// Convenience freshness gate (age present and within the fresh window).
    fn peer_present(&self) -> bool {
        match self.presence_age_s() {
            Some(age) => age <= PEER_PRESENCE_FRESH_S,
            None => false,
        }
    }
}

/// Persists the last-locked channel as a tmpfs runtime hint. Default
/// implementation writes the Contract-E hint file atomically; tests inject a
/// recording fake. NEVER writes the config home channel (see the module-level
/// invariant).
pub trait LockedChannelHint: Send + Sync {
    fn persist(&self, channel: u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_constants_match_python() {
        assert_eq!(VALID_RX_SILENCE_THRESHOLD_S, 12.0);
        assert_eq!(PEER_PRESENCE_FRESH_S, 30.0);
        assert_eq!(COLD_START_HOME_HOLD_S, 75.0);
        assert_eq!(PEER_PRESENCE_LOST_S, 120.0);
        assert_eq!(VALID_RX_POLL_INTERVAL_S, 5.0);
        assert_eq!(RX_HEALTH_SILENCE_THRESHOLD_S, 30.0);
    }
}
