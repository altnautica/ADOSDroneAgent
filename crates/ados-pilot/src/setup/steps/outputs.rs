//! `outputs`: motor order and spin direction, control-surface directions.
//!
//! Motors spin one at a time (DO_MOTOR_TEST at 5 % for 2 s; PX4
//! ACTUATOR_TEST) behind a "Propellers are removed" hold-confirm, and the
//! operator answers whether the right motor spun and whether it spun the
//! right way. A wrong direction on a DShot ESC is fixed in
//! `SERVO_BLH_RVMASK`; otherwise the step asks for two motor wires to be
//! swapped.
//!
//! Surfaces: ArduPilot refuses MAV_CMD_DO_SET_SERVO on any output assigned
//! to a flight-control function (`AP_ServoRelayEvents::do_set_servo`), so
//! surfaces are driven the way the pilot's sticks would: MANUAL mode,
//! disarmed, a 30 % MANUAL_CONTROL demand on the tested axis. A wrong answer
//! flips `SERVOn_REVERSED`. Trims are then learned in flight
//! (`SERVO_AUTO_TRIM` = 1).

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use serde_json::{json, Map, Value};

use crate::setup::adapters::ardupilot_copter::{self, MotorDef};
use crate::setup::adapters::{ardupilot_plane, Mode};
use crate::setup::fcstate::{Class, Fw};
use crate::setup::schema::{obj, Action, ActionStyle, Confirm};
use crate::setup::step::{step_core, writes_settled, ActErr, Cx, Reboot, Step, StepCore};

/// Motor test throttle (%) and duration (s).
pub const MOTOR_PCT: f32 = 5.0;
pub const MOTOR_S: f32 = 2.0;
/// Surface deflection demand.
pub const SURFACE_DEMAND: f32 = 0.3;
/// How long a surface is held deflected (ms).
const SURFACE_HOLD_MS: u64 = 3000;
/// MANUAL_CONTROL refresh while a surface is held.
const MANUAL_PERIOD_MS: u64 = 100;
/// PX4 actuator output function of motor 1.
const PX4_MOTOR1: f32 = 101.0;

#[derive(Debug, Clone, PartialEq)]
enum Item {
    Motor { def: MotorDef, px4: bool },
    Surface { channel: u8, function: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ask {
    /// Waiting for the operator to run the test.
    Ready,
    /// The output is moving.
    Moving { until: u64 },
    Position,
    Direction,
    Surface,
}

pub struct Outputs {
    core: StepCore,
    items: Vec<Item>,
    idx: usize,
    ask: Ask,
    last_manual: u64,
    /// Motors spun in the wrong place (sequence letters).
    wrong_position: Vec<u8>,
    /// Motors to fix by swapping wires.
    swap: Vec<u8>,
    reversed_dshot: Vec<u8>,
    reversed_servo: Vec<u8>,
    rvmask_changed: bool,
    finishing: bool,
    reboot: Reboot,
    final_result: Map<String, Value>,
}

impl Default for Outputs {
    fn default() -> Self {
        Outputs {
            core: StepCore::default(),
            items: Vec::new(),
            idx: 0,
            ask: Ask::Ready,
            last_manual: 0,
            wrong_position: Vec::new(),
            swap: Vec::new(),
            reversed_dshot: Vec::new(),
            reversed_servo: Vec::new(),
            rvmask_changed: false,
            finishing: false,
            reboot: Reboot::default(),
            final_result: Map::new(),
        }
    }
}

/// The output channel (1-based) carrying output function `f`.
fn channel_of(cx: &Cx, f: u8) -> Option<u8> {
    (1..=32u8).find(|k| cx.params.get(&format!("SERVO{k}_FUNCTION")).is_some_and(|v| v as u8 == f))
}

fn surfaces(cx: &Cx) -> Vec<Item> {
    let mut v = Vec::new();
    for k in 1..=16u8 {
        if let Some(f) = cx.params.get(&format!("SERVO{k}_FUNCTION")) {
            let f = f as u8;
            if ardupilot_plane::surface_test(f).is_some() {
                v.push(Item::Surface { channel: k, function: f });
            }
        }
    }
    v
}

fn motors(cx: &Cx) -> Result<(Option<&'static str>, Vec<Item>), ActErr> {
    if cx.adapter.fw == Fw::Px4 {
        let n = cx.params.get("CA_ROTOR_COUNT").unwrap_or(4.0) as u8;
        let v = (1..=n).map(|i| Item::Motor { def: MotorDef { num: i, angle: 0, cw: false, seq: i }, px4: true }).collect();
        return Ok((None, v));
    }
    let (c, t) = if cx.adapter.class == Class::Vtol { ("Q_FRAME_CLASS", "Q_FRAME_TYPE") } else { ("FRAME_CLASS", "FRAME_TYPE") };
    let fc = cx.params.get(c).unwrap_or(1.0) as u8;
    let ft = cx.params.get(t).unwrap_or(1.0) as u8;
    let (img, layout) = ardupilot_copter::layout(fc, ft).ok_or("unsupported_frame")?;
    let mut defs: Vec<MotorDef> = layout.to_vec();
    defs.sort_by_key(|d| d.seq);
    Ok((Some(img), defs.into_iter().map(|def| Item::Motor { def, px4: false }).collect()))
}

impl Outputs {
    fn show_current(&mut self, cx: &Cx) {
        let total = self.items.len();
        self.core.progress = Some(self.idx as f32 / total.max(1) as f32);
        match self.items.get(self.idx).cloned() {
            Some(Item::Motor { def, .. }) => {
                self.core.set_live("output_kind", "motor");
                self.core.set_live("output_index", def.seq);
                self.core.set_live("motor_number", def.num);
                self.core.set_live("expected_dir", if def.cw { "cw" } else { "ccw" });
                // The motor questions read "motor {n}": the number on the motor-order image.
                self.core.args.insert("n".into(), json!(def.num));
            }
            Some(Item::Surface { channel, function }) => {
                self.core.set_live("output_kind", "surface");
                self.core.set_live("output_index", channel);
                self.core.live.remove("motor_number");
                self.core.live.remove("expected_dir");
                self.core.args.insert("n".into(), json!(channel));
                if let Some((q, _)) = ardupilot_plane::surface_test(function) {
                    self.core.set_live("question_key", q);
                }
                if let Some(w) = &cx.data.wing {
                    self.core.image = Some(format!("surfaces_{w}"));
                }
            }
            None => {}
        }
    }

    fn test_action(&self) -> Action {
        match self.items.get(self.idx) {
            Some(Item::Motor { .. }) => Action::new("test", ActionStyle::Primary, Confirm::HoldPropsOff),
            _ => Action::primary("test"),
        }
    }

    fn ready(&mut self, cx: &Cx) {
        self.ask = Ask::Ready;
        self.core.live.remove("question_key");
        self.show_current(cx);
        self.core.wait_user(vec![self.test_action(), Action::secondary("cancel")]);
    }

    fn yes_no(&mut self, ask: Ask, question: &str) {
        self.ask = ask;
        self.core.set_live("question_key", question);
        self.core.wait_user(vec![Action::primary("answer_yes"), Action::secondary("answer_no")]);
    }

    fn next(&mut self, cx: &mut Cx) {
        self.idx += 1;
        if self.idx < self.items.len() {
            self.ready(cx);
        } else {
            self.finish(cx);
        }
    }

    fn finish(&mut self, cx: &mut Cx) {
        if cx.adapter.fw == Fw::ArduPilot && self.items.iter().any(|i| matches!(i, Item::Surface { .. })) {
            cx.params.set("SERVO_AUTO_TRIM", 1.0);
        }
        self.final_result = obj([
            ("tested", json!(self.items.len())),
            ("reversed_dshot", json!(self.reversed_dshot)),
            ("reversed_servo", json!(self.reversed_servo)),
            ("swap_wires", json!(self.swap)),
            ("wrong_position", json!(self.wrong_position)),
        ]);
        self.finishing = true;
        self.core.live.remove("question_key");
        self.core.running();
    }

    fn spin(&mut self, cx: &mut Cx) {
        match self.items.get(self.idx).cloned() {
            Some(Item::Motor { def, px4 }) => {
                if px4 {
                    cx.out.cmd(
                        MavCmd::MAV_CMD_ACTUATOR_TEST,
                        [MOTOR_PCT / 100.0, MOTOR_S, 0.0, 0.0, PX4_MOTOR1 + (def.num - 1) as f32, 0.0, 0.0],
                    );
                } else {
                    // Instance in test-sequence order, throttle in percent.
                    cx.out.cmd(MavCmd::MAV_CMD_DO_MOTOR_TEST, [def.seq as f32, 0.0, MOTOR_PCT, MOTOR_S, 0.0, 0.0, 0.0]);
                }
                self.ask = Ask::Moving { until: cx.now + (MOTOR_S * 1000.0) as u64 };
            }
            Some(Item::Surface { .. }) => {
                self.ask = Ask::Moving { until: cx.now + SURFACE_HOLD_MS };
                self.last_manual = 0;
            }
            None => {}
        }
        self.core.running();
    }

    fn surface_demand(function: u8) -> (f32, f32, f32) {
        // (pitch x, roll y, yaw r) for MANUAL_CONTROL: x > 0 is nose down.
        match function {
            ardupilot_plane::FN_AILERON => (0.0, SURFACE_DEMAND, 0.0),
            ardupilot_plane::FN_RUDDER => (0.0, 0.0, SURFACE_DEMAND),
            _ => (-SURFACE_DEMAND, 0.0, 0.0),
        }
    }
}

impl Step for Outputs {
    step_core!("outputs");

    fn begin(&mut self, cx: &mut Cx) {
        *self = Outputs::default();
        let mut items = Vec::new();
        if cx.adapter.class != Class::Multirotor && cx.adapter.fw == Fw::ArduPilot {
            items.extend(surfaces(cx));
            self.core.body_variant = Some(if cx.adapter.class == Class::Vtol { "vtol" } else { "plane" });
        }
        if cx.adapter.class != Class::Fixedwing || cx.adapter.fw == Fw::Px4 {
            match motors(cx) {
                Ok((img, m)) => {
                    self.core.image = img.map(str::to_string);
                    items.extend(m);
                }
                Err(e) => {
                    self.core.fail(&format!("setup.blocker.{e}"));
                    return;
                }
            }
        }
        self.items = items;
        if self.items.is_empty() {
            self.core.skip("no_outputs");
            return;
        }
        self.ready(cx);
    }

    fn tick(&mut self, cx: &mut Cx) {
        if self.reboot.active() {
            if self.reboot.tick(cx) {
                self.core.phase_key = None;
                self.complete();
            }
            return;
        }
        if self.finishing {
            match writes_settled(cx) {
                None => {}
                Some(Err(_)) => {
                    self.finishing = false;
                    self.core.fail("setup.blocker.param_write_failed");
                }
                Some(Ok(())) => {
                    self.finishing = false;
                    if self.rvmask_changed {
                        self.core.phase_key = Some("rebooting".into());
                        self.reboot.start(cx);
                    } else {
                        self.complete();
                    }
                }
            }
            return;
        }
        if let Ask::Moving { until } = self.ask {
            let item = self.items.get(self.idx).cloned();
            if let Some(Item::Surface { function, .. }) = item {
                if cx.now.saturating_sub(self.last_manual) >= MANUAL_PERIOD_MS {
                    self.last_manual = cx.now;
                    let (x, y, r) = Self::surface_demand(function);
                    cx.out.manual(x, y, 0.0, r);
                }
            }
            if cx.now >= until {
                match item {
                    Some(Item::Motor { .. }) => self.yes_no(Ask::Position, "setup.question.motor_position"),
                    Some(Item::Surface { function, .. }) => {
                        let q = ardupilot_plane::surface_test(function).map(|(q, _)| q).unwrap_or("");
                        self.yes_no(Ask::Surface, q);
                    }
                    None => {}
                }
            } else if matches!(item, Some(Item::Surface { .. })) {
                // Ask while the surface is still deflected.
                let q = item
                    .as_ref()
                    .and_then(|i| if let Item::Surface { function, .. } = i { ardupilot_plane::surface_test(*function) } else { None })
                    .map(|(q, _)| q)
                    .unwrap_or("");
                self.core.set_live("question_key", q);
            }
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        if cx.fc.armed {
            return Err("armed");
        }
        match (id, self.ask) {
            ("test", Ask::Ready) | ("retry", _) => {
                if id == "retry" {
                    self.begin(cx);
                    return Ok(());
                }
                if matches!(self.items.get(self.idx), Some(Item::Surface { .. })) && !cx.adapter.in_mode(cx.fc.custom_mode, Mode::Manual) {
                    if let Some((m, s)) = cx.adapter.mode(Mode::Manual) {
                        cx.out.set_mode(m, s);
                    }
                }
                self.spin(cx);
                Ok(())
            }
            ("answer_yes", Ask::Position) => {
                self.yes_no(Ask::Direction, "setup.question.motor_direction");
                Ok(())
            }
            ("answer_no", Ask::Position) => {
                if let Some(Item::Motor { def, .. }) = self.items.get(self.idx) {
                    self.wrong_position.push(def.seq);
                }
                self.next(cx);
                Ok(())
            }
            ("answer_yes", Ask::Direction) | ("answer_yes", Ask::Surface) => {
                self.next(cx);
                Ok(())
            }
            ("answer_no", Ask::Direction) => {
                let Some(Item::Motor { def, px4 }) = self.items.get(self.idx).cloned() else { return Err("bad_state") };
                let pwm_type = cx.params.get(&format!("{}_PWM_TYPE", cx.adapter.mot())).unwrap_or(0.0);
                let ch = channel_of(cx, 32 + def.num);
                match (px4, pwm_type >= 4.0, ch) {
                    (false, true, Some(ch)) => {
                        let mask = cx.params.get("SERVO_BLH_RVMASK").unwrap_or(0.0) as u32;
                        cx.params.set("SERVO_BLH_RVMASK", (mask ^ (1 << (ch - 1))) as f32);
                        if cx.params.get("SERVO_BLH_AUTO").is_some_and(|v| v == 0.0)
                            && cx.params.get("SERVO_BLH_MASK").unwrap_or(0.0) as u32 & (1 << (ch - 1)) == 0
                        {
                            cx.params.set("SERVO_BLH_AUTO", 1.0);
                        }
                        self.rvmask_changed = true;
                        self.reversed_dshot.push(def.seq);
                    }
                    _ => self.swap.push(def.seq),
                }
                self.next(cx);
                Ok(())
            }
            ("answer_no", Ask::Surface) => {
                let Some(Item::Surface { channel, .. }) = self.items.get(self.idx).cloned() else { return Err("bad_state") };
                let name = format!("SERVO{channel}_REVERSED");
                let cur = cx.params.get(&name).unwrap_or(0.0);
                cx.params.set(&name, if cur == 0.0 { 1.0 } else { 0.0 });
                self.reversed_servo.push(channel);
                self.next(cx);
                Ok(())
            }
            ("cancel", _) => {
                self.abort(cx);
                Ok(())
            }
            _ => Err("bad_state"),
        }
    }

    fn abort(&mut self, _cx: &mut Cx) {
        self.ask = Ask::Ready;
        self.core.fail("setup.blocker.cancelled");
    }
}

impl Outputs {
    fn complete(&mut self) {
        let result = std::mem::take(&mut self.final_result);
        if !self.wrong_position.is_empty() {
            self.core.result = Some(result);
            self.core.fail("setup.blocker.motor_position");
        } else if !self.swap.is_empty() {
            self.core.result = Some(result);
            self.core.fail("setup.blocker.swap_motor_wires");
        } else {
            self.core.done(result);
        }
    }
}
