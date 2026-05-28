//! Service lifecycle + gating + the monitor pass.
//!
//! The supervisor owns its `ServiceSpec` list on a single task; the run loop
//! (in `main`) drives `monitor_pass`, hotplug events, and shutdown serially,
//! so no service state is shared across tasks and there is no lock to hold
//! across a `systemctl` await.

use std::time::{Duration, Instant};

use tokio::time::sleep;

use crate::config::AgentConfig;
use crate::registry::{build_specs, Category, ServiceSpec, ServiceState, PARKED_RETRY_COOLDOWN};
use crate::systemctl;

/// Units whose auto-restart the monitor skips while a bind handshake owns the
/// radio. **Parity note:** the Python check (`is_bind_active`) reads an
/// in-process bind-orchestrator global that lives in the `ados-wfb` process,
/// so in multi-process production the separate supervisor process always sees
/// "no bind" and the gate is effectively inert. We reproduce that inert
/// behavior here rather than invent a new cross-process signal; a real bind
/// sentinel is a follow-up that belongs with the radio services.
const BIND_GATED_UNITS: [&str; 2] = ["ados-wfb", "ados-wfb-rx"];

/// Whether a service's profile + role gates allow it to run under `config`.
pub fn gate_allows(spec: &ServiceSpec, config: &AgentConfig) -> bool {
    if let Some(gate) = spec.profile_gate {
        // The registry gates are the underscore form; the resolved profile is
        // the hyphen wire form. Normalise once for the comparison.
        if config.profile_gate() != gate {
            return false;
        }
    }
    if let Some(role_gate) = spec.role_gate {
        let active = config.role.as_deref().unwrap_or("direct");
        if !role_gate.split('|').any(|r| r == active) {
            return false;
        }
    }
    true
}

pub struct Supervisor {
    services: Vec<ServiceSpec>,
    config: AgentConfig,
}

impl Supervisor {
    pub fn new(config: AgentConfig) -> Self {
        Supervisor {
            services: build_specs(),
            config,
        }
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.services.iter().position(|s| s.name == name)
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
            self.services[i].state = ServiceState::Stopped;
        }

        self.services[i].state = ServiceState::Starting;
        // Clear any prior failed / start-limit-hit state so `start` is not a
        // no-op on a unit that crash-looped past systemd's StartLimitBurst.
        systemctl::reset_failed(name).await;

        if systemctl::start(name).await {
            self.services[i].state = ServiceState::Running;
            tracing::info!(service = name, "service started");
            true
        } else {
            let opened = self.services[i].record_failure(Instant::now());
            if !opened {
                self.services[i].state = ServiceState::Failed;
            }
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
        self.services[i].state = ServiceState::Stopped;
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
        if self.index_of(name).is_some() {
            tracing::info!(service = name, ?kind, "hot-plug triggered restart");
            self.restart_service(name).await;
        }
    }

    /// Whether the monitor should skip auto-restarting `name` (bind gate).
    fn restart_blocked_by_bind(&self, name: &str) -> bool {
        // See BIND_GATED_UNITS: inert in multi-process production, by parity.
        let _ = BIND_GATED_UNITS.contains(&name);
        false
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
                let opened = self.services[i].record_failure(Instant::now());
                if !opened {
                    self.services[i].state = ServiceState::Failed;
                }
                if self.services[i].state != ServiceState::CircuitOpen
                    && !self.restart_blocked_by_bind(name)
                {
                    tracing::info!(service = name, "auto-restart");
                    self.start_service(name).await;
                }
            }
        }

        // Parked-service retry (bounded by the cooldown).
        for name in to_retry {
            if self.restart_blocked_by_bind(name) {
                continue;
            }
            if let Some(i) = self.index_of(name) {
                self.services[i].last_retry_at = Some(Instant::now());
            }
            tracing::info!(service = name, "parked retry");
            self.start_service(name).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::SERVICE_REGISTRY;

    fn cfg(profile_wire: &str, role: Option<&str>) -> AgentConfig {
        AgentConfig {
            profile_wire: profile_wire.to_string(),
            role: role.map(str::to_string),
            video_enabled: true,
            configured_gs_role: role.unwrap_or("direct").to_string(),
            raw_agent_profile: Some(profile_wire.replace('-', "_")),
        }
    }

    fn spec(name: &str) -> ServiceSpec {
        let d = SERVICE_REGISTRY.iter().find(|d| d.name == name).unwrap();
        ServiceSpec::from_def(d)
    }

    #[test]
    fn drone_gate_blocks_ground_station_units() {
        let c = cfg("drone", None);
        assert!(gate_allows(&spec("ados-mavlink"), &c)); // core, no gate
        assert!(gate_allows(&spec("ados-wfb"), &c)); // drone-gated
        assert!(!gate_allows(&spec("ados-wfb-rx"), &c)); // ground_station-gated
        assert!(!gate_allows(&spec("ados-oled"), &c));
        assert!(gate_allows(&spec("ados-wifi-client"), &c)); // cross-profile
    }

    #[test]
    fn ground_station_role_gate() {
        // direct role: wfb-rx runs, relay/receiver/batman do not.
        let direct = cfg("ground-station", Some("direct"));
        assert!(gate_allows(&spec("ados-wfb-rx"), &direct));
        assert!(!gate_allows(&spec("ados-batman"), &direct));
        assert!(!gate_allows(&spec("ados-wfb-relay"), &direct));
        assert!(!gate_allows(&spec("ados-wfb"), &direct)); // drone-only

        // relay role: batman + wfb-relay run, wfb-rx (direct) + receiver do not.
        let relay = cfg("ground-station", Some("relay"));
        assert!(gate_allows(&spec("ados-batman"), &relay)); // relay|receiver
        assert!(gate_allows(&spec("ados-wfb-relay"), &relay));
        assert!(!gate_allows(&spec("ados-wfb-receiver"), &relay));
        assert!(!gate_allows(&spec("ados-wfb-rx"), &relay)); // direct-only

        // receiver role: batman + wfb-receiver run.
        let receiver = cfg("ground-station", Some("receiver"));
        assert!(gate_allows(&spec("ados-batman"), &receiver));
        assert!(gate_allows(&spec("ados-wfb-receiver"), &receiver));
        assert!(!gate_allows(&spec("ados-wfb-relay"), &receiver));
    }
}
