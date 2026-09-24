//! Service lifecycle + gating + the monitor pass.
//!
//! The supervisor owns its `ServiceSpec` list on a single task; the run loop
//! (in `main`) drives `monitor_pass`, hotplug events, and shutdown serially,
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
/// `ados-setup-captive` serves the first-boot captive portal and terminates
/// once `/var/lib/ados/setup-complete` appears; its unit is `Restart=no` and
/// `ConditionPathExists=!` that same sentinel. Adopting it would turn its
/// successful completion into a "service died", and every restart attempt
/// afterwards runs against a condition that can never be met again — a
/// permanent retry loop against a unit that did its job.
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
    /// Per-unit byte-counter history for the units whose `active` state is not
    /// accepted as proof of work ([`crate::work_proof::WORK_PROVEN_UNITS`]).
    /// Without it the monitor's only liveness judgement is "has the process
    /// exited?", which a wedged FC link, a stopped swarm beacon, a frozen RC
    /// lane and a stalled vision engine all pass.
    work_proof: crate::work_proof::WorkProof,
    /// When the adoption sweep last ran. The sweep is what gives the monitor
    /// any coverage of the catalog rows this process did not itself start.
    last_adoption_sweep: Option<Instant>,
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

    /// Record a failure (which may open the breaker) and emit the resulting transition.
    /// Returns whether the breaker opened.
    fn record_failure_and_emit(&mut self, i: usize, now: Instant, reason: &str) -> bool {
        let from = self.services[i].state;
        let opened = self.services[i].record_failure(now);
        let to = if opened {
            // record_failure already set the state to CircuitOpen.
            ServiceState::CircuitOpen
        } else {
            self.services[i].state = ServiceState::Failed;
            ServiceState::Failed
        };
        if from != to {
            self.emit_transition(self.services[i].name, from, to, reason);
        }
        opened
    }

    /// Start a unit, honoring profile/role gates and the circuit breaker.
    /// Returns true only when the unit reached `active`.
    pub async fn start_service(&mut self, name: &str) -> bool {
        let Some(i) = self.index_of(name) else {
            tracing::warn!(service = name, "unknown service");
            return false;
        };

        if !gate_allows(&self.services[i], &self.config) {
            tracing::info!(service = name, "service gated off for this profile/role");
            return false;
        }

        let now = Instant::now();
        if self.services[i].breaker_blocks(now) {
            tracing::warn!(service = name, "circuit breaker open");
            return false;
        }
        // Breaker has cooled: clear the open state so the start can take.
        if self.services[i].state == ServiceState::CircuitOpen {
            self.set_state(i, ServiceState::Stopped, "breaker_cooldown");
        }

        self.set_state(i, ServiceState::Starting, "start_requested");
        // Clear any prior failed / start-limit-hit state so `start` is not a
        // no-op on a unit that crash-looped past the start-limit burst.
        self.pm.reset_failed(name).await;

        if self.pm.start(name).await {
            self.set_state(i, ServiceState::Running, "start_ok");
            tracing::info!(service = name, "service started");
            true
        } else {
            let _ = self.record_failure_and_emit(i, Instant::now(), "start_failed");
            tracing::error!(service = name, "service start failed");
            false
        }
    }

    /// Stop a unit and reset its runtime state.
    pub async fn stop_service(&mut self, name: &str) -> bool {
        let Some(i) = self.index_of(name) else {
            return false;
        };
        let ok = self.pm.stop(name).await;
        // The replacement process starts its byte counters at zero, so the
        // dead one's total must not survive as a baseline to compare against.
        self.work_proof.forget(name);
        self.set_state(i, ServiceState::Stopped, "stopped");
        tracing::info!(service = name, "service stopped");
        ok
    }

    /// Stop then start, with the same brief settle the Python path uses.
    pub async fn restart_service(&mut self, name: &str) -> bool {
        self.stop_service(name).await;
        sleep(Duration::from_millis(500)).await;
        self.start_service(name).await
    }

    /// Block until none of `names` is confirmed `active`, polling at 100ms up to
    /// `timeout`. A unit whose state cannot be read counts as still up (not
    /// confirmed stopped). Returns even if some remain up (logged), so a wedged
    /// unit cannot stall the rest of shutdown.
    async fn wait_for_stop(&self, names: &[&str], timeout: Duration) {
        if names.is_empty() {
            return;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let mut still_up = Vec::new();
            for n in names {
                if self.pm.is_active(n).await != Some(false) {
                    still_up.push(*n);
                }
            }
            if still_up.is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                tracing::warn!(services = ?still_up, "stop wait timed out");
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Full startup: apply GS role, start core, detect + start hardware.
    pub async fn start(&mut self) {
        tracing::info!("supervisor starting");

        // On a ground station, apply the configured mesh role so the sentinel,
        // systemd masks, and role-gate checks all agree before the hardware
        // pass tries to start role-gated units. No-op on a drone. Gated on the
        // RESOLVED profile, so a config that says `auto` (profile.conf decides)
        // or the hyphen spelling applies its role exactly like the literal one.
        if self.config.is_ground_station() {
            let role = self.config.configured_gs_role.clone();
            // The sentinel the gate re-reads (`mesh_role_path`, which honours
            // the `ADOS_MESH_ROLE` override), so the writer and the reader can
            // never name two different files.
            let role_path = self.config.mesh_role_path.clone();
            crate::role::apply_role_on_boot(self.pm.as_ref(), &role, &role_path).await;
        }

        // Start core units. They are independent of one another (hardware and
        // on-demand depend on core, not the reverse).
        let core: Vec<&'static str> = self
            .services
            .iter()
            .filter(|s| s.category == Category::Core)
            .map(|s| s.name)
            .collect();
        for name in core {
            self.start_service(name).await;
        }

        self.detect_and_start_hardware().await;
        tracing::info!(services = self.services.len(), "supervisor ready");
    }

    /// Graceful shutdown in dependency-aware tiers: HTTP frontend first (so no
    /// new requests land on dying hardware services), then hardware, on-demand,
    /// and finally the rest of core. Poll `is-active` between tiers.
    ///
    /// Tears down only the rows this process brought up. An adopted row is left
    /// to the service manager: every catalog unit is `PartOf=` the supervisor,
    /// so systemd already propagates the supervisor's stop to it, and on
    /// `systemctl restart ados-supervisor` it propagates a restart that brings
    /// the unit straight back. A `systemctl stop` from here lands after that
    /// restart and leaves the unit down for good, because the replacement
    /// process starts only its own rows and the adoption sweep only promotes a
    /// unit that is already active. That is how a supervisor restart used to take
    /// `ados-control`, the operator's `:8080`, down with every other
    /// installer-started unit. Under launchd the installer loads and unloads
    /// each LaunchAgent itself, and a `bootout` from here unloads the job with
    /// nothing to load it again.
    pub async fn stop(&mut self) {
        tracing::info!("supervisor stopping");

        // Tier 0: the API frontend stops accepting requests first.
        if self
            .index_of("ados-api")
            .is_some_and(|i| stops_on_shutdown(&self.services[i]))
        {
            self.stop_service("ados-api").await;
            self.wait_for_stop(&["ados-api"], Duration::from_secs(5))
                .await;
        }

        for category in [Category::Hardware, Category::OnDemand, Category::Core] {
            let tier: Vec<&'static str> = self
                .services
                .iter()
                .filter(|s| s.name != "ados-api" && s.category == category && stops_on_shutdown(s))
                .map(|s| s.name)
                .collect();
            for name in &tier {
                self.stop_service(name).await;
            }
            self.wait_for_stop(&tier, Duration::from_secs(5)).await;
        }
        tracing::info!("supervisor stopped");
    }

    /// Detect connected hardware and start the matching units.
    async fn detect_and_start_hardware(&mut self) {
        // Start the video service when video is enabled and either a local
        // camera node is present OR a network camera source is configured (the
        // IP-camera case: the feed is an rtsp://… / http://… URL, so there is no
        // /dev/video node to detect).
        let has_video_source =
            crate::hardware::has_camera().await || self.config.video_network_source.is_some();
        if self.config.video_enabled && self.index_of("ados-video").is_some() && has_video_source {
            self.start_service("ados-video").await;
        } else if !self.config.video_enabled {
            tracing::info!("video service skipped (video.mode disabled)");
        }

        // Start the vision engine when it is enabled and a camera source exists.
        // The engine feeds the frame stream the follow / designate plugins and
        // the world-model capture consume, so leaving it unstarted breaks that
        // whole pipeline. It is not in any install-time enable set, so the
        // supervisor is the ONLY thing that brings it up — without this branch a
        // `vision.enabled: true` config would silently never start the engine.
        //
        // A vision-enabled config that never runs vision is a misconfiguration,
        // not a silent no-op, so every reason the engine does NOT start is
        // surfaced loudly rather than skipped in silence:
        //   * profile/headless gate — vision runs on the drone profile only and
        //     is excluded from the lean headless core, so on any other node the
        //     unit's binary is absent; starting it would crash-loop. Report it
        //     instead of leaving the operator's `vision.enabled: true` dark.
        //   * no camera source — the engine has no frames to consume.
        if self.config.vision_enabled {
            if let Some(i) = self.index_of("ados-vision") {
                if !gate_allows(&self.services[i], &self.config) {
                    tracing::warn!(
                        profile = %self.config.profile_wire,
                        headless = self.config.headless_mode,
                        "vision enabled but ados-vision is gated off for this node; \
                         vision runs on the drone profile and is excluded from headless mode"
                    );
                } else if has_video_source {
                    self.start_service("ados-vision").await;
                } else {
                    tracing::warn!(
                        "vision enabled but no camera source configured; ados-vision not started"
                    );
                }
            }
        }

        // A relay / receiver ground station's units (batman, then its WFB
        // plane) are enabled by nothing, so after a reboot they only run if this
        // pass starts them. The direct role's receive plane is this node's radio
        // unit and starts below, behind the adapter check.
        if self.config.is_ground_station() {
            let role = self.config.live_role();
            for unit in crate::role::role_units(&role) {
                let name = crate::role::service_name(unit);
                if RADIO_UNITS.contains(&name) || self.index_of(name).is_none() {
                    continue;
                }
                self.start_service(name).await;
            }
        }

        // Start the radio unit this node owns: the drone TX plane, or a direct
        // ground station's receive plane.
        if crate::hardware::has_wfb_adapter() {
            if let Some(unit) = self.radio_unit() {
                self.start_service(unit).await;
            }
        }
    }

    /// The radio unit this process starts and restarts for this node's profile
    /// and live role, or `None` when it owns none (workstation, compute, or a
    /// relay / receiver ground station). Resolved through the same gate every
    /// start goes through, so it cannot disagree with it.
    fn radio_unit(&self) -> Option<&'static str> {
        RADIO_UNITS.iter().copied().find(|name| {
            self.index_of(name)
                .is_some_and(|i| gate_allows(&self.services[i], &self.config))
        })
    }

    /// Restart the service that owns a hot-plugged device class. A radio edge
    /// goes to the node's own radio unit ([`Self::radio_unit`]), and is held
    /// off while a bind session owns the adapter, exactly like the monitor's
    /// auto-restart.
    pub async fn handle_hotplug(&mut self, kind: crate::hotplug::DevKind) {
        use crate::hotplug::DevKind;
        let name = match kind {
            DevKind::Camera => "ados-video",
            DevKind::Fc => "ados-mavlink",
            DevKind::Radio => match self.radio_unit() {
                Some(unit) => unit,
                None => return,
            },
            // The CRSF lane service. Kept separate from Fc so an RC-module
            // replug never restarts the FC link.
            DevKind::Elrs => "ados-crsf",
        };
        if self.index_of(name).is_none() {
            return;
        }
        if self.restart_blocked_by_bind(name).await {
            tracing::info!(
                service = name,
                ?kind,
                "hot-plug restart held: a bind owns the radio"
            );
            return;
        }
        // Coalesce re-enumeration storms: a device that drops and re-appears
        // within the debounce window (DFU → flight, a flaky cable) must not
        // issue a second `systemctl restart` while the first is still settling.
        if !self
            .hotplug_coord
            .should_restart(kind, std::time::Instant::now())
        {
            tracing::debug!(service = name, ?kind, "hot-plug restart coalesced");
            return;
        }
        tracing::info!(service = name, ?kind, "hot-plug triggered restart");
        self.restart_service(name).await;
    }

    /// Whether the monitor should skip auto-restarting `name` because a bind
    /// handshake owns the radio adapter. Real now that the FSM is in-process:
    /// only the radio units are gated, and only while a bind is live.
    async fn restart_blocked_by_bind(&self, name: &str) -> bool {
        // `session_active` (not `is_active`) so the gate also holds during the
        // bind's `Idle` stop→`OpeningTunnel` setup window: the normal radio unit
        // is stopped + the injection iface re-prepared there, and a monitor pass
        // landing in that window would otherwise see the unit inactive-but-tracked-
        // Running and auto-restart it, re-claiming the adapter mid-bind.
        RADIO_UNITS.contains(&name) && self.bind.session_active()
    }

    /// Names of every parked service whose retry is due at `now`.
    ///
    /// Deliberately NOT filtered by category. `Category::OnDemand` used to be
    /// excluded, which let `ados-control` — the lean headless node's only HTTP
    /// surface — latch `CircuitOpen` permanently after five failures in a
    /// minute, clearable only over SSH. A unit the operator never enabled sits
    /// in `Stopped`, never `Failed`/`CircuitOpen`, so retrying every parked
    /// service cannot spuriously start one: `gate_allows` plus the unit's own
    /// `ConditionPathExists` marker remain the gate.
    fn parked_retries_due(&self, now: Instant) -> Vec<&'static str> {
        self.services
            .iter()
            .filter(|spec| {
                matches!(spec.state, ServiceState::Failed | ServiceState::CircuitOpen)
                    && spec
                        .last_retry_at
                        .map(|t| now.duration_since(t) >= PARKED_RETRY_COOLDOWN)
                        .unwrap_or(true)
            })
            .map(|spec| spec.name)
            .collect()
    }

    /// Promote gate-allowed catalog rows that are already `active` into the
    /// supervised set, so the monitor's death detection covers them.
    ///
    /// Only ever promotes: a row that is not active stays `Stopped` and is
    /// never started here. That is what makes the sweep safe — a unit the
    /// operator never opted into (an unset marker, an unmet
    /// `ConditionPathExists`) is inactive, so it is not adopted and the
    /// supervisor does not start it.
    async fn adopt_active_units(&mut self) {
        let candidates: Vec<&'static str> = self
            .services
            .iter()
            .filter(|spec| {
                spec.state == ServiceState::Stopped
                    && !SELF_TERMINATING_UNITS.contains(&spec.name)
                    && gate_allows(spec, &self.config)
            })
            .map(|spec| spec.name)
            .collect();
        for name in candidates {
            // Only a confirmed-active unit is adopted; an unreadable one is
            // left for the next sweep.
            let active = self.pm.is_active(name).await == Some(true);
            self.progress.mark();
            if !active {
                continue;
            }
            if let Some(i) = self.index_of(name) {
                self.services[i].adopted = true;
                self.set_state(i, ServiceState::Running, "adopted_already_active");
                tracing::info!(service = name, "adopted a unit this process did not start");
            }
        }
    }

    /// Judge one active, work-proven unit on its byte-counter delta and
    /// restart it when the counter has been flat across the whole stall
    /// window. Returns whether it was judged stalled.
    ///
    /// `restart`, not `start`: the process is alive, so there is something to
    /// tear down. The baseline is dropped either way — the replacement process
    /// starts its counters at zero.
    async fn enforce_work_proof(&mut self, name: &'static str) -> bool {
        use crate::work_proof::WorkVerdict;
        let counter = self.pm.work_counter(name).await;
        let verdict = self
            .work_proof
            .observe(name, counter, tokio::time::Instant::now());
        let WorkVerdict::Stalled { flat_for } = verdict else {
            return false;
        };
        tracing::warn!(
            service = name,
            flat_for_s = flat_for.as_secs(),
            "service is active but has moved no bytes; treating as dead"
        );
        let Some(i) = self.index_of(name) else {
            return false;
        };
        let _ = self.record_failure_and_emit(i, Instant::now(), "stalled");
        self.work_proof.forget(name);
        if self.services[i].state != ServiceState::CircuitOpen
            && !self.restart_blocked_by_bind(name).await
        {
            self.restart_service(name).await;
            self.work_proof.forget(name);
        }
        true
    }

    /// Whether the service manager is stopping or restarting this supervisor
    /// right now: a job is pending on its own unit. Every catalog unit is
    /// `PartOf=` the supervisor, so systemd stops or restarts all of them in the
    /// same transaction, and a unit that goes inactive in that window was
    /// stopped on purpose. Restarting it undoes `systemctl stop ados-supervisor`
    /// (the unit outlives the supervisor), and a start queued behind the
    /// supervisor's own job (ados-logd is ordered after it) blocks the monitor
    /// pass, so SIGTERM goes unhandled until systemd's stop timeout kills the
    /// process. A start job still pending right after READY holds one pass at
    /// most. `None` (launchd, a failed probe) reads as not cycling, so the
    /// monitor keeps restarting.
    async fn service_manager_cycling_self(&self) -> bool {
        self.pm.job_pending(SUPERVISOR_UNIT).await == Some(true)
    }

    /// The service half of a monitor pass: detect deaths and stalls, auto-
    /// restart, adopt units this process did not start, then retry every
    /// parked service whose cooldown has elapsed. Restarts are held while
    /// systemd stops or restarts the supervisor itself.
    ///
    /// Split out from [`monitor_pass`](Self::monitor_pass) so it is drivable
    /// without the network/hardware reconcilers, and stamps monitor progress
    /// per unit: a pass that walks 30-odd `systemctl` calls is advancing, and
    /// must keep the watchdog fed, while a pass stuck on any one of them is not.
    pub async fn reconcile_services(&mut self) {
        // Snapshot the names + states we need so we can issue async restarts
        // without holding an immutable borrow across the await.
        let mut to_restart: Vec<&'static str> = Vec::new();
        let now = Instant::now();
        // Latched once a death is seen while systemd cycles the supervisor, so
        // the rest of the pass holds without probing again.
        let mut held = false;

        for spec in &self.services {
            if matches!(spec.state, ServiceState::Running | ServiceState::Starting) {
                // Checked below via is_active (needs await); collect names.
                to_restart.push(spec.name);
            }
        }
        let to_retry = self.parked_retries_due(now);

        // Liveness check + auto-restart for running services.
        for name in to_restart {
            let verdict = self.pm.is_active(name).await;
            self.progress.mark();
            let Some(i) = self.index_of(name) else {
                continue;
            };
            // No verdict is not a death. A probe that timed out or could not
            // be spawned (a busy service manager, fork failing under memory
            // pressure) says nothing about the unit, and reading it as dead
            // would restart every healthy unit on the node at once.
            let Some(active) = verdict else {
                tracing::debug!(service = name, "liveness probe gave no verdict");
                continue;
            };
            if !active && self.services[i].state == ServiceState::Running {
                if held || self.service_manager_cycling_self().await {
                    // This is the supervisor's own stop or restart reaching the
                    // unit, not a failure. The process is about to get SIGTERM.
                    held = true;
                    tracing::info!(
                        service = name,
                        "unit stopped while systemd cycles the supervisor; not restarting"
                    );
                    continue;
                }
                tracing::warn!(service = name, "service died");
                self.work_proof.forget(name);
                let _ = self.record_failure_and_emit(i, Instant::now(), "died");
                let blocked = self.restart_blocked_by_bind(name).await;
                if self.services[i].state != ServiceState::CircuitOpen && !blocked {
                    tracing::info!(service = name, "auto-restart");
                    self.start_service(name).await;
                    self.progress.mark();
                }
                continue;
            }
            // The unit is alive. For the lanes whose silence is invisible in
            // `systemctl`, that is not the same as working: ask the delta
            // counter too. Process liveness is never proof of work.
            if active
                && self.services[i].state == ServiceState::Running
                && crate::work_proof::requires_work_proof(name)
            {
                self.enforce_work_proof(name).await;
                self.progress.mark();
            }
        }

        // Adopt the rows the installer started and this process never has, so
        // they get death detection too. Bounded to its own cadence: a row only
        // needs adopting once.
        if self
            .last_adoption_sweep
            .is_none_or(|t| now.duration_since(t) >= ADOPTION_SWEEP_INTERVAL)
        {
            self.last_adoption_sweep = Some(now);
            self.adopt_active_units().await;
        }

        // Parked-service retry (bounded by the cooldown). Held, like the
        // auto-restart above, while systemd cycles the supervisor.
        if to_retry.is_empty() || held || self.service_manager_cycling_self().await {
            return;
        }
        for name in to_retry {
            if self.restart_blocked_by_bind(name).await {
                continue;
            }
            if let Some(i) = self.index_of(name) {
                self.services[i].last_retry_at = Some(Instant::now());
            }
            tracing::info!(service = name, "parked retry");
            self.start_service(name).await;
            self.progress.mark();
        }
    }

    /// One monitor pass: the service half, then the prevention/repair
    /// reconcilers, in order.
    ///
    /// Every stage boundary stamps [`MonitorProgress`], which is what feeds the
    /// systemd watchdog. A slow-but-advancing recovery pass keeps the unit
    /// healthy; a pass wedged inside one stage stops stamping and systemd
    /// restarts the unit rather than reporting `active` while death-detection,
    /// auto-restart and hot-plug handling are all dead. Each subprocess these
    /// backends shell is separately bounded by [`crate::oscmd`], so a stall
    /// here is a real wedge and not a slow external command.
    pub async fn monitor_pass(&mut self) {
        self.reconcile_services().await;
        self.progress.mark();

        // Regulatory-domain reconcile (PREVENTION): re-assert the configured
        // wanted domain when a self-managed injection PHY has left a foreign
        // baked country as the global domain (which breaks the onboard WiFi's
        // data path). Channel-safety-validated so it never caps the WFB radio;
        // a cheap no-op when the domain is already in sync. Runs before the
        // reactive self-heal so a freshly-reconciled domain heads off the break
        // the self-heal would otherwise have to repair.
        self.reg_reconciler.tick().await;
        self.progress.mark();

        // WiFi power-save runtime reconcile (PREVENTION): the FullMAC onboard-WiFi
        // driver re-enables 802.11 power-save after an NM reconnect / hotplug /
        // driver reload, which drops unicast frames on an idle link so the box
        // silently falls off the LAN. Re-assert `power_save off` on every station
        // interface and record the verified per-interface state for the heartbeat.
        // Cheap (one `iw get` per iface; a `set` only on a real drift).
        self.wifi_powersave.tick().await;
        self.progress.mark();

        // Reactive network self-heal: detect + rebuild an onboard managed-WiFi
        // link whose data path died under the radio bring-up, so the box keeps a
        // working failover. Independent of service state; a no-op when there is
        // no onboard managed WiFi or the WiFi is healthy. Kept as the backstop
        // for a link that still needs an explicit rebuild after a domain drift.
        self.wifi_selfheal.tick().await;
        self.progress.mark();

        // Management-link guardian (REACTIVE backstop for the WHOLE link): detect
        // a dead operator management link (no carrier / no lease / unreachable
        // gateway) and walk a bounded, self-restoring software repair ladder
        // without a reboot, across NetworkManager and systemd-networkd. Runs
        // after the per-connection WiFi self-heal so the cheaper fix gets first
        // crack; cheap when the link is healthy, one repair rung per tick.
        self.mgmt_guardian.tick().await;
        self.progress.mark();

        // Onboard-WiFi heartbeat reach-back (LAST resort): when the wired
        // primary is physically down for a sustained window, declare a
        // heartbeat-only fallback so the box stays visible to the GCS. Runs
        // after the guardian (which repairs the link while it physically
        // exists); cheap when the wired primary is up.
        self.mgmt_failover.tick().await;
        self.progress.mark();

        // USB-rehome self-heal (LAST resort for a slow-port, not-radiating
        // adapter): the reconciler decides; if it authorizes an attempt, the
        // supervisor quiesces the radio unit, rebinds the USB device, and brings
        // the unit back (its startup re-probes the adapter). The next decide()
        // re-checks the fresh stats to confirm. Cheap when the adapter is fine.
        //
        // Never rehome during a bind: the bind sequence owns the adapter (it
        // stops the normal wfb unit and drives monitor mode + the key-transfer
        // tunnel), so yanking the USB device or restarting the wfb unit under it
        // would corrupt the handshake — the same exclusion `restart_blocked_by_bind`
        // applies to ordinary restarts. Gating the whole stop/rebind/start
        // sequence here also keeps the post-rebind start out of the bind window.
        // `session_active` (not `is_active`) so the rehome is also held off during
        // the bind's `Idle` setup window, before the data-plane state advances.
        if !self.bind.session_active() {
            if let Some(plan) = self.usb_rehome.decide().await {
                let unit = plan.unit;
                self.stop_service(unit).await;
                self.progress.mark();
                self.wait_for_stop(&[unit], Duration::from_secs(5)).await;
                crate::usb_rehome::execute_rebind(&plan).await;
                self.progress.mark();
                self.start_service(unit).await;
            }
        }
        self.progress.mark();

        // Camera USB-recovery (force re-enumeration of an absent/wedged camera
        // that failed its cold-boot port-enable). No unit stop: the video
        // pipeline re-discovers via udev once the device re-enumerates. Cheap
        // when the camera is present; drone-only (gated on a fresh camera-state).
        if let Some(plan) = self.camera_usb_recovery.decide().await {
            crate::usb_rehome::camera::execute_camera_recovery(&plan).await;
        }
        self.progress.mark();

        // Disk janitor (hourly, not per-tick): reclaim the apt cache, over-long
        // plugin logs and audit trail, aged recordings, and — once free space is
        // short — the package index, journal history, and older quarantined
        // stores. Cheap on the ticks it is not due, and a no-op on the ones it
        // is when nothing is over its cap.
        self.janitor.tick().await;
        self.progress.mark();
    }
}

#[cfg(test)]
mod tests {
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
        assert!(gate_allows(&spec("ados-peripherals"), &c)); // cross-profile
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
        // would permit it on a full drone: the HTTP front, cloud, health, and a
        // cross-profile Hardware unit that is not in the keep set.
        assert!(!gate_allows(&spec("ados-api"), &c));
        assert!(!gate_allows(&spec("ados-cloud"), &c));
        assert!(!gate_allows(&spec("ados-health"), &c));
        assert!(!gate_allows(&spec("ados-peripherals"), &c));
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
    /// the circuit breaker and then let it recover.
    struct MockProcessManager {
        calls: std::sync::Mutex<Vec<String>>,
        starts_fail: std::sync::atomic::AtomicBool,
        /// The byte counter served for every unit. Only meaningful while
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
        /// Serve `counter` as every unit's cumulative byte total, or `None` to
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
    async fn a_parked_on_demand_service_is_retried_and_recovers() {
        // Regression: the parked retry used to be filtered to Core|Hardware, so
        // an OnDemand unit that hit the breaker latched CircuitOpen for the rest
        // of the process. On a lean headless node ados-control is the ONLY
        // control surface, so that state left the box flying with no API, no
        // pairing and no diagnostics, clearable only over SSH.
        let pm = Arc::new(MockProcessManager::new());
        pm.fail_starts();
        let bind = Arc::new(BindOrchestrator::new());
        let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());
        // Six start attempts inside the failure window: five record a failure
        // and the fifth opens the breaker; the sixth is refused by it.
        for _ in 0..6 {
            assert!(!sup.start_service("ados-control").await);
        }
        let i = sup.index_of("ados-control").unwrap();
        assert_eq!(
            sup.services[i].state,
            ServiceState::CircuitOpen,
            "six failures in the window must open the breaker"
        );

        // The fix: a parked OnDemand unit is queued for retry like any other.
        let due = sup.parked_retries_due(Instant::now());
        assert!(
            due.contains(&"ados-control"),
            "a parked OnDemand service must be retried; due={due:?}"
        );

        // Once the failures age out of the window the breaker half-opens (see
        // `circuit_breaker_half_opens_after_window`); model that prune, let the
        // unit come up, and drive one service reconcile.
        sup.services[i].failure_times.clear();
        pm.let_starts_succeed();
        sup.reconcile_services().await;

        assert_eq!(
            sup.services[i].state,
            ServiceState::Running,
            "the retried OnDemand service must recover, not stay parked"
        );
        // The retry clears the systemd start-limit latch before starting, which
        // is the half that needed an SSH `reset-failed` before.
        let calls = pm.calls();
        assert!(
            calls.contains(&"reset_failed:ados-control".to_string()),
            "the retry must clear the start-limit latch; calls={calls:?}"
        );
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

        // The unit stays `active` across the whole window and moves no bytes.
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
        // The dangerous direction. On a host with no `/proc/<pid>/io` — or in
        // a PID recycling window — the counter is unreadable, and reading that
        // as "moved nothing" would restart every supervised lane every window.
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
        // `ados-peripherals` is enabled and started by the INSTALLER, never by
        // this process, so its row sat at `Stopped` forever and the monitor —
        // which only walked Running|Starting rows — never probed it once. It
        // could die on boot and stay dead through every "healthy" pass. Twenty-
        // odd catalog rows were in that state.
        let pm = Arc::new(MockProcessManager::new());
        let bind = Arc::new(BindOrchestrator::new());
        let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());
        let i = sup.index_of("ados-peripherals").unwrap();
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
}
