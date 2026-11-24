//! `frame`: frame class and type (copter, VTOL), wing outputs (plane),
//! airframe (PX4), plus the prop, motor and battery facts later steps use.
//!
//! A tailsitter (`Q_TAILSIT_ENABLE`) keeps its outputs: its elevons and
//! throttles belong to the tailsitter mixer, so no wing preset is offered or
//! written, and its own frame class (10) is accepted.

use serde_json::{json, Map, Value};

use crate::setup::adapters::{ardupilot_copter, ardupilot_plane};
use crate::setup::fcstate::{Class, Fw};
use crate::setup::schema::{input_enum, input_number, obj, Action};
use crate::setup::step::{
    arg_f32, arg_str, step_core, writes_settled, ActErr, Cx, Reboot, Rebooted, Step, StepCore,
};

#[derive(Default)]
pub struct Frame {
    core: StepCore,
    writing: bool,
    reboot: Reboot,
    needs_reboot: bool,
    result: Map<String, Value>,
}

fn class_names(cx: &Cx) -> (&'static str, &'static str) {
    if cx.adapter.class == Class::Vtol {
        ("Q_FRAME_CLASS", "Q_FRAME_TYPE")
    } else {
        ("FRAME_CLASS", "FRAME_TYPE")
    }
}

/// An ArduPlane tailsitter.
pub fn tailsitter(cx: &Cx) -> bool {
    cx.adapter.class == Class::Vtol
        && cx.adapter.fw == Fw::ArduPilot
        && cx.params.get("Q_TAILSIT_ENABLE").unwrap_or(0.0) > 0.0
}

/// The wing type to show: the one chosen earlier, else what the outputs are
/// set up as now.
fn wing(cx: &Cx) -> String {
    cx.data.wing.clone().unwrap_or_else(|| {
        ardupilot_plane::wing_from_outputs(|k| cx.params.get(&format!("SERVO{k}_FUNCTION")))
            .to_string()
    })
}

fn frame_classes(cx: &Cx) -> Vec<Value> {
    let mut v: Vec<Value> = vec![1.into(), 2.into(), 3.into()];
    if tailsitter(cx) {
        v.push(ardupilot_plane::FRAME_CLASS_TAILSITTER.into());
    }
    v
}

/// The picture of a frame class option (`setup-images/<key>.svg`): the
/// class's X layout, so the picker shows how many arms it has.
fn class_image(class: u8) -> Option<&'static str> {
    match class {
        ardupilot_plane::FRAME_CLASS_TAILSITTER => Some("vtol_transition"),
        c => ardupilot_copter::layout(c, 1).map(|(k, _)| k),
    }
}

/// The picture of a frame type option: the quad drawn in that arm layout.
fn type_image(frame_type: u8) -> Option<&'static str> {
    ardupilot_copter::layout(1, frame_type).map(|(k, _)| k)
}

/// An option value as the key of `images` (`1`, `"elevon"`), the way a
/// surface prints the value.
fn option_key(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// An enum input with `images: {<option>: <setup-image key>}`, so a surface
/// can draw the options as a picture picker. The options stay bare values.
fn with_images(mut input: Value, image: impl Fn(&Value) -> Option<String>) -> Value {
    let images: Map<String, Value> = input["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| Some((option_key(o), Value::from(image(o)?))))
        .collect();
    input["images"] = Value::Object(images);
    input
}

fn class_input(value: f64, options: Vec<Value>) -> Value {
    with_images(
        input_enum(Value::from(value), options, "setup.input.frame_class"),
        |o| class_image(o.as_u64()? as u8).map(str::to_string),
    )
}

fn type_input(value: f64) -> Value {
    with_images(
        input_enum(
            Value::from(value),
            vec![0.into(), 1.into(), 3.into()],
            "setup.input.frame_type",
        ),
        |o| type_image(o.as_u64()? as u8).map(str::to_string),
    )
}

fn wing_input(value: String) -> Value {
    with_images(
        input_enum(
            Value::from(value),
            vec!["normal".into(), "elevon".into(), "vtail".into()],
            "setup.input.wing",
        ),
        |o| Some(format!("surfaces_{}", o.as_str()?)),
    )
}

impl Frame {
    fn inputs(&self, cx: &Cx) -> Map<String, Value> {
        let mut inputs = Map::new();
        let p = |n: &str, d: f32| cx.params.get(n).unwrap_or(d) as f64;
        match (cx.adapter.fw, cx.adapter.class) {
            (Fw::Px4, _) => {
                inputs.insert(
                    "sys_autostart".into(),
                    input_number(
                        p("SYS_AUTOSTART", 4001.0),
                        0.0,
                        99_999.0,
                        1.0,
                        "",
                        "setup.input.sys_autostart",
                    ),
                );
            }
            (_, Class::Fixedwing) => {
                inputs.insert("wing".into(), wing_input(wing(cx)));
            }
            (_, class) => {
                let (c, t) = class_names(cx);
                inputs.insert(
                    "frame_class".into(),
                    class_input(p(c, 1.0), frame_classes(cx)),
                );
                inputs.insert("frame_type".into(), type_input(p(t, 1.0)));
                if class == Class::Vtol && !tailsitter(cx) {
                    inputs.insert("wing".into(), wing_input(wing(cx)));
                }
            }
        }
        if cx.adapter.copter_like() || cx.adapter.fw == Fw::Px4 {
            inputs.insert(
                "prop_in".into(),
                input_number(
                    cx.data.prop_in.unwrap_or(10.0) as f64,
                    2.0,
                    40.0,
                    0.5,
                    "in",
                    "setup.input.prop_in",
                ),
            );
            inputs.insert(
                "motor_kv".into(),
                input_number(
                    cx.data.motor_kv.unwrap_or(900.0) as f64,
                    50.0,
                    5000.0,
                    10.0,
                    "rpm/V",
                    "setup.input.motor_kv",
                ),
            );
        }
        inputs.insert(
            "cells".into(),
            input_number(
                cx.data.cells.unwrap_or(4.0) as f64,
                1.0,
                14.0,
                1.0,
                "S",
                "setup.input.cells",
            ),
        );
        inputs
    }
}

impl Step for Frame {
    step_core!("frame");

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.writing = false;
        self.needs_reboot = false;
        self.core.body_variant = match (cx.adapter.fw, cx.adapter.class) {
            (Fw::Px4, _) => Some("px4"),
            (_, Class::Fixedwing) => Some("plane"),
            (_, Class::Vtol) => Some("vtol"),
            _ => None,
        };
        let inputs = self.inputs(cx);
        self.core
            .args
            .insert("inputs".into(), Value::Object(inputs));
        if cx.adapter.copter_like() && cx.adapter.fw == Fw::ArduPilot {
            let (c, t) = class_names(cx);
            let fc = cx.params.get(c).unwrap_or(1.0) as u8;
            let ft = cx.params.get(t).unwrap_or(1.0) as u8;
            self.core.image = ardupilot_copter::layout(fc, ft).map(|(k, _)| k.to_string());
        } else if cx.adapter.class == Class::Fixedwing {
            self.core.image = Some(format!("surfaces_{}", wing(cx)));
        }
        self.core.wait_user(vec![Action::primary("apply")]);
    }

    fn tick(&mut self, cx: &mut Cx) {
        if self.reboot.active() {
            match self.reboot.tick(cx) {
                Rebooted::Waiting => {}
                Rebooted::Back => self.core.done(std::mem::take(&mut self.result)),
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
        if cx.fc.armed {
            return Err("armed");
        }
        let cells = arg_f32(args, "cells")
            .filter(|c| (1.0..=14.0).contains(c))
            .ok_or("bad_args")?;
        cx.data.cells = Some(cells.round());
        let mut result = obj([("cells", json!(cells.round()))]);
        if cx.adapter.copter_like() || cx.adapter.fw == Fw::Px4 {
            let prop = arg_f32(args, "prop_in")
                .filter(|v| (2.0..=40.0).contains(v))
                .ok_or("bad_args")?;
            let kv = arg_f32(args, "motor_kv")
                .filter(|v| (50.0..=5000.0).contains(v))
                .ok_or("bad_args")?;
            cx.data.prop_in = Some(prop);
            cx.data.motor_kv = Some(kv);
            result.insert("prop_in".into(), json!(prop));
            result.insert("motor_kv".into(), json!(kv));
        }
        match (cx.adapter.fw, cx.adapter.class) {
            (Fw::Px4, _) => {
                let sa = arg_f32(args, "sys_autostart").ok_or("bad_args")?.round();
                if cx.params.get("SYS_AUTOSTART").is_none_or(|v| v != sa) {
                    cx.params.set("SYS_AUTOSTART", sa);
                    self.needs_reboot = true;
                }
                result.insert("sys_autostart".into(), json!(sa));
            }
            (_, class) => {
                if class != Class::Fixedwing {
                    let fc = arg_f32(args, "frame_class").ok_or("bad_args")?.round();
                    let ft = arg_f32(args, "frame_type").ok_or("bad_args")?.round();
                    let own_class =
                        tailsitter(cx) && fc as u8 == ardupilot_plane::FRAME_CLASS_TAILSITTER;
                    if !own_class && ardupilot_copter::layout(fc as u8, ft as u8).is_none() {
                        return Err("unsupported_frame");
                    }
                    let (cn, tn) = class_names(cx);
                    if cx.params.get(cn).is_none_or(|v| v != fc) {
                        self.needs_reboot = true;
                    }
                    cx.params.set(cn, fc);
                    cx.params.set(tn, ft);
                    cx.data.frame_class = Some(fc as u8);
                    cx.data.frame_type = Some(ft as u8);
                    result.insert("frame_class".into(), json!(fc));
                    result.insert("frame_type".into(), json!(ft));
                }
                if class != Class::Multirotor && !tailsitter(cx) {
                    let wing = arg_str(args, "wing").ok_or("bad_args")?;
                    let preset = ardupilot_plane::wing_preset(wing).ok_or("bad_args")?;
                    // The outputs already describe this wing: keep their exact assignment.
                    let current = ardupilot_plane::wing_from_outputs(|k| {
                        cx.params.get(&format!("SERVO{k}_FUNCTION"))
                    });
                    if wing != current {
                        for (out, f) in preset {
                            cx.params.set(&format!("SERVO{out}_FUNCTION"), *f as f32);
                        }
                    }
                    cx.data.wing = Some(wing.to_string());
                    result.insert("wing".into(), json!(wing));
                }
            }
        }
        self.result = result;
        self.writing = true;
        self.core.running();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ados_protocol::mavlink::ardupilotmega::MavType;
    use serde_json::Value;

    use super::{ardupilot_copter, class_image, type_image};
    use crate::setup::fake_fc::{ap_copter_params, FakeFc, TICK};
    use crate::setup::schema::{StepState, WizardId};
    use crate::setup::Engine;

    fn image_exists(key: &str) -> bool {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../web-shared/src/cockpit-spec/setup-images")
            .join(format!("{key}.svg"))
            .is_file()
    }

    /// The copter frame step offers a picture for every frame class and type
    /// option, and each picture ships in the spec's setup-images.
    #[test]
    fn every_copter_frame_option_has_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = Engine::new(dir.path(), 1_794_268_800_000);
        let mut fc = FakeFc::new(MavType::MAV_TYPE_QUADROTOR, ap_copter_params());
        let mut now = 0;
        let mut tick = |e: &mut Engine, now: &mut u64| {
            *now += TICK;
            fc.step(e, *now);
        };
        while !(e.uid.is_some() && e.params.complete()) {
            assert!(now < 20_000, "FC not identified");
            tick(&mut e, &mut now);
        }
        assert_eq!(e.start(WizardId::Ground, now)["ok"], Value::Bool(true));
        let view = loop {
            assert!(now < 80_000, "frame step not reached");
            if let Some(v) = e
                .current_view()
                .filter(|v| v.step == "frame" && v.state == StepState::WaitingUser)
            {
                break v;
            }
            tick(&mut e, &mut now);
        };
        for name in ["frame_class", "frame_type"] {
            let input = &view.args["inputs"][name];
            let options = input["options"].as_array().expect("options");
            assert!(!options.is_empty(), "{name} has options");
            for o in options {
                let key = super::option_key(o);
                let image = input["images"][&key]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name} option {key} has no image"));
                assert!(
                    image_exists(image),
                    "{name} option {key}: {image}.svg missing"
                );
            }
        }
    }

    /// Every supported copter layout, and every option picture, ships.
    #[test]
    fn every_layout_image_ships() {
        for class in [1u8, 2, 3] {
            for frame_type in [0u8, 1, 3] {
                if let Some((key, _)) = ardupilot_copter::layout(class, frame_type) {
                    assert!(image_exists(key), "{key}.svg missing");
                }
            }
            assert!(class_image(class).is_some_and(image_exists));
        }
        for frame_type in [0u8, 1, 3] {
            assert!(type_image(frame_type).is_some_and(image_exists));
        }
        for wing in ["normal", "elevon", "vtail"] {
            assert!(image_exists(&format!("surfaces_{wing}")));
        }
        assert!(
            class_image(super::ardupilot_plane::FRAME_CLASS_TAILSITTER).is_some_and(image_exists)
        );
    }
}
