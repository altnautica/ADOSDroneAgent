//! WFB link connection state — the `state` string the radio sidecar surfaces.
//!
//! The wire strings match the Python `LinkState` `StrEnum` values exactly so the
//! REST handler and the GCS Radio panel render the same vocabulary regardless of
//! which implementation writes the sidecar.

use crate::link_quality::LinkStats;

/// Loss above this percentage marks the link degraded.
const DEGRADED_LOSS_PERCENT: f64 = 50.0;
/// RSSI below this dBm marks the link degraded.
const DEGRADED_RSSI_DBM: f64 = -85.0;

/// WFB link connection state. The `as_str` wire strings are byte-identical to
/// the Python `LinkState` `StrEnum` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Disconnected,
    Unpaired,
    AutoPairing,
    Binding,
    Connecting,
    Connected,
    Degraded,
}

impl LinkState {
    /// The status-surface wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            LinkState::Disconnected => "disconnected",
            LinkState::Unpaired => "unpaired",
            LinkState::AutoPairing => "auto_pairing",
            LinkState::Binding => "binding",
            LinkState::Connecting => "connecting",
            LinkState::Connected => "connected",
            LinkState::Degraded => "degraded",
        }
    }

    /// Whether the link is locked: a usable, connected link. The single source of
    /// truth for the lock/unlock transition the telemetry producers emit, so the
    /// "is the link up" question has one answer derived from this enum.
    pub fn is_locked(self) -> bool {
        matches!(self, LinkState::Connected)
    }
}

/// Derive the radio link state for the sidecar.
///
/// Precedence (highest first):
///   1. no WFB TX key on disk            → `unpaired`
///   2. a bind session is in flight      → `binding`
///   3. a REAL measurement (decoded packets) shows loss > 50% or RSSI < -85 dBm
///      → `degraded`
///   4. `tx_bytes` advanced in the last 5 s → `connected` (the radio is
///      injecting RF)
///   5. data packets are decoding        → `connected`
///   6. otherwise                        → `connecting`
///
/// `tx_live` is true when the interface `tx_bytes` counter is non-zero and has
/// moved within the last 5 s, the same liveness window the TX-health watchdog
/// uses. It is the strongest "RF is leaving the antenna" signal, so it promotes
/// the link to `connected` even before the stats RX has decoded a return packet.
///
/// The `degraded` verdict requires a REAL link measurement. The default
/// `LinkStats` sentinel (rssi -100, 0 packets, empty timestamp) means "no return
/// signal decoded" — NOT a bad link — and a transmit-dominant drone (its video
/// reaches the peer, yet it decodes no inbound stream) sits on that sentinel.
/// Calling the sentinel `degraded` reported a healthy injecting drone as
/// degraded. The same `packets_received`-based real-measurement gate the
/// reactive hop applies (`hop_supervisor::reactive_should_fire`) is used here so
/// the status surface and the hop logic agree on what counts as a measurement.
pub fn derive_link_state(
    tx_key_present: bool,
    bind_active: bool,
    link: &LinkStats,
    tx_live: bool,
) -> LinkState {
    if !tx_key_present {
        return LinkState::Unpaired;
    }
    if bind_active {
        return LinkState::Binding;
    }
    // Only a decoded measurement can be judged degraded; the sentinel cannot.
    let has_real_measurement = link.packets_received > 0;
    if has_real_measurement
        && (link.loss_percent > DEGRADED_LOSS_PERCENT || link.rssi_dbm < DEGRADED_RSSI_DBM)
    {
        return LinkState::Degraded;
    }
    if tx_live {
        return LinkState::Connected;
    }
    if link.packets_received > 0 {
        return LinkState::Connected;
    }
    LinkState::Connecting
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_strings_match_python_strenum() {
        assert_eq!(LinkState::Disconnected.as_str(), "disconnected");
        assert_eq!(LinkState::Unpaired.as_str(), "unpaired");
        assert_eq!(LinkState::AutoPairing.as_str(), "auto_pairing");
        assert_eq!(LinkState::Binding.as_str(), "binding");
        assert_eq!(LinkState::Connecting.as_str(), "connecting");
        assert_eq!(LinkState::Connected.as_str(), "connected");
        assert_eq!(LinkState::Degraded.as_str(), "degraded");
    }

    #[test]
    fn only_connected_is_locked() {
        assert!(LinkState::Connected.is_locked());
        for s in [
            LinkState::Disconnected,
            LinkState::Unpaired,
            LinkState::AutoPairing,
            LinkState::Binding,
            LinkState::Connecting,
            LinkState::Degraded,
        ] {
            assert!(!s.is_locked(), "{} must not be locked", s.as_str());
        }
    }

    #[test]
    fn degraded_thresholds_match_python() {
        assert_eq!(DEGRADED_LOSS_PERCENT, 50.0);
        assert_eq!(DEGRADED_RSSI_DBM, -85.0);
    }

    fn good_link() -> LinkStats {
        // A healthy decoded link: low loss, strong RSSI, packets flowing.
        LinkStats {
            rssi_dbm: -50.0,
            loss_percent: 2.0,
            packets_received: 500,
            ..LinkStats::default()
        }
    }

    #[test]
    fn no_key_is_unpaired_regardless_of_stats() {
        // Even a healthy link reports unpaired when the key is gone.
        assert_eq!(
            derive_link_state(false, false, &good_link(), true),
            LinkState::Unpaired
        );
    }

    #[test]
    fn bind_active_is_binding_over_everything_but_key() {
        // Bind outranks degraded / connected, but not the missing-key guard.
        assert_eq!(
            derive_link_state(true, true, &good_link(), true),
            LinkState::Binding
        );
        let degraded = LinkStats {
            loss_percent: 80.0,
            ..LinkStats::default()
        };
        assert_eq!(
            derive_link_state(true, true, &degraded, false),
            LinkState::Binding
        );
    }

    #[test]
    fn high_loss_is_degraded() {
        let link = LinkStats {
            loss_percent: 60.0,
            rssi_dbm: -50.0,
            packets_received: 100,
            ..LinkStats::default()
        };
        assert_eq!(
            derive_link_state(true, false, &link, true),
            LinkState::Degraded
        );
    }

    #[test]
    fn low_rssi_is_degraded() {
        let link = LinkStats {
            loss_percent: 0.0,
            rssi_dbm: -90.0,
            packets_received: 100,
            ..LinkStats::default()
        };
        assert_eq!(
            derive_link_state(true, false, &link, true),
            LinkState::Degraded
        );
    }

    #[test]
    fn loss_exactly_50_is_not_degraded() {
        // The threshold is strict (> 50), so exactly 50 stays connected.
        let link = LinkStats {
            loss_percent: 50.0,
            rssi_dbm: -50.0,
            packets_received: 100,
            ..LinkStats::default()
        };
        assert_eq!(
            derive_link_state(true, false, &link, false),
            LinkState::Connected
        );
    }

    #[test]
    fn rssi_exactly_minus_85_is_not_degraded() {
        // Default LinkStats has rssi -100, so use an explicit -85 with packets.
        let link = LinkStats {
            loss_percent: 0.0,
            rssi_dbm: -85.0,
            packets_received: 1,
            ..LinkStats::default()
        };
        assert_eq!(
            derive_link_state(true, false, &link, false),
            LinkState::Connected
        );
    }

    #[test]
    fn tx_live_is_connected_even_without_decode_stats() {
        // Drone-only rig: default LinkStats (rssi -100, 0 packets) but tx_bytes
        // is advancing → the radio is injecting → connected, not degraded.
        // The default rssi of -100 would trip degraded, so a tx-live rig must
        // still surface as connected. This is the drone-only-injection case:
        // provide a neutral rssi so the degraded guard does not pre-empt.
        let link = LinkStats {
            rssi_dbm: -50.0,
            ..LinkStats::default()
        };
        assert_eq!(
            derive_link_state(true, false, &link, true),
            LinkState::Connected
        );
    }

    #[test]
    fn packets_flow_is_connected() {
        let link = LinkStats {
            rssi_dbm: -55.0,
            packets_received: 10,
            ..LinkStats::default()
        };
        assert_eq!(
            derive_link_state(true, false, &link, false),
            LinkState::Connected
        );
    }

    #[test]
    fn paired_but_silent_is_connecting() {
        // Keyed, no bind, no tx liveness, no packets, neutral rssi → connecting.
        let link = LinkStats {
            rssi_dbm: -50.0,
            ..LinkStats::default()
        };
        assert_eq!(
            derive_link_state(true, false, &link, false),
            LinkState::Connecting
        );
    }

    #[test]
    fn default_sentinel_no_tx_is_connecting_not_degraded() {
        // The raw default LinkStats has rssi -100, but with 0 decoded packets it
        // is the "no measurement" sentinel, not a real reading — so an idle keyed
        // rig with no tx activity surfaces as connecting (trying), never degraded
        // (a bad measured link). The degraded verdict needs a real decode.
        assert_eq!(
            derive_link_state(true, false, &LinkStats::default(), false),
            LinkState::Connecting
        );
    }

    #[test]
    fn tx_live_on_sentinel_rssi_is_connected_not_degraded() {
        // The transmit-dominant drone case: the default sentinel (rssi -100, 0
        // packets) while the radio is injecting RF must report connected, NOT
        // degraded. The drone's video reaches the peer; it simply decodes no
        // return signal of its own, so the sentinel rssi must not mark it bad.
        assert_eq!(
            derive_link_state(true, false, &LinkStats::default(), true),
            LinkState::Connected
        );
    }

    #[test]
    fn real_low_rssi_still_degraded_with_tx_live() {
        // A genuine weak measured link (real decoded packets, rssi below the
        // floor) is still degraded even while injecting — the gate only spares
        // the no-measurement sentinel, not a real bad reading.
        let link = LinkStats {
            rssi_dbm: -92.0,
            packets_received: 50,
            ..LinkStats::default()
        };
        assert_eq!(
            derive_link_state(true, false, &link, true),
            LinkState::Degraded
        );
    }
}
