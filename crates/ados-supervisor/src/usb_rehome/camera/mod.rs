//! Camera USB-recovery self-heal (force re-enumeration of an absent/wedged camera).
//!
//! When an *expected* camera is reported missing and stays missing for a
//! confirm window, the agent forces a USB re-enumeration so a camera that failed
//! its cold-boot port-enable (the kernel tries once and gives up), dropped off
//! the bus in a brown-out, or wedged under its reader comes back without a human
//! reseating the cable. Three cases, topology-resolved:
//!
//!   (a) the camera device is still in `/sys` but reads missing (wedged: the
//!       pipeline's discovery cannot open it, or the vision engine reports it
//!       silent) → unbind/rebind the camera leaf only. Clean, no sibling effect.
//!   (b) the camera is gone and sits on a hub it SHARES with the radio/FC (the
//!       Pi 4B internal hub) → default detect + alert (`needs_hub_reset`); an
//!       opt-in, boot-time-only hub reset is allowed ONLY when the guard proves
//!       no protected device hangs off that hub.
//!   (c) the camera is gone but its hub exposes per-port power (an external hub)
//!       → re-enable just the camera's port. Clean.
//!
//! Every local camera the video pipeline lists in `camera-state.json` is
//! watched — the primary, a camera the vision engine captures, and each
//! secondary (belly) leg — each in its own episode with its own trigger, retry
//! machine and location record. USB actions are rationed bus-wide: at most one
//! per tick, and none within the cooldown of the last one on any camera, so a
//! brown-out that drops two cameras off one hub gets one re-enumeration at a
//! time instead of two racing each other; the camera whose last action is
//! oldest goes first, so none is starved.
//!
//! Reuses the `usb_rehome` trigger + bounded retry/cooldown machine + the
//! fail-closed topology guard. The reconciler decides + records; the supervisor
//! executes the sysfs op. Pipeline recovery itself is owned by the udev→SIGUSR1
//! path in the video service, so this never restarts `ados-video`. Default-ON
//! (detect + alert); actions gated under `video.usb_recovery`.
//!
//! Module layout:
//! - `config`: `CameraRecoveryConfig` + parsing + `camera_expected` resolution.
//! - `targets`: the cameras read off the camera-state sidecar.
//! - `episode`: one camera's trigger + retry machine + topology guard.
//! - `os`: the OS seam (`CameraOs`) and its Linux implementation, the
//!   protected-set builder, and the sysfs executor (`execute_camera_recovery`).
//! - this module root: the plan types and the `CameraUsbRecovery` reconciler
//!   that runs every camera's episode under the shared action gate.

#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;

pub mod config;
#[cfg(target_os = "linux")]
mod episode;
pub mod os;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod targets;

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
    /// The roster id of the camera the action recovers.
    pub camera_id: String,
    pub action: CameraRecoveryAction,
    pub attempt: u32,
}

/// The camera USB-recovery reconciler. Owns one episode per watched camera;
/// the supervisor drives it from the monitor pass.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct CameraUsbRecovery {
    #[cfg(target_os = "linux")]
    episodes: BTreeMap<String, episode::Episode>,
    /// The debounce the triggers were built with; rebuilt when config changes.
    hold: Duration,
    last_tick: Option<Instant>,
    /// When the last USB action on any camera was authorized.
    last_action: Option<Instant>,
    events: EventEmitter,
}

impl CameraUsbRecovery {
    pub fn new(events: EventEmitter) -> Self {
        CameraUsbRecovery {
            #[cfg(target_os = "linux")]
            episodes: BTreeMap::new(),
            hold: Duration::from_secs(config::DEFAULT_DEBOUNCE_S),
            last_tick: None,
            last_action: None,
            events,
        }
    }

    #[cfg(target_os = "linux")]
    fn due(&self, interval: Duration, now: Instant) -> bool {
        match self.last_tick {
            None => true,
            Some(last) => now.duration_since(last) >= interval,
        }
    }

    /// One reconcile decision against the live system.
    #[cfg(target_os = "linux")]
    pub async fn decide(&mut self) -> Option<CameraRecoveryPlan> {
        self.decide_with(&os::LinuxOs, Instant::now()).await
    }

    /// One reconcile decision. Reads the camera-state sidecar, runs each
    /// camera's trigger + retry machine + topology guard under the shared
    /// action gate, mirrors state to the recovery sidecar, and returns a plan
    /// only when an attempt is authorized.
    #[cfg(target_os = "linux")]
    pub(crate) async fn decide_with<O: os::CameraOs>(
        &mut self,
        os: &O,
        now: Instant,
    ) -> Option<CameraRecoveryPlan> {
        let cfg = os.config();
        if !cfg.enabled {
            return None;
        }
        if !self.due(cfg.tick_interval, now) {
            return None;
        }
        self.last_tick = Some(now);

        // Honor a changed debounce (rare; a reset on change is fine).
        if self.hold != cfg.debounce {
            self.hold = cfg.debounce;
            for ep in self.episodes.values_mut() {
                ep.set_hold(cfg.debounce);
            }
        }

        // No fresh camera-state → not a running video drone (e.g. ground
        // station): stay idle, write no sidecar.
        let doc = os.camera_state().await?;
        if !targets::fresh(&doc, os.now_unix()) {
            return None;
        }
        let targets = targets::targets_from(&doc);
        self.episodes
            .retain(|id, _| targets.iter().any(|t| &t.id == id));

        // Fair order through the gate: the camera acted on longest ago first.
        let mut order: Vec<&targets::Target> = targets.iter().collect();
        order.sort_by_key(|t| self.episodes.get(&t.id).and_then(|e| e.last_action));

        let mut plan = None;
        for target in order {
            let gate_open = plan.is_none()
                && self
                    .last_action
                    .is_none_or(|at| now.saturating_duration_since(at) >= cfg.cooldown);
            let ep = self
                .episodes
                .entry(target.id.clone())
                .or_insert_with(|| episode::Episode::new(target, cfg.debounce));
            if let Some(p) = ep
                .tick(os, &cfg, target, now, gate_open, &self.events)
                .await
            {
                self.last_action = Some(now);
                plan = Some(p);
            }
        }

        os.write_sidecar(&self.sidecar(&targets, cfg.cooldown.as_secs(), os.now_unix()));
        plan
    }

    /// The recovery sidecar: every camera's episode under `cameras`, and at the
    /// top level the one an operator should see — the first camera in a
    /// recovery, else the primary — so a reader of the single-camera shape
    /// still sees a recovering belly camera.
    #[cfg(target_os = "linux")]
    fn sidecar(
        &self,
        targets: &[targets::Target],
        cooldown_s: u64,
        now_unix: u64,
    ) -> serde_json::Value {
        let listed: Vec<&episode::Episode> = targets
            .iter()
            .filter_map(|t| self.episodes.get(&t.id))
            .collect();
        let focus = listed
            .iter()
            .find(|e| e.state != "idle")
            .or_else(|| listed.iter().find(|e| e.primary))
            .or_else(|| listed.first());
        let mut body = match focus {
            Some(e) => serde_json::to_value(e.snapshot()).unwrap_or_default(),
            None => serde_json::json!({"camera_usb_recovery_state": "idle"}),
        };
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "version".into(),
                os::CAMERA_USB_RECOVERY_SIDECAR_VERSION.into(),
            );
            obj.insert("cooldown_s".into(), cooldown_s.into());
            obj.insert(
                "cameras".into(),
                serde_json::to_value(listed.iter().map(|e| e.snapshot()).collect::<Vec<_>>())
                    .unwrap_or_default(),
            );
            obj.insert("updated_at_unix".into(), now_unix.into());
        }
        body
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn decide(&mut self) -> Option<CameraRecoveryPlan> {
        None
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
