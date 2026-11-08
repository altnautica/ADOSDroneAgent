//! `battery`: monitor type, capacity, cell count from voltage, and the
//! voltage multiplier corrected against a multimeter reading.

use serde_json::{json, Map, Value};

use crate::setup::fcstate::Fw;
use crate::setup::schema::{input_number, obj, Action};
use crate::setup::step::{arg_f32, step_core, writes_settled, ActErr, Cx, Reboot, Rebooted, Step, StepCore};

/// Nominal LiPo cell voltage used to guess the cell count from a reading.
const NOMINAL_CELL_V: f32 = 3.85;
/// ArduPilot analog voltage and current monitor.
const MONITOR_ANALOG: f32 = 4.0;

#[derive(Default)]
pub struct Battery {
    core: StepCore,
    writing: bool,
    needs_reboot: bool,
    reboot: Reboot,
    result: Map<String, Value>,
}

/// Cell count a pack voltage suggests.
pub fn cells_from_voltage(v: f32) -> u8 {
    (v / NOMINAL_CELL_V).round().clamp(1.0, 14.0) as u8
}

impl Battery {
    fn names(fw: Fw) -> (&'static str, &'static str) {
        match fw {
            Fw::Px4 => ("BAT1_CAPACITY", "BAT1_V_DIV"),
            _ => ("BATT_CAPACITY", "BATT_VOLT_MULT"),
        }
    }
}

impl Step for Battery {
    step_core!("battery");

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.writing = false;
        self.needs_reboot = false;
        let (cap, _) = Self::names(cx.adapter.fw);
        let capacity = cx.params.get(cap).unwrap_or(5000.0);
        let reported = cx.fc.battery_v.unwrap_or(0.0);
        let mut inputs = Map::new();
        inputs.insert("capacity_mah".into(), input_number(capacity as f64, 100.0, 100_000.0, 50.0, "mAh", "setup.input.capacity_mah"));
        inputs.insert("measured_v".into(), input_number(reported as f64, 0.0, 70.0, 0.01, "V", "setup.input.measured_v"));
        self.core.args.insert("inputs".into(), Value::Object(inputs));
        if cx.adapter.fw == Fw::Px4 {
            self.core.body_variant = Some("px4");
        }
        self.core.wait_user(vec![Action::primary("apply")]);
    }

    fn tick(&mut self, cx: &mut Cx) {
        if let Some(v) = cx.fc.battery_v {
            self.core.set_live("reported_v", (v * 100.0).round() / 100.0);
            self.core.set_live("cells_detected", cells_from_voltage(v));
        }
        if self.reboot.active() {
            match self.reboot.tick(cx) {
                Rebooted::Waiting => {}
                Rebooted::Back => {
                    self.core.phase_key = None;
                    self.core.done(std::mem::take(&mut self.result));
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
            Some(Err(_)) => {
                self.writing = false;
                self.core.fail("setup.blocker.param_write_failed");
            }
            Some(Ok(())) => {
                self.writing = false;
                if self.needs_reboot {
                    self.core.phase_key = Some("rebooting".into());
                    self.reboot.start(cx);
                } else {
                    self.core.done(std::mem::take(&mut self.result));
                }
            }
        }
    }

    fn action(&mut self, id: &str, args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        if id != "apply" {
            return Err("unknown_action");
        }
        let capacity = arg_f32(args, "capacity_mah").filter(|c| *c >= 100.0).ok_or("bad_args")?;
        let measured = arg_f32(args, "measured_v").filter(|v| *v >= 0.0).ok_or("bad_args")?;
        let (cap_name, mult_name) = Self::names(cx.adapter.fw);
        let mut result = obj([("capacity_mah", json!(capacity))]);
        if cx.adapter.fw == Fw::ArduPilot && cx.params.get("BATT_MONITOR").unwrap_or(0.0) == 0.0 {
            cx.params.set("BATT_MONITOR", MONITOR_ANALOG);
            self.needs_reboot = true;
            result.insert("monitor".into(), json!(MONITOR_ANALOG));
        }
        cx.params.set(cap_name, capacity);
        let reported = cx.fc.battery_v.unwrap_or(0.0);
        if measured > 1.0 && reported > 1.0 {
            let mult = cx.params.get(mult_name).ok_or("missing_param")?;
            let new = mult * measured / reported;
            cx.params.set(mult_name, new);
            result.insert("volt_mult_before".into(), json!(mult));
            result.insert("volt_mult_after".into(), json!(new));
            result.insert("measured_v".into(), json!(measured));
            result.insert("reported_v".into(), json!(reported));
        }
        let cells = cells_from_voltage(if measured > 1.0 { measured } else { reported });
        if reported > 1.0 || measured > 1.0 {
            cx.data.cells.get_or_insert(cells as f32);
            result.insert("cells_detected".into(), json!(cells));
            if cx.adapter.fw == Fw::Px4 {
                cx.params.set("BAT1_N_CELLS", cells as f32);
            }
        }
        self.result = result;
        self.writing = true;
        self.core.running();
        Ok(())
    }
}
