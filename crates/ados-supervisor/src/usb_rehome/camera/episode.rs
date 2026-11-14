//! One camera's recovery episode: its trigger, retry machine, topology case
//! and status, and the decision it takes on each tick.

use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;
use ados_protocol::logd::{Fields, Level, Value};

use super::super::machine::{RehomeMachine, RehomeStep, RehomeTrigger};
use super::super::topo::{self, GuardVerdict, UsbTopo};
use super::config::{camera_expected, CameraRecoveryConfig};
use super::os::{self, CameraOs, LastGood};
use super::targets::Target;
use super::{CameraRecoveryAction, CameraRecoveryPlan, CAMERA_RECOVERY_KIND};

mod status;

/// One camera's recovery state.
pub(super) struct Episode {
    pub(super) id: String,
    pub(super) primary: bool,
    trigger: RehomeTrigger,
    machine: RehomeMachine,
    /// Set once an attempt is refused (guard) or only an alert is possible, so
    /// we hold instead of spinning. Cleared when the camera verifies healthy.
    held: bool,
    /// When this camera last had a USB action authorized: the fair order in
    /// which cameras share the bus-wide action gate.
    pub(super) last_action: Option<Instant>,
    // Sidecar fields.
    pub(super) state: String,
    case: String,
    camera_present: bool,
    expected: bool,
    bind_id: String,
    hub: String,
    port: Option<u32>,
    ppps: bool,
    /// The camera shares its hub (with no per-port power) with the high-draw
    /// WFB radio — a power-contention brown-out risk. Surfaced in the sidecar;
    /// the diagnostic event is emitted once per false→true transition.
    power_contention: bool,
    contention_peer: String,
}

impl Episode {
    pub(super) fn new(target: &Target, hold: Duration) -> Self {
        Episode {
            id: target.id.clone(),
            primary: target.primary,
            trigger: RehomeTrigger::with_hold(hold),
            machine: RehomeMachine::new(),
            held: false,
            last_action: None,
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

    /// Rebuild the trigger for a changed debounce (rare; a reset is fine).
    pub(super) fn set_hold(&mut self, hold: Duration) {
        self.trigger = RehomeTrigger::with_hold(hold);
    }

    /// One reconcile decision for this camera. `gate_open` is whether a USB
    /// action may run this tick at all (one per tick, spaced bus-wide); an
    /// attempt the gate refuses is refunded and taken on a later tick.
    pub(super) async fn tick<O: CameraOs>(
        &mut self,
        os: &O,
        cfg: &CameraRecoveryConfig,
        target: &Target,
        now: Instant,
        gate_open: bool,
        events: &EventEmitter,
    ) -> Option<CameraRecoveryPlan> {
        self.primary = target.primary;
        let last_good = os.read_last_good(target.primary, &target.id).await;
        let expected = camera_expected(&cfg.expected, last_good.is_some());
        self.expected = expected;

        let ready = target.state == "ready";
        let missing = target.state == "missing";

        if ready {
            // Persist where the camera lives so the absent case can target it;
            // let the machine clear the episode.
            self.held = false;
            self.camera_present = true;
            if let Some(path) = &target.device_path {
                self.persist_last_good(os, path).await;
                self.check_power_contention(os, path, events).await;
            }
        }

        let cond = expected && missing;
        let armed = self.trigger.observe(cond, now);
        let step = self
            .machine
            .step(armed, ready, cfg.cooldown, cfg.healthy_reset, now);

        match step {
            RehomeStep::Recovered => {
                self.held = false;
                self.state = "idle".to_string();
                self.emit("success", "", cfg, 0, events);
                None
            }
            RehomeStep::Cooldown { .. } => {
                // Still failing, still trying. Announce once per episode, so a
                // camera that has not come back is loud without the vehicle
                // having given up on it.
                if self.state != "retrying" {
                    self.emit("retrying", "", cfg, self.machine.attempts(), events);
                }
                self.state = "retrying".to_string();
                None
            }
            RehomeStep::Idle => {
                if ready {
                    self.state = "idle".to_string();
                } else if cond {
                    // Armed window not yet elapsed.
                    self.state = "monitoring".to_string();
                }
                None
            }
            RehomeStep::Attempt { index } => {
                if self.held || !gate_open {
                    self.machine.refund_attempt();
                    return None;
                }
                let plan = self
                    .authorize_attempt(os, cfg, &last_good, target, index, events)
                    .await;
                if plan.is_some() {
                    self.last_action = Some(now);
                }
                plan
            }
        }
    }

    /// Resolve the camera's topology + pick the recovery case, running the
    /// guard. Returns a plan when an action is authorized, else holds
    /// (alert-only) and refunds the budget.
    async fn authorize_attempt<O: CameraOs>(
        &mut self,
        os: &O,
        cfg: &CameraRecoveryConfig,
        last_good: &Option<LastGood>,
        target: &Target,
        index: u32,
        events: &EventEmitter,
    ) -> Option<CameraRecoveryPlan> {
        // Resolve the camera bind id: prefer the live device node, fall back to
        // the persisted last-known-good record.
        let cam_topo = match &target.device_path {
            Some(p) => os.video_topo(p).await,
            None => None,
        };
        let (bind_id, present, cam_for_guard) = if let Some(t) = cam_topo {
            (t.bind_id.clone(), true, t)
        } else if let Some(lg) = last_good {
            let present = os.device_present(&lg.bind_id);
            (
                lg.bind_id.clone(),
                present,
                UsbTopo {
                    bind_id: lg.bind_id.clone(),
                    ancestors: os::name_ancestors(&lg.bind_id),
                },
            )
        } else {
            // Expected (e.g. config=true) but never seen + not present: nothing
            // to target. Alert only.
            self.hold_alert("needs_hub_reset", "no_target", cfg, index, events);
            return None;
        };
        self.bind_id = bind_id.clone();
        self.camera_present = present;

        let (hub, port) = topo::hub_and_port(&bind_id).unwrap_or_default();
        self.hub = hub.clone();
        self.port = if hub.is_empty() { None } else { Some(port) };

        let (protected_paths, protected_usb) = os.protected_set().await;
        let plan = |action| {
            Some(CameraRecoveryPlan {
                camera_id: self.id.clone(),
                action,
                attempt: index,
            })
        };

        if present {
            // Case (a): leaf rebind. Provably safe iff disjoint from the
            // protected devices (it is — the camera is a leaf, not their hub).
            if topo::target_safe_as_leaf(&cam_for_guard, &protected_usb) {
                self.case = "present_wedged".to_string();
                self.state = "rebinding".to_string();
                self.emit("rebinding", "", cfg, index, events);
                return plan(CameraRecoveryAction::RebindDevice { bind_id });
            }
            self.hold_alert("guard_blocked", "is_protected", cfg, index, events);
            return None;
        }

        // Absent. Case (c): a hub with per-port power → re-enable just the port.
        self.ppps = !hub.is_empty() && cfg.allow_ppps && os.ppps_capable(&hub, port);
        if self.ppps {
            self.case = "port_cycle".to_string();
            self.state = "port_cycling".to_string();
            self.emit("port_cycling", "", cfg, index, events);
            return plan(CameraRecoveryAction::CyclePort { hub, port });
        }

        // Case (b): shared hub, no per-port power. Default detect + alert; an
        // opt-in hub reset only inside the boot window AND only when the guard
        // proves the hub carries no protected device.
        if cfg.allow_hub_reset && os.within_boot_window(cfg.boot_reset_window) && !hub.is_empty() {
            let hub_topo = UsbTopo {
                bind_id: hub.clone(),
                ancestors: os::name_ancestors(&hub),
            };
            if topo::guard_verdict_multi(&hub_topo, &protected_paths) == GuardVerdict::Allow {
                self.case = "hub_reset".to_string();
                self.state = "hub_resetting".to_string();
                self.emit("hub_resetting", "", cfg, index, events);
                return plan(CameraRecoveryAction::ResetHub { bind_id: hub });
            }
        }

        // Opt-in AGGRESSIVE shared-hub reset (bench/ground): the camera is
        // wedged on a hub it shares with the radio/FC and the hub exposes no
        // per-port power, so the only re-enumeration is a whole-hub reset the
        // guard refuses. An operator who set `allow_shared_hub_reset` accepts
        // the brief radio+FC re-enumeration to recover the camera without a
        // manual replug — but only while the FC is provably DISARMED
        // (fail-closed on an unknown/absent armed state), so this can never
        // fire in flight. The radio re-pairs and the FC reconnects on their own
        // self-heal paths after the reset.
        if cfg.allow_shared_hub_reset && !hub.is_empty() {
            if matches!(os.fc_armed().await, Some(false)) {
                self.case = "hub_reset".to_string();
                self.state = "hub_resetting".to_string();
                self.emit("hub_resetting", "shared_hub_aggressive", cfg, index, events);
                return plan(CameraRecoveryAction::ResetHub { bind_id: hub });
            }
            self.hold_alert("needs_hub_reset", "armed_or_unknown", cfg, index, events);
            return None;
        }
        self.hold_alert("needs_hub_reset", "shared_hub", cfg, index, events);
        None
    }

    /// Record an alert-only outcome: refund the attempt, hold, and emit once.
    fn hold_alert(
        &mut self,
        state: &str,
        reason: &str,
        cfg: &CameraRecoveryConfig,
        index: u32,
        events: &EventEmitter,
    ) {
        self.machine.refund_attempt();
        self.held = true;
        if self.state != state {
            self.emit(state, reason, cfg, index, events);
        }
        self.case = "absent".to_string();
        self.state = state.to_string();
    }

    fn emit(
        &self,
        state: &str,
        reason: &str,
        cfg: &CameraRecoveryConfig,
        attempt: u32,
        events: &EventEmitter,
    ) {
        let mut d = Fields::new();
        d.insert("state".to_string(), Value::from(state));
        d.insert("camera".to_string(), Value::from(self.id.as_str()));
        d.insert("bind_id".to_string(), Value::from(self.bind_id.as_str()));
        d.insert("hub".to_string(), Value::from(self.hub.as_str()));
        if let Some(p) = self.port {
            d.insert("port".to_string(), Value::from(p as u64));
        }
        d.insert("attempt".to_string(), Value::from(attempt as u64));
        d.insert(
            "cooldown_s".to_string(),
            Value::from(cfg.cooldown.as_secs()),
        );
        d.insert("present".to_string(), Value::from(self.camera_present));
        if !reason.is_empty() {
            d.insert("reason".to_string(), Value::from(reason));
        }
        let level = match state {
            // `retrying` replaces the old terminal `exhausted`: still loud,
            // because a camera that has not come back is worth an operator's
            // attention, but no longer an announcement that we stopped trying.
            "retrying" | "needs_hub_reset" | "guard_blocked" => Level::Warn,
            _ => Level::Info,
        };
        events.emit(CAMERA_RECOVERY_KIND, level, d);
    }
}
