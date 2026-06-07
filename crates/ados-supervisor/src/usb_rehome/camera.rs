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

use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;

use super::machine::{RehomeMachine, RehomeTrigger};

#[cfg(target_os = "linux")]
use super::machine::RehomeStep;
#[cfg(target_os = "linux")]
use super::topo::{self, ControlPath, GuardVerdict, UsbTopo};

#[cfg(target_os = "linux")]
use crate::config::CONFIG_YAML;

/// The event kind recorded for a camera-recovery attempt + outcome.
pub const CAMERA_RECOVERY_KIND: &str = "camera.usb_recovery";

const DEFAULT_DEBOUNCE_S: u64 = 20;
const DEFAULT_MAX_ATTEMPTS: u32 = 3;
const DEFAULT_COOLDOWN_SCHEDULE_S: [u64; 3] = [10, 30, 60];
const DEFAULT_HEALTHY_RESET_S: u64 = 120;
const DEFAULT_TICK_INTERVAL_S: u64 = 5;
const DEFAULT_BOOT_RESET_WINDOW_S: u64 = 180;

#[cfg(target_os = "linux")]
const SIDECAR_PATH: &str = "/run/ados/camera-usb-recovery.json";
#[cfg(target_os = "linux")]
const CAMERA_STATE_PATH: &str = "/run/ados/camera-state.json";
#[cfg(target_os = "linux")]
const LAST_GOOD_PATH: &str = "/var/ados/camera-last-good.json";
#[cfg(target_os = "linux")]
const WFB_STATS_PATH: &str = "/run/ados/wfb-stats.json";
#[cfg(target_os = "linux")]
const USB_UNBIND_PATH: &str = "/sys/bus/usb/drivers/usb/unbind";
#[cfg(target_os = "linux")]
const USB_BIND_PATH: &str = "/sys/bus/usb/drivers/usb/bind";
#[cfg(target_os = "linux")]
const USB_DEVICES_DIR: &str = "/sys/bus/usb/devices";

/// Treat a camera-state snapshot older than this as unknown (do not act).
#[cfg(target_os = "linux")]
const STATE_FRESHNESS: Duration = Duration::from_secs(120);
/// Settle between the unbind and the bind / the disable toggle.
#[cfg(target_os = "linux")]
const SETTLE: Duration = Duration::from_millis(1500);
/// Hold the port disabled before re-enabling.
#[cfg(target_os = "linux")]
const PORT_DISABLE_HOLD: Duration = Duration::from_millis(600);

/// Configuration, read from `video.usb_recovery` (+ `video.camera.expected`).
/// Default-ON (detect + alert); destructive actions stay gated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraRecoveryConfig {
    pub enabled: bool,
    /// "auto" | "true" | "false" — whether a camera is expected on this rig.
    pub expected: String,
    pub debounce: Duration,
    pub max_attempts: u32,
    pub cooldown_schedule: Vec<u64>,
    pub healthy_reset: Duration,
    pub tick_interval: Duration,
    /// Opt-in: allow a shared-hub reset (boot-time-only, guard-gated).
    pub allow_hub_reset: bool,
    pub boot_reset_window: Duration,
    /// Allow a clean per-port re-enable on a hub that exposes it.
    pub allow_ppps: bool,
}

impl Default for CameraRecoveryConfig {
    fn default() -> Self {
        CameraRecoveryConfig {
            enabled: true,
            expected: "auto".to_string(),
            debounce: Duration::from_secs(DEFAULT_DEBOUNCE_S),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            cooldown_schedule: DEFAULT_COOLDOWN_SCHEDULE_S.to_vec(),
            healthy_reset: Duration::from_secs(DEFAULT_HEALTHY_RESET_S),
            tick_interval: Duration::from_secs(DEFAULT_TICK_INTERVAL_S),
            allow_hub_reset: false,
            boot_reset_window: Duration::from_secs(DEFAULT_BOOT_RESET_WINDOW_S),
            allow_ppps: true,
        }
    }
}

/// Parse `video.usb_recovery` + `video.camera.expected`. Absent / malformed →
/// enabled defaults.
pub fn read_config_from(text: &str) -> CameraRecoveryConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        video: Video,
    }
    #[derive(serde::Deserialize, Default)]
    struct Video {
        #[serde(default)]
        camera: Camera,
        #[serde(default)]
        usb_recovery: Option<Rec>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Camera {
        #[serde(default)]
        expected: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Rec {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        debounce_s: Option<u64>,
        #[serde(default)]
        max_attempts: Option<u32>,
        #[serde(default)]
        cooldown_schedule_s: Option<Vec<u64>>,
        #[serde(default)]
        healthy_reset_s: Option<u64>,
        #[serde(default)]
        tick_interval_s: Option<u64>,
        #[serde(default)]
        allow_hub_reset: Option<bool>,
        #[serde(default)]
        boot_reset_window_s: Option<u64>,
        #[serde(default)]
        allow_ppps: Option<bool>,
    }
    fn default_true() -> bool {
        true
    }
    let mut cfg = CameraRecoveryConfig::default();
    if let Ok(raw) = serde_norway::from_str::<Raw>(text) {
        if let Some(e) = raw.video.camera.expected {
            let e = e.trim().to_lowercase();
            if e == "true" || e == "false" || e == "auto" {
                cfg.expected = e;
            }
        }
        if let Some(r) = raw.video.usb_recovery {
            cfg.enabled = r.enabled;
            cfg.debounce = Duration::from_secs(r.debounce_s.unwrap_or(DEFAULT_DEBOUNCE_S).max(1));
            cfg.max_attempts = r.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS).max(1);
            cfg.cooldown_schedule = r
                .cooldown_schedule_s
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_COOLDOWN_SCHEDULE_S.to_vec());
            cfg.healthy_reset =
                Duration::from_secs(r.healthy_reset_s.unwrap_or(DEFAULT_HEALTHY_RESET_S).max(1));
            cfg.tick_interval =
                Duration::from_secs(r.tick_interval_s.unwrap_or(DEFAULT_TICK_INTERVAL_S).max(1));
            cfg.allow_hub_reset = r.allow_hub_reset.unwrap_or(false);
            cfg.boot_reset_window = Duration::from_secs(
                r.boot_reset_window_s
                    .unwrap_or(DEFAULT_BOOT_RESET_WINDOW_S)
                    .max(1),
            );
            cfg.allow_ppps = r.allow_ppps.unwrap_or(true);
        }
    }
    cfg
}

/// Resolve whether a camera is expected. `true`/`false` are explicit; `auto`
/// (default) expects a camera iff one enumerated successfully at least once on
/// this rig (the persisted last-known-good record exists — survives reboot, so
/// the boot case still arms). Pure.
pub fn camera_expected(expected_cfg: &str, last_good_exists: bool) -> bool {
    match expected_cfg {
        "true" => true,
        "false" => false,
        _ => last_good_exists,
    }
}

#[cfg(target_os = "linux")]
fn read_config() -> CameraRecoveryConfig {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_config_from(&t),
        Err(_) => CameraRecoveryConfig::default(),
    }
}

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

/// The camera-state snapshot read from the video pipeline's sidecar.
#[cfg(target_os = "linux")]
struct CameraSignals {
    state: String,
    primary_path: Option<String>,
    fresh: bool,
}

/// Persisted last-known-good camera record.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct LastGood {
    bind_id: String,
    hub: String,
    port: u32,
    #[serde(default)]
    vid: String,
    #[serde(default)]
    pid: String,
    #[serde(default)]
    updated_at_unix: u64,
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
}

impl CameraUsbRecovery {
    pub fn new(events: EventEmitter) -> Self {
        CameraUsbRecovery {
            trigger: RehomeTrigger::with_hold(Duration::from_secs(DEFAULT_DEBOUNCE_S)),
            hold: Duration::from_secs(DEFAULT_DEBOUNCE_S),
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
        let cfg = read_config();
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
        let sig = read_camera_signals().await?;
        if !sig.fresh {
            return None;
        }

        let last_good = read_last_good().await;
        let expected = camera_expected(&cfg.expected, last_good.is_some());
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
            }
        }

        let cond = expected && missing;
        let verified_healthy = ready;
        let armed = self.trigger.observe(cond, now);

        let schedule = cfg.cooldown_schedule.clone();
        let cooldown_for = |n: u32| {
            let idx = (n.saturating_sub(1) as usize).min(schedule.len().saturating_sub(1));
            Duration::from_secs(*schedule.get(idx).unwrap_or(&DEFAULT_COOLDOWN_SCHEDULE_S[0]))
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
        last_good: &Option<LastGood>,
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
            let present = device_present(&lg.bind_id);
            (
                lg.bind_id.clone(),
                present,
                UsbTopo {
                    bind_id: lg.bind_id.clone(),
                    ancestors: name_ancestors(&lg.bind_id),
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

        let (protected_paths, protected_usb) = build_protected_set().await;

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
        if cfg.allow_hub_reset && within_boot_window(cfg.boot_reset_window) && !hub.is_empty() {
            let hub_topo = UsbTopo {
                bind_id: hub.clone(),
                ancestors: name_ancestors(&hub),
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
            updated_at_unix: now_unix(),
        };
        if let Err(e) = write_json_atomic(std::path::Path::new(SIDECAR_PATH), &snap, 0o644) {
            tracing::debug!(error = %e, "camera recovery sidecar write failed");
        }
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
        let vid = read_sysfs_id(&t.bind_id, "idVendor").await;
        let pid = read_sysfs_id(&t.bind_id, "idProduct").await;
        let lg = LastGood {
            bind_id: t.bind_id,
            hub,
            port,
            vid,
            pid,
            updated_at_unix: now_unix(),
        };
        if let Err(e) = write_json_atomic(std::path::Path::new(LAST_GOOD_PATH), &lg, 0o644) {
            tracing::debug!(error = %e, "camera last-good write failed");
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn decide(&mut self) -> Option<CameraRecoveryPlan> {
        None
    }
}

/// Execute the sysfs op for an authorized plan. Best-effort; the video pipeline
/// re-discovers the camera via udev once it re-enumerates.
#[cfg(target_os = "linux")]
pub async fn execute_camera_recovery(plan: &CameraRecoveryPlan) {
    match &plan.action {
        CameraRecoveryAction::RebindDevice { bind_id } => {
            tracing::warn!(bind_id = %bind_id, attempt = plan.attempt, "camera_recovery_rebind");
            rebind(bind_id).await;
        }
        CameraRecoveryAction::ResetHub { bind_id } => {
            tracing::warn!(bind_id = %bind_id, attempt = plan.attempt, "camera_recovery_hub_reset");
            rebind(bind_id).await;
        }
        CameraRecoveryAction::CyclePort { hub, port } => {
            tracing::warn!(hub = %hub, port = port, attempt = plan.attempt, "camera_recovery_port_cycle");
            let attr = format!("{}/{}", USB_DEVICES_DIR, topo::port_disable_rel(hub, *port));
            if let Err(e) = sysfs_write(&attr, "1").await {
                tracing::warn!(error = %e, "camera recovery port disable failed");
            }
            tokio::time::sleep(PORT_DISABLE_HOLD).await;
            if let Err(e) = sysfs_write(&attr, "0").await {
                tracing::warn!(error = %e, "camera recovery port enable failed");
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn rebind(bind_id: &str) {
    if let Err(e) = sysfs_write(USB_UNBIND_PATH, bind_id).await {
        tracing::warn!(error = %e, "camera recovery unbind failed");
    }
    tokio::time::sleep(SETTLE).await;
    if let Err(e) = sysfs_write(USB_BIND_PATH, bind_id).await {
        tracing::warn!(error = %e, "camera recovery bind failed");
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn execute_camera_recovery(_plan: &CameraRecoveryPlan) {}

#[cfg(target_os = "linux")]
async fn sysfs_write(path: &str, val: &str) -> std::io::Result<()> {
    tokio::fs::write(path, val).await
}

/// Read the camera-state sidecar. `None` when absent.
#[cfg(target_os = "linux")]
async fn read_camera_signals() -> Option<CameraSignals> {
    let txt = tokio::fs::read_to_string(CAMERA_STATE_PATH).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let state = v.get("state")?.as_str()?.to_string();
    let primary_path = v
        .get("primary_path")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let updated = v
        .get("updated_at_unix")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    let fresh = updated > 0.0 && {
        let age = now_unix().saturating_sub(updated as u64);
        Duration::from_secs(age) <= STATE_FRESHNESS
    };
    Some(CameraSignals {
        state,
        primary_path,
        fresh,
    })
}

#[cfg(target_os = "linux")]
async fn read_last_good() -> Option<LastGood> {
    let txt = tokio::fs::read_to_string(LAST_GOOD_PATH).await.ok()?;
    serde_json::from_str::<LastGood>(&txt).ok()
}

/// The WFB radio interface, from the radio's stats sidecar (for the guard set).
#[cfg(target_os = "linux")]
async fn read_wfb_iface() -> Option<String> {
    let txt = tokio::fs::read_to_string(WFB_STATS_PATH).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let iface = v.get("interface")?.as_str()?.to_string();
    if iface.is_empty() {
        None
    } else {
        Some(iface)
    }
}

/// Build the protected set: the management link AND the WFB radio AND the FC. A
/// hub reset must disturb none of them.
#[cfg(target_os = "linux")]
async fn build_protected_set() -> (Vec<ControlPath>, Vec<UsbTopo>) {
    let mut paths = Vec::new();
    let mut usb = Vec::new();

    let default_iface = crate::mgmt_link_guardian::detection::default_route_iface().await;
    let control = topo::resolve_control_path(default_iface.as_deref()).await;
    if let ControlPath::Usb(t) = &control {
        usb.push(t.clone());
    }
    paths.push(control);

    if let Some(iface) = read_wfb_iface().await {
        if let Some(t) = topo::resolve_usb_topo(&iface).await {
            usb.push(t.clone());
            paths.push(ControlPath::Usb(t));
        }
    }

    for tty in ["ttyACM0", "ttyACM1", "ttyUSB0", "ttyUSB1"] {
        if let Some(t) = topo::resolve_usb_topo_for_tty(tty).await {
            usb.push(t.clone());
            paths.push(ControlPath::Usb(t));
        }
    }

    (paths, usb)
}

/// Synthesize a device node's USB-node ancestors purely from its bind id, e.g.
/// `1-1.1 -> ["1-1", "usb1"]`. Used when the device is absent so it cannot be
/// walked in `/sys`.
#[cfg(any(target_os = "linux", test))]
fn name_ancestors(bind_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = bind_id.to_string();
    for _ in 0..8 {
        match topo_hub_and_port(&cur) {
            Some((hub, _)) => {
                out.push(hub.clone());
                if hub.starts_with("usb") {
                    break;
                }
                cur = hub;
            }
            None => break,
        }
    }
    out
}

/// Local mirror of `topo::hub_and_port` so `name_ancestors` is pure + buildable
/// on every host (the topo fn is the same logic).
#[cfg(any(target_os = "linux", test))]
fn topo_hub_and_port(bind_id: &str) -> Option<(String, u32)> {
    if let Some(idx) = bind_id.rfind('.') {
        let hub = bind_id[..idx].to_string();
        let port = bind_id[idx + 1..].parse::<u32>().ok()?;
        if hub.is_empty() {
            return None;
        }
        return Some((hub, port));
    }
    let (bus, port) = bind_id.split_once('-')?;
    let busn = bus.parse::<u32>().ok()?;
    let portn = port.parse::<u32>().ok()?;
    Some((format!("usb{}", busn), portn))
}

#[cfg(target_os = "linux")]
fn device_present(bind_id: &str) -> bool {
    std::path::Path::new(USB_DEVICES_DIR)
        .join(bind_id)
        .join("idVendor")
        .is_file()
}

#[cfg(target_os = "linux")]
async fn read_sysfs_id(bind_id: &str, attr: &str) -> String {
    let p = format!("{}/{}/{}", USB_DEVICES_DIR, bind_id, attr);
    tokio::fs::read_to_string(&p)
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Whether the box is still inside the post-boot window (a proxy for "on the
/// ground, not yet flying"). Reads `/proc/uptime`.
#[cfg(target_os = "linux")]
fn within_boot_window(window: Duration) -> bool {
    match std::fs::read_to_string("/proc/uptime") {
        Ok(s) => s
            .split_whitespace()
            .next()
            .and_then(|f| f.parse::<f64>().ok())
            .map(|up| up <= window.as_secs() as f64)
            .unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn write_json_atomic<T: serde::Serialize>(
    path: &std::path::Path,
    value: &T,
    mode: u32,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_is_enabled_with_defaults() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert_eq!(cfg.expected, "auto");
        assert_eq!(cfg.max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(cfg.debounce, Duration::from_secs(DEFAULT_DEBOUNCE_S));
        assert!(!cfg.allow_hub_reset);
        assert!(cfg.allow_ppps);
    }

    #[test]
    fn explicit_tunables_and_expected() {
        let cfg = read_config_from(
            "video:\n  camera:\n    expected: \"true\"\n  usb_recovery:\n    enabled: false\n    debounce_s: 5\n    max_attempts: 2\n    allow_hub_reset: true\n    allow_ppps: false\n",
        );
        assert!(!cfg.enabled);
        assert_eq!(cfg.expected, "true");
        assert_eq!(cfg.debounce, Duration::from_secs(5));
        assert_eq!(cfg.max_attempts, 2);
        assert!(cfg.allow_hub_reset);
        assert!(!cfg.allow_ppps);
    }

    #[test]
    fn malformed_config_defaults_enabled() {
        assert!(read_config_from(": : : not yaml").enabled);
    }

    #[test]
    fn expected_resolution_matrix() {
        // auto = expected iff a last-good record exists.
        assert!(!camera_expected("auto", false));
        assert!(camera_expected("auto", true));
        // Explicit wins regardless of last-good. This is the key anti-false-
        // positive guarantee: a camera-less drone with no last-good stays idle.
        assert!(camera_expected("true", false));
        assert!(!camera_expected("false", true));
    }

    #[test]
    fn name_ancestors_synthesizes_from_bind_id() {
        assert_eq!(
            name_ancestors("1-1.1"),
            vec!["1-1".to_string(), "usb1".to_string()]
        );
        assert_eq!(name_ancestors("1-1"), vec!["usb1".to_string()]);
        assert_eq!(
            name_ancestors("2-1.4.3"),
            vec!["2-1.4".to_string(), "2-1".to_string(), "usb2".to_string()]
        );
    }
}
