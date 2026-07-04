//! Always-on runtime reconciler that keeps 802.11 power-save OFF on the onboard
//! WiFi station interfaces.
//!
//! The FullMAC WiFi drivers (Broadcom `brcmfmac` and friends) bring the station
//! interface up with power-save ENABLED, which parks the radio between beacons.
//! On an idle onboard link that silently drops unicast frames — broadcast ARP
//! still lands, but a unicast SSH / ping to the box times out — the classic "the
//! board falls off the LAN when it goes quiet" fault. Disabling power-save
//! reliably needs `iw dev <wlan> set power_save off`; a `NetworkManager`
//! `wifi.powersave` setting alone does not stick on these drivers.
//!
//! The install- and boot-time provisioning already sets power-save off once (an
//! NM drop-in + a udev rule + a boot oneshot). This reconciler is the missing
//! ALWAYS-ON runtime half: the driver re-enables power-save after an NM
//! reconnect, a WiFi hotplug, or a driver reload, so the supervisor re-asserts it
//! off from the monitor tick and records the verified per-interface state on a
//! sidecar for the heartbeat.
//!
//! Safety + cost:
//! - Read-mostly: one `iw ... get power_save` per station interface each tick; a
//!   `set` fires only on a real drift (measured on), and the readback is verified
//!   before the state is reported off.
//! - It only touches `wlan*` station interfaces (`iw dev <if> set power_save`),
//!   never a global regulatory or routing edge, so it cannot disturb the WFB
//!   radio or the operator's link topology.
//! - Default-ON, configurable under `network.wifi_powersave`. The pure config
//!   parsing + `iw` parsers are unit-tested on every host; the OS edges are
//!   Linux-only and the tick is an inert no-op on a non-Linux dev host.
//!
//! Module layout:
//! - `config`: `WifiPowersaveConfig` + its parsing.
//! - `parse`: the pure `iw` output parsers.
//! - `os`: the canonical-path config read, the per-interface reconcile body, the
//!   sidecar writer, and the `iw` shells.

use std::collections::HashMap;
#[cfg(any(target_os = "linux", test))]
use std::time::Duration;
use std::time::Instant;

use ados_protocol::logd::emitter::EventEmitter;

pub mod config;
pub mod os;
pub mod parse;

pub use config::{read_config_from, WifiPowersaveConfig};

/// The event kind recorded when the reconciler re-asserts power-save OFF on an
/// interface. Bland and reader-facing: it names what the code did, and mirrors
/// the naming of the other supervisor reconcilers so an RCA can query one family.
pub const WIFI_POWERSAVE_REASSERT_KIND: &str = "wifi.powersave_reasserted";

/// Persistent per-interface re-assert state, carried across ticks so the sidecar
/// reflects running totals. The measured power-save / link fields are read fresh
/// each tick and are NOT stored here.
#[derive(Debug, Clone, Default)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct IfaceReassertState {
    /// How many times this reconciler has flipped power-save on→off for the iface.
    pub count: u64,
    /// ISO-8601 UTC timestamp of the last real re-assert, or `None`.
    pub last_reassert: Option<String>,
}

/// The periodic WiFi power-save reconciler. Holds the last-attempt timestamp so
/// the reconcile is throttled to the configured interval regardless of how fast
/// the monitor pass runs, the construction instant so the fast-initial window is
/// measured against this process's uptime (a supervisor restart re-arms the fast
/// window), the per-interface re-assert counters, and the `events` shipper (only
/// driven on a real re-assert).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct WifiPowersaveReconciler {
    last_tick: Option<Instant>,
    /// When this reconciler was constructed (≈ process start). The fast-initial
    /// window is `now - started_at < cfg.fast_initial_window`.
    started_at: Instant,
    events: EventEmitter,
    /// Per-interface re-assert totals, persisted across ticks so the sidecar can
    /// report running counts + the last-reassert timestamp.
    reasserts: HashMap<String, IfaceReassertState>,
}

impl WifiPowersaveReconciler {
    /// Build a reconciler that records re-assert events through `events`.
    pub fn new(events: EventEmitter) -> Self {
        WifiPowersaveReconciler {
            last_tick: None,
            started_at: Instant::now(),
            events,
            reasserts: HashMap::new(),
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
    /// cadence), then re-assert power-save off on every station interface and
    /// mirror the sidecar. Re-reads config each tick so an edit takes effect
    /// without a restart. A no-op when disabled or not yet due.
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
        os::reconcile_wifi_powersave(&mut self.reasserts, &self.events).await;
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn tick(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn due_when_never_ticked_then_throttled() {
        let r = WifiPowersaveReconciler::new(EventEmitter::with_socket(
            "ados-test",
            "/nonexistent/ados/logd.sock",
        ));
        let now = Instant::now();
        // Never ticked → due.
        assert!(r.due(Duration::from_secs(30), now));
        // A recent tick throttles the next attempt inside the interval.
        let mut r2 = WifiPowersaveReconciler::new(EventEmitter::with_socket(
            "ados-test",
            "/nonexistent/ados/logd.sock",
        ));
        r2.last_tick = Some(now);
        assert!(!r2.due(Duration::from_secs(30), now + Duration::from_secs(10)));
        assert!(r2.due(Duration::from_secs(30), now + Duration::from_secs(31)));
    }

    #[tokio::test]
    async fn tick_is_inert_without_a_real_radio_environment() {
        // A first tick on a freshly-constructed reconciler is "due" (fast window
        // open, never ticked) but must complete without panic on any host: off
        // Linux it is a no-op; on Linux CI `iw` reads fall through safely. This
        // exercises the started_at / effective_interval wiring end to end.
        let mut r = WifiPowersaveReconciler::new(EventEmitter::with_socket(
            "ados-test",
            "/nonexistent/ados/logd.sock",
        ));
        r.tick().await;
    }
}
