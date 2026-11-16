//! `ados_profile`: the FC parameter profile ADOS Pilot depends on (one table
//! with the pilot's readiness check; the battery actions follow
//! [`crate::batt_failsafe::recommended`], Land for both on a multirotor that
//! navigates without GNSS), and `ados_intro`: "Meet ADOS Pilot", where the
//! operator chooses the control mode. A changed row the FC reads only at
//! boot (`OA_TYPE`, `SRTL_POINTS`) reboots the FC before the step is done, as
//! the battery and frame steps do. In a confined place `SRTL_ACCURACY` keeps
//! a value within the confined bound and is written to the bound otherwise
//! ([`crate::fc_profile::CONFINED_SRTL_ACCURACY_MAX_M`]).

use ados_protocol::pilot::VehicleClass;
use serde_json::{json, Map, Value};

use crate::fc_profile::{check_row, fc_profile, Firmware, ProfileParam, RowCheck};
use crate::setup::fcstate::{Class, Fw};
use crate::setup::out::Effect;
use crate::setup::schema::{obj, Action};
use crate::setup::step::{step_core, writes_settled, ActErr, Cx, Reboot, Rebooted, Step, StepCore};

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
    needs_reboot: bool,
    reboot: Reboot,
    applied: Vec<Value>,
    absent: Vec<Value>,
}

impl AdosProfile {
    fn finish(&mut self) {
        self.core.done(obj([
            ("applied", Value::Array(std::mem::take(&mut self.applied))),
            ("absent", Value::Array(std::mem::take(&mut self.absent))),
            ("rebooted", json!(self.needs_reboot)),
        ]));
    }
}

impl Step for AdosProfile {
    step_core!("ados_profile");

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.writing = false;
        let class = vehicle_class(cx.adapter.class);
        self.needs_reboot = false;
        let rows: Vec<Value> = firmware(cx.adapter.fw)
            .map(|f| fc_profile(f, class))
            .unwrap_or(&[])
            .iter()
            .map(|r| json!({"name": r.name, "value": row_value(cx, r)}))
            .collect();
        self.core.args.insert("params".into(), Value::Array(rows));
        self.core.wait_user(vec![Action::primary("apply")]);
    }

    fn tick(&mut self, cx: &mut Cx) {
        if self.reboot.active() {
            match self.reboot.tick(cx) {
                Rebooted::Waiting => {}
                Rebooted::Back => {
                    self.core.phase_key = None;
                    self.finish();
                }
                Rebooted::Lost => {
                    self.core.phase_key = None;
                    self.core.fail("setup.blocker.timeout");
                }
            }
            return;
        }
        if !self.writing {
            return;
        }
        match writes_settled(cx) {
            None => {}
            Some(Ok(())) => {
                self.writing = false;
                if self.needs_reboot {
                    self.core.phase_key = Some("rebooting".into());
                    self.reboot.start(cx);
                } else {
                    self.finish();
                }
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
        self.needs_reboot = false;
        for row in fc_profile(fw, vehicle_class(cx.adapter.class)) {
            let name = std::iter::once(row.name)
                .chain(row.alt_names.iter().copied())
                .find(|n| cx.params.has(n));
            match name {
                Some(n) => {
                    let v = row_value(cx, row);
                    self.needs_reboot |= row.reboot
                        && check_row(row, cx.inputs.place_class, |k| cx.params.get(k))
                            != RowCheck::Ok;
                    cx.params.set(n, v);
                    self.applied.push(json!({"name": n, "value": v}));
                }
                None => self.absent.push(json!(row.name)),
            }
        }
        self.writing = true;
        self.core.running();
        Ok(())
    }
}

/// The value to write for a profile row, from the same rules the readiness
/// items read (the place class for `fc_profile`, the GNSS rule for
/// `battery_failsafe`), so the fix settles them.
fn row_value(cx: &Cx, row: &ProfileParam) -> f32 {
    let have = std::iter::once(row.name)
        .chain(row.alt_names.iter().copied())
        .find_map(|n| cx.params.get(n));
    let value = row.value_in(cx.inputs.place_class, have);
    let free = crate::batt_failsafe::gnss_free(|n| cx.params.get(n));
    crate::batt_failsafe::profile_value(row.name, value, vehicle_class(cx.adapter.class), free)
}

#[derive(Default)]
pub struct AdosIntro {
    core: StepCore,
}

impl Step for AdosIntro {
    step_core!("ados_intro");

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        let fallback = if cx.fc.rc_present(cx.now) {
            "elrs"
        } else {
            "fc_manual"
        };
        self.core
            .args
            .insert("fallback_mode".into(), json!(fallback));
        self.core.wait_user(vec![
            Action::primary("use_ados"),
            Action::secondary("keep_fc"),
        ]);
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
