//! Structured bind-session lifecycle events for the logging daemon.
//!
//! The bind orchestrator transitions a session through a small FSM and, until
//! now, recorded its lifecycle only as `tracing` log lines (`bind_session_started`,
//! `bind_session_failed`). An RCA that wanted "did the last bind fail, and why"
//! had to scrape those lines. This module promotes the lifecycle to two discrete,
//! queryable events with a bounded reason taxonomy on failure, so the answer is a
//! single event query. The log lines stay; this is additive durable capture.
//!
//! The reason taxonomy is bland and reader-facing (no internal tags): a started
//! event carries no reason; a failed event carries one of a fixed set so a
//! consumer can branch without parsing a free-text message.

use ados_protocol::logd::{Fields, Value as MpVal};

/// The event kind emitted when a bind session opens.
pub const BIND_STARTED_KIND: &str = "radio.bind";

/// The event kind emitted when a bind session ends without pairing.
pub const BIND_FAILED_KIND: &str = "radio.bind_failed";

/// Why a bind session ended without pairing. A bounded set so a consumer can
/// branch on the cause without parsing the free-text message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindFailReason {
    /// The transmit key was absent, so binding could not start.
    NoTxKey,
    /// The regulatory gate blocked the radio, so no link could form.
    RegBlocked,
    /// The peer never appeared during the bind window.
    NoPeer,
    /// The session timed out (the no-progress watchdog fired) with no peer.
    Timeout,
    /// The session was interrupted (cancelled by the caller or the control op).
    Interrupted,
    /// Any other protocol or environment failure.
    Other,
}

impl BindFailReason {
    /// The bland, stable reason code carried in the event detail.
    pub fn as_str(self) -> &'static str {
        match self {
            BindFailReason::NoTxKey => "no_tx_key",
            BindFailReason::RegBlocked => "reg_blocked",
            BindFailReason::NoPeer => "no_peer",
            BindFailReason::Timeout => "timeout",
            BindFailReason::Interrupted => "interrupted",
            BindFailReason::Other => "other",
        }
    }

    /// Classify a protocol-error message into the reason taxonomy. The bind
    /// protocol surfaces failures as a human message + an optional phase; this
    /// maps the recognisable causes (a missing key artifact, a peer that never
    /// connected) to a code and falls back to [`BindFailReason::Other`].
    pub fn classify_error(message: &str, phase: Option<&str>) -> Self {
        let low = message.to_ascii_lowercase();
        if low.contains("key") && (low.contains("missing") || low.contains("not present")) {
            // A missing key artifact preventing the handshake from completing.
            BindFailReason::NoTxKey
        } else if low.contains("peer") || phase == Some("waiting_peer") {
            BindFailReason::NoPeer
        } else {
            BindFailReason::Other
        }
    }
}

/// Build the `radio.bind` (session started) detail map.
///
/// - `role` — `drone` | `gs`;
/// - `source` — who started the session (e.g. `auto`, `operator`);
/// - `session_id` — the session identifier;
/// - `peer_device_id` — the targeted peer, when one was named.
pub fn bind_started_detail(
    role: &str,
    source: &str,
    session_id: &str,
    peer_device_id: Option<&str>,
) -> Fields {
    let mut d = Fields::new();
    d.insert("role".to_string(), MpVal::from(role));
    d.insert("source".to_string(), MpVal::from(source));
    d.insert("session_id".to_string(), MpVal::from(session_id));
    if let Some(peer) = peer_device_id {
        d.insert("peer_device_id".to_string(), MpVal::from(peer));
    }
    d
}

/// Build the `radio.bind_failed` (session failed) detail map.
///
/// - `role` — `drone` | `gs`;
/// - `reason` — the bounded reason code;
/// - `phase` — the FSM phase the failure surfaced in, when known;
/// - `message` — the human-readable failure detail (redacted at the emitter);
/// - `session_id` — the session identifier;
/// - `elapsed_s` — seconds the session ran before it failed.
pub fn bind_failed_detail(
    role: &str,
    reason: BindFailReason,
    phase: Option<&str>,
    message: &str,
    session_id: &str,
    elapsed_s: u64,
) -> Fields {
    let mut d = Fields::new();
    d.insert("role".to_string(), MpVal::from(role));
    d.insert("reason".to_string(), MpVal::from(reason.as_str()));
    if let Some(phase) = phase {
        d.insert("phase".to_string(), MpVal::from(phase));
    }
    d.insert("message".to_string(), MpVal::from(message));
    d.insert("session_id".to_string(), MpVal::from(session_id));
    d.insert("elapsed_s".to_string(), MpVal::from(elapsed_s));
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_codes_are_bland_and_stable() {
        assert_eq!(BindFailReason::NoTxKey.as_str(), "no_tx_key");
        assert_eq!(BindFailReason::RegBlocked.as_str(), "reg_blocked");
        assert_eq!(BindFailReason::NoPeer.as_str(), "no_peer");
        assert_eq!(BindFailReason::Timeout.as_str(), "timeout");
        assert_eq!(BindFailReason::Interrupted.as_str(), "interrupted");
        assert_eq!(BindFailReason::Other.as_str(), "other");
    }

    #[test]
    fn classify_error_maps_recognisable_causes() {
        assert_eq!(
            BindFailReason::classify_error("upstream wfb-ng artifact missing: /etc/bind.key", None),
            BindFailReason::NoTxKey
        );
        assert_eq!(
            BindFailReason::classify_error("no progress", Some("waiting_peer")),
            BindFailReason::NoPeer
        );
        assert_eq!(
            BindFailReason::classify_error("peer did not connect over the tunnel", None),
            BindFailReason::NoPeer
        );
        assert_eq!(
            BindFailReason::classify_error("socat client exited rc=1", Some("transferring_keys")),
            BindFailReason::Other
        );
    }

    #[test]
    fn started_detail_omits_peer_when_absent() {
        let d = bind_started_detail("drone", "auto", "abcd1234", None);
        assert_eq!(d.get("role").and_then(|v| v.as_str()), Some("drone"));
        assert_eq!(d.get("source").and_then(|v| v.as_str()), Some("auto"));
        assert_eq!(
            d.get("session_id").and_then(|v| v.as_str()),
            Some("abcd1234")
        );
        assert!(!d.contains_key("peer_device_id"));

        let with_peer = bind_started_detail("gs", "operator", "ef56", Some("dev-7"));
        assert_eq!(
            with_peer.get("peer_device_id").and_then(|v| v.as_str()),
            Some("dev-7")
        );
    }

    #[test]
    fn failed_detail_carries_reason_phase_and_elapsed() {
        let d = bind_failed_detail(
            "gs",
            BindFailReason::Timeout,
            Some("waiting_peer"),
            "watchdog fired after 120s with no progress",
            "abcd1234",
            120,
        );
        assert_eq!(d.get("role").and_then(|v| v.as_str()), Some("gs"));
        assert_eq!(d.get("reason").and_then(|v| v.as_str()), Some("timeout"));
        assert_eq!(
            d.get("phase").and_then(|v| v.as_str()),
            Some("waiting_peer")
        );
        assert_eq!(d.get("elapsed_s").and_then(|v| v.as_u64()), Some(120));
        assert!(d.get("message").and_then(|v| v.as_str()).is_some());
    }
}
