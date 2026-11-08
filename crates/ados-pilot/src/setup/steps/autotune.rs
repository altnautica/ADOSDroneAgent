//! `autotune`: the FC's own autotune, with the pilot as the RC proxy.
//!
//! Copter (and the VTOL motors in QAUTOTUNE): take off to 8 m, turn nose
//! into the wind, set `AUTOTUNE_AXES` 3 (roll + pitch) and `AUTOTUNE_AGGR`
//! 0.075, switch to AUTOTUNE and stream centred MANUAL_CONTROL at 50 Hz with
//! the operator's sticks above 0.3 blended in. Gains are saved by landing
//! and disarming in AUTOTUNE. Yaw follows with battery ≥ 50 %, or later as a
//! resumable sub-step after a battery swap.
//!
//! Plane: AUTOTUNE mode with `AUTOTUNE_LEVEL` 6; the pilot generates roll and
//! pitch doublets and steers back to the box between them until both axes
//! report done or 4 minutes pass, then returns to a GUIDED loiter (the mode
//! exit saves FF and I; P and D are saved as each axis completes).
//!
//! PX4: Position mode and MAV_CMD_DO_AUTOTUNE_ENABLE, polled each second;
//! the ACK reports in progress, accepted (done) or failed.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavResult};
use serde_json::{json, Map, Value};

use crate::setup::adapters::Mode;
use crate::setup::doublets::Doublets;
use crate::setup::fcstate::{Class, Fw};
use crate::setup::flight::{self, height, plane_loiter, Hop, Leg, PLANE_LOITER_ALT_M};
use crate::setup::schema::{obj, Action};
use crate::setup::statustext::{parse_tune, Axis, TuneEvent};
use crate::setup::step::{step_core, ActErr, Cx, Step, StepCore};

use super::plane_air::ToLoiter;

mod helpers;
mod phases;

pub const TUNE_ALT_M: f32 = 8.0;
pub const AGGR: f32 = 0.075;
pub const AXES_ROLL_PITCH: f32 = 3.0;
/// Yaw (4) plus yaw D (8) on firmware that has it (4.5+); yaw alone before.
pub const AXES_YAW: f32 = 12.0;
pub const AXES_YAW_OLD: f32 = 4.0;
pub const PLANE_LEVEL: f32 = 6.0;
pub const PLANE_MAX_MS: u64 = 240_000;
pub const YAW_MIN_BATTERY: i8 = 50;
/// Operator stick deflection that is blended in.
pub const BLEND: f32 = 0.3;
const MANUAL_MS: u64 = 20;
const SETTLE_MS: u64 = 5000;
const PRESTREAM_MS: u64 = 1000;
/// Time in the hold mode before switching to the tune mode.
const HOLD_MS: u64 = 1000;
/// The FC has refused the tune (or hold) mode when it is not in it by now.
const TUNE_ENTER_MS: u64 = 10_000;
const POLL_MS: u64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    /// Copter-style tune (copter, VTOL in QAUTOTUNE, PX4).
    Copter,
    /// Fixed-wing tune (plane, VTOL forward flight).
    Plane,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    TakeOff,
    Settle {
        since: u64,
    },
    Params,
    Prestream {
        since: u64,
    },
    /// ArduPilot copter-style: in the hold mode the tune mode is entered from.
    Hold {
        since: u64,
    },
    Tuning {
        since: u64,
    },
    LandSave,
    Saved,
    /// Waiting for the operator: battery swap before yaw.
    YawWait,
    PlanePosition,
    PlaneTuning {
        since: u64,
    },
    PlaneExit,
    ReadGains,
}

pub struct Autotune {
    core: StepCore,
    hop: Hop,
    to: ToLoiter,
    stage: Stage,
    phase: Phase,
    yaw_round: bool,
    copter_done: Vec<Axis>,
    plane_done: Vec<Axis>,
    doublets: Option<Doublets>,
    last_manual: u64,
    last_poll: u64,
    text_cursor: u64,
    before: Map<String, Value>,
    notes: Vec<Value>,
    /// The FC took the tune mode at least once this round.
    entered: bool,
}

impl Default for Autotune {
    fn default() -> Self {
        Autotune {
            core: StepCore::default(),
            hop: Hop::default(),
            to: ToLoiter::default(),
            stage: Stage::Copter,
            phase: Phase::Idle,
            yaw_round: false,
            copter_done: Vec::new(),
            plane_done: Vec::new(),
            doublets: None,
            last_manual: 0,
            last_poll: 0,
            text_cursor: 0,
            before: Map::new(),
            notes: Vec::new(),
            entered: false,
        }
    }
}

/// Centred MANUAL_CONTROL with the operator's large deflections blended in.
pub fn blended(axes: [f32; 4]) -> (f32, f32, f32, f32) {
    let b = |v: f32| if v.abs() > BLEND { v } else { 0.0 };
    let [roll, pitch, thr, yaw] = axes;
    (b(pitch), b(roll), 0.5 + 0.5 * b(thr), b(yaw))
}

impl Step for Autotune {
    step_core!("autotune");

    fn flight(&self) -> bool {
        true
    }

    fn blends_sticks(&self) -> bool {
        true
    }

    fn begin(&mut self, cx: &mut Cx) {
        *self = Autotune::default();
        self.core.body_variant = match (cx.adapter.fw, cx.adapter.class) {
            (Fw::Px4, _) => Some("px4"),
            (_, Class::Fixedwing) => Some("plane"),
            (_, Class::Vtol) => Some("vtol"),
            _ => None,
        };
        let mut actions = vec![Action::primary("start")];
        if cx.data.autotune_yaw_pending {
            actions.push(Action::secondary("resume_yaw"));
        }
        actions.push(Action::danger("abort"));
        self.core.wait_user(actions);
    }

    fn tick(&mut self, cx: &mut Cx) {
        if !matches!(self.hop.leg, Leg::Idle) {
            self.hop.tick(cx);
            if let Leg::Failed(w) = self.hop.leg {
                self.hop.leg = Leg::Idle;
                self.phase = Phase::Idle;
                self.core.fail(&format!("setup.blocker.{w}"));
                return;
            }
        }
        let axes = cx.inputs.axes;
        match self.phase {
            Phase::Idle | Phase::YawWait => {}
            Phase::TakeOff => {
                if self.hop.leg == Leg::Holding {
                    // Nose into the wind (WIND.direction is where it blows from).
                    if let (Some((dir, _, _)), Some((l, _))) = (cx.fc.wind, cx.fc.local) {
                        self.hop.set_hold(l[0], l[1], l[2], dir.to_radians());
                    }
                    self.phase = Phase::Settle { since: cx.now };
                }
            }
            Phase::Settle { since } => {
                if cx.now.saturating_sub(since) >= SETTLE_MS {
                    if cx.adapter.fw == Fw::ArduPilot {
                        let axes_v = if !self.yaw_round {
                            AXES_ROLL_PITCH
                        } else if cx.fc.version().is_some_and(|v| v >= (4, 5)) {
                            AXES_YAW
                        } else {
                            AXES_YAW_OLD
                        };
                        let p = cx.adapter.autotune_prefix();
                        cx.params.set(&self.axes_param(cx), axes_v);
                        cx.params.set(&format!("{p}_AGGR"), AGGR);
                    }
                    self.phase = Phase::Params;
                }
            }
            Phase::Params => {
                if cx.params.idle() {
                    if !cx.params.take_failed().is_empty() {
                        self.hop.land(cx);
                        self.phase = Phase::Idle;
                        self.core.fail("setup.blocker.param_write_failed");
                        return;
                    }
                    self.phase = Phase::Prestream { since: cx.now };
                    self.entered = false;
                    self.text_cursor = cx.fc.text_seq;
                }
            }
            Phase::Prestream { since } => {
                self.manual(cx, 0.0, 0.0, 0.5, 0.0);
                if cx.now.saturating_sub(since) >= PRESTREAM_MS {
                    // The FC's own hold (or the tune mode) takes over from the hold setpoints.
                    self.hop.leg = Leg::Idle;
                    if let Some(h) = self.hold_mode(cx) {
                        flight::set_mode(cx, h);
                        self.phase = Phase::Hold { since: cx.now };
                    } else {
                        flight::set_mode(cx, self.tune_mode(cx));
                        self.phase = Phase::Tuning { since: cx.now };
                    }
                }
            }
            Phase::Hold { since } => {
                self.manual(cx, 0.0, 0.0, 0.5, 0.0);
                let el = cx.now.saturating_sub(since);
                let hold = self.hold_mode(cx).unwrap_or(Mode::Loiter);
                if flight::in_mode(cx, hold) && el >= HOLD_MS {
                    flight::set_mode(cx, self.tune_mode(cx));
                    self.phase = Phase::Tuning { since: cx.now };
                } else if el > TUNE_ENTER_MS {
                    self.refused(cx);
                } else if !flight::in_mode(cx, hold) && el > 2000 && el % 2000 < MANUAL_MS {
                    flight::set_mode(cx, hold);
                }
            }
            Phase::Tuning { since } => {
                let (x, y, z, r) = blended(axes);
                self.manual(cx, x, y, z, r);
                let el = cx.now.saturating_sub(since);
                if flight::in_mode(cx, self.tune_mode(cx)) {
                    self.entered = true;
                } else if self.entered {
                    // The FC left the tune mode on its own (a failsafe, "AutoTune:
                    // Stopped"): it is flying now; never force the mode back over it.
                    self.phase = Phase::Idle;
                    self.core.fail(if cx.fc.armed {
                        "setup.blocker.autotune_failed"
                    } else {
                        "setup.blocker.disarmed"
                    });
                    return;
                } else if el > TUNE_ENTER_MS {
                    // Refused (STATUSTEXT "Mode change to … failed"): stop rather than ask forever.
                    self.refused(cx);
                    return;
                } else if el > 2000 && el % 2000 < MANUAL_MS {
                    flight::set_mode(cx, self.tune_mode(cx));
                }
                if cx.adapter.fw == Fw::Px4 {
                    if cx.now.saturating_sub(self.last_poll) >= POLL_MS {
                        self.last_poll = cx.now;
                        cx.out.cmd(
                            MavCmd::MAV_CMD_DO_AUTOTUNE_ENABLE,
                            [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                        );
                    }
                    match cx.fc.ack_since(212, since) {
                        Some(MavResult::MAV_RESULT_ACCEPTED) if el > 3000 => {
                            self.copter_done = vec![Axis::Roll, Axis::Pitch, Axis::Yaw];
                            self.hop.land(cx);
                            self.phase = Phase::LandSave;
                        }
                        Some(MavResult::MAV_RESULT_FAILED | MavResult::MAV_RESULT_DENIED) => {
                            self.hop.land(cx);
                            self.phase = Phase::Idle;
                            self.core.fail("setup.blocker.autotune_failed");
                        }
                        _ => {}
                    }
                    return;
                }
                for e in self.tick_texts(cx) {
                    match e {
                        TuneEvent::Success => {
                            self.phase = Phase::LandSave;
                            self.core.phase_key = Some("landing_to_save".into());
                        }
                        TuneEvent::Failed => {
                            // Land in AUTOTUNE: a failed tune keeps the original gains.
                            self.notes.push(json!({"round": if self.yaw_round { "yaw" } else { "roll_pitch" }, "failed": true}));
                            self.phase = Phase::LandSave;
                        }
                        TuneEvent::Saved(ax) => self.copter_done.extend(ax),
                        TuneEvent::Phase(p) => self.core.set_live("phase", p),
                        TuneEvent::PilotOverride => self.core.set_live("phase", "pilot_override"),
                        _ => {}
                    }
                }
                self.core.progress = Some(if self.yaw_round { 0.6 } else { 0.1 });
            }
            Phase::LandSave => {
                if cx.adapter.fw == Fw::Px4 {
                    if self.hop.leg == Leg::Landed {
                        self.hop.leg = Leg::Idle;
                        self.after_copter(cx);
                    }
                    return;
                }
                // Throttle down in AUTOTUNE until landed, then disarm: the
                // FC saves a completed tune on disarm in this mode.
                if cx.fc.armed {
                    let landed = cx.fc.on_ground() && cx.fc.landed.is_some();
                    if landed {
                        if cx.now.saturating_sub(self.last_poll) >= POLL_MS {
                            self.last_poll = cx.now;
                            cx.out.arm(false);
                        }
                    } else {
                        self.manual(cx, 0.0, 0.0, 0.0, 0.0);
                    }
                } else {
                    for e in self.tick_texts(cx) {
                        if let TuneEvent::Saved(ax) = e {
                            self.copter_done.extend(ax);
                        }
                    }
                    self.phase = Phase::Saved;
                    self.last_poll = cx.now;
                }
            }
            Phase::Saved => {
                // Let the save STATUSTEXT arrive.
                for e in self.tick_texts(cx) {
                    if let TuneEvent::Saved(ax) = e {
                        self.copter_done.extend(ax);
                    }
                }
                if cx.now.saturating_sub(self.last_poll) >= 1500 {
                    flight::set_mode(cx, Mode::Loiter);
                    self.after_copter(cx);
                }
            }
            Phase::PlanePosition => self.plane_position(cx),
            Phase::PlaneTuning { since } => self.plane_tuning(cx, since, axes),
            Phase::PlaneExit => self.plane_exit(cx),
            Phase::ReadGains => self.report_gains(cx),
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        match id {
            "start" => {
                self.snapshot_gains(cx);
                self.core.blockers.clear();
                self.core.running();
                self.core.actions = vec![Action::danger("abort")];
                if cx.adapter.class == Class::Fixedwing && cx.adapter.fw == Fw::ArduPilot {
                    self.start_plane(cx);
                } else {
                    self.yaw_round = false;
                    self.start_copter_round(cx);
                }
                Ok(())
            }
            "resume_yaw" => {
                if cx.fc.armed && !matches!(self.phase, Phase::YawWait) {
                    return Err("bad_state");
                }
                // "Tune yaw now" is for after a battery swap: a yaw round started on the
                // pack that just flew roll and pitch runs into the FC's battery failsafe.
                if cx.fc.battery_pct.is_none_or(|p| p < YAW_MIN_BATTERY) {
                    return Err("battery_low");
                }
                if self.before.is_empty() {
                    self.snapshot_gains(cx);
                }
                self.yaw_round = true;
                self.core.running();
                self.core.actions = vec![Action::danger("abort")];
                self.start_copter_round(cx);
                Ok(())
            }
            "skip_yaw" => {
                if self.phase != Phase::YawWait {
                    return Err("bad_state");
                }
                self.notes.push(json!({"yaw": "skipped"}));
                self.core.running();
                if cx.adapter.class == Class::Vtol {
                    self.start_plane(cx);
                } else {
                    self.read_gains(cx);
                }
                Ok(())
            }
            _ => Err("unknown_action"),
        }
    }

    fn abort(&mut self, _cx: &mut Cx) {
        self.phase = Phase::Idle;
        self.hop.leg = Leg::Idle;
    }
}
