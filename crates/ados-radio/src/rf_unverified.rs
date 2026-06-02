//! Edge detector for the "transmitting, zero confirmed reception" link state.
//!
//! An advancing transmit byte counter only proves the driver accepted frames
//! into its ring; it never proves the energy reached a receiver. The radio
//! already derives an instantaneous `rf_unverified` flag (transmitting RF while
//! no verified return signal has been heard within the grace window — a loose
//! antenna, a forbidden-band power cap, a dead peer). This detector turns that
//! flag into a debounced, self-clearing pair of discrete events so an RCA can
//! query "when did the link enter unverified, and when did it clear" instead of
//! reconstructing it from a flag sampled into the sidecar.
//!
//! The state machine debounces: the instantaneous condition must hold for a
//! short hold window before an `entry` event fires (a single transient beacon
//! gap does not flap), and a `clear` event fires the moment the condition
//! releases after an entry. Exactly one `entry` and one matching `clear` are
//! emitted per episode; the detector is bounded (a tiny struct) and emits
//! nothing while the link is healthy.

use std::time::{Duration, Instant};

use ados_protocol::logd::{Fields, Value as MpVal};

/// The event kind for an rf-unverified state change.
pub const RF_UNVERIFIED_KIND: &str = "radio.rf_unverified";

/// How long the instantaneous unverified condition must hold continuously before
/// an `entry` event fires. Debounces a brief return-signal gap so the event
/// reflects a sustained "transmitting into the void", not a single missed beacon.
pub const RF_UNVERIFIED_HOLD: Duration = Duration::from_secs(10);

/// The detector's debounce state across heartbeat ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// The link is verified or idle; nothing to report.
    Clear,
    /// The instantaneous unverified condition is holding but has not yet been
    /// continuous for [`RF_UNVERIFIED_HOLD`]; carries when it began.
    Pending(Instant),
    /// An `entry` event has fired and not yet been cleared; carries the entry
    /// instant so the `clear` event can report the episode duration.
    Entered(Instant),
}

/// What one [`RfUnverifiedDetector::observe`] decided this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RfUnverifiedEdge {
    /// No state change; do not emit.
    None,
    /// The link just entered the sustained unverified state.
    Entry,
    /// The link just left the unverified state; carries the episode duration.
    Clear { episode_s: u64 },
}

/// Debounced detector for the rf-unverified episode. Fed the instantaneous
/// `tx_live && !rx_proven` condition each heartbeat tick; returns an edge only on
/// a true entry (after the hold window) or a clear.
#[derive(Debug, Clone, Copy)]
pub struct RfUnverifiedDetector {
    state: State,
    hold: Duration,
}

impl RfUnverifiedDetector {
    /// A fresh detector starting in the clear state, with the default hold.
    pub fn new() -> Self {
        Self {
            state: State::Clear,
            hold: RF_UNVERIFIED_HOLD,
        }
    }

    /// A detector with an explicit hold window (used by tests).
    pub fn with_hold(hold: Duration) -> Self {
        Self {
            state: State::Clear,
            hold,
        }
    }

    /// Feed the instantaneous unverified condition at `now`. Returns the edge to
    /// act on: `Entry` once when it has held for the hold window, `Clear` once
    /// when it releases after an entry, `None` otherwise. Pure aside from `now`,
    /// so the debounce logic is unit-testable with synthetic instants.
    pub fn observe(&mut self, unverified: bool, now: Instant) -> RfUnverifiedEdge {
        match self.state {
            State::Clear => {
                if unverified {
                    self.state = State::Pending(now);
                }
                RfUnverifiedEdge::None
            }
            State::Pending(since) => {
                if !unverified {
                    // Released before the hold elapsed: a transient, no event.
                    self.state = State::Clear;
                    RfUnverifiedEdge::None
                } else if now.saturating_duration_since(since) >= self.hold {
                    self.state = State::Entered(now);
                    RfUnverifiedEdge::Entry
                } else {
                    RfUnverifiedEdge::None
                }
            }
            State::Entered(entered) => {
                if unverified {
                    RfUnverifiedEdge::None
                } else {
                    self.state = State::Clear;
                    RfUnverifiedEdge::Clear {
                        episode_s: now.saturating_duration_since(entered).as_secs(),
                    }
                }
            }
        }
    }
}

impl Default for RfUnverifiedDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the `radio.rf_unverified` detail map. All fields are bland and
/// reader-facing:
///
/// - `state` — `entry` | `clear`;
/// - `iface` — the injection interface;
/// - `tx_rate_bps` — the transmit byte rate over the heartbeat interval;
/// - `rx_packets_per_s` — the received valid-packet rate (zero on the
///   transmit-only end with no return decode stats);
/// - `usb_speed_mbps` — the adapter's negotiated USB speed, when known (a slow
///   port is one root cause this event helps disambiguate);
/// - `window_s` — the proof grace window the verification used;
/// - `episode_s` — present only on `clear`: how long the episode lasted.
#[allow(clippy::too_many_arguments)]
pub fn rf_unverified_detail(
    state: &str,
    iface: &str,
    tx_rate_bps: f64,
    rx_packets_per_s: f64,
    usb_speed_mbps: Option<u32>,
    window_s: u64,
    episode_s: Option<u64>,
) -> Fields {
    let mut d = Fields::new();
    d.insert("state".to_string(), MpVal::from(state));
    d.insert("iface".to_string(), MpVal::from(iface));
    d.insert("tx_rate_bps".to_string(), MpVal::from(tx_rate_bps));
    d.insert(
        "rx_packets_per_s".to_string(),
        MpVal::from(rx_packets_per_s),
    );
    if let Some(speed) = usb_speed_mbps {
        d.insert("usb_speed_mbps".to_string(), MpVal::from(speed as u64));
    }
    d.insert("window_s".to_string(), MpVal::from(window_s));
    if let Some(episode) = episode_s {
        d.insert("episode_s".to_string(), MpVal::from(episode));
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_link_never_emits() {
        let mut d = RfUnverifiedDetector::new();
        let t = Instant::now();
        for i in 0..10 {
            assert_eq!(
                d.observe(false, t + Duration::from_secs(i)),
                RfUnverifiedEdge::None
            );
        }
    }

    #[test]
    fn entry_fires_once_after_the_hold_window() {
        let hold = Duration::from_secs(10);
        let mut d = RfUnverifiedDetector::with_hold(hold);
        let t = Instant::now();
        // Condition holds but the hold window has not elapsed: no event yet.
        assert_eq!(d.observe(true, t), RfUnverifiedEdge::None);
        assert_eq!(
            d.observe(true, t + Duration::from_secs(5)),
            RfUnverifiedEdge::None
        );
        // Past the hold window: entry fires exactly once.
        assert_eq!(
            d.observe(true, t + Duration::from_secs(10)),
            RfUnverifiedEdge::Entry
        );
        // Still unverified afterwards: no repeat entry.
        assert_eq!(
            d.observe(true, t + Duration::from_secs(12)),
            RfUnverifiedEdge::None
        );
    }

    #[test]
    fn a_transient_gap_under_the_hold_does_not_flap() {
        let hold = Duration::from_secs(10);
        let mut d = RfUnverifiedDetector::with_hold(hold);
        let t = Instant::now();
        assert_eq!(d.observe(true, t), RfUnverifiedEdge::None);
        // Released before the hold elapsed: back to clear, no event.
        assert_eq!(
            d.observe(false, t + Duration::from_secs(4)),
            RfUnverifiedEdge::None
        );
        // A fresh onset restarts the hold; it must run the full window again.
        assert_eq!(
            d.observe(true, t + Duration::from_secs(5)),
            RfUnverifiedEdge::None
        );
        assert_eq!(
            d.observe(true, t + Duration::from_secs(14)),
            RfUnverifiedEdge::None
        );
        assert_eq!(
            d.observe(true, t + Duration::from_secs(15)),
            RfUnverifiedEdge::Entry
        );
    }

    #[test]
    fn clear_fires_once_with_episode_duration_after_entry() {
        let hold = Duration::from_secs(10);
        let mut d = RfUnverifiedDetector::with_hold(hold);
        let t = Instant::now();
        assert_eq!(d.observe(true, t), RfUnverifiedEdge::None);
        assert_eq!(
            d.observe(true, t + Duration::from_secs(10)),
            RfUnverifiedEdge::Entry
        );
        // Clears 20 s after entry → episode_s = 20, fires exactly once.
        assert_eq!(
            d.observe(false, t + Duration::from_secs(30)),
            RfUnverifiedEdge::Clear { episode_s: 20 }
        );
        assert_eq!(
            d.observe(false, t + Duration::from_secs(31)),
            RfUnverifiedEdge::None
        );
    }

    #[test]
    fn re_entry_after_a_clear_is_a_new_episode() {
        let hold = Duration::from_secs(10);
        let mut d = RfUnverifiedDetector::with_hold(hold);
        let t = Instant::now();
        d.observe(true, t);
        assert_eq!(
            d.observe(true, t + Duration::from_secs(10)),
            RfUnverifiedEdge::Entry
        );
        assert_eq!(
            d.observe(false, t + Duration::from_secs(15)),
            RfUnverifiedEdge::Clear { episode_s: 5 }
        );
        // A new onset starts a fresh hold-and-entry cycle.
        d.observe(true, t + Duration::from_secs(20));
        assert_eq!(
            d.observe(true, t + Duration::from_secs(31)),
            RfUnverifiedEdge::Entry
        );
    }

    #[test]
    fn detail_carries_episode_only_on_clear() {
        let entry = rf_unverified_detail("entry", "wlan1", 12345.0, 0.0, Some(12), 30, None);
        assert_eq!(entry.get("state").and_then(|v| v.as_str()), Some("entry"));
        assert_eq!(entry.get("iface").and_then(|v| v.as_str()), Some("wlan1"));
        assert_eq!(
            entry.get("usb_speed_mbps").and_then(|v| v.as_u64()),
            Some(12)
        );
        assert_eq!(entry.get("window_s").and_then(|v| v.as_u64()), Some(30));
        assert!(!entry.contains_key("episode_s"));

        let clear = rf_unverified_detail("clear", "wlan1", 0.0, 4.0, None, 30, Some(42));
        assert_eq!(clear.get("state").and_then(|v| v.as_str()), Some("clear"));
        assert_eq!(clear.get("episode_s").and_then(|v| v.as_u64()), Some(42));
        // usb speed omitted when unknown.
        assert!(!clear.contains_key("usb_speed_mbps"));
    }
}
