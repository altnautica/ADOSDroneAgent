//! The lane state machine: a pure derivation from observed inputs to the
//! sidecar's `state` and `rf_unverified` fields.
//!
//! Liveness discipline: bytes accepted by the serial driver prove only that
//! the driver took them — never that the RC module radiated, and never that
//! the far receiver heard anything. The only received-side proof this lane
//! has is the inbound link-statistics telemetry (frame type 0x14) and its
//! uplink link-quality figure, so the verdict keys on that: transmitting
//! without fresh link statistics — or with fresh statistics whose uplink LQ
//! reads ZERO (the module is talking to us but hears no receiver) — is
//! `rf_unverified`, not "up". Before the proof window has even had a chance
//! to fill, the honest answer is "no verdict yet" (`ready`,
//! `rf_unverified: null`) — never a fabricated false. Only a state with a
//! real received-side proof ([`LaneState::flyable`]) may ever be reported
//! usable for flight.

use std::time::Duration;

/// How fresh the last link-statistics frame must be to count as a
/// received-side proof of the link.
pub const STATS_FRESH_WINDOW: Duration = Duration::from_secs(3);

/// How long transmission may run without any link statistics before the
/// no-verdict grace ends and the lane reads `rf_unverified`.
pub const RF_PROOF_GRACE: Duration = Duration::from_secs(5);

/// Uplink link-quality percentage below which a proven link reads `degraded`.
pub const LQ_DEGRADED_BELOW: u8 = 50;

/// The lane state, exactly the sidecar vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneState {
    /// The lane is opted out (or this node's profile does not run it).
    Disabled,
    /// Enabled but no usable serial device is open.
    Unconfigured,
    /// Standing by with no liveness verdict. In `crsf_rc` mode: the device is
    /// open but unproven (not transmitting, or still inside the proof grace
    /// window). In `mavlink` mode: the lane itself — the module's port
    /// belongs to the MAVLink router and this service transmits no RC, so
    /// `ready` is the honest "alive, holding off" standby (written directly
    /// by the mode gate, not derived).
    Ready,
    /// Fresh link statistics with healthy uplink quality.
    LinkOk,
    /// Fresh link statistics but poor uplink quality.
    Degraded,
    /// Transmitting beyond the grace window with no received-side proof.
    RfUnverified,
}

impl LaneState {
    pub fn as_str(self) -> &'static str {
        match self {
            LaneState::Disabled => "disabled",
            LaneState::Unconfigured => "unconfigured",
            LaneState::Ready => "ready",
            LaneState::LinkOk => "link_ok",
            LaneState::Degraded => "degraded",
            LaneState::RfUnverified => "rf_unverified",
        }
    }

    /// The sidecar's `rf_unverified` field for this state: `Some(false)` when
    /// a received-side proof exists, `Some(true)` when transmission is
    /// provably unheard, `None` when there is no verdict to report.
    pub fn rf_unverified_flag(self) -> Option<bool> {
        match self {
            LaneState::LinkOk | LaneState::Degraded => Some(false),
            LaneState::RfUnverified => Some(true),
            LaneState::Disabled | LaneState::Unconfigured | LaneState::Ready => None,
        }
    }

    /// Whether the lane may be reported usable for flight: true ONLY when a
    /// received-side proof exists (a fresh link-statistics frame with a
    /// non-zero uplink LQ). `rf_unverified` — and every no-verdict state — is
    /// never flyable: a lane that cannot prove a receiver hears it must not
    /// be offered as a control path.
    pub fn flyable(self) -> bool {
        matches!(self, LaneState::LinkOk | LaneState::Degraded)
    }
}

/// The observed inputs the state derivation reads.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkInputs {
    /// The lane is opted in AND this profile runs it.
    pub enabled: bool,
    /// A serial device is open.
    pub device_open: bool,
    /// How long the transmitter has been running, `None` when it is not.
    pub tx_running_for: Option<Duration>,
    /// Age of the last valid link-statistics frame, `None` when never seen.
    pub stats_age: Option<Duration>,
    /// Uplink link quality from that frame, 0..=100.
    pub uplink_lq: Option<u8>,
}

/// Derive the lane state from the observed inputs. Pure.
pub fn derive_state(i: &LinkInputs) -> LaneState {
    if !i.enabled {
        return LaneState::Disabled;
    }
    if !i.device_open {
        return LaneState::Unconfigured;
    }
    // A fresh link-statistics frame proves the serial telemetry hop — but
    // only a NON-ZERO uplink LQ proves a receiver hears the transmission.
    if let Some(age) = i.stats_age {
        if age <= STATS_FRESH_WINDOW {
            return match i.uplink_lq {
                Some(lq) if lq >= LQ_DEGRADED_BELOW => LaneState::LinkOk,
                Some(lq) if lq > 0 => LaneState::Degraded,
                // Zero (or defensively-missing) uplink LQ: the module reports
                // that nobody hears it — the same no-received-proof ladder as
                // missing statistics, never "degraded".
                _ => no_proof_state(i.tx_running_for),
            };
        }
    }
    no_proof_state(i.tx_running_for)
}

/// The no-received-proof ladder: transmitting past the grace window means the
/// energy is provably going unheard; anything earlier is simply "no verdict
/// yet".
fn no_proof_state(tx_running_for: Option<Duration>) -> LaneState {
    match tx_running_for {
        Some(running) if running > RF_PROOF_GRACE => LaneState::RfUnverified,
        _ => LaneState::Ready,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn disabled_wins_over_everything() {
        let i = LinkInputs {
            enabled: false,
            device_open: true,
            tx_running_for: Some(secs(60)),
            stats_age: Some(secs(0)),
            uplink_lq: Some(100),
        };
        assert_eq!(derive_state(&i), LaneState::Disabled);
        assert_eq!(LaneState::Disabled.rf_unverified_flag(), None);
    }

    #[test]
    fn no_device_reads_unconfigured() {
        let i = LinkInputs {
            enabled: true,
            device_open: false,
            ..Default::default()
        };
        assert_eq!(derive_state(&i), LaneState::Unconfigured);
        assert_eq!(LaneState::Unconfigured.rf_unverified_flag(), None);
    }

    #[test]
    fn open_but_not_transmitting_is_ready_with_no_verdict() {
        let i = LinkInputs {
            enabled: true,
            device_open: true,
            tx_running_for: None,
            stats_age: None,
            uplink_lq: None,
        };
        assert_eq!(derive_state(&i), LaneState::Ready);
        assert_eq!(LaneState::Ready.rf_unverified_flag(), None);
    }

    #[test]
    fn transmitting_inside_grace_stays_ready() {
        let i = LinkInputs {
            enabled: true,
            device_open: true,
            tx_running_for: Some(secs(3)),
            stats_age: None,
            uplink_lq: None,
        };
        assert_eq!(derive_state(&i), LaneState::Ready);
    }

    #[test]
    fn transmitting_past_grace_with_no_stats_is_rf_unverified() {
        let i = LinkInputs {
            enabled: true,
            device_open: true,
            tx_running_for: Some(secs(6)),
            stats_age: None,
            uplink_lq: None,
        };
        assert_eq!(derive_state(&i), LaneState::RfUnverified);
        assert_eq!(LaneState::RfUnverified.rf_unverified_flag(), Some(true));
    }

    #[test]
    fn fresh_stats_with_good_lq_is_link_ok() {
        let i = LinkInputs {
            enabled: true,
            device_open: true,
            tx_running_for: Some(secs(60)),
            stats_age: Some(secs(1)),
            uplink_lq: Some(99),
        };
        assert_eq!(derive_state(&i), LaneState::LinkOk);
        assert_eq!(LaneState::LinkOk.rf_unverified_flag(), Some(false));
    }

    #[test]
    fn fresh_stats_with_poor_lq_is_degraded() {
        let i = LinkInputs {
            enabled: true,
            device_open: true,
            tx_running_for: Some(secs(60)),
            stats_age: Some(secs(1)),
            uplink_lq: Some(LQ_DEGRADED_BELOW - 1),
        };
        assert_eq!(derive_state(&i), LaneState::Degraded);
        // RF is verified (frames ARE arriving); the link is merely poor.
        assert_eq!(LaneState::Degraded.rf_unverified_flag(), Some(false));
    }

    #[test]
    fn stale_stats_fall_back_to_the_no_proof_ladder() {
        // Stats went stale while transmitting: the proof has lapsed, so the
        // lane is back to unverified — a dead link must not keep reading ok.
        let i = LinkInputs {
            enabled: true,
            device_open: true,
            tx_running_for: Some(secs(60)),
            stats_age: Some(secs(10)),
            uplink_lq: Some(100),
        };
        assert_eq!(derive_state(&i), LaneState::RfUnverified);
    }

    #[test]
    fn tx_advancing_with_zero_uplink_lq_is_rf_unverified() {
        // The module answers on serial (fresh statistics) but reports that no
        // receiver hears it: transmission past the grace window is provably
        // unheard — never "degraded", never flyable.
        let i = LinkInputs {
            enabled: true,
            device_open: true,
            tx_running_for: Some(secs(60)),
            stats_age: Some(secs(1)),
            uplink_lq: Some(0),
        };
        assert_eq!(derive_state(&i), LaneState::RfUnverified);
        assert_eq!(LaneState::RfUnverified.rf_unverified_flag(), Some(true));
        assert!(!LaneState::RfUnverified.flyable());
        // A defensively-missing LQ reads the same: zero proof, not degraded.
        let missing = LinkInputs {
            uplink_lq: None,
            ..i
        };
        assert_eq!(derive_state(&missing), LaneState::RfUnverified);
    }

    #[test]
    fn zero_lq_inside_the_grace_window_is_still_no_verdict() {
        // At bring-up the module reports LQ 0 while the RF link acquires; the
        // grace window applies to the zero-LQ path exactly as to missing
        // statistics — "no verdict yet", not a premature rf_unverified.
        let i = LinkInputs {
            enabled: true,
            device_open: true,
            tx_running_for: Some(secs(3)),
            stats_age: Some(secs(1)),
            uplink_lq: Some(0),
        };
        assert_eq!(derive_state(&i), LaneState::Ready);
    }

    #[test]
    fn recovery_from_rf_unverified_needs_a_real_lq() {
        // The rf_unverified → link_ok transition happens the moment a fresh
        // statistics frame carries a non-zero LQ again.
        let unheard = LinkInputs {
            enabled: true,
            device_open: true,
            tx_running_for: Some(secs(60)),
            stats_age: Some(secs(1)),
            uplink_lq: Some(0),
        };
        assert_eq!(derive_state(&unheard), LaneState::RfUnverified);
        let heard = LinkInputs {
            uplink_lq: Some(97),
            ..unheard
        };
        assert_eq!(derive_state(&heard), LaneState::LinkOk);
        assert!(LaneState::LinkOk.flyable());
    }

    #[test]
    fn only_received_side_proven_states_are_flyable() {
        assert!(LaneState::LinkOk.flyable());
        assert!(LaneState::Degraded.flyable());
        for state in [
            LaneState::Disabled,
            LaneState::Unconfigured,
            LaneState::Ready,
            LaneState::RfUnverified,
        ] {
            assert!(!state.flyable(), "{state:?} must never read flyable");
        }
    }

    #[test]
    fn state_strings_match_the_sidecar_vocabulary() {
        assert_eq!(LaneState::Disabled.as_str(), "disabled");
        assert_eq!(LaneState::Unconfigured.as_str(), "unconfigured");
        assert_eq!(LaneState::Ready.as_str(), "ready");
        assert_eq!(LaneState::LinkOk.as_str(), "link_ok");
        assert_eq!(LaneState::Degraded.as_str(), "degraded");
        assert_eq!(LaneState::RfUnverified.as_str(), "rf_unverified");
    }
}
