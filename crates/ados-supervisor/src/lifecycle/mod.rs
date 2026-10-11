//! Service lifecycle + gating + the monitor pass.
//!
//! The supervisor owns its `ServiceSpec` list on a single task; the run loop
//! (in [`crate::daemon`]) drives `monitor_pass`, hotplug events, and shutdown serially,
//! so no service state is shared across tasks and there is no lock to hold
//! across a `systemctl` await.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::sleep;

use crate::bind::orchestrator::BindOrchestrator;
use crate::config::AgentConfig;
use crate::process_manager::{select, ProcessManager};
use crate::registry::{build_specs, Category, ServiceSpec, ServiceState, PARKED_RETRY_COOLDOWN};
use crate::sdnotify::MonitorProgress;

mod adoption;
mod monitor;
mod reconcile;
mod retry;

/// The radio units the supervisor itself starts and restarts, in gate order:
/// the drone TX plane and the ground station's direct receive plane. Their
/// profile and role gates are mutually exclusive, so at most one is allowed on
/// any node. (A relay or receiver ground station's WFB plane is a role unit:
/// the boot hardware pass and the role transition start it, and its own loop
/// re-detects the adapter, so it is not cycled on a radio replug.)
///
/// Auto-restart of these is also held while a bind handshake owns the radio
/// adapter. The bind FSM lives in this supervisor process, so that gate reads
/// the orchestrator's in-process liveness directly.
const RADIO_UNITS: [&str; 2] = ["ados-wfb", "ados-wfb-rx"];

/// The durable logging + telemetry store's unit. Behind one config key
/// (`logging.store.enabled`) because it is by far the largest writer on the
/// box, and the installer masks the unit when that key is off — so the
/// supervisor must honour the same gate or it start-fails a masked unit on
/// every pass.
const LOG_STORE_UNIT: &str = "ados-logd";

/// This process's own unit, whose pending job tells the monitor that the
/// service manager is stopping or restarting it (and so every `PartOf=` unit).
const SUPERVISOR_UNIT: &str = "ados-supervisor";

/// How often the monitor adopts gate-allowed catalog rows that are `active`
/// but were never started by this process.
///
/// Death detection only ever looked at rows the supervisor itself had moved to
/// `Running`, so the ~20 units the INSTALLER enables and starts — the AP, DHCP,
/// the kiosk, the OLED, the RC lane, the peripheral registry, the uplink
/// router, the native HTTP front — were never probed at all. They could die
/// and stay dead with the supervisor reporting a healthy pass. Adoption closes
/// that by promoting an already-active row into the supervised set.
///
/// Slower than the 5 s monitor tick on purpose: the sweep costs one
/// `systemctl is-active` per not-yet-adopted row, and a row only needs
/// adopting once.
const ADOPTION_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Catalog rows the adoption sweep skips because they are MEANT to exit.
///
/// `ados-setup-captive` serves the first-boot captive portal and exits 0 when
/// it has nothing to serve: `/var/lib/ados/setup-complete` exists, the ground
/// station's dnsmasq owns DNS, or port 53 is taken. Its unit carries
/// `RestartPreventExitStatus=0`, so that exit is final, and
/// `ConditionPathExists=!` the sentinel skips it once setup is done. Adopting
/// it would turn its clean exit into a "service died", and every restart
/// attempt afterwards either exits the same way or runs against a condition
/// that can never be met again: a permanent retry loop against a unit that
/// did its job.
const SELF_TERMINATING_UNITS: [&str; 1] = ["ados-setup-captive"];

/// Whether a service's profile + role gates allow it to run under `config`.
///
/// The role gate re-reads the on-disk role sentinel on every call rather than
/// trusting the boot-time snapshot. An operator role switch flips the sentinel
/// and stops/starts the role-gated units without restarting this process, so a
/// cached role would leave the monitor self-healing the wrong unit set (it
/// would skip the now-active relay/receiver units and churn the now-masked
/// direct unit). Reading the sentinel here keeps the gate in lock-step with the
/// live role, matching the Python `start_service` semantics.
pub fn gate_allows(spec: &ServiceSpec, config: &AgentConfig) -> bool {
    // The lean headless gate runs first and is purely subtractive: when the
    // agent is headless, only the KEEP set (MAVLink / camera / radio / HTTP
    // front) may run; every other unit gates off so the box boots zero-Python.
    // A complete no-op on the full agent (`headless_mode` false), so the profile
    // and role gates below are unchanged for every non-headless rig.
    if config.headless_mode && !spec.headless_keep {
        return false;
    }
    // The store is opt-in and its unit is masked when the key is off, so the
    // registry row gates on the resolved key rather than on a profile.
    if spec.name == LOG_STORE_UNIT && !config.log_store_enabled {
        return false;
    }
    if let Some(gate) = spec.profile_gate {
        // The registry gates are the underscore form; the resolved profile is
        // the hyphen wire form. A gate is a pipe-separated set (like role_gate),
        // so a unit can scope to more than one profile but not all — e.g. the FC
        // router runs on `drone|ground_station` but not the FC-less compute node.
        // A single-value gate is a one-element set, so this is backward-compatible.
        if !gate.split('|').any(|p| p == config.profile_gate()) {
            return false;
        }
    }
    if let Some(role_gate) = spec.role_gate {
        let active = config.live_role();
        if !role_gate.split('|').any(|r| r == active) {
            return false;
        }
    }
    true
}

/// Whether the supervisor's own shutdown stops this row: it is running and
/// this process brought it up, rather than adopting a unit its enablement
/// started (see [`Supervisor::stop`]).
fn stops_on_shutdown(spec: &ServiceSpec) -> bool {
    spec.state == ServiceState::Running && !spec.adopted
}

/// The agent config file's modification time, or `None` when it is absent or
/// unreadable (a fresh node has no config yet; its appearance is a change too).
fn config_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub struct Supervisor {
    services: Vec<ServiceSpec>,
    config: AgentConfig,
    /// The host service-manager backend (systemd / launchd / inert) every
    /// start/stop/restart lifecycle call routes through. Selected for the host
    /// OS by default; a test injects a recording double via
    /// [`with_process_manager`](Supervisor::with_process_manager).
    pm: Arc<dyn ProcessManager>,
    /// The bind orchestrator, shared with the control socket task. The monitor
    /// consults its in-process liveness to gate radio-unit auto-restart.
    bind: Arc<BindOrchestrator>,
    /// Debounces + coalesces hot-plug-driven restarts so a re-enumerating
    /// device does not thrash `systemctl`.
    hotplug_coord: crate::hotplug::HotplugCoordinator,
    /// Reactive self-heal for the onboard management-WiFi data path. The radio
    /// bring-up can leave the onboard WiFi associated-but-dead (a strong link +
    /// valid IP yet zero traffic); this re-associates it so the box keeps a
    /// working failover when the wired link is unplugged. Runs on both profiles
    /// from the monitor tick; inert when there is no onboard managed WiFi.
    wifi_selfheal: crate::wifi_selfheal::WifiSelfHeal,
    /// Periodic regulatory-domain reconciler. A self-managed injection PHY can
    /// leave its EEPROM-baked country as the global regulatory domain after a
    /// monitor/bind re-churn, which breaks the onboard WiFi's data path. This
    /// re-asserts the configured wanted domain (channel-safety-validated, never
    /// capping the radio) so the break is PREVENTED, not just reacted to. Runs on
    /// both profiles from the monitor tick; a cheap no-op when the domain is in
    /// sync. The reactive WiFi self-heal above stays as the backstop.
    reg_reconciler: crate::reg_reconciler::RegReconciler,
    /// WiFi power-save runtime reconciler. The FullMAC onboard-WiFi drivers bring
    /// the station interface up with 802.11 power-save enabled (and re-enable it
    /// after an NM reconnect / hotplug / driver reload), which drops unicast
    /// frames on an idle link so the box falls off the LAN when it goes quiet.
    /// This re-asserts `power_save off` on every station interface from the
    /// monitor tick and records the verified per-interface state to
    /// /run/ados/wifi-powersave.json. Cheap (one `iw get` per iface; a `set` only
    /// on a real drift); the install/boot-time provisioning is the one-shot half.
    wifi_powersave: crate::wifi_powersave::WifiPowersaveReconciler,
    /// Management-link guardian: the stack-agnostic backstop for the operator's
    /// whole management link (the default-route interface, never the WFB
    /// injection adapter). Detects a dead data path (no carrier / no lease /
    /// unreachable gateway) and walks a bounded, self-restoring software repair
    /// ladder without a reboot, across both NetworkManager and systemd-networkd.
    /// A cheap health check each tick; the ladder runs only on a sustained
    /// break, one rung per tick. Mirrors the state to /run/ados/mgmt-link.json.
    mgmt_guardian: crate::mgmt_link_guardian::MgmtLinkGuardian,
    /// Onboard-WiFi heartbeat reach-back: when the wired primary is physically
    /// down for a sustained window, declares a heartbeat-only fallback over the
    /// onboard WiFi so the box stays visible to the GCS (degraded, no data
    /// plane). Composes with the guardian (which repairs the link while it
    /// exists). Mirrors the mode to /run/ados/mgmt-failover.json.
    mgmt_failover: crate::mgmt_failover::MgmtFailover,
    /// USB-rehome self-heal: when the WFB adapter is on a slow USB port AND its
    /// RF is unverified, unbind/rebind the USB device for a clean
    /// re-enumeration that can land it on a faster lane. Bounded budget +
    /// fail-closed control-interface guard. The supervisor drives the stop →
    /// rebind → start sequence; the reconciler decides + records.
    usb_rehome: crate::usb_rehome::UsbRehome,
    /// Camera USB-recovery self-heal: when an expected primary camera is missing
    /// (a cold-boot port-enable failure the kernel tries once and abandons), force
    /// a USB re-enumeration so it comes back without a human reseating the cable.
    /// Leaf rebind when the device is wedged, a clean per-port re-enable on a hub
    /// that exposes it, else detect + alert (an opt-in boot-time hub reset only
    /// when the guard proves no radio/FC/control device shares the hub). Mirrors
    /// state to /run/ados/camera-usb-recovery.json. Drone-only (gated on a fresh
    /// camera-state); the video pipeline owns pipeline recovery via udev.
    camera_usb_recovery: crate::usb_rehome::camera::CameraUsbRecovery,
    /// Disk janitor: reclaim, hourly, the things on this box that accumulate
    /// with nothing else bounding them — the apt cache and index, per-plugin
    /// logs systemd appends to with no rotation, the audit trail, operator
    /// recordings, journal history, and quarantined copies of a torn store.
    /// Three rungs keyed on free space where /var lives; every category has a
    /// floor and every pass emits what it freed. Mirrors the last pass to
    /// /run/ados/janitor.json for the storage diagnostic.
    janitor: crate::janitor::Janitor,
    /// Discrete service-transition events shipped to the logging daemon so an
    /// RCA can query the lifecycle of every managed unit (a death + auto-restart,
    /// a circuit-breaker open, a stop) off-box and across reboots. Best-effort
    /// and non-blocking, like the other emitters above.
    events: ados_protocol::logd::emitter::EventEmitter,
    /// Monitor-pass progress, stamped at every stage boundary of
    /// [`monitor_pass`](Self::monitor_pass) and read by the systemd watchdog
    /// ticker. The watchdog is fed only while this advances, so a pass wedged
    /// inside one stage takes the unit down (systemd restarts it) instead of
    /// reporting healthy forever with death-detection and auto-restart dead.
    progress: MonitorProgress,
    /// Per-unit work-counter history for the units whose `active` state is not
    /// accepted as proof of work ([`crate::work_proof::WORK_PROVEN_UNITS`]).
    /// Without it the monitor's only liveness judgement is "has the process
    /// exited?", which a wedged FC link, a stopped swarm beacon, a frozen RC
    /// lane and a stalled vision engine all pass.
    work_proof: crate::work_proof::WorkProof,
    /// When the adoption sweep last ran. The sweep is what gives the monitor
    /// any coverage of the catalog rows this process did not itself start.
    last_adoption_sweep: Option<Instant>,
    /// The agent config file's modification time as last seen. A change wakes
    /// every idle row (see [`Self::idle_rows_due_on_config_change`]).
    config_stamp: Option<std::time::SystemTime>,
}

impl Supervisor {
    /// Construct with the process-manager backend selected for the host OS.
    pub fn new(config: AgentConfig, bind: Arc<BindOrchestrator>) -> Self {
        Self::with_process_manager(config, bind, select())
    }

    /// Construct with an explicit process-manager backend. The default
    /// [`new`](Self::new) selects the host backend; tests inject a recording
    /// double to assert the lifecycle routes through the trait.
    pub fn with_process_manager(
        config: AgentConfig,
        bind: Arc<BindOrchestrator>,
        pm: Arc<dyn ProcessManager>,
    ) -> Self {
        Supervisor {
            services: build_specs(),
            config_stamp: config_mtime(&config.config_yaml),
            config,
            pm,
            bind,
            hotplug_coord: crate::hotplug::HotplugCoordinator::new(),
            events: ados_protocol::logd::emitter::EventEmitter::new("ados-supervisor"),
            wifi_selfheal: crate::wifi_selfheal::WifiSelfHeal::new(
                ados_protocol::logd::emitter::EventEmitter::new("ados-supervisor"),
            ),
            reg_reconciler: crate::reg_reconciler::RegReconciler::new(
                ados_protocol::logd::emitter::EventEmitter::new("ados-supervisor"),
            ),
            wifi_powersave: crate::wifi_powersave::WifiPowersaveReconciler::new(
                ados_protocol::logd::emitter::EventEmitter::new("ados-supervisor"),
            ),
            mgmt_guardian: crate::mgmt_link_guardian::MgmtLinkGuardian::new(
                ados_protocol::logd::emitter::EventEmitter::new("ados-supervisor"),
            ),
            mgmt_failover: crate::mgmt_failover::MgmtFailover::new(
                ados_protocol::logd::emitter::EventEmitter::new("ados-supervisor"),
            ),
            usb_rehome: crate::usb_rehome::UsbRehome::new(
                ados_protocol::logd::emitter::EventEmitter::new("ados-supervisor"),
            ),
            camera_usb_recovery: crate::usb_rehome::camera::CameraUsbRecovery::new(
                ados_protocol::logd::emitter::EventEmitter::new("ados-supervisor"),
            ),
            janitor: crate::janitor::Janitor::new(ados_protocol::logd::emitter::EventEmitter::new(
                "ados-supervisor",
            )),
            progress: MonitorProgress::new(),
            work_proof: crate::work_proof::WorkProof::new(),
            last_adoption_sweep: None,
        }
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// A handle on the monitor-pass progress marker, for the systemd watchdog
    /// ticker. Cloned rather than borrowed so the ticker task outlives any
    /// borrow of the supervisor.
    pub fn progress(&self) -> MonitorProgress {
        self.progress.clone()
    }

    /// The process-manager backend, for the role transition's mask/unmask.
    pub(crate) fn process_manager(&self) -> Arc<dyn ProcessManager> {
        self.pm.clone()
    }

    /// Whether a bind handshake owns the radio adapter right now.
    pub(crate) fn bind_session_active(&self) -> bool {
        self.bind.session_active()
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.services.iter().position(|s| s.name == name)
    }

    /// Ship one `service.transition` event with the from/to states and a reason.
    /// Non-blocking and best-effort; an absent logging daemon drops it.
    fn emit_transition(&self, name: &str, from: ServiceState, to: ServiceState, reason: &str) {
        use ados_protocol::logd::{Fields, Level, Value};
        let mut detail = Fields::new();
        detail.insert("service".to_string(), Value::from(name));
        detail.insert("from_state".to_string(), Value::from(from.as_str()));
        detail.insert("to_state".to_string(), Value::from(to.as_str()));
        detail.insert("reason".to_string(), Value::from(reason));
        self.events.emit("service.transition", Level::Info, detail);
    }

    /// Set a service's state and emit a transition event when it actually
    /// changes. The single seam every direct state write goes through so the
    /// event stream mirrors the in-memory lifecycle exactly.
    fn set_state(&mut self, i: usize, to: ServiceState, reason: &str) {
        let from = self.services[i].state;
        self.services[i].state = to;
        if from != to {
            self.emit_transition(self.services[i].name, from, to, reason);
        }
    }

    /// Record a failure (which may label the unit crash-looping) and emit the
    /// resulting transition. Returns whether it is now crash-looping.
    fn record_failure_and_emit(&mut self, i: usize, now: Instant, reason: &str) -> bool {
        let from = self.services[i].state;
        let opened = self.services[i].record_failure(now);
        let to = if opened {
            // record_failure already set the state to CrashLooping.
            ServiceState::CrashLooping
        } else {
            self.services[i].state = ServiceState::Failed;
            ServiceState::Failed
        };
        if from != to {
            self.emit_transition(self.services[i].name, from, to, reason);
        }
        opened
    }
}

#[cfg(test)]
mod tests;
