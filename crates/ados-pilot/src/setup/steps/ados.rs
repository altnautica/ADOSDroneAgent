//! `ados_profile`: the FC parameter profile ADOS Pilot depends on (one table
//! with the pilot's readiness check; the battery actions follow
//! [`crate::batt_failsafe::recommended`], Land for both on a multirotor that
//! lands on a low battery, [`crate::batt_failsafe::lands_on_low`]), and
//! `ados_intro`: "Meet ADOS Pilot", where the
//! operator chooses the control mode and declares whether an RC transmitter
//! flies with the vehicle ([`crate::no_rc`]). A changed row the FC reads only at
//! boot (`OA_TYPE`, `SRTL_POINTS`) reboots the FC before the step is done, as
//! the battery and frame steps do. In a confined place `SRTL_ACCURACY` keeps
//! a value within the confined bound and is written to the bound otherwise
//! ([`crate::fc_profile::CONFINED_SRTL_ACCURACY_MAX_M`]).

use ados_protocol::pilot::{ControlMode, RcReceiver, VehicleClass};
use serde_json::{json, Map, Value};

use crate::fc_profile::{check_row, Firmware, ProfileParam, RowCheck};
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
            .into_iter()
            .flat_map(|f| crate::no_rc::profile(f, class, cx.inputs.rc_receiver))
            .map(|r| json!({"name": r.name, "value": row_value(cx, &r)}))
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
        let declared =
            crate::no_rc::profile(fw, vehicle_class(cx.adapter.class), cx.inputs.rc_receiver);
        for row in declared {
            let row = &row;
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
/// items read (the place class for `fc_profile` and `battery_failsafe`, the
/// GNSS rule for `battery_failsafe`), so the fix settles them.
fn row_value(cx: &Cx, row: &ProfileParam) -> f32 {
    let have = std::iter::once(row.name)
        .chain(row.alt_names.iter().copied())
        .find_map(|n| cx.params.get(n));
    let value = row.value_in(cx.inputs.place_class, have);
    let lands = crate::batt_failsafe::lands_on_low(cx.inputs.place_class, |n| cx.params.get(n));
    crate::batt_failsafe::profile_value(row.name, value, vehicle_class(cx.adapter.class), lands)
}

#[derive(Default)]
pub struct AdosIntro {
    core: StepCore,
    /// The declaration rows being written, and the result once they settle.
    writing: Option<Map<String, Value>>,
}

/// Why `use_ados_no_rc` is refused here ([`crate::no_rc::declare_refusal`]
/// in ADOS control, the mode it sets).
fn no_rc_refusal(cx: &Cx) -> Option<&'static str> {
    crate::no_rc::declare_refusal(
        cx.fc.armed,
        ControlMode::Ados,
        firmware(cx.adapter.fw),
        vehicle_class(cx.adapter.class),
        cx.inputs.place_class,
    )
}

impl Step for AdosIntro {
    step_core!("ados_intro");

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.writing = None;
        let rc_present = cx.fc.rc_present(cx.now);
        let fallback = if rc_present { "elrs" } else { "fc_manual" };
        self.core
            .args
            .insert("fallback_mode".into(), json!(fallback));
        self.core
            .args
            .insert("rc_present".into(), json!(rc_present));
        // Flying with no RC transmitter is offered where it may be declared,
        // first when no receiver feeds the FC. A receiver whose transmitter
        // is off reads as none, so it is never chosen without the operator.
        let no_rc = no_rc_refusal(cx).is_none_or(|e| e == "armed");
        let actions = match (no_rc, rc_present) {
            (true, false) => vec![
                Action::primary("use_ados_no_rc"),
                Action::secondary("use_ados"),
                Action::secondary("keep_fc"),
            ],
            (true, true) => vec![
                Action::primary("use_ados"),
                Action::secondary("use_ados_no_rc"),
                Action::secondary("keep_fc"),
            ],
            (false, _) => vec![Action::primary("use_ados"), Action::secondary("keep_fc")],
        };
        self.core.wait_user(actions);
    }

    fn tick(&mut self, cx: &mut Cx) {
        if self.writing.is_none() {
            return;
        }
        match writes_settled(cx) {
            None => {}
            Some(Ok(())) => {
                let result = self.writing.take().unwrap_or_default();
                self.core.done(result);
            }
            Some(Err(names)) => {
                self.writing = None;
                self.core.result = Some(obj([("failed", json!(names))]));
                self.core.fail("setup.blocker.param_write_failed");
            }
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        let (mode, rc): (&'static str, RcReceiver) = match id {
            "use_ados" => ("ados", RcReceiver::Present),
            "use_ados_no_rc" => {
                if let Some(err) = no_rc_refusal(cx) {
                    return Err(err);
                }
                ("ados", RcReceiver::Absent)
            }
            "keep_fc" => {
                if cx.fc.rc_present(cx.now) {
                    ("elrs", RcReceiver::Present)
                } else {
                    ("fc_manual", RcReceiver::Present)
                }
            }
            _ => return Err("unknown_action"),
        };
        if cx.fc.armed {
            return Err("armed");
        }
        // The profile was written under the earlier declaration: the rows
        // this one decides (the RC failsafe, the fence) are written here.
        let mut applied = Vec::new();
        if let Some(fw) = firmware(cx.adapter.fw) {
            let current = |n: &str| cx.params.get(n);
            let rows =
                crate::no_rc::declaration_rows(fw, vehicle_class(cx.adapter.class), rc, current);
            for row in rows {
                let name = std::iter::once(row.name)
                    .chain(row.alt_names.iter().copied())
                    .find(|n| cx.params.has(n));
                if let Some(n) = name {
                    cx.params.set(n, row.value);
                    applied.push(json!({"name": n, "value": row.value}));
                }
            }
        }
        // The declaration after the control mode: `absent` needs `ados`, and
        // `present` must come before leaving it.
        if rc == RcReceiver::Present {
            cx.out.effect(Effect::SetRcReceiver(rc));
            cx.out.effect(Effect::SetControlMode(mode));
        } else {
            cx.out.effect(Effect::SetControlMode(mode));
            cx.out.effect(Effect::SetRcReceiver(rc));
        }
        self.writing = Some(obj([
            ("control_mode", json!(mode)),
            ("rc_receiver", json!(rc.as_str())),
            ("applied", Value::Array(applied)),
        ]));
        self.core.running();
        Ok(())
    }
}
