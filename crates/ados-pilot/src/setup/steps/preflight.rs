//! `preflight`: the `air` wizard's preconditions. A completed `ground`
//! wizard, every pilot readiness item green, the operator's open-area
//! confirmation and a battery at 80 % or more.

use serde_json::{json, Map, Value};

use crate::setup::fcstate::Class;
use crate::setup::schema::{obj, Action, ActionStyle, Confirm};
use crate::setup::step::{step_core, ActErr, Cx, Step, StepCore};

pub const MIN_BATTERY_PCT: i8 = 80;
/// Clear radius the operator confirms (m).
pub fn open_area_m(class: Class) -> u32 {
    if class == Class::Fixedwing {
        300
    } else {
        30
    }
}

#[derive(Default)]
pub struct Preflight {
    core: StepCore,
}

pub fn blockers(cx: &Cx) -> Vec<String> {
    let mut b = Vec::new();
    if !cx.ground_done {
        b.push("setup.blocker.ground_not_done".to_string());
    }
    if cx.fc.armed {
        b.push("setup.blocker.armed".to_string());
    }
    match cx.fc.battery_pct {
        Some(p) if p >= MIN_BATTERY_PCT => {}
        _ => b.push("setup.blocker.battery_low".to_string()),
    }
    for r in &cx.inputs.readiness_blockers {
        b.push(r.clone());
    }
    b
}

impl Step for Preflight {
    step_core!("preflight");

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.core
            .args
            .insert("open_area_m".into(), json!(open_area_m(cx.adapter.class)));
        // Interpolated by the body ("nothing within {radius} m") and the battery blocker ("below {pct} %").
        self.core
            .args
            .insert("radius".into(), json!(open_area_m(cx.adapter.class)));
        self.core.args.insert("pct".into(), json!(MIN_BATTERY_PCT));
        self.core.body_variant = (cx.adapter.class == Class::Fixedwing).then_some("plane");
        self.refresh(cx);
    }

    fn tick(&mut self, cx: &mut Cx) {
        if !self.core.state.is_terminal() {
            self.refresh(cx);
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        if id != "confirm_area" {
            return Err("unknown_action");
        }
        let b = blockers(cx);
        if !b.is_empty() {
            return Err("blocked");
        }
        cx.data.area_confirmed = true;
        self.core.done(obj([
            ("battery_pct", json!(cx.fc.battery_pct)),
            ("open_area_m", json!(open_area_m(cx.adapter.class))),
        ]));
        Ok(())
    }
}

impl Preflight {
    fn refresh(&mut self, cx: &Cx) {
        self.core.blockers = blockers(cx);
        if let Some(p) = cx.fc.battery_pct {
            self.core.set_live("battery_pct", p);
        }
        let confirm = Action::new("confirm_area", ActionStyle::Primary, Confirm::Hold);
        self.core.wait_user(if self.core.blockers.is_empty() {
            vec![confirm]
        } else {
            Vec::new()
        });
    }
}
