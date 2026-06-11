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
use crate::registry::{build_specs, Category, ServiceSpec, ServiceState, PARKED_RETRY_COOLDOWN};
use crate::systemctl;

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
    if let Some(gate) = spec.profile_gate {
        // The registry gates are the underscore form; the resolved profile is
        // the hyphen wire form. Normalise once for the comparison.
        if config.profile_gate() != gate {
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
    pub fn new(config: AgentConfig, bind: Arc<BindOrchestrator>) -> Self {
        Supervisor {
            services: build_specs(),
            config,
            bind,
            hotplug_coord: crate::hotplug::HotplugCoordinator::new(),
            events: ados_protocol::logd::emitter::EventEmitter::new("ados-supervisor"),
            wifi_selfheal: crate::wifi_selfheal::WifiSelfHeal::new(
                ados_protocol::logd::emitter::EventEmitter::new("ados-supervisor"),
            ),
            reg_reconciler: crate::reg_reconciler::RegReconciler::new(
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
        // no-op on a unit that crash-looped past systemd's StartLimitBurst.
        systemctl::reset_failed(name).await;

        if systemctl::start(name).await {
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
        let ok = systemctl::stop(name).await;
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
                if systemctl::is_active(n).await {
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
            crate::role::apply_role_on_boot(&role, &crate::config::mesh_role_path()).await;
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
        if self.config.video_enabled
            && self.index_of("ados-video").is_some()
            && crate::hardware::has_camera().await
        {
            self.start_service("ados-video").await;
        } else if !self.config.video_enabled {
            tracing::info!("video service skipped (video.mode disabled)");
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
        BIND_GATED_UNITS.contains(&name) && self.bind.is_active().await
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
            let active = systemctl::is_active(name).await;
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
        if let Some(plan) = self.usb_rehome.decide().await {
            let unit = plan.unit;
            self.stop_service(unit).await;
            self.wait_for_stop(&[unit], Duration::from_secs(5)).await;
            crate::usb_rehome::execute_rebind(&plan).await;
            self.start_service(unit).await;
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
            cloud_relay_enabled: false,
            configured_gs_role: "direct".to_string(),
            raw_agent_profile: Some(profile_wire.replace('-', "_")),
            mesh_role_path: role_path.to_path_buf(),
        }
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
        assert!(gate_allows(&spec("ados-mavlink"), &c)); // core, no gate
        assert!(gate_allows(&spec("ados-wfb"), &c)); // drone-gated
        assert!(!gate_allows(&spec("ados-wfb-rx"), &c)); // ground_station-gated
        assert!(!gate_allows(&spec("ados-oled"), &c));
        assert!(gate_allows(&spec("ados-wifi-client"), &c)); // cross-profile
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
}
