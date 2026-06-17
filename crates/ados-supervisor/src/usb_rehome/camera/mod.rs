//! Camera USB-recovery self-heal (force re-enumeration of an absent/wedged camera).
//!
//! When an *expected* primary camera is reported missing and stays missing for a
//! confirm window, the agent forces a USB re-enumeration so a camera that failed
//! its cold-boot port-enable (the kernel tries once and gives up) comes back
//! without a human reseating the cable. Three cases, topology-resolved:
//!
//!   (a) the camera device is still in `/sys` but the pipeline reports missing
//!       (wedged) → unbind/rebind the camera leaf only. Clean, no sibling effect.
//!   (b) the camera is gone and sits on a hub it SHARES with the radio/FC (the
//!       Pi 4B internal hub) → default detect + alert (`needs_hub_reset`); an
//!       opt-in, boot-time-only hub reset is allowed ONLY when the guard proves
//!       no protected device hangs off that hub.
//!   (c) the camera is gone but its hub exposes per-port power (an external hub)
//!       → re-enable just the camera's port. Clean.
//!
//! Reuses the `usb_rehome` trigger + bounded retry/cooldown machine + the
//! fail-closed topology guard. The reconciler decides + records; the supervisor
//! executes the sysfs op. Pipeline recovery itself is owned by the udev→SIGUSR1
//! path in the video service, so this never restarts `ados-video`. Default-ON
//! (detect + alert); actions gated under `video.usb_recovery`.
//!
//! Module layout:
//! - `config`: `CameraRecoveryConfig` + parsing + `camera_expected` resolution.
//! - `os`: the camera-state / last-good sidecar IO, the protected-set builder,
//!   and the sysfs unbind/rebind + port-cycle executor (`execute_camera_recovery`).
//! - this module root: the recovery action/plan types and the `CameraUsbRecovery`
//!   reconciler FSM (the trigger + retry machine + topology guard).

use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;

use super::machine::{RehomeMachine, RehomeTrigger};

#[cfg(target_os = "linux")]
use super::machine::RehomeStep;
#[cfg(target_os = "linux")]
use super::topo::{self, GuardVerdict, UsbTopo};

pub mod config;
pub mod os;

pub use config::{camera_expected, read_config_from, CameraRecoveryConfig};
pub use os::execute_camera_recovery;

/// The event kind recorded for a camera-recovery attempt + outcome.
pub const CAMERA_RECOVERY_KIND: &str = "camera.usb_recovery";

/// The event kind for the power-contention diagnostic: the camera shares an
/// over-subscribed hub (no per-port power) with the high-draw WFB radio, so a
/// radio TX spike can brown the camera off the bus. Surfaced so the operator
/// sees the real cause + the hardware fix.
pub const CAMERA_CONTENTION_KIND: &str = "camera.power_contention";

/// The recovery action the supervisor executes (no unit lifecycle: the video
/// pipeline re-discovers via udev once the device re-enumerates).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraRecoveryAction {
    /// Unbind/rebind the camera leaf device (present-but-wedged).
    RebindDevice { bind_id: String },
    /// Re-enable a single hub downstream port (external hub with per-port power).
    CyclePort { hub: String, port: u32 },
    /// Unbind/rebind a whole hub (guard-proven isolated, boot-time-only).
    ResetHub { bind_id: String },
}

/// The plan returned to the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraRecoveryPlan {
    pub action: CameraRecoveryAction,
    pub attempt: u32,
}

/// The camera USB-recovery reconciler. Owns the trigger + retry machine; the
/// supervisor drives it from the monitor pass.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct CameraUsbRecovery {
    trigger: RehomeTrigger,
    /// The debounce the trigger was built with; rebuilt when config changes.
    hold: Duration,
    machine: RehomeMachine,
    last_tick: Option<Instant>,
    /// Set once an attempt is refused (guard) or only an alert is possible, so we
    /// hold instead of spinning. Cleared when the camera verifies healthy.
    held: bool,
    events: EventEmitter,
    // Sidecar fields.
    state: String,
    case: String,
    camera_present: bool,
    expected: bool,
    bind_id: String,
    hub: String,
    port: Option<u32>,
    ppps: bool,
    /// The camera shares its hub (with no per-port power) with the high-draw WFB
    /// radio — a power-contention brown-out risk. Surfaced in the sidecar; the
    /// diagnostic event is emitted once per false→true transition.
    power_contention: bool,
    contention_peer: String,
}

impl CameraUsbRecovery {
    pub fn new(events: EventEmitter) -> Self {
        CameraUsbRecovery {
            trigger: RehomeTrigger::with_hold(Duration::from_secs(config::DEFAULT_DEBOUNCE_S)),
            hold: Duration::from_secs(config::DEFAULT_DEBOUNCE_S),
            machine: RehomeMachine::new(),
            last_tick: None,
            held: false,
            events,
            state: "idle".to_string(),
            case: String::new(),
            camera_present: false,
            expected: false,
            bind_id: String::new(),
            hub: String::new(),
            port: None,
            ppps: false,
            power_contention: false,
            contention_peer: String::new(),
        }
    }

    #[cfg(target_os = "linux")]
    fn due(&self, interval: Duration, now: Instant) -> bool {
        match self.last_tick {
            None => true,
            Some(last) => now.duration_since(last) >= interval,
        }
    }

    /// One reconcile decision. Reads the camera-state sidecar, runs the trigger +
    /// retry machine + topology guard, mirrors state to the recovery sidecar, and
    /// returns a plan only when an attempt is authorized.
    #[cfg(target_os = "linux")]
    pub async fn decide(&mut self) -> Option<CameraRecoveryPlan> {
        let cfg = config::read_config();
        if !cfg.enabled {
            return None;
        }
        let now = Instant::now();
        if !self.due(cfg.tick_interval, now) {
            return None;
        }
        self.last_tick = Some(now);

        // Honor a changed debounce (rare; a reset on change is fine).
        if self.hold != cfg.debounce {
            self.trigger = RehomeTrigger::with_hold(cfg.debounce);
            self.hold = cfg.debounce;
        }

        // No fresh camera-state → not a running video drone (e.g. ground station):
        // stay idle, write no sidecar.
        let sig = os::read_camera_signals().await?;
        if !sig.fresh {
            return None;
        }

        let last_good = os::read_last_good().await;
        let expected = config::camera_expected(&cfg.expected, last_good.is_some());
        self.expected = expected;

        let ready = sig.state == "ready";
        let missing = sig.state == "missing";

        if ready {
            // Persist where the camera lives so the boot (absent) case can target
            // it; reset the trigger; let the machine clear the episode.
            self.held = false;
            self.camera_present = true;
            if let Some(path) = &sig.primary_path {
                self.persist_last_good(path).await;
                self.check_power_contention(path).await;
            }
        }

        let cond = expected && missing;
        let verified_healthy = ready;
        let armed = self.trigger.observe(cond, now);

        let schedule = cfg.cooldown_schedule.clone();
        let cooldown_for = |n: u32| {
            let idx = (n.saturating_sub(1) as usize).min(schedule.len().saturating_sub(1));
            Duration::from_secs(
                *schedule
                    .get(idx)
                    .unwrap_or(&config::DEFAULT_COOLDOWN_SCHEDULE_S[0]),
            )
        };

        let step = self.machine.step(
            armed,
            verified_healthy,
            cfg.max_attempts,
            cooldown_for,
            cfg.healthy_reset,
            now,
        );

        let plan = match step {
            RehomeStep::Recovered => {
                self.held = false;
                self.state = "idle".to_string();
                self.emit("success", &cfg, 0);
                None
            }
            RehomeStep::Exhausted => {
                if self.state != "exhausted" {
                    self.emit("exhausted", &cfg, self.machine.attempts());
                }
                self.state = "exhausted".to_string();
                None
            }
            RehomeStep::Cooldown { .. } => {
                self.state = "monitoring".to_string();
                None
            }
            RehomeStep::Idle => {
                if verified_healthy {
                    self.state = "idle".to_string();
                } else if cond {
                    // Armed window not yet elapsed.
                    self.state = "monitoring".to_string();
                }
                None
            }
            RehomeStep::Attempt { index } => {
                if self.held {
                    self.machine.refund_attempt();
                    None
                } else {
                    self.authorize_attempt(&cfg, &last_good, sig.primary_path.as_deref(), index)
                        .await
                }
            }
        };

        self.write_sidecar(self.machine.attempts(), cfg.max_attempts);
        plan
    }

    /// Resolve the camera's topology + pick the recovery case, running the guard.
    /// Returns a plan when an action is authorized, else holds (alert-only) and
    /// refunds the budget.
    #[cfg(target_os = "linux")]
    async fn authorize_attempt(
        &mut self,
        cfg: &CameraRecoveryConfig,
        last_good: &Option<os::LastGood>,
        primary_path: Option<&str>,
        index: u32,
    ) -> Option<CameraRecoveryPlan> {
        // Resolve the camera bind id: prefer the live device path, fall back to
        // the persisted last-known-good record.
        let cam_topo = match primary_path {
            Some(p) => topo::resolve_usb_topo_for_video(p).await,
            None => None,
        };
        let (bind_id, present, cam_for_guard) = if let Some(t) = cam_topo {
            (t.bind_id.clone(), true, t)
        } else if let Some(lg) = last_good {
            let present = os::device_present(&lg.bind_id);
            (
                lg.bind_id.clone(),
                present,
                UsbTopo {
                    bind_id: lg.bind_id.clone(),
                    ancestors: os::name_ancestors(&lg.bind_id),
                },
            )
        } else {
            // Expected (e.g. config=true) but never seen + not present: nothing to
            // target. Alert only.
            self.hold_alert("needs_hub_reset", "no_target", cfg, index);
            return None;
        };
        self.bind_id = bind_id.clone();
        self.camera_present = present;

        let (hub, port) = topo::hub_and_port(&bind_id).unwrap_or_default();
        self.hub = hub.clone();
        self.port = if hub.is_empty() { None } else { Some(port) };

        let (protected_paths, protected_usb) = os::build_protected_set().await;

        if present {
            // Case (a): leaf rebind. Provably safe iff disjoint from the protected
            // devices (it is — the camera is a leaf, not their hub).
            if topo::target_safe_as_leaf(&cam_for_guard, &protected_usb) {
                self.case = "present_wedged".to_string();
                self.state = "rebinding".to_string();
                self.emit("rebinding", cfg, index);
                return Some(CameraRecoveryPlan {
                    action: CameraRecoveryAction::RebindDevice { bind_id },
                    attempt: index,
                });
            }
            self.hold_alert("guard_blocked", "is_protected", cfg, index);
            return None;
        }

        // Absent. Case (c): a hub with per-port power → re-enable just the port.
        self.ppps = !hub.is_empty() && cfg.allow_ppps && topo::ppps_capable(&hub, port);
        if self.ppps {
            self.case = "port_cycle".to_string();
            self.state = "port_cycling".to_string();
            self.emit("port_cycling", cfg, index);
            return Some(CameraRecoveryPlan {
                action: CameraRecoveryAction::CyclePort { hub, port },
                attempt: index,
            });
        }

        // Case (b): shared hub, no per-port power. Default detect + alert; an
        // opt-in hub reset only inside the boot window AND only when the guard
        // proves the hub carries no protected device.
        if cfg.allow_hub_reset && os::within_boot_window(cfg.boot_reset_window) && !hub.is_empty() {
            let hub_topo = UsbTopo {
                bind_id: hub.clone(),
                ancestors: os::name_ancestors(&hub),
            };
            if topo::guard_verdict_multi(&hub_topo, &protected_paths) == GuardVerdict::Allow {
                self.case = "hub_reset".to_string();
                self.state = "hub_resetting".to_string();
                self.emit("hub_resetting", cfg, index);
                return Some(CameraRecoveryPlan {
                    action: CameraRecoveryAction::ResetHub { bind_id: hub },
                    attempt: index,
                });
            }
        }

        // Opt-in AGGRESSIVE shared-hub reset (bench/ground): the camera is wedged
        // on a hub it shares with the radio/FC and the hub exposes no per-port
        // power, so the only re-enumeration is a whole-hub reset the guard refuses.
        // An operator who set `allow_shared_hub_reset` accepts the brief radio+FC
        // re-enumeration to recover the camera without a manual replug — but only
        // while the FC is provably DISARMED (fail-closed on an unknown/absent armed
        // state), so this can never fire in flight. The radio re-pairs and the FC
        // reconnects on their own self-heal paths after the reset.
        if cfg.allow_shared_hub_reset && !hub.is_empty() {
            if matches!(os::read_fc_armed().await, Some(false)) {
                self.case = "hub_reset".to_string();
                self.state = "hub_resetting".to_string();
                self.emit_reason("hub_resetting", "shared_hub_aggressive", cfg, index);
                return Some(CameraRecoveryPlan {
                    action: CameraRecoveryAction::ResetHub { bind_id: hub },
                    attempt: index,
                });
            }
            self.hold_alert("needs_hub_reset", "armed_or_unknown", cfg, index);
            return None;
        }
        self.hold_alert("needs_hub_reset", "shared_hub", cfg, index);
        None
    }

    /// Record an alert-only outcome: refund the attempt, hold, and emit once.
    #[cfg(target_os = "linux")]
    fn hold_alert(&mut self, state: &str, reason: &str, cfg: &CameraRecoveryConfig, index: u32) {
        self.machine.refund_attempt();
        self.held = true;
        if self.state != state {
            self.emit_reason(state, reason, cfg, index);
        }
        self.case = "absent".to_string();
        self.state = state.to_string();
    }

    #[cfg(target_os = "linux")]
    fn emit(&self, state: &str, cfg: &CameraRecoveryConfig, attempt: u32) {
        self.emit_reason(state, "", cfg, attempt);
    }

    #[cfg(target_os = "linux")]
    fn emit_reason(&self, state: &str, reason: &str, cfg: &CameraRecoveryConfig, attempt: u32) {
        use ados_protocol::logd::{Fields, Level, Value};
        let mut d = Fields::new();
        d.insert("state".to_string(), Value::from(state));
        d.insert("bind_id".to_string(), Value::from(self.bind_id.as_str()));
        d.insert("hub".to_string(), Value::from(self.hub.as_str()));
        if let Some(p) = self.port {
            d.insert("port".to_string(), Value::from(p as u64));
        }
        d.insert("attempt".to_string(), Value::from(attempt as u64));
        d.insert(
            "max_attempts".to_string(),
            Value::from(cfg.max_attempts as u64),
        );
        d.insert("present".to_string(), Value::from(self.camera_present));
        if !reason.is_empty() {
            d.insert("reason".to_string(), Value::from(reason));
        }
        let level = match state {
            "exhausted" | "needs_hub_reset" | "guard_blocked" => Level::Warn,
            _ => Level::Info,
        };
        self.events.emit(CAMERA_RECOVERY_KIND, level, d);
    }

    #[cfg(target_os = "linux")]
    fn write_sidecar(&self, attempts: u32, max_attempts: u32) {
        #[derive(serde::Serialize)]
        struct Snap<'a> {
            camera_usb_recovery_state: &'a str,
            case: &'a str,
            attempts: u32,
            max_attempts: u32,
            camera_present: bool,
            expected: bool,
            bind_id: &'a str,
            hub: &'a str,
            port: Option<u32>,
            ppps_capable: bool,
            power_contention: bool,
            contention_peer: &'a str,
            updated_at_unix: u64,
        }
        let snap = Snap {
            camera_usb_recovery_state: &self.state,
            case: &self.case,
            attempts,
            max_attempts,
            camera_present: self.camera_present,
            expected: self.expected,
            bind_id: &self.bind_id,
            hub: &self.hub,
            port: self.port,
            ppps_capable: self.ppps,
            power_contention: self.power_contention,
            contention_peer: &self.contention_peer,
            updated_at_unix: os::now_unix(),
        };
        if let Err(e) = os::write_json_atomic(std::path::Path::new(os::SIDECAR_PATH), &snap, 0o644)
        {
            tracing::debug!(error = %e, "camera recovery sidecar write failed");
        }
    }

    /// Detect the power-contention brown-out risk: the camera sharing a hub (with
    /// no per-port power) with the high-draw WFB radio. The radio's TX spikes
    /// starve the camera on an over-subscribed shared hub — the real cause of the
    /// "works for hours then drops, port won't re-enable" symptom. Recomputed each
    /// ready tick (cheap sysfs reads); the diagnostic is emitted once per
    /// false→true transition so the operator sees the cause + the hardware fix.
    #[cfg(target_os = "linux")]
    async fn check_power_contention(&mut self, primary_path: &str) {
        let Some(cam) = topo::resolve_usb_topo_for_video(primary_path).await else {
            return;
        };
        let Some((cam_hub, cam_port)) = topo::hub_and_port(&cam.bind_id) else {
            return;
        };
        let radio = os::read_radio_topo().await;
        let radio_bind = radio.as_ref().map(|r| r.bind_id.clone());
        let shares_hub = radio
            .as_ref()
            .and_then(|r| topo::hub_and_port(&r.bind_id))
            .map(|(rhub, _)| rhub == cam_hub)
            .unwrap_or(false);
        // Contention only bites when the hub cannot isolate the port: a hub with
        // per-port power gives the camera its own budget, so it is not at risk.
        let now_contention = shares_hub && !topo::ppps_capable(&cam_hub, cam_port);

        if now_contention && !self.power_contention {
            use ados_protocol::logd::{Fields, Level, Value};
            let mut d = Fields::new();
            d.insert(
                "camera_bind_id".to_string(),
                Value::from(cam.bind_id.as_str()),
            );
            d.insert("hub".to_string(), Value::from(cam_hub.as_str()));
            d.insert(
                "radio_bind_id".to_string(),
                Value::from(radio_bind.as_deref().unwrap_or("")),
            );
            d.insert(
                "advice".to_string(),
                Value::from(
                    "camera shares an over-subscribed USB hub with the radio; move \
                     the camera to a separate port or a self-powered hub",
                ),
            );
            self.events.emit(CAMERA_CONTENTION_KIND, Level::Warn, d);
        }
        self.power_contention = now_contention;
        self.contention_peer = if now_contention {
            radio_bind.unwrap_or_default()
        } else {
            String::new()
        };
    }

    /// Persist where the camera lives (bind id + hub + port + ids) so the absent
    /// case can target it and `auto` expectation arms across reboots.
    #[cfg(target_os = "linux")]
    async fn persist_last_good(&self, primary_path: &str) {
        let Some(t) = topo::resolve_usb_topo_for_video(primary_path).await else {
            return;
        };
        let Some((hub, port)) = topo::hub_and_port(&t.bind_id) else {
            return;
        };
        let vid = os::read_sysfs_id(&t.bind_id, "idVendor").await;
        let pid = os::read_sysfs_id(&t.bind_id, "idProduct").await;
        let lg = os::LastGood {
            bind_id: t.bind_id,
            hub,
            port,
            vid,
            pid,
            updated_at_unix: os::now_unix(),
        };
        if let Err(e) = os::write_json_atomic(std::path::Path::new(os::LAST_GOOD_PATH), &lg, 0o644)
        {
            tracing::debug!(error = %e, "camera last-good write failed");
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn decide(&mut self) -> Option<CameraRecoveryPlan> {
        None
    }
}
