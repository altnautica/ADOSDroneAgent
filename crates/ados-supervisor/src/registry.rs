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

impl ServiceState {
    /// The lowercase wire string used in the service-transition event detail.
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceState::Stopped => "stopped",
            ServiceState::Starting => "starting",
            ServiceState::Running => "running",
            ServiceState::Failed => "failed",
            ServiceState::CircuitOpen => "circuit_open",
        }
    }
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
    /// True for the lean headless KEEP set: the minimal services a zero-Python
    /// flight node runs (the MAVLink router, the camera encode, the radio TX,
    /// and the native HTTP front). When the agent is in headless mode, the gate
    /// blocks every service that is NOT in this set, so the box boots only the
    /// Rust core. `false` for all other services (the default via `def`); the
    /// KEEP set is marked with `def_keep`.
    pub headless_keep: bool,
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
        headless_keep: false,
    }
}

/// A registry entry in the lean headless KEEP set (`headless_keep = true`): the
/// minimal services a zero-Python flight node keeps running. Identical to `def`
/// in every other respect.
const fn def_keep(
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
        headless_keep: true,
    }
}

use Category::{Core, Hardware, OnDemand};

/// Every service the supervisor knows about, in canonical order.
pub const SERVICE_REGISTRY: &[ServiceDef] = &[
    // Core (always running). ados-mavlink is the sole command-and-control path
    // to the flight controller: it execs the native router binary and has no
    // packaged fallback, so it must always be started (Core) on every profile
    // (no profile gate). A missing router binary makes this unit crash-loop, so
    // the installer Hard-gates the binary's fetch and re-checks its presence in
    // the health gate before reporting the install OK.
    def_keep("ados-mavlink", Core, None, None),
    def("ados-api", Core, None, None),
    def("ados-cloud", Core, None, None),
    def("ados-health", Core, None, None),
    // Hardware-dependent (started on detection). Drone-only: the camera encode
    // pipeline runs on the air side; a ground station receives video through
    // ados-mediamtx-gs, never ados-video. The prebuilt catalog fetches the
    // ados-video binary on the drone profile only, so leaving this ungated made
    // a ground station start a unit whose binary is (correctly) absent — an
    // endless restart loop. Gate it to drone to match the catalog.
    def_keep("ados-video", Hardware, Some("drone"), None),
    // NPU inference sidecar (RKNN). Drone-only: the camera + vision pipeline
    // runs air-side, so the detector sidecar follows the camera. Python (it
    // owns the proprietary rknn-toolkit-lite2 wheel the Rust engine cannot
    // link), so it is NOT in the headless KEEP set. The unit self-gates on the
    // NPU runtime library, so it is a clean no-op on a board without the NPU.
    def("ados-vision-rknn", Hardware, Some("drone"), None),
    // Drone-side WFB-ng TX manager. Profile-gated to drone so a ground station
    // does not bring up wfb_tx and fight the GS wfb_rx for the same adapter.
    def_keep("ados-wfb", Hardware, Some("drone"), None),
    // The onboard vision engine: loads the configured detector and publishes the
    // detection stream that follow / designate plugins consume. Drone-only (the
    // air side owns the cameras) to match the prebuilt catalog, which fetches the
    // ados-vision binary on the drone profile only. The engine self-gates on
    // `vision.enabled` (it exits cleanly when vision is off, the default), so the
    // unit is a clean no-op until provisioning enables vision + a detector. NOT
    // in the headless KEEP set: vision/AI is excluded from the lean headless
    // core. On an NPU board the engine reaches its model through the
    // ados-vision-rknn sidecar.
    def("ados-vision", Hardware, Some("drone"), None),
    // The world-model capture service: subscribes the vision frame ring, selects
    // pose-tagged keyframes, and publishes the keyframe + pose + capture-state
    // streams a compute node reconstructs from. Drone-only (the air side owns the
    // cameras) to match the prebuilt catalog. It self-gates on `atlas.enabled`
    // (it exits cleanly when atlas is off, the default), so the unit is a clean
    // no-op until provisioning enables it. NOT in the headless KEEP set:
    // world-model capture is excluded from the lean headless core.
    def("ados-atlas", Hardware, Some("drone"), None),
    // The compute-node engine: the job store, the scheduler, and the REST job
    // API. Profile-gated to `compute` so it runs only on a compute node (a GPU
    // box, a Mac, or any spare box), never on a drone or ground station. It
    // serves the local-first job API a drone or GCS submits reconstruction and
    // offload work to. NOT in the headless KEEP set: compute is a heavy profile,
    // not the lean headless core.
    def("ados-compute", Core, Some("compute"), None),
    // On-demand.
    def("ados-ota", OnDemand, None, None),
    def("ados-discovery", OnDemand, None, None),
    // The native HTTP control surface. Cross-profile and on-demand: it ships
    // disabled (the GCS uses the FastAPI surface) and only runs when the operator
    // enables it, so the supervisor never auto-starts it on boot. In the KEEP set
    // because the lean headless profile binds it on `:8080` in place of FastAPI;
    // the headless gate must permit it while blocking the rest.
    def_keep("ados-control", OnDemand, None, None),
    // Peripheral Manager registry. Cross-profile.
    def("ados-peripherals", Hardware, None, None),
    // GPIO-output substrate (status buzzer / LED). Cross-profile (a header GPIO
    // can drive an indicator on either an air or a ground node) and not in the
    // headless KEEP set. The unit ships disabled until the operator turns it on,
    // so on a board with no GPIO header it is a clean no-op.
    def("ados-gpio", Hardware, None, None),
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
    /// Mirrors `ServiceDef::headless_keep`: whether this unit is in the lean
    /// headless KEEP set the gate permits when the agent runs headless.
    pub headless_keep: bool,
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
            headless_keep: d.headless_keep,
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
        assert_eq!(specs.len(), 33, "service count drifted from the catalog");
        // Core tier members. ados-mavlink/api/cloud/health are the cross-profile
        // always-on core (the single cloud unit serves the gateway + heartbeat on
        // both profiles, spawning the ground-station bridge when the role resolves
        // to a ground station). ados-compute is the Core service of the compute
        // profile (it auto-runs only on a compute node).
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
                "ados-compute"
            ]
        );
        // The compute engine is the Core service of the compute profile and is
        // NOT in the headless keep set (compute is a heavy profile).
        let compute = specs.iter().find(|s| s.name == "ados-compute").unwrap();
        assert_eq!(compute.profile_gate, Some("compute"));
        assert_eq!(compute.category, Category::Core);
        assert!(!compute.headless_keep);
        // The drone TX manager is drone-gated; the GS RX is role-gated to direct.
        let wfb = specs.iter().find(|s| s.name == "ados-wfb").unwrap();
        assert_eq!(wfb.profile_gate, Some("drone"));
        let rx = specs.iter().find(|s| s.name == "ados-wfb-rx").unwrap();
        assert_eq!(rx.profile_gate, Some("ground_station"));
        assert_eq!(rx.role_gate, Some("direct"));
        // wifi-client stays cross-profile.
        let wc = specs.iter().find(|s| s.name == "ados-wifi-client").unwrap();
        assert_eq!(wc.profile_gate, None);
        // The vision engine is a drone-gated Hardware unit and is NOT in the
        // headless keep set (vision/AI is excluded from the lean core).
        let vis = specs.iter().find(|s| s.name == "ados-vision").unwrap();
        assert_eq!(vis.profile_gate, Some("drone"));
        assert_eq!(vis.category, Category::Hardware);
        assert!(!vis.headless_keep);
        // The world-model capture service is a drone-gated Hardware unit and is
        // NOT in the headless keep set (world-model capture is excluded from the
        // lean core, like vision).
        let atlas = specs.iter().find(|s| s.name == "ados-atlas").unwrap();
        assert_eq!(atlas.profile_gate, Some("drone"));
        assert_eq!(atlas.category, Category::Hardware);
        assert!(!atlas.headless_keep);
    }

    #[test]
    fn headless_keep_set_is_exactly_the_lean_core() {
        // The lean headless profile boots only the Rust core: the MAVLink
        // router, the camera encode, the radio TX, and the native HTTP front.
        // Everything else (FastAPI, cloud, health, GS units, on-demand) is NOT
        // in the KEEP set, so the headless gate blocks it.
        let specs = build_specs();
        let kept: Vec<_> = specs
            .iter()
            .filter(|s| s.headless_keep)
            .map(|s| s.name)
            .collect();
        assert_eq!(
            kept,
            vec!["ados-mavlink", "ados-video", "ados-wfb", "ados-control"],
            "headless KEEP set drifted"
        );
        // ados-api (FastAPI) is explicitly NOT kept: headless is zero-Python.
        let api = specs.iter().find(|s| s.name == "ados-api").unwrap();
        assert!(!api.headless_keep);
    }

    #[test]
    fn mavlink_is_core_and_ungated_on_every_profile() {
        // The MAVLink router is the sole C2 path with no packaged fallback, so
        // it must always be started: Core tier, no profile gate, no role gate.
        let specs = build_specs();
        let mav = specs
            .iter()
            .find(|s| s.name == "ados-mavlink")
            .expect("ados-mavlink must be in the registry");
        assert_eq!(mav.category, Category::Core);
        assert_eq!(mav.profile_gate, None);
        assert_eq!(mav.role_gate, None);
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
