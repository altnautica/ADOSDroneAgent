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

/// Units whose auto-restart the monitor skips while a bind handshake owns the
/// radio adapter. The bind FSM now lives in this supervisor process, so the
/// gate reads the orchestrator's in-process liveness directly. (The Python
/// check read an in-process global that the separate supervisor never saw, so
/// it was inert in production; hosting the FSM here makes the gate real.)
const BIND_GATED_UNITS: [&str; 2] = ["ados-wfb", "ados-wfb-rx"];

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
    /// Discrete service-transition events shipped to the logging daemon so an
    /// RCA can query the lifecycle of every managed unit (a death + auto-restart,
    /// a circuit-breaker open, a stop) off-box and across reboots. Best-effort
    /// and non-blocking, like the other emitters above.
    events: ados_protocol::logd::emitter::EventEmitter,
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
        }
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
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

    /// Record a failure (which may open the breaker) and emit the resulting
    /// transition. Mirrors the prior inline `record_failure` + conditional
    /// `Failed` assignment, plus the event. Returns whether the breaker opened.
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

    /// Block until none of `names` report `is-active`, polling at 100ms up to
    /// `timeout`. Returns even if some remain up (logged), so a wedged unit
    /// cannot stall the rest of shutdown.
    async fn wait_for_stop(&self, names: &[&str], timeout: Duration) {
        if names.is_empty() {
            return;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let mut still_up = Vec::new();
            for n in names {
                if self.pm.is_active(n).await {
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
        // RAW config profile (not the resolved one) to match the Python
        // supervisor exactly; see AgentConfig::raw_is_ground_station.
        if self.config.raw_is_ground_station() {
            let role = self.config.configured_gs_role.clone();
            crate::role::apply_role_on_boot(
                self.pm.as_ref(),
                &role,
                &crate::config::mesh_role_path(),
            )
            .await;
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
    pub async fn stop(&mut self) {
        tracing::info!("supervisor stopping");

        // Tier 0: the API frontend stops accepting requests first.
        if self
            .index_of("ados-api")
            .map(|i| self.services[i].state == ServiceState::Running)
            .unwrap_or(false)
        {
            self.stop_service("ados-api").await;
            self.wait_for_stop(&["ados-api"], Duration::from_secs(5))
                .await;
        }

        for category in [Category::Hardware, Category::OnDemand, Category::Core] {
            let tier: Vec<&'static str> = self
                .services
                .iter()
                .filter(|s| {
                    s.name != "ados-api"
                        && s.category == category
                        && s.state == ServiceState::Running
                })
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

        // Start the right side of the radio pair for our profile. Gated on the
        // RAW config profile to match the Python supervisor: a ground station
        // starts ados-wfb-rx, anything else starts the drone-side ados-wfb.
        if crate::hardware::has_wfb_adapter() {
            if self.config.raw_is_ground_station() {
                if self.index_of("ados-wfb-rx").is_some() {
                    self.start_service("ados-wfb-rx").await;
                }
            } else if self.index_of("ados-wfb").is_some() {
                self.start_service("ados-wfb").await;
            }
        }
    }

    /// Restart the service that owns a hot-plugged device class. Radio routes
    /// to `ados-wfb` to match the Python hot-plug router exactly; the
    /// `start_service` gate is the backstop (on a ground station the drone-side
    /// `ados-wfb` gates off, so radio hot-plug is a no-op there — a faithfully
    /// ported Python behavior; routing it to `ados-wfb-rx` on a GS is a
    /// separate gated improvement).
    pub async fn handle_hotplug(&mut self, kind: crate::hotplug::DevKind) {
        use crate::hotplug::DevKind;
        let name = match kind {
            DevKind::Camera => "ados-video",
            DevKind::Fc => "ados-mavlink",
            DevKind::Radio => "ados-wfb",
            // The CRSF lane service. Kept separate from Fc so an RC-module
            // replug never restarts the FC link.
            DevKind::Elrs => "ados-crsf",
        };
        if self.index_of(name).is_none() {
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
        BIND_GATED_UNITS.contains(&name) && self.bind.session_active()
    }

    /// One monitor pass: detect deaths + auto-restart, retry parked services.
    pub async fn monitor_pass(&mut self) {
        // Snapshot the names + states we need so we can issue async restarts
        // without holding an immutable borrow across the await.
        let mut to_restart: Vec<&'static str> = Vec::new();
        let mut to_retry: Vec<&'static str> = Vec::new();
        let now = Instant::now();

        for i in 0..self.services.len() {
            let spec = &self.services[i];
            match spec.state {
                ServiceState::Running | ServiceState::Starting => {
                    // Checked below via is_active (needs await); collect names.
                    to_restart.push(spec.name);
                }
                ServiceState::Failed | ServiceState::CircuitOpen
                    if matches!(spec.category, Category::Core | Category::Hardware) =>
                {
                    let due = spec
                        .last_retry_at
                        .map(|t| now.duration_since(t) >= PARKED_RETRY_COOLDOWN)
                        .unwrap_or(true);
                    if due {
                        to_retry.push(spec.name);
                    }
                }
                _ => {}
            }
        }

        // Liveness check + auto-restart for running services.
        for name in to_restart {
            let active = self.pm.is_active(name).await;
            let Some(i) = self.index_of(name) else {
                continue;
            };
            if !active && self.services[i].state == ServiceState::Running {
                tracing::warn!(service = name, "service died");
                let _ = self.record_failure_and_emit(i, Instant::now(), "died");
                let blocked = self.restart_blocked_by_bind(name).await;
                if self.services[i].state != ServiceState::CircuitOpen && !blocked {
                    tracing::info!(service = name, "auto-restart");
                    self.start_service(name).await;
                }
            }
        }

        // Parked-service retry (bounded by the cooldown).
        for name in to_retry {
            if self.restart_blocked_by_bind(name).await {
                continue;
            }
            if let Some(i) = self.index_of(name) {
                self.services[i].last_retry_at = Some(Instant::now());
            }
            tracing::info!(service = name, "parked retry");
            self.start_service(name).await;
        }

        // Regulatory-domain reconcile (PREVENTION): re-assert the configured
        // wanted domain when a self-managed injection PHY has left a foreign
        // baked country as the global domain (which breaks the onboard WiFi's
        // data path). Channel-safety-validated so it never caps the WFB radio;
        // a cheap no-op when the domain is already in sync. Runs before the
        // reactive self-heal so a freshly-reconciled domain heads off the break
        // the self-heal would otherwise have to repair.
        self.reg_reconciler.tick().await;

        // WiFi power-save runtime reconcile (PREVENTION): the FullMAC onboard-WiFi
        // driver re-enables 802.11 power-save after an NM reconnect / hotplug /
        // driver reload, which drops unicast frames on an idle link so the box
        // silently falls off the LAN. Re-assert `power_save off` on every station
        // interface and record the verified per-interface state for the heartbeat.
        // Cheap (one `iw get` per iface; a `set` only on a real drift).
        self.wifi_powersave.tick().await;

        // Reactive network self-heal: detect + rebuild an onboard managed-WiFi
        // link whose data path died under the radio bring-up, so the box keeps a
        // working failover. Independent of service state; a no-op when there is
        // no onboard managed WiFi or the WiFi is healthy. Kept as the backstop
        // for a link that still needs an explicit rebuild after a domain drift.
        self.wifi_selfheal.tick().await;

        // Management-link guardian (REACTIVE backstop for the WHOLE link): detect
        // a dead operator management link (no carrier / no lease / unreachable
        // gateway) and walk a bounded, self-restoring software repair ladder
        // without a reboot, across NetworkManager and systemd-networkd. Runs
        // after the per-connection WiFi self-heal so the cheaper fix gets first
        // crack; cheap when the link is healthy, one repair rung per tick.
        self.mgmt_guardian.tick().await;

        // Onboard-WiFi heartbeat reach-back (LAST resort): when the wired
        // primary is physically down for a sustained window, declare a
        // heartbeat-only fallback so the box stays visible to the GCS. Runs
        // after the guardian (which repairs the link while it physically
        // exists); cheap when the wired primary is up.
        self.mgmt_failover.tick().await;

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
                self.wait_for_stop(&[unit], Duration::from_secs(5)).await;
                crate::usb_rehome::execute_rebind(&plan).await;
                self.start_service(unit).await;
            }
        }

        // Camera USB-recovery (force re-enumeration of an absent/wedged camera
        // that failed its cold-boot port-enable). No unit stop: the video
        // pipeline re-discovers via udev once the device re-enumerates. Cheap
        // when the camera is present; drone-only (gated on a fresh camera-state).
        if let Some(plan) = self.camera_usb_recovery.decide().await {
            crate::usb_rehome::camera::execute_camera_recovery(&plan).await;
        }
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
            raw_agent_profile: Some(profile_wire.replace('-', "_")),
            headless_mode: false,
            mesh_role_path: role_path.to_path_buf(),
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
        assert!(gate_allows(&spec("ados-wifi-client"), &c)); // cross-profile
    }

    #[test]
    fn workstation_profile_excludes_the_fc_router_and_runs_the_core_infra() {
        let c = cfg("workstation");
        // The FC router never runs on the FC-less workstation node — it never fetches
        // the router binary, so an unconditional start would crash-loop.
        assert!(!gate_allows(&spec("ados-mavlink"), &c));
        // The core infra the workstation node DOES run, plus the compute daemon.
        assert!(gate_allows(&spec("ados-cloud"), &c));
        assert!(gate_allows(&spec("ados-compute"), &c));
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
        // would permit it on a full drone: FastAPI, cloud, health, wifi-client.
        assert!(!gate_allows(&spec("ados-api"), &c));
        assert!(!gate_allows(&spec("ados-cloud"), &c));
        assert!(!gate_allows(&spec("ados-health"), &c));
        assert!(!gate_allows(&spec("ados-wifi-client"), &c));
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
    struct MockProcessManager {
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl MockProcessManager {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn record(&self, verb: &str, unit: &str) {
            self.calls.lock().unwrap().push(format!("{verb}:{unit}"));
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ProcessManager for MockProcessManager {
        async fn start(&self, unit: &str) -> bool {
            self.record("start", unit);
            true
        }
        async fn stop(&self, unit: &str) -> bool {
            self.record("stop", unit);
            true
        }
        async fn restart(&self, unit: &str) -> bool {
            self.record("restart", unit);
            true
        }
        async fn reset_failed(&self, unit: &str) {
            self.record("reset_failed", unit);
        }
        async fn is_active(&self, unit: &str) -> bool {
            self.record("is_active", unit);
            true
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
}
