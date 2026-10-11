use super::*;
use crate::registry::SERVICE_REGISTRY;
use std::io::Write;
use std::path::Path;

/// Build a config whose role gate reads from `role_path`. The boot-time
/// `role` snapshot is set from the file's current contents (or `direct`),
/// but the gate itself always re-reads the path.
fn cfg_with_role_path(profile_wire: &str, role_path: &Path) -> AgentConfig {
    let boot_role = if profile_wire == "ground-station" {
        Some(crate::config::read_current_role(role_path))
    } else {
        None
    };
    AgentConfig {
        profile_wire: profile_wire.to_string(),
        role: boot_role,
        video_enabled: true,
        video_network_source: None,
        vision_enabled: false,
        cloud_relay_enabled: false,
        configured_gs_role: "direct".to_string(),
        headless_mode: false,
        // The store ships off; the tests that care turn it on explicitly.
        log_store_enabled: false,
        mesh_role_path: role_path.to_path_buf(),
        run_dir: role_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
        config_yaml: std::path::PathBuf::from("/nonexistent/ados/config.yaml"),
    }
}

/// A headless drone config (the lean KEEP-set gate active), pointed at a
/// nonexistent sentinel (role irrelevant on a drone).
fn cfg_headless() -> AgentConfig {
    let mut c = cfg("drone");
    c.headless_mode = true;
    c
}

/// A config pointed at a nonexistent sentinel (gate sees `direct`). Used by
/// the drone-profile test where role is irrelevant.
fn cfg(profile_wire: &str) -> AgentConfig {
    cfg_with_role_path(profile_wire, Path::new("/nonexistent/ados/mesh/role"))
}

fn write_role(path: &Path, role: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(format!("{role}\n").as_bytes()).unwrap();
}

fn spec(name: &str) -> ServiceSpec {
    let d = SERVICE_REGISTRY.iter().find(|d| d.name == name).unwrap();
    ServiceSpec::from_def(d)
}

#[test]
fn drone_gate_blocks_ground_station_units() {
    let c = cfg("drone");
    assert!(gate_allows(&spec("ados-mavlink"), &c)); // drone is in the FC set
    assert!(gate_allows(&spec("ados-wfb"), &c)); // drone-gated
    assert!(!gate_allows(&spec("ados-wfb-rx"), &c)); // ground_station-gated
    assert!(!gate_allows(&spec("ados-oled"), &c));
    assert!(gate_allows(&spec("ados-gpio"), &c)); // cross-profile
}

#[test]
fn workstation_profile_excludes_the_fc_router_and_runs_the_core_infra() {
    let c = cfg("workstation");
    // The FC router never runs on the FC-less workstation node — it never fetches
    // the router binary, so an unconditional start would crash-loop.
    assert!(!gate_allows(&spec("ados-mavlink"), &c));
    // The core infra the workstation node DOES run.
    assert!(gate_allows(&spec("ados-cloud"), &c));
    // The pipe-set gate keeps the router on the FC-bearing profiles.
    assert!(gate_allows(&spec("ados-mavlink"), &cfg("drone")));
    assert!(gate_allows(&spec("ados-mavlink"), &cfg("ground_station")));
}

#[test]
fn headless_gate_keeps_only_the_lean_core() {
    let c = cfg_headless();
    // The KEEP set runs (radio TX is drone-gated, which the drone profile
    // also permits, so it stays up).
    assert!(gate_allows(&spec("ados-mavlink"), &c));
    assert!(gate_allows(&spec("ados-video"), &c));
    assert!(gate_allows(&spec("ados-wfb"), &c));
    assert!(gate_allows(&spec("ados-control"), &c));
    // Everything else gates off even though the profile/role gates alone
    // would permit it on a full drone: the HTTP front, cloud, an NPU
    // sidecar, and a cross-profile Hardware unit that is not in the keep set.
    assert!(!gate_allows(&spec("ados-api"), &c));
    assert!(!gate_allows(&spec("ados-cloud"), &c));
    assert!(!gate_allows(&spec("ados-vision-hailo"), &c));
    assert!(!gate_allows(&spec("ados-gpio"), &c));
    // The full-agent gate is unchanged: with headless off, ados-api runs.
    assert!(gate_allows(&spec("ados-api"), &cfg("drone")));
}

#[test]
fn ground_station_role_gate() {
    let dir = tempfile::tempdir().unwrap();
    let role = dir.path().join("mesh/role");

    // direct role: wfb-rx runs, relay/receiver/batman do not.
    write_role(&role, "direct");
    let direct = cfg_with_role_path("ground-station", &role);
    assert!(gate_allows(&spec("ados-wfb-rx"), &direct));
    assert!(!gate_allows(&spec("ados-batman"), &direct));
    assert!(!gate_allows(&spec("ados-wfb-relay"), &direct));
    assert!(!gate_allows(&spec("ados-wfb"), &direct)); // drone-only

    // relay role: batman + wfb-relay run, wfb-rx (direct) + receiver do not.
    write_role(&role, "relay");
    let relay = cfg_with_role_path("ground-station", &role);
    assert!(gate_allows(&spec("ados-batman"), &relay)); // relay|receiver
    assert!(gate_allows(&spec("ados-wfb-relay"), &relay));
    assert!(!gate_allows(&spec("ados-wfb-receiver"), &relay));
    assert!(!gate_allows(&spec("ados-wfb-rx"), &relay)); // direct-only

    // receiver role: batman + wfb-receiver run.
    write_role(&role, "receiver");
    let receiver = cfg_with_role_path("ground-station", &role);
    assert!(gate_allows(&spec("ados-batman"), &receiver));
    assert!(gate_allows(&spec("ados-wfb-receiver"), &receiver));
    assert!(!gate_allows(&spec("ados-wfb-relay"), &receiver));
}

/// A runtime role switch (operator flips the sentinel on disk, without
/// restarting the supervisor) changes which units the gate permits, using
/// the SAME config object. This is the regression guard: the gate must not
/// trust a boot-time role snapshot.
#[test]
fn live_role_switch_flips_gate_without_reconstructing_config() {
    let dir = tempfile::tempdir().unwrap();
    let role = dir.path().join("mesh/role");
    write_role(&role, "direct");

    // Config captured while the sentinel said "direct".
    let config = cfg_with_role_path("ground-station", &role);
    assert_eq!(config.role.as_deref(), Some("direct"));
    assert!(gate_allows(&spec("ados-wfb-rx"), &config)); // direct-only unit
    assert!(!gate_allows(&spec("ados-wfb-relay"), &config)); // relay-only unit
    assert!(!gate_allows(&spec("ados-batman"), &config)); // relay|receiver

    // Operator switches the node to relay: only the sentinel changes.
    write_role(&role, "relay");

    // Same config object, no reconstruction. The gate follows the sentinel.
    assert!(!gate_allows(&spec("ados-wfb-rx"), &config)); // now masked off
    assert!(gate_allows(&spec("ados-wfb-relay"), &config)); // now permitted
    assert!(gate_allows(&spec("ados-batman"), &config)); // now permitted

    // And on to receiver.
    write_role(&role, "receiver");
    assert!(gate_allows(&spec("ados-wfb-receiver"), &config));
    assert!(!gate_allows(&spec("ados-wfb-relay"), &config));
    assert!(gate_allows(&spec("ados-batman"), &config));
}

#[tokio::test]
async fn radio_restart_gate_holds_during_the_idle_bind_setup_window() {
    // Regression: during the bind's `Idle` setup window (normal radio unit
    // stopped, injection iface re-prepared, FSM not yet at `opening_tunnel`)
    // the session's data-plane `is_active` reads false. If the restart gate
    // keyed on that, a monitor pass landing in this window would auto-restart
    // a gated radio unit and re-claim the adapter mid-bind. The gate keys on
    // `session_active` (the whole-body in-progress flag) instead, so it holds.
    let bind = Arc::new(BindOrchestrator::new());
    let sup = Supervisor::new(cfg("drone"), bind.clone());

    // No bind in progress → the gate permits restarts of every unit.
    assert!(!sup.restart_blocked_by_bind("ados-wfb").await);
    assert!(!sup.restart_blocked_by_bind("ados-mavlink").await);

    // Enter the `Idle` setup window exactly as `start_local_bind` does: an
    // `Idle` (terminal-state) session installed + the in-progress flag raised.
    bind.enter_idle_setup_window_for_test().await;

    // Data-plane liveness is false here (Idle is terminal) ...
    assert!(!bind.is_active().await);
    // ... yet the radio-unit gate must block the bind-gated units ...
    assert!(
        sup.restart_blocked_by_bind("ados-wfb").await,
        "ados-wfb restart must be blocked across the Idle bind setup window"
    );
    assert!(
        sup.restart_blocked_by_bind("ados-wfb-rx").await,
        "ados-wfb-rx restart must be blocked across the Idle bind setup window"
    );
    // ... while non-radio units are never gated.
    assert!(!sup.restart_blocked_by_bind("ados-mavlink").await);
}

/// A recording process-manager double: every verb logs `verb:unit` and
/// reports success so the lifecycle proceeds as if the unit really started.
/// `fail_starts` flips `start` to failure so a test can drive a unit into
/// a crash loop and then let it recover.
struct MockProcessManager {
    calls: std::sync::Mutex<Vec<String>>,
    starts_fail: std::sync::atomic::AtomicBool,
    /// The work counter served for every unit. Only meaningful while
    /// `work_counter_known` is set; otherwise the backend answers `None`,
    /// which is the honest "cannot resolve it" and must never read as a
    /// stall.
    work_counter: std::sync::atomic::AtomicU64,
    work_counter_known: std::sync::atomic::AtomicBool,
    /// When set, every unit reports inactive — a whole-stack death.
    all_inactive: std::sync::atomic::AtomicBool,
    /// When set, every liveness probe returns no verdict — a busy service
    /// manager, or fork failing under memory pressure.
    probes_fail: std::sync::atomic::AtomicBool,
    /// When set, the supervisor's own unit has a job pending: systemd is
    /// stopping or restarting it, and every `PartOf=` unit with it.
    supervisor_job_pending: std::sync::atomic::AtomicBool,
    /// When set, every unit's last run reads as a clean `exit 0`.
    clean_exit: std::sync::atomic::AtomicBool,
    /// When set, every start is skipped on an unmet unit condition.
    condition_unmet: std::sync::atomic::AtomicBool,
}

impl MockProcessManager {
    fn new() -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            starts_fail: std::sync::atomic::AtomicBool::new(false),
            work_counter: std::sync::atomic::AtomicU64::new(0),
            work_counter_known: std::sync::atomic::AtomicBool::new(false),
            all_inactive: std::sync::atomic::AtomicBool::new(false),
            probes_fail: std::sync::atomic::AtomicBool::new(false),
            supervisor_job_pending: std::sync::atomic::AtomicBool::new(false),
            clean_exit: std::sync::atomic::AtomicBool::new(false),
            condition_unmet: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn record(&self, verb: &str, unit: &str) {
        self.calls.lock().unwrap().push(format!("{verb}:{unit}"));
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn fail_starts(&self) {
        self.starts_fail
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn let_starts_succeed(&self) {
        self.starts_fail
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
    /// Serve `counter` as every unit's cumulative work count, or `None` to
    /// model a backend that cannot read it.
    fn set_work_counter(&self, counter: Option<u64>) {
        self.work_counter_known
            .store(counter.is_some(), std::sync::atomic::Ordering::Relaxed);
        self.work_counter
            .store(counter.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    }
    fn deactivate_all(&self) {
        self.all_inactive
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn fail_probes(&self) {
        self.probes_fail
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn cycle_supervisor(&self) {
        self.supervisor_job_pending
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn exit_cleanly(&self) {
        self.clean_exit
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn unmet_condition(&self) {
        self.condition_unmet
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl ProcessManager for MockProcessManager {
    async fn start(&self, unit: &str) -> bool {
        self.record("start", unit);
        !self.starts_fail.load(std::sync::atomic::Ordering::Relaxed)
    }
    async fn stop(&self, unit: &str) -> bool {
        self.record("stop", unit);
        true
    }
    async fn restart(&self, unit: &str) -> bool {
        self.record("restart", unit);
        true
    }
    async fn try_restart(&self, unit: &str) -> bool {
        self.record("try_restart", unit);
        true
    }
    async fn reset_failed(&self, unit: &str) {
        self.record("reset_failed", unit);
    }
    async fn is_active(&self, unit: &str) -> Option<bool> {
        self.record("is_active", unit);
        if self.probes_fail.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        Some(!self.all_inactive.load(std::sync::atomic::Ordering::Relaxed))
    }
    async fn work_counter(&self, unit: &str) -> Option<u64> {
        self.record("work_counter", unit);
        self.work_counter_known
            .load(std::sync::atomic::Ordering::Relaxed)
            .then(|| self.work_counter.load(std::sync::atomic::Ordering::Relaxed))
    }
    async fn job_pending(&self, unit: &str) -> Option<bool> {
        self.record("job_pending", unit);
        Some(
            unit == SUPERVISOR_UNIT
                && self
                    .supervisor_job_pending
                    .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
    async fn exited_cleanly(&self, unit: &str) -> Option<bool> {
        self.record("exited_cleanly", unit);
        Some(self.clean_exit.load(std::sync::atomic::Ordering::Relaxed))
    }
    async fn condition_result(&self, _unit: &str) -> Option<bool> {
        Some(
            !self
                .condition_unmet
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
    async fn mask(&self, unit: &str) {
        self.record("mask", unit);
    }
    async fn unmask(&self, unit: &str) {
        self.record("unmask", unit);
    }
}

#[tokio::test]
async fn lifecycle_routes_start_stop_restart_through_the_process_manager() {
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    // ados-mavlink is a drone-gated core unit, so the profile/role gates
    // permit it and the lifecycle calls reach the injected backend.
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, mock.clone());

    assert!(sup.start_service("ados-mavlink").await); // reset_failed + start
    assert!(sup.stop_service("ados-mavlink").await); // stop
    assert!(sup.restart_service("ados-mavlink").await); // stop + reset_failed + start

    // Every verb routed through the trait, in order.
    assert_eq!(
        mock.calls(),
        vec![
            "reset_failed:ados-mavlink",
            "start:ados-mavlink",
            "stop:ados-mavlink",
            "stop:ados-mavlink",
            "reset_failed:ados-mavlink",
            "start:ados-mavlink",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_liveness_probe_with_no_verdict_is_not_a_death() {
    // A timed-out or unspawnable `systemctl is-active` says nothing about
    // the unit. Read as "inactive" it restarts every healthy unit on the
    // node at once — the flight-controller link included — exactly when
    // the box is already short of memory or its service manager is busy.
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, mock.clone());
    assert!(sup.start_service("ados-mavlink").await);
    let i = sup.index_of("ados-mavlink").unwrap();
    let starts_before = mock
        .calls()
        .iter()
        .filter(|c| c.starts_with("start:"))
        .count();

    mock.fail_probes();
    for _ in 0..3 {
        sup.reconcile_services().await;
        tokio::time::advance(Duration::from_secs(5)).await;
    }

    assert_eq!(sup.services[i].state, ServiceState::Running);
    assert!(
        sup.services[i].failure_times.is_empty(),
        "an unanswered probe must not be counted as a failure"
    );
    let starts_after = mock
        .calls()
        .iter()
        .filter(|c| c.starts_with("start:"))
        .count();
    assert_eq!(
        starts_after,
        starts_before,
        "nothing may be restarted on an unanswered probe: {:?}",
        mock.calls()
    );
}

#[tokio::test(start_paused = true)]
async fn a_unit_that_exits_zero_with_nothing_to_run_goes_idle_not_failed() {
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, mock.clone());
    assert!(sup.start_service("ados-gpio").await);
    let i = sup.index_of("ados-gpio").unwrap();
    let starts = |m: &MockProcessManager| {
        m.calls()
            .iter()
            .filter(|c| c.as_str() == "start:ados-gpio")
            .count()
    };
    let before = starts(&mock);

    mock.deactivate_all();
    mock.exit_cleanly();
    sup.reconcile_services().await;

    assert_eq!(sup.services[i].state, ServiceState::Idle);
    assert!(sup.services[i].failure_times.is_empty());
    assert_eq!(starts(&mock), before, "a clean idle exit is not restarted");
    assert!(!sup
        .parked_retries_due(Instant::now())
        .contains(&"ados-gpio"));
}

#[tokio::test(start_paused = true)]
async fn an_idle_unit_is_started_again_when_the_agent_config_changes() {
    // The fleet-slot case: the swarm bus exits 0 on a drone with no slot,
    // and a slot arrives later as a config write. Nothing but the config
    // change may start it, and a still-idle unit must not loop.
    let dir = tempfile::tempdir().unwrap();
    let config_yaml = dir.path().join("config.yaml");
    std::fs::write(&config_yaml, "agent:\n  profile: drone\n").unwrap();
    let mut config = cfg("drone");
    config.config_yaml = config_yaml.clone();
    let mock = Arc::new(MockProcessManager::new());
    let mut sup =
        Supervisor::with_process_manager(config, Arc::new(BindOrchestrator::new()), mock.clone());
    let bus = "ados-swarmbus";
    let i = sup.index_of(bus).unwrap();
    let starts = |m: &MockProcessManager| {
        m.calls()
            .iter()
            .filter(|c| c.as_str() == "start:ados-swarmbus")
            .count()
    };

    assert!(sup.start_service(bus).await);
    mock.deactivate_all();
    mock.exit_cleanly();
    sup.reconcile_services().await;
    assert_eq!(sup.services[i].state, ServiceState::Idle);
    assert!(sup.services[i].failure_times.is_empty());
    let idle_starts = starts(&mock);

    // No config change: the idle unit stays down, pass after pass.
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(5)).await;
        sup.reconcile_services().await;
    }
    assert_eq!(
        starts(&mock),
        idle_starts,
        "an idle unit has no retry cadence"
    );

    // The slot is written. Stamp a later mtime so the change is visible
    // whatever the filesystem's timestamp granularity.
    std::fs::write(
        &config_yaml,
        "agent:\n  profile: drone\nvideo:\n  wfb:\n    fleet_slot: 2\n",
    )
    .unwrap();
    let later = std::time::SystemTime::now() + Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&config_yaml)
        .unwrap()
        .set_modified(later)
        .unwrap();
    sup.reconcile_services().await;
    assert_eq!(
        starts(&mock),
        idle_starts + 1,
        "the config change starts it"
    );
    assert_eq!(sup.services[i].state, ServiceState::Running);

    // One start per change: a unit that still has nothing to run settles
    // idle again and is not started a second time.
    sup.reconcile_services().await;
    assert_eq!(sup.services[i].state, ServiceState::Idle);
    sup.reconcile_services().await;
    assert_eq!(starts(&mock), idle_starts + 1);
}

#[tokio::test(start_paused = true)]
async fn a_clean_exit_of_a_unit_that_must_keep_running_is_a_death() {
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, mock.clone());
    assert!(sup.start_service("ados-mavlink").await);
    let i = sup.index_of("ados-mavlink").unwrap();
    mock.deactivate_all();
    mock.exit_cleanly();
    sup.reconcile_services().await;
    assert_eq!(sup.services[i].failure_times.len(), 1);
}

#[tokio::test]
async fn a_radio_replug_restarts_the_radio_unit_this_node_owns() {
    // Drone: the TX plane.
    let mock = Arc::new(MockProcessManager::new());
    let mut sup = Supervisor::with_process_manager(
        cfg("drone"),
        Arc::new(BindOrchestrator::new()),
        mock.clone(),
    );
    sup.handle_hotplug(crate::hotplug::DevKind::Radio).await;
    assert!(
        mock.calls().iter().any(|c| c == "start:ados-wfb"),
        "{:?}",
        mock.calls()
    );

    // Ground station on the direct role: the receive plane. The drone TX
    // unit is gated off there, which used to make every ground-station
    // radio replug a no-op.
    let dir = tempfile::tempdir().unwrap();
    let role = dir.path().join("mesh/role");
    write_role(&role, "direct");
    let mock = Arc::new(MockProcessManager::new());
    let mut sup = Supervisor::with_process_manager(
        cfg_with_role_path("ground-station", &role),
        Arc::new(BindOrchestrator::new()),
        mock.clone(),
    );
    sup.handle_hotplug(crate::hotplug::DevKind::Radio).await;
    assert!(
        mock.calls().iter().any(|c| c == "start:ados-wfb-rx"),
        "{:?}",
        mock.calls()
    );
    assert!(!mock.calls().iter().any(|c| c.ends_with(":ados-wfb")));

    // Relay role: the relay plane re-detects its adapter in its own loop,
    // so a replug cycles nothing rather than a unit that would fight it for
    // the adapter.
    write_role(&role, "relay");
    let mock = Arc::new(MockProcessManager::new());
    let mut sup = Supervisor::with_process_manager(
        cfg_with_role_path("ground-station", &role),
        Arc::new(BindOrchestrator::new()),
        mock.clone(),
    );
    sup.handle_hotplug(crate::hotplug::DevKind::Radio).await;
    assert!(mock.calls().is_empty(), "{:?}", mock.calls());
}

#[tokio::test]
async fn a_radio_replug_during_a_bind_leaves_the_radio_to_the_bind() {
    let bind = Arc::new(BindOrchestrator::new());
    let mock = Arc::new(MockProcessManager::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind.clone(), mock.clone());
    bind.enter_idle_setup_window_for_test().await;
    sup.handle_hotplug(crate::hotplug::DevKind::Radio).await;
    assert!(
        mock.calls().is_empty(),
        "a bind owns the adapter; no radio unit may be cycled under it: {:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn a_relay_ground_station_starts_its_role_units_at_boot() {
    // Nothing enables the relay role's units, so after a reboot the relay
    // plane only runs if the hardware pass starts it: batman first (the WFB
    // side binds to its interface), then the relay plane, and never the
    // direct receive plane beside it on the same adapter.
    let dir = tempfile::tempdir().unwrap();
    let role = dir.path().join("mesh/role");
    write_role(&role, "relay");
    let mock = Arc::new(MockProcessManager::new());
    let mut sup = Supervisor::with_process_manager(
        cfg_with_role_path("ground-station", &role),
        Arc::new(BindOrchestrator::new()),
        mock.clone(),
    );
    sup.detect_and_start_hardware().await;
    let role_starts: Vec<String> = mock
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("start:ados-batman") || c.starts_with("start:ados-wfb"))
        .collect();
    assert_eq!(
        role_starts,
        vec!["start:ados-batman", "start:ados-wfb-relay"],
        "{:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn vision_enabled_starts_the_vision_engine() {
    // Regression: a `vision.enabled: true` config must actually bring the
    // vision engine up. The unit is not in any install-time enable set and
    // the monitor never starts a Stopped service, so the hardware-detect
    // pass is the ONLY thing that starts it — if it does not, the vision →
    // world-model pipeline silently never comes up.
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    // Drone profile (the vision unit's profile gate) with vision enabled and
    // a network camera source, so the camera-source precondition holds
    // without a local /dev/video node (host-independent).
    let mut config = cfg("drone");
    config.vision_enabled = true;
    config.video_network_source = Some("rtsp://cam/scene".to_string());
    let mut sup = Supervisor::with_process_manager(config, bind, mock.clone());

    sup.detect_and_start_hardware().await;

    assert!(
        mock.calls().iter().any(|c| c == "start:ados-vision"),
        "vision.enabled=true must reach a start of ados-vision; calls={:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn vision_disabled_does_not_start_the_vision_engine() {
    // The mirror of the above: with vision off, the engine must stay down
    // even though a camera source is present.
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut config = cfg("drone");
    config.vision_enabled = false;
    config.video_network_source = Some("rtsp://cam/scene".to_string());
    let mut sup = Supervisor::with_process_manager(config, bind, mock.clone());

    sup.detect_and_start_hardware().await;

    assert!(
        !mock.calls().iter().any(|c| c == "start:ados-vision"),
        "ados-vision must not start when vision is disabled; calls={:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn vision_enabled_without_a_camera_source_is_surfaced_not_started() {
    // A genuine hard precondition: vision enabled but no camera source at
    // all. The engine is not started (there are no frames to feed it); the
    // code path logs a loud warning rather than skipping in silence.
    //
    // `has_video_source` = a local /dev/video node OR a configured network
    // URL. This test forces the network URL absent; the assertion is only
    // meaningful when the test host also has no local camera node, so it
    // guards on the real probe rather than assuming the host state (a dev
    // box with a webcam would legitimately have a source and start it).
    if crate::hardware::has_camera().await {
        return;
    }
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut config = cfg("drone");
    config.vision_enabled = true;
    config.video_network_source = None;
    let mut sup = Supervisor::with_process_manager(config, bind, mock.clone());

    sup.detect_and_start_hardware().await;

    assert!(
        !mock.calls().iter().any(|c| c == "start:ados-vision"),
        "ados-vision must not start without a camera source; calls={:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn vision_enabled_on_a_non_drone_profile_is_gated_off_not_started() {
    // A genuine hard precondition: vision runs on the drone profile only
    // (the prebuilt catalog fetches the ados-vision binary there), so on any
    // other profile the unit's binary is absent and starting it would
    // crash-loop. Even with vision enabled AND a camera source present, the
    // engine must NOT be started on a non-drone node. The detect pass
    // surfaces the reason loudly; the behavioral contract locked here is that
    // it never reaches a start of the gated-off unit.
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    // Workstation profile (ados-vision gates off) with vision enabled and a
    // network camera source, so the camera-source precondition is satisfied
    // and the ONLY thing keeping the engine down is the profile gate.
    let mut config = cfg("workstation");
    config.vision_enabled = true;
    config.video_network_source = Some("rtsp://cam/scene".to_string());
    let mut sup = Supervisor::with_process_manager(config, bind, mock.clone());

    sup.detect_and_start_hardware().await;

    assert!(
        !mock.calls().iter().any(|c| c == "start:ados-vision"),
        "ados-vision must not start on a non-drone profile; calls={:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn vision_enabled_headless_drone_is_gated_off_not_started() {
    // The lean headless core excludes vision/AI (ados-vision is not in the
    // KEEP set). Even on a drone with vision enabled and a camera source, a
    // headless node must NOT start the engine — the headless gate is
    // subtractive. Locks that the world-model capture path stays off in the
    // zero-Python profile rather than crash-looping a binary the lean profile
    // never provisions.
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut config = cfg_headless();
    config.vision_enabled = true;
    config.video_network_source = Some("rtsp://cam/scene".to_string());
    let mut sup = Supervisor::with_process_manager(config, bind, mock.clone());

    sup.detect_and_start_hardware().await;

    assert!(
        !mock.calls().iter().any(|c| c == "start:ados-vision"),
        "ados-vision must not start on a headless node; calls={:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn a_crash_looping_service_is_still_retried_on_the_fixed_cadence() {
    // Regression: the breaker used to refuse every start while five
    // failures sat inside its 60 s window, so a parked retry every 5 s was
    // a no-op and a crash-looped unit (an FC link after a brownout) waited
    // up to a minute for its next real attempt. On a lean headless node
    // ados-control is the ONLY control surface, so that left the box with
    // no API for the whole window.
    let pm = Arc::new(MockProcessManager::new());
    pm.fail_starts();
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());
    for _ in 0..6 {
        assert!(!sup.start_service("ados-control").await);
    }
    let i = sup.index_of("ados-control").unwrap();
    assert_eq!(sup.services[i].state, ServiceState::CrashLooping);
    // Every attempt reached the service manager; none was refused.
    let starts = pm
        .calls()
        .iter()
        .filter(|c| c.as_str() == "start:ados-control")
        .count();
    assert_eq!(starts, 6);

    let due = sup.parked_retries_due(Instant::now());
    assert!(
        due.contains(&"ados-control"),
        "a crash-looping service must be retried; due={due:?}"
    );

    // The failures are all still inside the window; the next retry takes.
    pm.let_starts_succeed();
    sup.reconcile_services().await;
    assert_eq!(
        sup.services[i].state,
        ServiceState::Running,
        "the retried service must recover inside the failure window"
    );
    // The retry clears the systemd start-limit latch before starting.
    let calls = pm.calls();
    assert!(
        calls.contains(&"reset_failed:ados-control".to_string()),
        "the retry must clear the start-limit latch; calls={calls:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_start_skipped_on_an_unmet_condition_goes_idle_without_a_failure() {
    let mock = Arc::new(MockProcessManager::new());
    mock.unmet_condition();
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, mock.clone());
    assert!(!sup.start_service("ados-mavlink").await);
    let i = sup.index_of("ados-mavlink").unwrap();
    // The unit is absent, not dead: two monitor passes start nothing more.
    mock.deactivate_all();
    sup.reconcile_services().await;
    sup.reconcile_services().await;
    assert_eq!(sup.services[i].state, ServiceState::Idle);
    assert!(sup.services[i].failure_times.is_empty());
    let starts = mock
        .calls()
        .iter()
        .filter(|c| c.as_str() == "start:ados-mavlink")
        .count();
    assert_eq!(starts, 1);
}

#[tokio::test]
async fn a_failed_row_whose_gate_closes_is_stopped_and_not_retried() {
    let pm = Arc::new(MockProcessManager::new());
    pm.fail_starts();
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());
    // A ground-station-only unit forced into Failed, as after a role switch
    // away from ground station.
    let name = sup
        .services
        .iter()
        .find(|s| !gate_allows(s, &sup.config))
        .map(|s| s.name)
        .expect("a unit gated off on a drone");
    let i = sup.index_of(name).unwrap();
    sup.services[i].state = ServiceState::Failed;
    assert!(sup.parked_retries_due(Instant::now()).contains(&name));

    assert!(!sup.start_service(name).await);
    assert_eq!(sup.services[i].state, ServiceState::Stopped);
    assert!(!sup.parked_retries_due(Instant::now()).contains(&name));
    let want = format!("start:{name}");
    let starts = pm.calls().iter().filter(|c| **c == want).count();
    assert_eq!(starts, 0);
}

#[tokio::test]
async fn the_log_store_unit_gates_on_its_config_key() {
    // The store ships off and the installer MASKS the unit when it is off,
    // so an ungated row would have the supervisor start-fail a masked unit
    // on every pass forever.
    let mut off = cfg("drone");
    off.log_store_enabled = false;
    assert!(!gate_allows(&spec("ados-logd"), &off));

    let mut on = cfg("drone");
    on.log_store_enabled = true;
    assert!(gate_allows(&spec("ados-logd"), &on));

    // And it is cross-profile: a ground station with the store on runs it.
    let mut gs = cfg("ground-station");
    gs.log_store_enabled = true;
    assert!(gate_allows(&spec("ados-logd"), &gs));

    // Kept in the lean headless set, where it matters most.
    let mut headless = cfg_headless();
    headless.log_store_enabled = true;
    assert!(gate_allows(&spec("ados-logd"), &headless));
}

#[tokio::test(start_paused = true)]
async fn a_service_reconcile_stamps_monitor_progress() {
    // The systemd watchdog is fed only while this marker advances, so the
    // pass advancing MUST stamp it — otherwise the coupling would take a
    // healthy unit down.
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, mock.clone());
    assert!(sup.start_service("ados-mavlink").await);

    let progress = sup.progress();
    tokio::time::advance(Duration::from_secs(40)).await;
    assert!(
        progress.since_mark() >= Duration::from_secs(40),
        "nothing has stamped progress yet"
    );

    sup.reconcile_services().await;
    assert!(
        progress.since_mark() < Duration::from_secs(1),
        "a reconcile that walked the unit set must stamp progress"
    );
}

#[tokio::test(start_paused = true)]
async fn a_supervised_service_with_a_flat_delta_counter_is_reported_unhealthy() {
    // The defect: `systemctl is-active` was the supervisor's ONLY liveness
    // judgement, so a MAVLink router whose serial reader had wedged stayed
    // `active` forever and the monitor reported a clean pass while the
    // aircraft had no command-and-control path.
    let pm = Arc::new(MockProcessManager::new());
    pm.set_work_counter(Some(4096)); // alive, and never moves again
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());
    assert!(sup.start_service("ados-mavlink").await);
    let i = sup.index_of("ados-mavlink").unwrap();

    // One reading is only a baseline: nothing is concluded from it.
    sup.reconcile_services().await;
    assert_eq!(sup.services[i].state, ServiceState::Running);
    assert!(
        sup.services[i].failure_times.is_empty(),
        "a single sample must not condemn a unit"
    );

    // The unit stays `active` across the whole window and its counter is flat.
    tokio::time::advance(crate::work_proof::STALL_WINDOW + Duration::from_secs(1)).await;
    sup.reconcile_services().await;

    assert_eq!(
        sup.services[i].failure_times.len(),
        1,
        "a stalled lane must be recorded as a failure, exactly as a death is"
    );
    let calls = pm.calls();
    assert!(
        calls.contains(&"stop:ados-mavlink".to_string()),
        "the stalled unit was never torn down; calls={calls:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_service_whose_counter_keeps_moving_is_left_alone() {
    let pm = Arc::new(MockProcessManager::new());
    pm.set_work_counter(Some(1_000));
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());
    assert!(sup.start_service("ados-mavlink").await);

    for step in 1..=8u64 {
        sup.reconcile_services().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        pm.set_work_counter(Some(1_000 + step * 512));
    }

    let i = sup.index_of("ados-mavlink").unwrap();
    assert!(
        sup.services[i].failure_times.is_empty(),
        "a working lane must never be restarted, however long the run"
    );
    assert!(
        !pm.calls().contains(&"stop:ados-mavlink".to_string()),
        "a healthy unit was torn down"
    );
}

#[tokio::test(start_paused = true)]
async fn a_counter_the_backend_cannot_read_never_condemns_a_unit() {
    // The dangerous direction. A unit's counter can be unreadable (its
    // socket or sidecar absent, or its lane idle), and reading that as
    // "moved nothing" would restart every supervised lane every window.
    let pm = Arc::new(MockProcessManager::new());
    pm.set_work_counter(None);
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());
    assert!(sup.start_service("ados-mavlink").await);

    sup.reconcile_services().await;
    tokio::time::advance(crate::work_proof::STALL_WINDOW * 4).await;
    sup.reconcile_services().await;

    let i = sup.index_of("ados-mavlink").unwrap();
    assert!(sup.services[i].failure_times.is_empty());
    assert!(!pm.calls().contains(&"stop:ados-mavlink".to_string()));
}

#[tokio::test(start_paused = true)]
async fn a_unit_the_installer_started_is_adopted_and_then_death_detected() {
    // `ados-gpio` is enabled and started by the INSTALLER, never by
    // this process, so its row sat at `Stopped` forever and the monitor —
    // which only walked Running|Starting rows — never probed it once. It
    // could die on boot and stay dead through every "healthy" pass. Twenty-
    // odd catalog rows were in that state.
    let pm = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());
    let i = sup.index_of("ados-gpio").unwrap();
    assert_eq!(sup.services[i].state, ServiceState::Stopped);

    // Pass 1: systemd reports it active, so the supervisor adopts it.
    sup.reconcile_services().await;
    assert_eq!(
        sup.services[i].state,
        ServiceState::Running,
        "an already-active gate-allowed unit must be adopted into the supervised set"
    );

    // Pass 2: it dies. Now — and only because it was adopted — that is seen.
    pm.deactivate_all();
    sup.reconcile_services().await;
    assert_eq!(
        sup.services[i].failure_times.len(),
        1,
        "the adopted unit's death went unnoticed"
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_leaves_an_adopted_unit_to_the_service_manager() {
    // Regression: `systemctl restart ados-supervisor` restarts every
    // `PartOf=` unit, then this process's shutdown ran `systemctl stop` on
    // every Running row, adopted ones included. The replacement process
    // starts only its own rows and adopts only active ones, so ados-control
    // (the operator's :8080) stayed down until someone started it by hand.
    let pm = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());
    assert!(sup.start_service("ados-mavlink").await);
    sup.reconcile_services().await;
    let adopted: Vec<&str> = sup
        .services
        .iter()
        .filter(|s| s.adopted)
        .map(|s| s.name)
        .collect();
    assert!(adopted.contains(&"ados-control"), "adopted={adopted:?}");

    // Every unit reports stopped, so each tier's stop wait returns at once.
    pm.deactivate_all();
    sup.stop().await;

    let calls = pm.calls();
    assert!(
        calls.contains(&"stop:ados-mavlink".to_string()),
        "a row this process started must still be torn down; calls={calls:?}"
    );
    for name in adopted {
        assert!(
            !calls.contains(&format!("stop:{name}")),
            "shutdown stopped {name}, a unit systemd brings up and cycles; calls={calls:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn nothing_is_restarted_while_systemd_cycles_the_supervisor() {
    // `systemctl stop|restart ados-supervisor` stops every PartOf= unit in
    // the same transaction, before this process gets SIGTERM. Read as deaths,
    // those stops were auto-restarted: after a `stop` the control front
    // outlived the supervisor, and a restart queued behind the supervisor's
    // own job blocked the pass until systemd SIGKILLed the process.
    let pm = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());
    assert!(sup.start_service("ados-mavlink").await);
    sup.reconcile_services().await; // adopts ados-control and the rest
    let parked = sup.index_of("ados-video").unwrap();
    sup.services[parked].state = ServiceState::Failed; // due for a parked retry
    let starts_before = pm
        .calls()
        .iter()
        .filter(|c| c.starts_with("start:"))
        .count();

    pm.cycle_supervisor();
    pm.deactivate_all();
    sup.reconcile_services().await;

    let calls = pm.calls();
    let starts = calls.iter().filter(|c| c.starts_with("start:")).count();
    assert_eq!(
        starts, starts_before,
        "a unit was restarted while systemd cycles the supervisor; calls={calls:?}"
    );
    for name in ["ados-mavlink", "ados-control"] {
        let i = sup.index_of(name).unwrap();
        assert!(
            sup.services[i].failure_times.is_empty(),
            "{name}'s stop by the supervisor's own cycle was counted as a failure"
        );
    }

    // The same transaction already restarts every PartOf= unit before the
    // SIGTERM lands. A `systemctl stop` from the shutdown path then hits the
    // fresh instance (ados-vision died in ExecStartPre and stayed failed) and
    // costs every other unit a second start.
    let before_stop = pm.calls().len();
    sup.stop().await;
    let stops: Vec<String> = pm.calls()[before_stop..]
        .iter()
        .filter(|c| c.starts_with("stop:"))
        .cloned()
        .collect();
    assert!(
        stops.is_empty(),
        "shutdown stopped units systemd is already cycling; stops={stops:?}"
    );
}

#[tokio::test]
async fn startup_clears_a_stale_failure_on_a_unit_it_leaves_off() {
    // A previous process can leave an intentionally-off unit `failed`
    // (a stop that landed mid-start). Nothing starts it again, so without a
    // reset the status surface reports a failure for a unit meant to be off.
    let pm = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());

    sup.start().await;

    let calls = pm.calls();
    assert!(
        !calls.contains(&"start:ados-vision".to_string()),
        "vision is off in this config; calls={calls:?}"
    );
    assert!(
        calls.contains(&"reset_failed:ados-vision".to_string()),
        "the stale failed state of a unit left off was not cleared; calls={calls:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_unit_that_is_meant_to_exit_is_never_adopted() {
    // ados-setup-captive serves the first-boot portal and terminates for
    // good once setup completes, against a systemd condition that can then
    // never be met again. Adopting it would turn that success into a
    // permanent restart loop.
    let pm = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("ground-station"), bind, pm.clone());
    let i = sup.index_of("ados-setup-captive").unwrap();

    sup.reconcile_services().await;
    assert_eq!(
        sup.services[i].state,
        ServiceState::Stopped,
        "a self-terminating unit must stay outside the supervised set"
    );
}

#[tokio::test(start_paused = true)]
async fn adoption_never_starts_a_unit_that_is_not_already_running() {
    // The safety property of the sweep: it promotes, it does not start. A
    // unit the operator never opted into is inactive, so it is untouched.
    let pm = Arc::new(MockProcessManager::new());
    pm.deactivate_all();
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());

    sup.reconcile_services().await;

    assert!(
        sup.services
            .iter()
            .all(|s| s.state == ServiceState::Stopped),
        "the adoption sweep started or promoted an inactive unit"
    );
    assert!(
        !pm.calls().iter().any(|c| c.starts_with("start:")),
        "the adoption sweep must never issue a start; calls={:?}",
        pm.calls()
    );
}
