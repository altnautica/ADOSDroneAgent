//! The catalog of managed systemd service units and the per-service runtime
//! state used by the orchestration loop.
//!
//! Mirrors the canonical Python registry one-for-one (name, category, profile
//! gate, role gate). The supervisor never spawns these processes itself; it
//! issues `systemctl` against this catalog. systemd remains the process
//! manager and owns the cgroup, restart, and journald wiring.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Stop restarting a service after this many failures inside the window.
pub const MAX_FAILURES: usize = 5;
/// Sliding window over which failures are counted for the circuit breaker.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// How often the monitor re-attempts a service parked in `Failed`/`CircuitOpen`
/// so it self-recovers when the underlying condition (e.g. a hot-plugged
/// camera) returns, instead of staying dead until a manual restart.
pub const PARKED_RETRY_COOLDOWN: Duration = Duration::from_secs(30);

/// Service tier. Start order is core first, then hardware, then on-demand;
/// shutdown tears down in the reverse-ish order handled by the lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Core,
    Hardware,
    OnDemand,
}

/// Runtime lifecycle state of a single managed unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
    Failed,
    CircuitOpen,
}

/// A static registry entry. `profile_gate` scopes the unit to one agent
/// profile (`None` = any). `role_gate` is a pipe-separated set of ground
/// station roles the unit runs under (`None` = any role); only consulted when
/// the profile gate is `ground_station`.
#[derive(Debug, Clone, Copy)]
pub struct ServiceDef {
    pub name: &'static str,
    pub category: Category,
    pub profile_gate: Option<&'static str>,
    pub role_gate: Option<&'static str>,
}

const fn def(
    name: &'static str,
    category: Category,
    profile_gate: Option<&'static str>,
    role_gate: Option<&'static str>,
) -> ServiceDef {
    ServiceDef {
        name,
        category,
        profile_gate,
        role_gate,
    }
}

use Category::{Core, Hardware, OnDemand};

/// Every service the supervisor knows about, in canonical order.
pub const SERVICE_REGISTRY: &[ServiceDef] = &[
    // Core (always running).
    def("ados-mavlink", Core, None, None),
    def("ados-api", Core, None, None),
    def("ados-cloud", Core, None, None),
    def("ados-health", Core, None, None),
    // Hardware-dependent (started on detection).
    def("ados-video", Hardware, None, None),
    // Drone-side WFB-ng TX manager. Profile-gated to drone so a ground station
    // does not bring up wfb_tx and fight the GS wfb_rx for the same adapter.
    def("ados-wfb", Hardware, Some("drone"), None),
    // Scripting tier, on-demand by default.
    def("ados-scripting", OnDemand, None, None),
    // On-demand.
    def("ados-ota", OnDemand, None, None),
    def("ados-discovery", OnDemand, None, None),
    // Peripheral Manager registry. Cross-profile.
    def("ados-peripherals", Hardware, None, None),
    // Ground-station-only services. ados-wfb-rx is the single-node RX path,
    // gated to the direct role so it does not grab the adapter the relay or
    // receiver units drive.
    def(
        "ados-wfb-rx",
        Hardware,
        Some("ground_station"),
        Some("direct"),
    ),
    def("ados-mediamtx-gs", Hardware, Some("ground_station"), None),
    def("ados-usb-gadget", Hardware, Some("ground_station"), None),
    // Physical UI + AP + first-boot captive portal.
    def("ados-oled", Hardware, Some("ground_station"), None),
    def("ados-buttons", Hardware, Some("ground_station"), None),
    def("ados-hostapd", Hardware, Some("ground_station"), None),
    def("ados-dnsmasq-gs", Hardware, Some("ground_station"), None),
    def("ados-setup-captive", OnDemand, Some("ground_station"), None),
    // Standalone flight stack.
    def("ados-kiosk", Hardware, Some("ground_station"), None),
    def("ados-input", Hardware, Some("ground_station"), None),
    def("ados-pic", Hardware, Some("ground_station"), None),
    // Uplink matrix and cloud relay.
    def("ados-uplink-router", Hardware, Some("ground_station"), None),
    def("ados-modem", Hardware, Some("ground_station"), None),
    // WiFi client is profile-agnostic on purpose: a drone-profile rig can also
    // join a home / bench WiFi instead of running off Ethernet.
    def("ados-wifi-client", Hardware, None, None),
    def("ados-ethernet", Hardware, Some("ground_station"), None),
    def("ados-cloud-relay", Core, Some("ground_station"), None),
    // Distributed-receive role-gated services.
    def(
        "ados-batman",
        Hardware,
        Some("ground_station"),
        Some("relay|receiver"),
    ),
    def(
        "ados-wfb-relay",
        Hardware,
        Some("ground_station"),
        Some("relay"),
    ),
    def(
        "ados-wfb-receiver",
        Hardware,
        Some("ground_station"),
        Some("receiver"),
    ),
];

/// Per-service mutable runtime state held by the supervisor.
#[derive(Debug)]
pub struct ServiceSpec {
    pub name: &'static str,
    pub category: Category,
    pub profile_gate: Option<&'static str>,
    pub role_gate: Option<&'static str>,
    pub state: ServiceState,
    /// Failure timestamps inside the circuit-breaker window.
    pub failure_times: VecDeque<Instant>,
    /// Last monitor-driven retry of a parked service (cooldown bound).
    pub last_retry_at: Option<Instant>,
}

impl ServiceSpec {
    pub fn from_def(d: &ServiceDef) -> Self {
        ServiceSpec {
            name: d.name,
            category: d.category,
            profile_gate: d.profile_gate,
            role_gate: d.role_gate,
            state: ServiceState::Stopped,
            failure_times: VecDeque::new(),
            last_retry_at: None,
        }
    }

    /// Record a failure and open the breaker if the window threshold is hit.
    /// Returns true if the breaker is now open.
    pub fn record_failure(&mut self, now: Instant) -> bool {
        self.failure_times.push_back(now);
        self.prune_failures(now);
        if self.failure_times.len() >= MAX_FAILURES {
            self.state = ServiceState::CircuitOpen;
            true
        } else {
            false
        }
    }

    /// Drop failure timestamps older than the window.
    pub fn prune_failures(&mut self, now: Instant) {
        while let Some(&front) = self.failure_times.front() {
            if now.duration_since(front) >= FAILURE_WINDOW {
                self.failure_times.pop_front();
            } else {
                break;
            }
        }
    }

    /// Whether the breaker should still block a start attempt. Half-opens once
    /// the recent-failure count falls back under the threshold.
    pub fn breaker_blocks(&mut self, now: Instant) -> bool {
        self.prune_failures(now);
        self.state == ServiceState::CircuitOpen && self.failure_times.len() >= MAX_FAILURES
    }
}

/// Build the ordered runtime spec list from the static registry.
pub fn build_specs() -> Vec<ServiceSpec> {
    SERVICE_REGISTRY.iter().map(ServiceSpec::from_def).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_expected_shape() {
        let specs = build_specs();
        assert_eq!(specs.len(), 30, "service count drifted from the catalog");
        // Core tier members.
        let core: Vec<_> = specs
            .iter()
            .filter(|s| s.category == Category::Core)
            .map(|s| s.name)
            .collect();
        assert_eq!(
            core,
            vec![
                "ados-mavlink",
                "ados-api",
                "ados-cloud",
                "ados-health",
                "ados-cloud-relay"
            ]
        );
        // The drone TX manager is drone-gated; the GS RX is role-gated to direct.
        let wfb = specs.iter().find(|s| s.name == "ados-wfb").unwrap();
        assert_eq!(wfb.profile_gate, Some("drone"));
        let rx = specs.iter().find(|s| s.name == "ados-wfb-rx").unwrap();
        assert_eq!(rx.profile_gate, Some("ground_station"));
        assert_eq!(rx.role_gate, Some("direct"));
        // wifi-client stays cross-profile.
        let wc = specs.iter().find(|s| s.name == "ados-wifi-client").unwrap();
        assert_eq!(wc.profile_gate, None);
    }

    #[test]
    fn circuit_breaker_opens_after_threshold_in_window() {
        let mut spec = ServiceSpec::from_def(&SERVICE_REGISTRY[0]);
        let t0 = Instant::now();
        for i in 0..(MAX_FAILURES - 1) {
            assert!(!spec.record_failure(t0 + Duration::from_millis(i as u64)));
        }
        assert!(spec.record_failure(t0 + Duration::from_millis(MAX_FAILURES as u64)));
        assert_eq!(spec.state, ServiceState::CircuitOpen);
        assert!(spec.breaker_blocks(t0 + Duration::from_millis(MAX_FAILURES as u64 + 1)));
    }

    #[test]
    fn circuit_breaker_half_opens_after_window() {
        let mut spec = ServiceSpec::from_def(&SERVICE_REGISTRY[0]);
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES {
            spec.record_failure(t0);
        }
        assert_eq!(spec.state, ServiceState::CircuitOpen);
        // After the window the stale failures prune and the breaker stops blocking.
        let later = t0 + FAILURE_WINDOW + Duration::from_secs(1);
        assert!(!spec.breaker_blocks(later));
    }
}
