//! Received-side link proof for the drone transmit plane.
//!
//! A transmit counter that is advancing only proves the driver accepted frames
//! into its transmit ring; it never proves the energy reached a receiver. The
//! drone closes that gap by tracking the last instant it heard a verified return
//! signal on the control plane — a HopAck or a peer PresenceBeacon. Both are
//! HMAC-signed with the pair key, so only the bound peer can produce one.
//!
//! The transmit-only end has no decode statistics of its own (no `rx.key`, the
//! drone is the video source, not a receiver), so this is the drone's only
//! received-side signal. `channel_locked` is derived from it (false until a
//! return signal is heard), and `rf_unverified` is raised when the transmit
//! counter advances while no return signal has been heard for a grace window.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Grace window after which an advancing transmit counter with no heard return
/// signal is flagged `rf_unverified`. Matches the transmit-liveness window used
/// elsewhere (30 s): long enough that a brief beacon gap does not flap the flag,
/// short enough that a loose antenna or a forbidden-band cap surfaces quickly.
pub const RX_PROOF_GRACE: Duration = Duration::from_secs(30);

/// Tracks the last time the drone heard a verified return signal (HopAck or peer
/// PresenceBeacon) on the control plane. Cheap, lock-free, and cloneable so the
/// always-on control-plane listener can record proof while the heartbeat reads
/// it on its own cadence.
#[derive(Clone)]
pub struct RxProof {
    inner: Arc<RxProofInner>,
}

struct RxProofInner {
    /// Monotonic millis (since the service's reference instant) of the last
    /// verified return signal, or 0 when none has been heard.
    last_ms: AtomicU64,
    /// True once any return signal has ever been heard. Distinguishes "never
    /// proven" from "proven then went stale".
    ever: AtomicBool,
}

impl RxProof {
    /// A fresh proof tracker with no return signal heard yet. `reference` is the
    /// monotonic origin all observations are measured against (typically the
    /// service start instant) so the millis fit a u64 without wall-clock skew.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RxProofInner {
                last_ms: AtomicU64::new(0),
                ever: AtomicBool::new(false),
            }),
        }
    }

    /// Record that a verified return signal was heard `now`. `reference` is the
    /// shared monotonic origin.
    pub fn observe(&self, now: Instant, reference: Instant) {
        let ms = now.saturating_duration_since(reference).as_millis() as u64;
        // A return signal at the reference instant itself would store 0, which
        // reads as "never"; clamp to 1 ms so the first observation always counts.
        self.inner.last_ms.store(ms.max(1), Ordering::Relaxed);
        self.inner.ever.store(true, Ordering::Relaxed);
    }

    /// True when a return signal was heard within `window` of `now`. False when
    /// none has ever been heard. This is the drone's received-side lock proof.
    pub fn proven_within(&self, window: Duration, now: Instant, reference: Instant) -> bool {
        if !self.inner.ever.load(Ordering::Relaxed) {
            return false;
        }
        let last_ms = self.inner.last_ms.load(Ordering::Relaxed);
        let now_ms = now.saturating_duration_since(reference).as_millis() as u64;
        now_ms.saturating_sub(last_ms) <= window.as_millis() as u64
    }
}

impl Default for RxProof {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether an advancing transmit counter with no confirmed reception should be
/// flagged `rf_unverified`. The transmit-only end is `rf_unverified` when it is
/// actively injecting RF (`tx_live`) yet has heard no verified return signal
/// within the grace window — the exact "transmitting, zero confirmed reception"
/// case (a loose antenna, a forbidden-band power cap, a dead peer). Pure so the
/// decision is unit-testable without standing up the control-plane listener.
///
/// Not `rf_unverified` when the transmit counter is flat (that is a separate
/// idle/stalled case the transmit watchdog owns) or when a return signal is
/// fresh (the link is proven).
pub fn is_rf_unverified(tx_live: bool, rx_proven: bool) -> bool {
    tx_live && !rx_proven
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grace_window_is_30s() {
        assert_eq!(RX_PROOF_GRACE.as_secs(), 30);
    }

    #[test]
    fn never_heard_is_not_proven() {
        let p = RxProof::new();
        let reference = Instant::now();
        assert!(!p.proven_within(RX_PROOF_GRACE, reference, reference));
    }

    #[test]
    fn fresh_observation_is_proven() {
        let reference = Instant::now();
        let p = RxProof::new();
        p.observe(reference, reference);
        // At the reference instant the proof is fresh.
        assert!(p.proven_within(RX_PROOF_GRACE, reference, reference));
        // Still proven a few seconds later (inside the window).
        let later = reference + Duration::from_secs(5);
        assert!(p.proven_within(RX_PROOF_GRACE, later, reference));
    }

    #[test]
    fn stale_observation_is_not_proven() {
        let reference = Instant::now();
        let p = RxProof::new();
        p.observe(reference, reference);
        // 31 s later — past the 30 s grace — the proof is stale.
        let later = reference + Duration::from_secs(31);
        assert!(!p.proven_within(RX_PROOF_GRACE, later, reference));
    }

    #[test]
    fn observation_clamps_to_at_least_one_ms() {
        // A return signal exactly at the reference must still register as proof,
        // not read back as "never heard" (the 0-millis ambiguity).
        let reference = Instant::now();
        let p = RxProof::new();
        p.observe(reference, reference);
        assert!(p.inner.last_ms.load(Ordering::Relaxed) >= 1);
        assert!(p.inner.ever.load(Ordering::Relaxed));
    }

    #[test]
    fn rf_unverified_only_when_transmitting_without_proof() {
        // Transmitting + no return signal → unverified (the failure case).
        assert!(is_rf_unverified(true, false));
        // Transmitting + proven → verified link, not flagged.
        assert!(!is_rf_unverified(true, true));
        // Idle transmit counter → not the unverified case (the flat-TX watchdog
        // owns that), regardless of proof state.
        assert!(!is_rf_unverified(false, false));
        assert!(!is_rf_unverified(false, true));
    }

    #[test]
    fn clone_shares_underlying_state() {
        let reference = Instant::now();
        let p = RxProof::new();
        let q = p.clone();
        // An observation through one handle is visible through the other.
        q.observe(reference, reference);
        assert!(p.proven_within(RX_PROOF_GRACE, reference, reference));
    }
}
