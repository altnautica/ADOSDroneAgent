//! `compass`: onboard magnetometer calibration.
//!
//! ArduPilot: MAV_CMD_DO_START_MAG_CAL (42424) with autosave, progress from
//! MAG_CAL_PROGRESS, the verdict from MAG_CAL_REPORT (fitness must be
//! ≤ 16 mGauss), at most three attempts, then a reboot so the new offsets
//! load. The "large vehicle" route uses MAV_CMD_FIXED_MAG_CAL_YAW (42006) with
//! a known heading. PX4: PREFLIGHT_CALIBRATION p2 = 1 and `[cal]` texts.

use std::collections::BTreeMap;

use ados_protocol::mavlink::ardupilotmega::{MagCalStatus, MavCmd, MavMessage, MavResult};
use serde_json::{json, Map, Value};

use crate::setup::adapters::px4::{parse_cal, CalEvent};
use crate::setup::fcstate::Fw;
use crate::setup::schema::{input_number, obj, Action};
use crate::setup::step::{arg_f32, step_core, ActErr, Cx, Reboot, Step, StepCore};

/// Largest accepted fitness (mGauss).
pub const MAX_FITNESS: f32 = 16.0;
pub const MAX_ATTEMPTS: u8 = 3;
const TIMEOUT_MS: u64 = 300_000;
const CMD_START: u32 = 42424;
const CMD_FIXED_YAW: u32 = 42006;
const CMD_241: u32 = 241;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Running,
    KnownHeading,
    Rebooting,
}

pub struct Compass {
    core: StepCore,
    phase: Phase,
    started_ms: u64,
    attempt: u8,
    progress: BTreeMap<u8, u8>,
    reports: BTreeMap<u8, (MagCalStatus, f32)>,
    mask: u8,
    text_cursor: u64,
    reboot: Reboot,
    result: Map<String, Value>,
}

impl Default for Compass {
    fn default() -> Self {
        Compass {
            core: StepCore::default(),
            phase: Phase::Idle,
            started_ms: 0,
            attempt: 0,
            progress: BTreeMap::new(),
            reports: BTreeMap::new(),
            mask: 0,
            text_cursor: 0,
            reboot: Reboot::default(),
            result: Map::new(),
        }
    }
}

impl Compass {
    fn start(&mut self, cx: &mut Cx) {
        self.phase = Phase::Running;
        self.started_ms = cx.now;
        self.attempt += 1;
        self.progress.clear();
        self.reports.clear();
        self.mask = 0;
        self.text_cursor = cx.fc.text_seq;
        if cx.adapter.fw == Fw::Px4 {
            cx.out.cmd(MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION, [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        } else {
            // mask 0 = every compass, no FC-side retry, autosave, no delay, no autoreboot.
            cx.out.cmd(MavCmd::MAV_CMD_DO_START_MAG_CAL, [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        }
        self.core.running();
        self.core.phase_key = None;
        self.core.actions = vec![Action::secondary("cancel")];
        self.core.set_live("attempt", self.attempt);
    }

    fn cancel_fc(&self, cx: &mut Cx) {
        if cx.adapter.fw == Fw::Px4 {
            cx.out.cmd(MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION, [0.0; 7]);
        } else {
            cx.out.cmd(MavCmd::MAV_CMD_DO_CANCEL_MAG_CAL, [0.0; 7]);
        }
    }

    fn succeeded(&mut self, cx: &mut Cx, result: Map<String, Value>) {
        self.result = result;
        self.phase = Phase::Rebooting;
        self.core.phase_key = Some("rebooting".into());
        self.reboot.start(cx);
    }

    fn evaluate(&mut self, cx: &mut Cx) {
        // Every compass in the mask has reported.
        if self.mask == 0 || (0..8).any(|i| self.mask & (1 << i) != 0 && !self.reports.contains_key(&i)) {
            return;
        }
        let ok = self
            .reports
            .values()
            .all(|(s, fit)| *s == MagCalStatus::MAG_CAL_SUCCESS && *fit <= MAX_FITNESS);
        let fitness: Map<String, Value> =
            self.reports.iter().map(|(id, (_, f))| (id.to_string(), json!((f * 100.0).round() / 100.0))).collect();
        if ok {
            self.succeeded(cx, obj([("fitness_mgauss", Value::Object(fitness)), ("attempts", json!(self.attempt))]));
        } else if self.attempt < MAX_ATTEMPTS {
            self.core.set_live("last_fitness_mgauss", Value::Object(fitness));
            self.start(cx);
        } else {
            self.phase = Phase::Idle;
            self.core.result = Some(obj([("fitness_mgauss", Value::Object(fitness)), ("attempts", json!(self.attempt))]));
            self.core.fail("setup.blocker.compass_fitness");
        }
    }
}

impl Step for Compass {
    step_core!("compass");

    fn optional(&self) -> bool {
        true
    }

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.phase = Phase::Idle;
        self.attempt = 0;
        if cx.adapter.fw == Fw::Px4 {
            self.core.body_variant = Some("px4");
        }
        self.core.image = Some("compass_rotate".into());
        let mut actions = vec![Action::primary("start")];
        if cx.adapter.fw == Fw::ArduPilot {
            let mut inputs = Map::new();
            inputs.insert("heading_deg".into(), input_number(0.0, 0.0, 359.0, 1.0, "deg", "setup.input.heading_deg"));
            self.core.args.insert("inputs".into(), Value::Object(inputs));
            actions.push(Action::secondary("known_heading"));
        }
        actions.push(Action::secondary("skip"));
        self.core.wait_user(actions);
    }

    fn on_msg(&mut self, msg: &MavMessage, _cx: &mut Cx) {
        if self.phase != Phase::Running {
            return;
        }
        match msg {
            MavMessage::MAG_CAL_PROGRESS(p) => {
                self.mask |= p.cal_mask;
                self.progress.insert(p.compass_id, p.completion_pct);
            }
            MavMessage::MAG_CAL_REPORT(r) => {
                self.mask |= r.cal_mask;
                self.progress.insert(r.compass_id, 100);
                self.reports.insert(r.compass_id, (r.cal_status, r.fitness));
            }
            _ => {}
        }
    }

    fn tick(&mut self, cx: &mut Cx) {
        match self.phase {
            Phase::Idle => {}
            Phase::Rebooting => {
                if self.reboot.tick(cx) {
                    self.phase = Phase::Idle;
                    self.core.phase_key = None;
                    self.core.done(std::mem::take(&mut self.result));
                }
            }
            Phase::KnownHeading => {
                if let Some(r) = cx.fc.ack_since(CMD_FIXED_YAW, self.started_ms) {
                    if r == MavResult::MAV_RESULT_ACCEPTED {
                        let res = obj([("method", json!("known_heading"))]);
                        self.succeeded(cx, res);
                    } else {
                        self.phase = Phase::Idle;
                        self.core.fail("setup.blocker.calibration_refused");
                    }
                }
            }
            Phase::Running => {
                if !self.progress.is_empty() {
                    let mean = self.progress.values().map(|v| *v as f32).sum::<f32>() / self.progress.len() as f32;
                    self.core.progress = Some(mean / 100.0);
                    let per: Map<String, Value> = self.progress.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
                    self.core.set_live("compass_pct", Value::Object(per));
                }
                let cmd = if cx.adapter.fw == Fw::Px4 { CMD_241 } else { CMD_START };
                if cx.fc.ack_since(cmd, self.started_ms).is_some_and(|r| {
                    !matches!(r, MavResult::MAV_RESULT_ACCEPTED | MavResult::MAV_RESULT_IN_PROGRESS)
                }) {
                    self.phase = Phase::Idle;
                    self.core.fail("setup.blocker.calibration_refused");
                    return;
                }
                if cx.adapter.fw == Fw::Px4 {
                    let (texts, cursor) = cx.fc.texts_after(self.text_cursor);
                    self.text_cursor = cursor;
                    for t in texts {
                        match parse_cal(&t) {
                            Some(CalEvent::Progress(p)) => self.core.progress = Some(p as f32 / 100.0),
                            Some(CalEvent::Done) => {
                                let res = obj([("attempts", json!(self.attempt))]);
                                self.succeeded(cx, res);
                                return;
                            }
                            Some(CalEvent::Failed(_)) => {
                                if self.attempt < MAX_ATTEMPTS {
                                    self.start(cx);
                                } else {
                                    self.phase = Phase::Idle;
                                    self.core.fail("setup.blocker.calibration_failed");
                                }
                                return;
                            }
                            _ => {}
                        }
                    }
                } else {
                    self.evaluate(cx);
                }
                if self.phase == Phase::Running && cx.now.saturating_sub(self.started_ms) > TIMEOUT_MS {
                    self.cancel_fc(cx);
                    self.phase = Phase::Idle;
                    self.core.fail("setup.blocker.timeout");
                }
            }
        }
    }

    fn action(&mut self, id: &str, args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        match id {
            "start" | "retry" => {
                if cx.fc.armed {
                    return Err("armed");
                }
                self.attempt = 0;
                self.start(cx);
                Ok(())
            }
            "known_heading" => {
                if cx.adapter.fw != Fw::ArduPilot {
                    return Err("unsupported");
                }
                let yaw = arg_f32(args, "heading_deg").filter(|y| (0.0..360.0).contains(y)).ok_or("bad_args")?;
                cx.out.cmd(MavCmd::MAV_CMD_FIXED_MAG_CAL_YAW, [yaw, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
                self.phase = Phase::KnownHeading;
                self.started_ms = cx.now;
                self.core.running();
                Ok(())
            }
            "cancel" => {
                self.abort(cx);
                Ok(())
            }
            _ => Err("unknown_action"),
        }
    }

    fn abort(&mut self, cx: &mut Cx) {
        if self.phase == Phase::Running {
            self.cancel_fc(cx);
        }
        self.phase = Phase::Idle;
        self.core.fail("setup.blocker.cancelled");
    }
}
