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

/// The event kind emitted once per session after the injection interface is
/// prepared for monitor mode, recording whether the iface is ready to radiate.
/// A durable breadcrumb: "did the injection iface reach monitor mode, and was it
/// NM-enumerable" is then a single event query rather than a wfb-server stderr
/// scrape — the failure mode that needed a multi-process trace to diagnose.
pub const BIND_PRECHECK_KIND: &str = "radio.bind_precheck";

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
    /// The wire protocol completed but no peer traffic was decoded on the bind
    /// tunnel — the exchange ran against a stale/leaked local endpoint, not a
    /// real peer, so the session refused to pair (the peer-evidence gate).
    NoPeerProof,
    /// The wire protocol completed but the upstream key file was not refreshed
    /// by this session's transfer (a leftover key from an earlier bind) — no key
    /// actually crossed the radio (the drone-side key-freshness gate).
    StaleKey,
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
            BindFailReason::NoPeerProof => "no_peer_proof",
            BindFailReason::StaleKey => "stale_key",
            BindFailReason::Timeout => "timeout",
            BindFailReason::Interrupted => "interrupted",
            BindFailReason::Other => "other",
        }
    }

    /// Classify a protocol-error message into the reason taxonomy. The bind
    /// protocol surfaces failures as a human message + an optional phase; this
    /// maps the recognisable causes (a missing key artifact, a peer that never
    /// connected) to a code and falls back to [`BindFailReason::Other`]. It is
    /// the fallback only: the peer-evidence gate carries its precise cause
    /// ([`NoPeerProof`](Self::NoPeerProof) / [`StaleKey`](Self::StaleKey)) as an
    /// explicit reason on the error, so those never reach the string-matching
    /// path here.
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

/// Build the `radio.bind_precheck` detail map.
///
/// - `role` — `drone` | `gs`;
/// - `ok` — every injection candidate reached verified monitor mode;
/// - `reason` — bland code when not ok (`iface_not_found` | `monitor_unverified`);
/// - `injection_mode` — the readback mode (`monitor` | `managed` | `unknown`);
/// - `nm_enumerable` — whether NetworkManager lists the injection iface;
/// - `iface` — the injection interface the result describes, when one was found.
pub fn bind_precheck_detail(
    role: &str,
    ok: bool,
    reason: Option<&str>,
    injection_mode: &str,
    nm_enumerable: bool,
    iface: Option<&str>,
) -> Fields {
    let mut d = Fields::new();
    d.insert("role".to_string(), MpVal::from(role));
    d.insert("ok".to_string(), MpVal::from(ok));
    if let Some(reason) = reason {
        d.insert("reason".to_string(), MpVal::from(reason));
    }
    d.insert("injection_mode".to_string(), MpVal::from(injection_mode));
    d.insert("nm_enumerable".to_string(), MpVal::from(nm_enumerable));
    if let Some(iface) = iface {
        d.insert("iface".to_string(), MpVal::from(iface));
    }
    d
}

#[cfg(test)]
mod tests;
