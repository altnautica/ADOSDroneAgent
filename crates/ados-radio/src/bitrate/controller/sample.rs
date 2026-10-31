//! Per-tick link measurement resolution.

/// Where this tick's link measurement came from.
///
/// Surfaced on the snapshot so an operator can tell a rung chosen from a real
/// measurement apart from one chosen from congestion or held for want of any
/// signal at all — the cases previously looked identical from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleSource {
    /// Reported by the peer that receives our transmission: the only
    /// measurement of the downlink this controller drives.
    Peer,
    /// No usable measurement this tick.
    None,
}

impl SampleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Peer => "peer",
            Self::None => "none",
        }
    }
}

/// One tick's resolved link measurement.
pub struct ResolvedSample {
    pub loss_percent: f64,
    pub rssi_dbm: f64,
    pub snr_db: f64,
    pub source: SampleSource,
}

impl ResolvedSample {
    /// Whether the ladders may act on this sample.
    pub fn has_sample(&self) -> bool {
        self.source != SampleSource::None
    }
}

/// Pick this tick's measurement of the downlink.
///
/// The controller drives this node's transmitter, so only the receiving peer
/// can measure what it does: a single radio in monitor mode cannot capture its
/// own injected frames. This node's own receivers measure the uplink, the other
/// direction, with a different transmitter and noise floor, so they never stand
/// in for the downlink.
///
/// The peer sample must be BOTH fresh and an actual measurement. Feedback most
/// often stops because the link got worse, so treating an old report as current
/// would hold the rate high at exactly the moment it should fall; and a peer
/// that heard nothing is reporting deafness, not a clean link.
pub fn resolve_sample(
    peer: Option<&ados_protocol::link_feedback::LinkFeedbackSidecar>,
    now_unix_ms: u64,
) -> ResolvedSample {
    match peer {
        Some(p) if p.is_usable_at(now_unix_ms) => ResolvedSample {
            loss_percent: p.loss_percent,
            rssi_dbm: p.rssi_dbm,
            snr_db: p.snr_db,
            source: SampleSource::Peer,
        },
        _ => ResolvedSample {
            loss_percent: 0.0,
            rssi_dbm: -100.0,
            snr_db: 0.0,
            source: SampleSource::None,
        },
    }
}

/// Current wall clock in epoch milliseconds, for the freshness comparison.
pub(super) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
