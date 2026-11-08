//! `initial_params`: the starting tune.
//!
//! Copter and VTOL (`Q_` params): Mission Planner's "Initial tune
//! parameters" formulas from the prop size and cell count. Plane: the
//! operator's airspeed limits and cruise throttle, plus TECS defaults the
//! `air` wizard refines.

use serde_json::{json, Map, Value};

use crate::setup::adapters::{ardupilot_copter, ardupilot_plane};
use crate::setup::fcstate::{Class, Fw};
use crate::setup::schema::{input_enum, input_number, obj, Action};
use crate::setup::step::{arg_f32, arg_str, step_core, writes_settled, ActErr, Cx, Step, StepCore};

/// TECS defaults before the `air` wizard measures them.
pub const TECS_DEFAULTS: &[(&str, f32)] = &[
    ("TECS_CLMB_MAX", 5.0),
    ("TECS_SINK_MAX", 5.0),
    ("TECS_SINK_MIN", 2.0),
];

#[derive(Default)]
pub struct InitialParams {
    core: StepCore,
    writing: bool,
    result: Map<String, Value>,
}

fn plane_part(cx: &Cx) -> bool {
    matches!(cx.adapter.class, Class::Fixedwing | Class::Vtol)
}

fn copter_part(cx: &Cx) -> bool {
    matches!(cx.adapter.class, Class::Multirotor | Class::Vtol)
}

impl Step for InitialParams {
    step_core!("initial_params");

    fn skip_reason(&self, cx: &Cx) -> Option<&'static str> {
        (cx.adapter.fw != Fw::ArduPilot).then_some("not_applicable")
    }

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.writing = false;
        let mut inputs = Map::new();
        if copter_part(cx) {
            inputs.insert(
                "chemistry".into(),
                input_enum(
                    Value::from(cx.data.chemistry.clone().unwrap_or_else(|| "lipo".into())),
                    vec!["lipo".into(), "lihv".into(), "liion".into()],
                    "setup.input.chemistry",
                ),
            );
        }
        if plane_part(cx) {
            let get =
                |names: &[&str], d: f32| names.iter().find_map(|n| cx.params.get(n)).unwrap_or(d);
            let cruise = match cx.params.first_present(ardupilot_plane::AIRSPEED_CRUISE) {
                Some("TRIM_ARSPD_CM") => cx.params.get("TRIM_ARSPD_CM").unwrap_or(1500.0) / 100.0,
                Some(n) => cx.params.get(n).unwrap_or(15.0),
                None => 15.0,
            };
            inputs.insert(
                "airspeed_min".into(),
                input_number(
                    get(ardupilot_plane::AIRSPEED_MIN, 10.0) as f64,
                    5.0,
                    50.0,
                    0.5,
                    "m/s",
                    "setup.input.airspeed_min",
                ),
            );
            inputs.insert(
                "airspeed_max".into(),
                input_number(
                    get(ardupilot_plane::AIRSPEED_MAX, 22.0) as f64,
                    8.0,
                    80.0,
                    0.5,
                    "m/s",
                    "setup.input.airspeed_max",
                ),
            );
            inputs.insert(
                "airspeed_cruise".into(),
                input_number(
                    cruise as f64,
                    6.0,
                    70.0,
                    0.5,
                    "m/s",
                    "setup.input.airspeed_cruise",
                ),
            );
            inputs.insert(
                "trim_throttle".into(),
                input_number(
                    cx.params.get("TRIM_THROTTLE").unwrap_or(45.0) as f64,
                    10.0,
                    90.0,
                    1.0,
                    "%",
                    "setup.input.trim_throttle",
                ),
            );
        }
        self.core
            .args
            .insert("inputs".into(), Value::Object(inputs));
        if let (Some(p), Some(c)) = (cx.data.prop_in, cx.data.cells) {
            self.core.args.insert("prop_in".into(), json!(p));
            self.core.args.insert("cells".into(), json!(c));
        }
        self.core.body_variant = match cx.adapter.class {
            Class::Fixedwing => Some("plane"),
            Class::Vtol => Some("vtol"),
            Class::Multirotor => None,
        };
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
                self.core.done(std::mem::take(&mut self.result));
            }
            Some(Err(names)) => {
                self.writing = false;
                self.core.result = Some(obj([("failed", json!(names))]));
                self.core.fail("setup.blocker.param_write_failed");
            }
        }
    }

    fn action(&mut self, id: &str, args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        if id != "apply" {
            return Err("unknown_action");
        }
        if cx.fc.armed {
            return Err("armed");
        }
        let mut written: Vec<(String, f32)> = Vec::new();
        if copter_part(cx) {
            let prop = cx.data.prop_in.ok_or("frame_first")? as f64;
            let cells = cx.data.cells.ok_or("frame_first")? as f64;
            let chem = arg_str(args, "chemistry").unwrap_or("lipo");
            if !matches!(chem, "lipo" | "lihv" | "liion") {
                return Err("bad_args");
            }
            cx.data.chemistry = Some(chem.to_string());
            let atc = cx.adapter.atc();
            let new_names = cx.params.has(&format!("{atc}_ACCEL_P_MAX"));
            for (name, v) in ardupilot_copter::initial_params(
                prop,
                cells,
                chem,
                atc,
                cx.adapter.mot(),
                new_names,
            ) {
                // A name this firmware does not have (e.g. ACRO_YAW_P on Plane) is left out.
                if cx.params.has(&name) {
                    written.push((name, v));
                }
            }
        }
        if plane_part(cx) {
            let min = arg_f32(args, "airspeed_min").ok_or("bad_args")?;
            let max = arg_f32(args, "airspeed_max").ok_or("bad_args")?;
            let cruise = arg_f32(args, "airspeed_cruise").ok_or("bad_args")?;
            let trim = arg_f32(args, "trim_throttle").ok_or("bad_args")?;
            if !(min < cruise && cruise < max) || !(10.0..=90.0).contains(&trim) {
                return Err("bad_args");
            }
            if let Some(n) = cx.params.first_present(ardupilot_plane::AIRSPEED_MIN) {
                written.push((n.into(), min));
            }
            if let Some(n) = cx.params.first_present(ardupilot_plane::AIRSPEED_MAX) {
                written.push((n.into(), max));
            }
            match cx.params.first_present(ardupilot_plane::AIRSPEED_CRUISE) {
                Some("TRIM_ARSPD_CM") => written.push(("TRIM_ARSPD_CM".into(), cruise * 100.0)),
                Some(n) => written.push((n.into(), cruise)),
                None => {}
            }
            written.push(("TRIM_THROTTLE".into(), trim));
            for (n, v) in TECS_DEFAULTS {
                written.push(((*n).into(), *v));
            }
        }
        let mut result = Map::new();
        for (n, v) in &written {
            cx.params.set(n, *v);
            result.insert(n.clone(), json!(*v));
        }
        self.result = obj([("params", Value::Object(result))]);
        self.writing = true;
        self.core.running();
        Ok(())
    }
}
