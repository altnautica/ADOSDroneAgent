//! The link-aware freshness gate and the lock-state safety gate.
//!
//! Remote perception is near-real-time, not real-time (200-500 ms on a direct
//! LAN, more on a relay). The one invariant the whole offload path is built
//! around: **a result past its freshness budget, or a dropped link, is treated
//! as absent — never held forward, never extrapolated.** A behaviour that has a
//! lock loses it (stop and hold) and **never auto-re-acquires**; only an explicit
//! re-designation re-locks. This is the link-aware tightening of the shipped
//! Follow-Me lock-state gate, applied to any consumer of offloaded results.

use serde::{Deserialize, Serialize};

/// The freshness of one offloaded stream (a detection/target stream, or a pose
/// stream), derived from the last result's age and the link state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateState {
    /// A result arrived within the budget and the link is up — usable.
    Fresh,
    /// A result arrived but it is older than the budget — not usable (stop).
    Stale,
    /// No result yet, or the link is down — not usable (stop).
    Lost,
}

impl GateState {
    /// Only a `Fresh` reading may drive a behaviour. `Stale` and `Lost` both mean
    /// stop and hold.
    pub fn is_usable(self) -> bool {
        matches!(self, GateState::Fresh)
    }
}

/// Tracks the freshness of one offloaded result stream.
///
/// Freshness is anchored on the **local monotonic time the result arrived**, not
/// on the result's own (remote) timestamp. This is deliberate and load-bearing:
/// the drone and the compute node are not clock-synced, so a node whose clock is
/// skewed ahead, whose detector then hangs (the socket staying up, so no link
/// drop fires), would keep a frozen stream reading fresh forever if age were
/// measured from the remote timestamp — the aircraft would follow a frozen
/// target. Measuring "time since a result last arrived, on the drone's own
/// monotonic clock" catches exactly that stall (the Rule-37 / advancing-is-not-
/// proof-of-work class). The `now_ms` passed to [`record`](Self::record) and
/// [`state`](Self::state) MUST come from the same local monotonic clock; the
/// remote result timestamp is for telemetry, never for this gate.
#[derive(Debug, Clone)]
pub struct FreshnessGate {
    budget_ms: i64,
    last_arrival_ms: Option<i64>,
    link_up: bool,
}

impl FreshnessGate {
    /// A gate with the stream's freshness `budget_ms` (e.g. a target-age or a
    /// pose-age budget). A negative budget is clamped to 0 (a misconfiguration
    /// must not flip the fail-safe direction). Starts `Lost` (no result yet);
    /// the link starts up.
    pub fn new(budget_ms: i64) -> Self {
        Self {
            budget_ms: budget_ms.max(0),
            last_arrival_ms: None,
            link_up: true,
        }
    }

    /// Record that a result arrived. `now_ms` is the **local monotonic time of
    /// arrival** (NOT the result's own timestamp) — see the type docs.
    pub fn record(&mut self, now_ms: i64) {
        self.last_arrival_ms = Some(now_ms);
    }

    /// Set the link state (a drop trips the gate to `Lost` regardless of the
    /// last result's age).
    pub fn set_link(&mut self, up: bool) {
        self.link_up = up;
    }

    /// The gate state at the local monotonic time `now_ms`.
    pub fn state(&self, now_ms: i64) -> GateState {
        if !self.link_up {
            return GateState::Lost;
        }
        match self.last_arrival_ms {
            None => GateState::Lost,
            // now_ms and the arrival are on the same monotonic clock, so the age
            // is non-negative; saturating_sub guards a caller that violates that.
            Some(arrival) if now_ms.saturating_sub(arrival) <= self.budget_ms => GateState::Fresh,
            Some(_) => GateState::Stale,
        }
    }

    /// Whether a behaviour may act on this stream at `now_ms`.
    pub fn is_usable(&self, now_ms: i64) -> bool {
        self.state(now_ms).is_usable()
    }
}

/// The lock-state machine. A behaviour locks onto a target (an operator
/// designate, or an auto-lock). While `Locked` it is *acquiring* until the
/// stream first becomes usable; once it has had a usable reading, the stream
/// going stale or the link dropping drops it to `Lost`. The only way out of
/// `Lost` is an explicit re-lock — there is no auto-re-acquire. A lock that has
/// never yet had a usable reading does NOT drop to `Lost` (there is nothing to
/// lose); it stays `Locked`, holding, until a result arrives or it is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockState {
    /// No lock (idle).
    Unlocked,
    /// Locked: either acquiring (no usable reading yet) or tracking.
    Locked,
    /// The lock was dropped (a stream that HAD been usable went stale / the link
    /// dropped). Stays here until an explicit re-lock; never auto-re-acquires.
    Lost,
}

/// The lock-state safety gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockGate {
    state: LockState,
    /// Whether the stream has been usable at least once since the current lock.
    /// Until it has, a not-usable stream is "acquiring", not "lost".
    ever_usable_since_lock: bool,
}

impl Default for LockGate {
    fn default() -> Self {
        Self {
            state: LockState::Unlocked,
            ever_usable_since_lock: false,
        }
    }
}

impl LockGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> LockState {
        self.state
    }

    /// Whether the behaviour is locked (acquiring or tracking). `commanding` (a
    /// behaviour actually driving the FC) additionally requires the stream to be
    /// usable this cycle; the session combines the two.
    pub fn is_locked(&self) -> bool {
        self.state == LockState::Locked
    }

    /// Explicitly acquire (or re-acquire) the lock — an operator designate. This
    /// is the ONLY transition out of `Lost`.
    pub fn lock(&mut self) {
        self.state = LockState::Locked;
        self.ever_usable_since_lock = false;
    }

    /// Release the lock (back to idle).
    pub fn unlock(&mut self) {
        self.state = LockState::Unlocked;
        self.ever_usable_since_lock = false;
    }

    /// Advance the gate for one cycle given whether the underlying stream is
    /// usable (fresh + link up). A `Locked` gate that HAS been usable and then
    /// is not drops to `Lost`; a `Locked` gate still acquiring (never usable yet)
    /// stays `Locked`. A `Lost` gate stays `Lost` even when the stream becomes
    /// usable again — it never auto-re-acquires.
    pub fn update(&mut self, stream_usable: bool) {
        if self.state != LockState::Locked {
            return;
        }
        if stream_usable {
            self.ever_usable_since_lock = true;
        } else if self.ever_usable_since_lock {
            self.state = LockState::Lost;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_gate_is_lost_until_a_result_arrives() {
        let mut g = FreshnessGate::new(500);
        assert_eq!(g.state(1000), GateState::Lost);
        g.record(1000);
        assert_eq!(g.state(1100), GateState::Fresh); // 100ms <= 500
    }

    #[test]
    fn a_result_past_the_budget_is_stale() {
        let mut g = FreshnessGate::new(500);
        g.record(1000);
        assert_eq!(g.state(1400), GateState::Fresh); // 400 <= 500
        assert_eq!(g.state(1501), GateState::Stale); // 501 > 500
        assert!(!g.is_usable(1501));
    }

    #[test]
    fn a_link_drop_is_lost_even_with_a_fresh_result() {
        let mut g = FreshnessGate::new(500);
        g.record(1000);
        g.set_link(false);
        assert_eq!(g.state(1050), GateState::Lost);
        g.set_link(true);
        assert_eq!(g.state(1050), GateState::Fresh);
    }

    #[test]
    fn a_frozen_stream_goes_stale_on_local_elapsed_not_the_result_timestamp() {
        // The safety regression: anchored on local arrival, a stream that stops
        // arriving goes Stale once the LOCAL clock passes the budget — a remote
        // clock skew or a frozen-but-socket-up node can no longer mask staleness.
        let mut g = FreshnessGate::new(500);
        g.record(1000); // arrived at local time 1000
        assert_eq!(g.state(1400), GateState::Fresh);
        // No new result; local time advances past the budget.
        assert_eq!(g.state(1501), GateState::Stale);
        assert_eq!(g.state(100_000), GateState::Stale); // stays stale forever
    }

    #[test]
    fn the_budget_boundary_is_inclusive() {
        let mut g = FreshnessGate::new(500);
        g.record(1000);
        assert_eq!(g.state(1500), GateState::Fresh); // age == budget is fresh
        assert_eq!(g.state(1501), GateState::Stale); // age == budget+1 is stale
    }

    #[test]
    fn a_negative_budget_is_clamped_not_permanently_stale() {
        let mut g = FreshnessGate::new(-100);
        g.record(1000);
        // Clamped to 0: an age-0 reading is still fresh (not always-stale).
        assert_eq!(g.state(1000), GateState::Fresh);
        assert_eq!(g.state(1001), GateState::Stale);
    }

    #[test]
    fn the_lock_gate_starts_unlocked() {
        assert_eq!(LockGate::new().state(), LockState::Unlocked);
    }

    #[test]
    fn a_locked_gate_stays_locked_while_the_stream_is_usable() {
        let mut g = LockGate::new();
        g.lock();
        g.update(true);
        assert_eq!(g.state(), LockState::Locked);
        assert!(g.is_locked());
    }

    #[test]
    fn a_lock_still_acquiring_stays_locked_not_lost() {
        // Designated but the stream has not produced a usable reading yet: this
        // is acquiring, not lost — the behaviour holds, it has nothing to lose.
        let mut g = LockGate::new();
        g.lock();
        g.update(false);
        g.update(false);
        assert_eq!(g.state(), LockState::Locked);
    }

    #[test]
    fn a_tracking_gate_drops_to_lost_when_the_stream_goes_stale() {
        let mut g = LockGate::new();
        g.lock();
        g.update(true); // acquired (a usable reading)
        g.update(false); // then stale / link-lost -> lost
        assert_eq!(g.state(), LockState::Lost);
        assert!(!g.is_locked());
    }

    #[test]
    fn a_lost_gate_never_auto_re_acquires() {
        // The single most important safety invariant.
        let mut g = LockGate::new();
        g.lock();
        g.update(true); // acquire
        g.update(false); // -> Lost
        assert_eq!(g.state(), LockState::Lost);
        // The stream becomes usable again; the gate must NOT re-lock by itself.
        g.update(true);
        g.update(true);
        assert_eq!(g.state(), LockState::Lost);
        // Only an explicit re-designate re-acquires.
        g.lock();
        assert_eq!(g.state(), LockState::Locked);
    }

    #[test]
    fn an_unlocked_gate_does_not_lock_itself_on_a_usable_stream() {
        let mut g = LockGate::new();
        g.update(true); // a usable stream must not auto-lock an idle gate
        assert_eq!(g.state(), LockState::Unlocked);
    }

    #[test]
    fn unlock_returns_to_idle() {
        let mut g = LockGate::new();
        g.lock();
        g.unlock();
        assert_eq!(g.state(), LockState::Unlocked);
    }
}
