//! The catalog of managed systemd service units and the per-service runtime
//! state used by the orchestration loop.
//!
//! The catalog is every [`ados_protocol::units::UNITS`] row that carries a
//! supervisor tier, in table order. A shipped unit with no tier (the
//! supervisor itself, boot oneshots, units pulled up by `WantedBy=`) is not
//! managed here, and neither is a unit a native daemon owns in-process: those
//! are retired and pruned by the installer. The supervisor never spawns these
//! processes itself; it issues `systemctl` against this catalog. systemd
//! remains the process manager and owns the cgroup, restart, and journald
//! wiring.

use std::collections::VecDeque;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use ados_protocol::units::{self, UnitSpec};

/// Service tier, from the unit table. Start order is core first, then
/// hardware, then on-demand.
pub use ados_protocol::units::Category;

/// Failures inside the window at which a service is labelled crash-looping.
pub const MAX_FAILURES: usize = 5;
/// Sliding window over which failures are counted for the crash-loop label.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// How often the monitor re-attempts a service parked in `Failed`/`CrashLooping`
/// so it self-recovers when the underlying condition (e.g. a hot-plugged
/// camera) returns, instead of staying dead until a manual restart. Fixed, in
/// the 2-5 s recovery band and matching the monitor tick. The crash-loop label
/// never stretches it: a drone whose FC needs one more attempt after a brownout
/// gets that attempt within 5 s however many came before.
pub const PARKED_RETRY_COOLDOWN: Duration = Duration::from_secs(5);

/// Runtime lifecycle state of a single managed unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
    Failed,
    /// Failed [`MAX_FAILURES`] times inside [`FAILURE_WINDOW`]. A label only:
    /// the unit is still retried on the fixed parked cadence.
    CrashLooping,
    /// The unit's main process exited 0 on purpose: the node has nothing for
    /// it to run. Not a failure, and not restarted.
    Idle,
}

impl ServiceState {
    /// The lowercase wire string used in the service-transition event detail.
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceState::Stopped => "stopped",
            ServiceState::Starting => "starting",
            ServiceState::Running => "running",
            ServiceState::Failed => "failed",
            ServiceState::CrashLooping => "crash_looping",
            ServiceState::Idle => "idle",
        }
    }
}

/// A catalog entry. `profile_gate` scopes the unit to a pipe-separated set of
/// agent profiles (`None` = any). `role_gate` is a pipe-separated set of
/// ground station roles the unit runs under (`None` = any role); only
/// consulted when the profile gate is `ground_station`.
#[derive(Debug, Clone, Copy)]
pub struct ServiceDef {
    /// The unit name without `.service` (`ados-video`).
    pub name: &'static str,
    pub category: Category,
    pub profile_gate: Option<&'static str>,
    pub role_gate: Option<&'static str>,
    /// True for the lean headless KEEP set: the minimal services a zero-Python
    /// flight node runs. When the agent is in headless mode, the gate blocks
    /// every service that is NOT in this set, so the box boots only the Rust
    /// core.
    pub headless_keep: bool,
    /// True for units whose `exit 0` means "this node has nothing for me to
    /// run" (the unit file carries `RestartPreventExitStatus=0`). Such an exit
    /// parks the row in [`ServiceState::Idle`] instead of counting a death.
    pub nothing_to_run_ok: bool,
}

/// The pipe-joined form of every multi-entry profile or role list in the
/// table, built once and owned by this static.
static JOINED_GATES: LazyLock<Vec<(&'static [&'static str], String)>> = LazyLock::new(|| {
    let mut out: Vec<(&'static [&'static str], String)> = Vec::new();
    for unit in units::UNITS {
        for list in [unit.profiles, unit.roles] {
            if list.len() > 1 && !out.iter().any(|(k, _)| *k == list) {
                out.push((list, list.join("|")));
            }
        }
    }
    out
});

/// A gate string for a table list: `None` when empty, else the pipe-joined
/// set.
fn gate(list: &'static [&'static str]) -> Option<&'static str> {
    match list {
        [] => None,
        [one] => Some(one),
        many => JOINED_GATES
            .iter()
            .find(|(k, _)| *k == many)
            .map(|(_, joined)| joined.as_str()),
    }
}

impl ServiceDef {
    fn from_unit(unit: &'static UnitSpec, category: Category) -> Self {
        ServiceDef {
            name: unit.stem(),
            category,
            profile_gate: gate(unit.profiles),
            role_gate: gate(unit.roles),
            headless_keep: unit.headless_keep,
            nothing_to_run_ok: unit.idle_ok,
        }
    }
}

/// Every service the supervisor knows about, in canonical order.
pub static SERVICE_REGISTRY: LazyLock<Vec<ServiceDef>> = LazyLock::new(|| {
    units::UNITS
        .iter()
        .filter_map(|u| u.category.map(|c| ServiceDef::from_unit(u, c)))
        .collect()
});

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
    /// Mirrors `ServiceDef::nothing_to_run_ok`.
    pub nothing_to_run_ok: bool,
    pub state: ServiceState,
    /// Failure timestamps inside the crash-loop window.
    pub failure_times: VecDeque<Instant>,
    /// Last monitor-driven retry of a parked service (cooldown bound).
    pub last_retry_at: Option<Instant>,
    /// Set when the adoption sweep promoted this row: the unit was brought up
    /// by its own enablement (the installer enables it, `WantedBy=` the
    /// supervisor or `multi-user.target`), not by this process. Sticky for the
    /// life of the process, because a death-restart by the monitor does not
    /// change what brings the unit up. Shutdown leaves an adopted unit to the
    /// service manager (see `Supervisor::stop`).
    pub adopted: bool,
}

impl ServiceSpec {
    pub fn from_def(d: &ServiceDef) -> Self {
        ServiceSpec {
            name: d.name,
            category: d.category,
            profile_gate: d.profile_gate,
            role_gate: d.role_gate,
            headless_keep: d.headless_keep,
            nothing_to_run_ok: d.nothing_to_run_ok,
            state: ServiceState::Stopped,
            failure_times: VecDeque::new(),
            last_retry_at: None,
            adopted: false,
        }
    }

    /// Record a failure and label the unit crash-looping when the window
    /// threshold is hit. Returns true when it is now crash-looping. The label
    /// never blocks a start; the parked retry keeps its fixed cadence.
    pub fn record_failure(&mut self, now: Instant) -> bool {
        self.failure_times.push_back(now);
        self.prune_failures(now);
        if self.failure_times.len() >= MAX_FAILURES {
            self.state = ServiceState::CrashLooping;
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
        // ados-discovery and ados-health were removed; ados-vision-hailo added.
        assert_eq!(specs.len(), 28, "service count drifted from the catalog");
        // Core tier members. ados-mavlink/api/cloud/health/logd are the
        // cross-profile always-on core (the single cloud unit serves the gateway
        // + heartbeat on both profiles, spawning the ground-station bridge when
        // the role resolves to a ground station).
        let core: Vec<_> = specs
            .iter()
            .filter(|s| s.category == Category::Core)
            .map(|s| s.name)
            .collect();
        assert_eq!(
            core,
            vec![
                "ados-mavlink",
                "ados-swarmbus",
                "ados-api",
                "ados-cloud",
                "ados-logd"
            ]
        );
        // The drone TX manager is drone-gated; the GS RX is role-gated to direct.
        let wfb = specs.iter().find(|s| s.name == "ados-wfb").unwrap();
        assert_eq!(wfb.profile_gate, Some("drone"));
        let rx = specs.iter().find(|s| s.name == "ados-wfb-rx").unwrap();
        assert_eq!(rx.profile_gate, Some("ground_station"));
        assert_eq!(rx.role_gate, Some("direct"));
        // The units ados-net owns in-process carry no row at all: a packaged
        // unit that every profile tears down must not be something the
        // supervisor reports on or tries to reconcile.
        for absent in ["ados-modem", "ados-wifi-client", "ados-ethernet"] {
            assert!(
                !specs.iter().any(|s| s.name == absent),
                "{absent} is torn down on every profile and must not be registered"
            );
        }
        // The vision engine is a drone-gated Hardware unit and is NOT in the
        // headless keep set (vision/AI is excluded from the lean core).
        let vis = specs.iter().find(|s| s.name == "ados-vision").unwrap();
        assert_eq!(vis.profile_gate, Some("drone"));
        assert_eq!(vis.category, Category::Hardware);
        assert!(!vis.headless_keep);
        // The CRSF RC lane is a Hardware unit on both FC-bearing profiles
        // (ground-side transmitter today, drone-side for the relay last mile),
        // never in the headless keep set. Its presence here is what arms the
        // Elrs hot-plug class routing to it.
        let crsf = specs.iter().find(|s| s.name == "ados-crsf").unwrap();
        assert_eq!(crsf.profile_gate, Some("drone|ground_station"));
        assert_eq!(crsf.category, Category::Hardware);
        assert!(!crsf.headless_keep);
        // The config-over-radio channel is a cross-profile Hardware unit
        // (drone terminator + ground injector), never in the headless keep set.
        let tunnel = specs
            .iter()
            .find(|s| s.name == "ados-tunnel-config")
            .unwrap();
        assert_eq!(tunnel.profile_gate, Some("drone|ground_station"));
        assert_eq!(tunnel.category, Category::Hardware);
        assert!(!tunnel.headless_keep);
    }

    #[test]
    fn registry_has_hailo_and_no_deleted_units() {
        let specs = build_specs();
        let hailo = specs
            .iter()
            .find(|s| s.name == "ados-vision-hailo")
            .expect("hailo sidecar registered");
        assert_eq!(hailo.category, Category::Hardware);
        assert_eq!(hailo.profile_gate, Some("drone"));
        assert_eq!(hailo.role_gate, None);
        assert!(!hailo.headless_keep);
        for gone in ["ados-discovery", "ados-health", "ados-wifi-client"] {
            assert!(
                !specs.iter().any(|s| s.name == gone),
                "{gone} was removed and must not be registered"
            );
        }
    }

    #[test]
    fn headless_keep_set_is_exactly_the_lean_core() {
        // The lean headless profile boots only the Rust core: the MAVLink
        // router, the swarm state bus, the durable store, the camera encode, the
        // radio TX, and the native HTTP front. Everything else (FastAPI, cloud,
        // health, GS units, on-demand) is NOT in the KEEP set, so the headless
        // gate blocks it. The store is in the set because it is Rust (zero-Python
        // holds) and a lean flight node is the node an operator cannot reach —
        // the one that most needs its own flight recorder.
        let specs = build_specs();
        let kept: Vec<_> = specs
            .iter()
            .filter(|s| s.headless_keep)
            .map(|s| s.name)
            .collect();
        assert_eq!(
            kept,
            vec![
                "ados-mavlink",
                "ados-swarmbus",
                "ados-logd",
                "ados-video",
                "ados-wfb",
                "ados-control"
            ],
            "headless KEEP set drifted"
        );
        // ados-api (FastAPI) is explicitly NOT kept: headless is zero-Python.
        let api = specs.iter().find(|s| s.name == "ados-api").unwrap();
        assert!(!api.headless_keep);
    }

    #[test]
    fn the_log_store_is_supervised_cross_profile_and_kept_headless() {
        // Regression: an unregistered ados-logd got no liveness check, no
        // auto-restart, no parked retry and — the one that matters — no
        // `systemctl reset-failed`, so five restarts in a minute left the black
        // box permanently dead and only an SSH session could clear it.
        let specs = build_specs();
        let logd = specs
            .iter()
            .find(|s| s.name == "ados-logd")
            .expect("ados-logd must be in the supervised set");
        assert_eq!(logd.category, Category::Core);
        assert_eq!(logd.profile_gate, None, "the store runs on every profile");
        assert_eq!(logd.role_gate, None);
        assert!(
            logd.headless_keep,
            "a lean flight node is the node that most needs its own recorder"
        );
    }

    #[test]
    fn every_catalog_unit_is_part_of_the_supervisor() {
        // The supervisor's shutdown leaves every adopted row to systemd, which is
        // sound only because `PartOf=` carries the supervisor's own stop and
        // restart to the unit. A unit without it would keep running after
        // `systemctl stop ados-supervisor` and would not be cycled by a restart.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
        for def in SERVICE_REGISTRY.iter() {
            let path = dir.join(format!("{}.service", def.name));
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let part_of = body
                .lines()
                .filter_map(|l| l.trim().strip_prefix("PartOf="))
                .flat_map(str::split_whitespace)
                .any(|u| u == "ados-supervisor.service");
            assert!(
                part_of,
                "{} is not PartOf=ados-supervisor.service",
                def.name
            );
        }
    }

    #[test]
    fn the_parked_retry_cadence_is_in_the_fixed_recovery_band() {
        // A parked service is retried inside the fixed 2-5 s recovery band, not
        // on a half-minute cooldown: a unit that needs one more attempt after a
        // brownout must not cost the operator 30 s per try.
        assert!(PARKED_RETRY_COOLDOWN >= Duration::from_secs(2));
        assert!(PARKED_RETRY_COOLDOWN <= Duration::from_secs(5));
    }

    #[test]
    fn mavlink_is_core_on_the_fc_profiles_only() {
        // The MAVLink router is the sole C2 path with no packaged fallback, so
        // it is Core (always started) on the FC-bearing profiles. It is gated to
        // `drone|ground_station`, NOT the FC-less compute node (which never
        // fetches the router binary and would crash-loop on an unconditional
        // start). No role gate.
        let specs = build_specs();
        let mav = specs
            .iter()
            .find(|s| s.name == "ados-mavlink")
            .expect("ados-mavlink must be in the registry");
        assert_eq!(mav.category, Category::Core);
        assert_eq!(mav.profile_gate, Some("drone|ground_station"));
        assert_eq!(mav.role_gate, None);
    }

    #[test]
    fn the_swarm_bus_is_core_on_both_fc_profiles_and_kept_headless() {
        // Gated exactly like ados-mavlink, and for the same reason: the bus is that
        // seam seen from either end. A drone fills its beacon from the router's
        // state socket; a ground station listens so the operator's fleet view is
        // local-first. Gating it to `drone` alone would make the fleet view depend
        // on a cloud round-trip, and gating it wider would start it on an FC-less
        // compute node that never fetches the binary.
        let specs = build_specs();
        let bus = specs
            .iter()
            .find(|s| s.name == "ados-swarmbus")
            .expect("ados-swarmbus must be in the registry");
        assert_eq!(bus.category, Category::Core);
        assert_eq!(bus.profile_gate, Some("drone|ground_station"));
        assert_eq!(bus.role_gate, None);
        // Onboard collision avoidance reads this table, so a lean headless flight
        // node that keeps the router must keep the bus that feeds separation.
        assert!(bus.headless_keep);
        let mav = specs.iter().find(|s| s.name == "ados-mavlink").unwrap();
        assert_eq!(
            bus.profile_gate, mav.profile_gate,
            "same gate as the router"
        );
    }

    #[test]
    fn crash_loop_label_applies_after_threshold_in_window() {
        let mut spec = ServiceSpec::from_def(&SERVICE_REGISTRY[0]);
        let t0 = Instant::now();
        for i in 0..(MAX_FAILURES - 1) {
            assert!(!spec.record_failure(t0 + Duration::from_millis(i as u64)));
        }
        assert!(spec.record_failure(t0 + Duration::from_millis(MAX_FAILURES as u64)));
        assert_eq!(spec.state, ServiceState::CrashLooping);
        assert_eq!(spec.state.as_str(), "crash_looping");
    }

    #[test]
    fn failures_outside_the_window_do_not_count_toward_a_crash_loop() {
        let mut spec = ServiceSpec::from_def(&SERVICE_REGISTRY[0]);
        let t0 = Instant::now();
        for _ in 0..(MAX_FAILURES - 1) {
            spec.record_failure(t0);
        }
        let later = t0 + FAILURE_WINDOW + Duration::from_secs(1);
        assert!(!spec.record_failure(later));
        assert_eq!(spec.failure_times.len(), 1);
    }
}
