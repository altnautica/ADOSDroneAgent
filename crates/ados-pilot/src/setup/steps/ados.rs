//! `ados_profile`: the FC parameter profile ADOS Pilot depends on (one table
//! with the pilot's readiness check), and `ados_intro`: "Meet ADOS Pilot",
//! where the operator chooses the control mode.

use ados_protocol::pilot::VehicleClass;
use serde_json::{json, Map, Value};

use crate::fc_profile::{fc_profile, Firmware};
use crate::setup::fcstate::{Class, Fw};
use crate::setup::out::Effect;
use crate::setup::schema::{obj, Action};
use crate::setup::step::{step_core, writes_settled, ActErr, Cx, Step, StepCore};

pub fn firmware(fw: Fw) -> Option<Firmware> {
    match fw {
        Fw::ArduPilot => Some(Firmware::Ardupilot),
        Fw::Px4 => Some(Firmware::Px4),
        Fw::Other => None,
    }
}

pub fn vehicle_class(c: Class) -> VehicleClass {
    match c {
        Class::Multirotor => VehicleClass::Multirotor,
        Class::Fixedwing => VehicleClass::Fixedwing,
        Class::Vtol => VehicleClass::Vtol,
    }
}

#[derive(Default)]
pub struct AdosProfile {
    core: StepCore,
    writing: bool,
    applied: Vec<Value>,
    absent: Vec<Value>,
}

impl Step for AdosProfile {
    step_core!("ados_profile");

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.writing = false;
        let rows: Vec<Value> = firmware(cx.adapter.fw)
            .map(|f| fc_profile(f, vehicle_class(cx.adapter.class)))
            .unwrap_or(&[])
            .iter()
            .map(|r| json!({"name": r.name, "value": r.value}))
            .collect();
        self.core.args.insert("params".into(), Value::Array(rows));
        self.core.wait_user(vec![Action::primary("apply")]);
    }

    fn tick(&mut self, cx: &mut Cx) {
        if !self.writing {
            return;
        }
        match writes_settled(cx) {
            None => {}
            Some(Ok(())) => {
                self.writing = false;
                self.core.done(obj([
                    ("applied", Value::Array(std::mem::take(&mut self.applied))),
                    ("absent", Value::Array(std::mem::take(&mut self.absent))),
                ]));
            }
            Some(Err(names)) => {
                self.writing = false;
                self.core.result = Some(obj([("failed", json!(names))]));
                self.core.fail("setup.blocker.param_write_failed");
            }
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        if id != "apply" {
            return Err("unknown_action");
        }
        if cx.fc.armed {
            return Err("armed");
        }
        let fw = firmware(cx.adapter.fw).ok_or("unsupported")?;
        self.applied.clear();
        self.absent.clear();
        for row in fc_profile(fw, vehicle_class(cx.adapter.class)) {
            let name = std::iter::once(row.name).chain(row.alt_names.iter().copied()).find(|n| cx.params.has(n));
            match name {
                Some(n) => {
                    cx.params.set(n, row.value);
                    self.applied.push(json!({"name": n, "value": row.value}));
                }
                None => self.absent.push(json!(row.name)),
            }
        }
        self.writing = true;
        self.core.running();
        Ok(())
    }
}

#[derive(Default)]
pub struct AdosIntro {
    core: StepCore,
}

impl Step for AdosIntro {
    step_core!("ados_intro");

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        let fallback = if cx.fc.rc_present(cx.now) { "elrs" } else { "fc_manual" };
        self.core.args.insert("fallback_mode".into(), json!(fallback));
        self.core.wait_user(vec![Action::primary("use_ados"), Action::secondary("keep_fc")]);
    }

    fn tick(&mut self, _cx: &mut Cx) {}

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        let mode: &'static str = match id {
            "use_ados" => "ados",
            "keep_fc" => {
                if cx.fc.rc_present(cx.now) {
                    "elrs"
                } else {
                    "fc_manual"
                }
            }
            _ => return Err("unknown_action"),
        };
        cx.out.effect(Effect::SetControlMode(mode));
        self.core.done(obj([("control_mode", json!(mode))]));
        Ok(())
    }
}
