//! Periodic regulatory-domain reconciler for the supervisor monitor pass.
//!
//! A self-managed-regulatory USB injection PHY (the RTL family) asserts its
//! EEPROM-baked country (e.g. `BO`) as the GLOBAL cfg80211 regulatory domain
//! when it loads and enters / re-enters monitor mode. A normal onboard FullMAC
//! adapter (the Pi-family Broadcom, the Rock-family AIC8800) obeys that global
//! domain. When the baked country is one whose rules the onboard driver cannot
//! satisfy on its associated channel, the onboard WiFi keeps its association and
//! IP but loses its data path entirely (the gateway never resolves, 100% loss),
//! so the management link dies with no failover.
//!
//! The radio service re-asserts the configured wanted domain right after its
//! monitor-mode bring-up (the prevention layer). This supervisor reconciler is
//! the symmetric, always-running half: it runs on BOTH profiles from the monitor
//! tick (the same place as the reactive WiFi self-heal) and catches any LATER
//! drift — a bind re-entry, a monitor re-init, or a profile/role change that
//! re-churns the injection PHY long after the radio's one-shot reconcile. When
//! the live global domain drifts off the configured wanted value, it re-asserts
//! the wanted domain so the onboard WiFi is never left under a foreign domain.
//!
//! Safety invariants (it can never cap the WFB radio):
//! - It only ever forces a domain that PERMITS the configured rendezvous
//!   channel. It reads the injection interface's enabled channel set (`iw phy
//!   channels`, which already excludes DFS / disabled / no-IR) and re-asserts
//!   only when the rendezvous channel is in that set (or the set is unknown,
//!   matching the bring-up gate's "empty = do not restrict").
//! - It never forces the all-restrictive world default (`00`) or a malformed
//!   domain.
//! - It is idempotent: a no-op when the live domain already equals the wanted
//!   value (the cheap steady-state path, one `iw reg get` + a compare).
//! - It NEVER touches an interface — `iw reg set` is a global per-phy call — so
//!   it cannot disturb the operator's management link directly. The onboard
//!   WiFi recovers because it re-reads the now-sane global domain; the reactive
//!   self-heal remains the backstop for a link that needs an explicit rebuild.
//!
//! Default-ON, configurable under `network.reg_reconciler`. The pure decision
//! logic and config parsing are unit-tested on every host; the OS edges (iw)
//! are Linux-only and the tick is an inert no-op on a non-Linux dev host.
//!
//! Module layout:
//! - `config`: `RegReconcilerConfig` + `WantedReg` and their parsing.
//! - `policy`: the pure `ReconcileDecision` + `reconcile_decision` + the
//!   forceable-domain predicate.
//! - `parse`: the `iw` output parsers.
//! - `os`: the canonical-path config reads, the channel-safety-gated reconcile
//!   body (`reconcile_global_domain`), and the `iw` shells.

#[cfg(any(target_os = "linux", test))]
use std::time::Duration;
use std::time::Instant;

use ados_protocol::logd::emitter::EventEmitter;

pub mod config;
pub mod os;
pub mod parse;
pub mod policy;

pub use config::{read_config_from, read_wanted_from, RegReconcilerConfig, WantedReg};
pub use os::reconcile_global_domain;
pub use policy::{is_forceable_domain, reconcile_decision, ReconcileDecision};

/// The event kind recorded when the reconciler re-asserts the global domain.
/// Bland and reader-facing: it names what the code did. Mirrors the radio-side
/// event kind so an RCA queries one classifier across both halves.
pub const REG_REASSERT_KIND: &str = "radio.reg_reasserted";

/// The periodic regulatory reconciler. Holds the last-attempt timestamp so the
/// reconcile is throttled to the configured interval regardless of how fast the
/// monitor pass runs, plus the construction instant so the fast-initial window
/// is measured against this process's uptime (a supervisor restart re-arms the
/// fast window). The `events` shipper is only driven on a real re-assert.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct RegReconciler {
    last_tick: Option<Instant>,
    /// When this reconciler was constructed (≈ process start). The fast-initial
    /// window is `now - started_at < cfg.fast_initial_window`.
    started_at: Instant,
    events: EventEmitter,
}

impl RegReconciler {
    /// Build a reconciler that records re-assert events through `events`.
    pub fn new(events: EventEmitter) -> Self {
        RegReconciler {
            last_tick: None,
            started_at: Instant::now(),
            events,
        }
    }

    /// Whether the reconcile is due given the configured interval and the last
    /// attempt time. Pure so the throttle is testable without a real clock.
    #[cfg(any(target_os = "linux", test))]
    fn due(&self, interval: Duration, now: Instant) -> bool {
        match self.last_tick {
            None => true,
            Some(last) => now.duration_since(last) >= interval,
        }
    }

    /// One reconcile tick: throttle to the effective interval (the faster
    /// fast-initial cadence while uptime is inside the window, else the steady
    /// cadence), then run the channel-safety-gated reconcile. Re-reads config
    /// each tick so an edit takes effect without a restart. A no-op when
    /// disabled, when not due, when `iw` is absent, or when the domain is already
    /// in sync.
    #[cfg(target_os = "linux")]
    pub async fn tick(&mut self) {
        let cfg = os::read_config();
        if !cfg.enabled {
            return;
        }
        let now = Instant::now();
        let interval = cfg.effective_interval(now.duration_since(self.started_at));
        if !self.due(interval, now) {
            return;
        }
        self.last_tick = Some(now);
        os::reconcile_global_domain(&self.events).await;
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn tick(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn due_when_never_ticked_then_throttled() {
        let r = RegReconciler::new(EventEmitter::with_socket(
            "ados-test",
            "/nonexistent/ados/logd.sock",
        ));
        let now = Instant::now();
        // Never ticked → due.
        assert!(r.due(Duration::from_secs(30), now));
        // Simulate a recent tick by constructing one with last_tick set.
        let mut r2 = RegReconciler::new(EventEmitter::with_socket(
            "ados-test",
            "/nonexistent/ados/logd.sock",
        ));
        r2.last_tick = Some(now);
        // Not yet due inside the interval.
        assert!(!r2.due(Duration::from_secs(30), now + Duration::from_secs(10)));
        // Due once the interval elapses.
        assert!(r2.due(Duration::from_secs(30), now + Duration::from_secs(31)));
    }

    #[tokio::test]
    async fn tick_is_inert_without_a_real_radio_environment() {
        // A first tick on a freshly-constructed reconciler is "due" (fast window
        // open, never ticked) but must complete without panic on any host: off
        // Linux it is a no-op; on Linux CI `iw` is read-only and the wanted
        // domain resolves to the safe default. This exercises the started_at /
        // effective_interval wiring end to end.
        let mut r = RegReconciler::new(EventEmitter::with_socket(
            "ados-test",
            "/nonexistent/ados/logd.sock",
        ));
        r.tick().await;
    }
}
