//! Setup engine tests against the scripted fake FC.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, MavType};
use serde_json::{json, Map, Value};

use super::doublets::Doublets;
use super::fake_fc::{ap_copter_params, ap_plane_params, FakeFc, TICK};
use super::out::Effect;
use super::schema::{StepState, WizardId, WizardState};
use super::Engine;

struct Sim {
    e: Engine,
    fc: FakeFc,
    now: u64,
    _dir: tempfile::TempDir,
}

impl Sim {
    fn new(mav_type: MavType, params: std::collections::BTreeMap<String, f32>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Sim { e: Engine::new(dir.path(), 1_794_268_800_000), fc: FakeFc::new(mav_type, params), now: 0, _dir: dir };
        // FC identified and parameters loaded.
        s.until(|s| s.e.uid.is_some() && s.e.params.complete(), 20_000);
        s
    }

    fn copter() -> Self {
        Sim::new(MavType::MAV_TYPE_QUADROTOR, ap_copter_params())
    }

    fn run(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.now += TICK;
            self.fc.step(&mut self.e, self.now);
            self.check_placeholders();
        }
    }

    fn until(&mut self, cond: impl Fn(&Sim) -> bool, max_ms: u64) {
        let end = self.now + max_ms;
        while !cond(self) {
            assert!(
                self.now < end,
                "condition not met in {max_ms} ms; step: {:?}; params idle {} complete {}",
                self.e.current_view().map(|v| (v.step, v.state, v.blockers, v.body_key, v.live)),
                self.e.params.idle(),
                self.e.params.complete()
            );
            self.now += TICK;
            self.fc.step(&mut self.e, self.now);
            self.check_placeholders();
        }
    }

    /// Every `{arg}` in a string the running step names (title, body with the
    /// spec's fallback to `setup.<step>.body`, the live question, blockers) is
    /// in the step's `args`, so no surface ever shows a literal `{n}`.
    fn check_placeholders(&self) {
        let Some(v) = self.e.current_view() else { return };
        let strings = &SETUP_STRINGS["strings"];
        let mut keys = vec![v.title_key.clone(), v.body_key.clone()];
        if strings.get(&v.body_key).is_none() {
            keys.push(format!("setup.{}.body", v.step));
        }
        if let Some(q) = v.live.get("question_key").and_then(Value::as_str) {
            keys.push(q.to_string());
        }
        keys.extend(v.blockers.iter().cloned());
        for k in keys {
            let Some(text) = strings.get(&k).and_then(Value::as_str) else { continue };
            for ph in placeholders(text) {
                assert!(
                    v.args.contains_key(&ph),
                    "step {} state {:?}: `{k}` = {text:?} interpolates {{{ph}}} but args are {:?}",
                    v.step,
                    v.state,
                    v.args.keys().collect::<Vec<_>>()
                );
            }
        }
    }
}

static SETUP_STRINGS: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../../web-shared/src/cockpit-spec/setup-strings.json"))
        .expect("setup-strings.json parses")
});

/// The `{name}` placeholders of a spec string.
fn placeholders(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else { break };
        let name = &after[..close];
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            out.push(name.to_string());
        }
        rest = &after[close + 1..];
    }
    out
}

impl Sim {
    fn step_id(&self) -> Option<String> {
        self.e.current_view().map(|v| v.step)
    }

    fn state(&self) -> Option<StepState> {
        self.e.current_view().map(|v| v.state)
    }

    fn wait_step(&mut self, id: &str, state: StepState) {
        let id = id.to_string();
        self.until(move |s| s.step_id().as_deref() == Some(id.as_str()) && s.state() == Some(state), 60_000);
    }

    fn act(&mut self, w: WizardId, step: &str, action: &str, args: Value) -> Value {
        let args: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
        let r = self.e.action(w, step, action, &args, self.now);
        assert_eq!(r["ok"], json!(true), "{step}/{action}: {r}");
        r
    }

    fn sent_cmds(&self, cmd: MavCmd) -> Vec<[f32; 7]> {
        self.fc
            .sent
            .iter()
            .filter_map(|m| match m {
                MavMessage::COMMAND_LONG(c) if c.command == cmd => {
                    Some([c.param1, c.param2, c.param3, c.param4, c.param5, c.param6, c.param7])
                }
                _ => None,
            })
            .collect()
    }

    fn result(&self, w: WizardId, step: &str) -> Value {
        let v = self.e.view(self.now);
        let wz = v.wizards.iter().find(|x| x.id == w).unwrap();
        let s = wz.steps.iter().find(|s| s.step == step).unwrap();
        serde_json::to_value(&s.result).unwrap()
    }

    fn step_state(&self, w: WizardId, step: &str) -> StepState {
        let v = self.e.view(self.now);
        v.wizards.iter().find(|x| x.id == w).unwrap().steps.iter().find(|s| s.step == step).unwrap().state
    }

    fn param(&self, n: &str) -> f32 {
        self.fc.params[n]
    }
}

const G: WizardId = WizardId::Ground;
const A: WizardId = WizardId::Air;

/// Accel through all six poses: the engine shows each pose the FC asks for
/// and answers with the same pose.
fn do_accel(s: &mut Sim) {
    s.wait_step("accel", StepState::WaitingUser);
    s.act(G, "accel", "start", json!({}));
    for pose in 1..=6u8 {
        s.until(move |s| s.e.current_view().is_some_and(|v| v.state == StepState::WaitingUser && v.live.get("pose").is_some()
            && v.image.as_deref() == Some(["", "accel_level", "accel_left", "accel_right", "accel_nose_down", "accel_nose_up", "accel_back"][pose as usize]))
            && s.fc_pose() == Some(pose), 5_000);
        s.act(G, "accel", "next_pose", json!({}));
    }
}

impl Sim {
    fn fc_pose(&self) -> Option<u8> {
        self.sent_cmds(MavCmd::MAV_CMD_ACCELCAL_VEHICLE_POS).len().checked_add(1).map(|n| n as u8)
    }
}

#[test]
fn full_ground_copter_flow() {
    let mut s = Sim::copter();
    let r = s.e.start(G, s.now);
    assert_eq!(r["ok"], json!(true), "{r}");
    // detect
    s.wait_step("frame", StepState::WaitingUser);
    assert_eq!(s.step_state(G, "detect"), StepState::Done);
    assert_eq!(s.result(G, "detect")["vehicle_class"], json!("multirotor"));
    // A snapshot exists before the first parameter-changing step.
    let snaps = s.e.view(s.now).report.snapshots;
    assert_eq!(snaps.len(), 1);
    s.act(G, "frame", "apply", json!({"frame_class": 1, "frame_type": 1, "prop_in": 10, "motor_kv": 900, "cells": 4}));
    do_accel(&mut s);
    s.wait_step("level", StepState::WaitingUser);
    assert_eq!(s.step_state(G, "accel"), StepState::Done);
    s.act(G, "level", "start", json!({}));
    s.wait_step("compass", StepState::WaitingUser);
    s.act(G, "compass", "start", json!({}));
    s.wait_step("outputs", StepState::WaitingUser);
    assert_eq!(s.fc.reboots, 1, "compass reboots the FC");
    // No RC lane: radio and the ADOS switch are skipped.
    assert_eq!(s.step_state(G, "radio"), StepState::Skipped);
    assert_eq!(s.step_state(G, "rc_ados_switch"), StepState::Skipped);
    for _ in 0..4 {
        s.act(G, "outputs", "test", json!({}));
        s.until(|s| s.e.current_view().unwrap().live.get("question_key") == Some(&json!("setup.question.motor_position")), 5_000);
        s.act(G, "outputs", "answer_yes", json!({}));
        s.act(G, "outputs", "answer_yes", json!({}));
    }
    let tests = s.sent_cmds(MavCmd::MAV_CMD_DO_MOTOR_TEST);
    assert_eq!(tests.iter().map(|p| p[0] as u8).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    assert!(tests.iter().all(|p| p[2] == 5.0 && p[3] == 2.0));
    s.wait_step("battery", StepState::WaitingUser);
    s.act(G, "battery", "apply", json!({"capacity_mah": 5000, "measured_v": 16.4}));
    s.wait_step("initial_params", StepState::WaitingUser);
    assert!((s.param("BATT_VOLT_MULT") - 10.1 * 16.4 / 16.0).abs() < 1e-3);
    assert_eq!(s.param("BATT_CAPACITY"), 5000.0);
    s.act(G, "initial_params", "apply", json!({"chemistry": "lipo"}));
    s.wait_step("ados_profile", StepState::WaitingUser);
    // 10 in props: Mission Planner's initial-tune formulas.
    assert_eq!(s.param("INS_GYRO_FILTER"), 42.0);
    assert_eq!(s.param("ATC_ACCEL_P_MAX"), 116_700.0);
    assert_eq!(s.param("ATC_ACCEL_Y_MAX"), 27_000.0);
    assert_eq!(s.param("ATC_RAT_RLL_FLTD"), 21.0);
    assert_eq!(s.param("MOT_THST_EXPO"), 0.6);
    assert!((s.param("MOT_BAT_VOLT_MAX") - 16.8).abs() < 1e-4);
    // No ESC telemetry: the static notch waits for the air wizard.
    assert_eq!(s.step_state(G, "notch_static"), StepState::Skipped);
    assert!(s.e.data().notch_deferred);
    s.act(G, "ados_profile", "apply", json!({}));
    s.wait_step("ados_intro", StepState::WaitingUser);
    assert_eq!(s.param("MAV_GCS_SYSID"), 1.0);
    assert_eq!(s.param("FS_GCS_ENABLE"), 1.0);
    assert_eq!(s.param("FS_GCS_TIMEOUT"), 3.0);
    assert_eq!(s.param("BATT_FS_LOW_ACT"), 2.0);
    assert_eq!(s.result(G, "ados_profile")["absent"], json!(["GUID_TIMEOUT"]));
    s.act(G, "ados_intro", "use_ados", json!({}));
    s.run(100);
    assert_eq!(s.e.wizard_state(G), WizardState::Done);
    assert!(s.fc.effects.contains(&Effect::SetControlMode("ados")));
    assert_eq!(s.e.setup_needed(s.now), None);
    // Persisted: a fresh engine on the same directory sees the completed wizard.
    let mut e2 = Engine::new(s._dir.path(), 0);
    let mut fc2 = FakeFc::new(MavType::MAV_TYPE_QUADROTOR, ap_copter_params());
    let mut t = 0;
    while e2.uid.is_none() {
        t += TICK;
        fc2.step(&mut e2, t);
    }
    assert_eq!(e2.wizard_state(G), WizardState::Done);
}

#[test]
fn accel_pose_sequence() {
    let mut s = Sim::copter();
    assert_eq!(s.e.run_step(G, "accel", s.now)["ok"], json!(true));
    do_accel(&mut s);
    s.until(|s| s.e.current_view().is_none(), 5_000);
    assert_eq!(s.step_state(G, "accel"), StepState::Done);
    let poses: Vec<u8> = s.sent_cmds(MavCmd::MAV_CMD_ACCELCAL_VEHICLE_POS).iter().map(|p| p[0] as u8).collect();
    assert_eq!(poses, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(s.sent_cmds(MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION)[0][4], 1.0);
}

#[test]
fn compass_fitness_retry() {
    let mut s = Sim::copter();
    s.fc.script.mag_fitness = vec![30.0, 9.5].into();
    s.e.run_step(G, "compass", s.now);
    s.act(G, "compass", "start", json!({}));
    s.until(|s| s.e.current_view().is_none(), 30_000);
    assert_eq!(s.step_state(G, "compass"), StepState::Done);
    assert_eq!(s.sent_cmds(MavCmd::MAV_CMD_DO_START_MAG_CAL).len(), 2);
    let r = s.result(G, "compass");
    assert_eq!(r["attempts"], json!(2));
    assert_eq!(r["fitness_mgauss"]["0"], json!(9.5));
    // Three bad attempts fail the step.
    s.fc.script.mag_fitness = vec![40.0, 40.0, 40.0].into();
    s.e.run_step(G, "compass", s.now);
    s.act(G, "compass", "start", json!({}));
    s.until(|s| s.state() == Some(StepState::Failed), 30_000);
    assert_eq!(s.e.current_view().unwrap().blockers, vec!["setup.blocker.compass_fitness".to_string()]);
}

#[test]
fn motor_direction_fix_through_blh_rvmask() {
    let mut p = ap_copter_params();
    p.insert("MOT_PWM_TYPE".into(), 6.0);
    let mut s = Sim::new(MavType::MAV_TYPE_QUADROTOR, p);
    s.e.run_step(G, "outputs", s.now);
    for seq in 1..=4 {
        s.act(G, "outputs", "test", json!({}));
        s.until(|s| s.e.current_view().unwrap().live.get("question_key") == Some(&json!("setup.question.motor_position")), 5_000);
        s.act(G, "outputs", "answer_yes", json!({}));
        // Motor B (sequence 2) is motor 4 on output 4 in the quad X layout.
        s.act(G, "outputs", if seq == 2 { "answer_no" } else { "answer_yes" }, json!({}));
    }
    s.until(|s| s.e.current_view().is_none(), 20_000);
    assert_eq!(s.step_state(G, "outputs"), StepState::Done);
    assert_eq!(s.param("SERVO_BLH_RVMASK"), 8.0);
    assert_eq!(s.param("SERVO_BLH_AUTO"), 1.0);
    assert_eq!(s.fc.reboots, 1);
    // Without DShot the same answer asks for two wires swapped.
    let mut s = Sim::copter();
    s.e.run_step(G, "outputs", s.now);
    for seq in 1..=4 {
        s.act(G, "outputs", "test", json!({}));
        s.until(|s| s.e.current_view().unwrap().live.get("question_key") == Some(&json!("setup.question.motor_position")), 5_000);
        s.act(G, "outputs", "answer_yes", json!({}));
        s.act(G, "outputs", if seq == 3 { "answer_no" } else { "answer_yes" }, json!({}));
    }
    s.until(|s| s.state() == Some(StepState::Failed), 5_000);
    assert_eq!(s.e.current_view().unwrap().blockers, vec!["setup.blocker.swap_motor_wires".to_string()]);
    assert_eq!(s.param("SERVO_BLH_RVMASK"), 0.0);
}

#[test]
fn plane_surface_reversal() {
    let mut s = Sim::new(MavType::MAV_TYPE_FIXED_WING, ap_plane_params());
    s.fc.custom_mode = 5;
    s.run(1100);
    s.e.run_step(G, "outputs", s.now);
    // Aileron (1), elevator (2), rudder (4); the elevator moves the wrong way.
    for ch in [1u8, 2, 4] {
        assert_eq!(s.e.current_view().unwrap().live["output_index"], json!(ch));
        s.act(G, "outputs", "test", json!({}));
        s.until(|s| s.state() == Some(StepState::WaitingUser), 5_000);
        s.act(G, "outputs", if ch == 2 { "answer_no" } else { "answer_yes" }, json!({}));
    }
    s.until(|s| s.e.current_view().is_none(), 5_000);
    assert_eq!(s.step_state(G, "outputs"), StepState::Done);
    assert_eq!(s.param("SERVO2_REVERSED"), 1.0);
    assert_eq!(s.param("SERVO1_REVERSED"), 0.0);
    assert_eq!(s.param("SERVO_AUTO_TRIM"), 1.0);
    // The surfaces were driven in MANUAL with a nose-up demand for the elevator.
    assert_eq!(s.sent_cmds(MavCmd::MAV_CMD_DO_SET_MODE)[0][1], 0.0);
    assert!(s.fc.sent.iter().any(|m| matches!(m, MavMessage::MANUAL_CONTROL(c) if c.x == -300 && c.y == 0)));
    assert!(s.fc.sent.iter().any(|m| matches!(m, MavMessage::MANUAL_CONTROL(c) if c.y == 300)));
}

#[test]
fn snapshot_then_revert() {
    let mut s = Sim::copter();
    s.e.run_step(G, "frame", s.now);
    s.act(G, "frame", "apply", json!({"frame_class": 1, "frame_type": 1, "prop_in": 7, "motor_kv": 1800, "cells": 4}));
    s.until(|s| s.e.current_view().is_none(), 10_000);
    let snaps = s.e.view(s.now).report.snapshots;
    assert_eq!(snaps.len(), 1, "snapshot taken before the first parameter-changing step");
    s.e.run_step(G, "initial_params", s.now);
    s.act(G, "initial_params", "apply", json!({"chemistry": "lipo"}));
    s.until(|s| s.e.current_view().is_none(), 10_000);
    assert_eq!(s.step_state(G, "initial_params"), StepState::Done);
    assert_eq!(s.param("INS_GYRO_FILTER"), 57.0);
    assert_eq!(s.param("MOT_HOVER_LEARN"), 2.0);
    let r = s.e.revert(None, s.now);
    assert_eq!(r["ok"], json!(true), "{r}");
    assert!(r["restored"].as_u64().unwrap() >= 10);
    s.run(3000);
    assert_eq!(s.param("INS_GYRO_FILTER"), 20.0);
    assert_eq!(s.param("ATC_ACCEL_P_MAX"), 110_000.0);
    assert_eq!(s.param("MOT_THST_EXPO"), 0.65);
    // A snapshot name cannot leave the aircraft's directory.
    assert_eq!(s.e.revert(Some("../state.json"), s.now)["ok"], json!(false));
}

/// Ground done, battery full: the air wizard may run.
fn air_ready() -> Sim {
    let mut s = Sim::copter();
    s.e.state.ground.state = WizardState::Done;
    s.e.state.data.prop_in = Some(10.0);
    s.e.state.data.motor_kv = Some(900.0);
    s.e.state.data.cells = Some(4.0);
    s
}

#[test]
fn air_wizard_needs_ground_and_battery() {
    let mut s = Sim::copter();
    assert_eq!(s.e.start(A, s.now)["err"], json!("ground_not_done"));
    let mut s2 = air_ready();
    s2.fc.battery_pct = 60;
    s2.run(1500);
    assert_eq!(s2.e.run_step(A, "first_hover", s2.now)["err"], json!("battery_low"));
    s.e.state.ground.state = WizardState::Done;
    assert_eq!(s.e.start(A, s.now)["ok"], json!(true));
    s.wait_step("preflight", StepState::WaitingUser);
    s.act(A, "preflight", "confirm_area", json!({}));
    s.wait_step("first_hover", StepState::WaitingUser);
    s.run(100);
    assert!(s.fc.effects.contains(&Effect::EnterTuning));
}

fn hover(s: &mut Sim) {
    assert_eq!(s.e.run_step(A, "first_hover", s.now)["ok"], json!(true));
    s.run(100);
    s.act(A, "first_hover", "start", json!({}));
    s.until(|s| s.fc.alt > 2.9, 20_000);
    s.run(2000);
}

#[test]
fn first_hover_measures_and_lands() {
    let mut s = air_ready();
    hover(&mut s);
    s.until(|s| s.e.current_view().is_none(), 80_000);
    assert_eq!(s.step_state(A, "first_hover"), StepState::Done);
    let r = s.result(A, "first_hover");
    assert_eq!(r["vibration_verdict"], json!("pass"));
    assert_eq!(r["vibration_max"], json!([8.0, 9.0, 12.0]));
    assert!((r["hover_throttle"].as_f64().unwrap() - 0.35).abs() < 1e-6);
    assert!(!s.fc.armed);
    // Landed and disarmed: the tuning owner is handed back.
    assert!(s.fc.effects.contains(&Effect::ExitTuning { operator: false }));
}

fn abort_case(inject: impl Fn(&mut Sim), reason: &str, operator: bool) {
    let mut s = air_ready();
    hover(&mut s);
    let lands_before = s.sent_cmds(MavCmd::MAV_CMD_DO_SET_MODE).iter().filter(|p| p[1] == 9.0).count();
    inject(&mut s);
    s.until(|s| s.e.current_view().is_none(), 5_000);
    assert_eq!(s.step_state(A, "first_hover"), StepState::Failed);
    let v = s.e.view(s.now);
    let fh = v.wizards[1].steps.iter().find(|x| x.step == "first_hover").unwrap();
    let _ = fh;
    let rec = &s.e.state.air.steps["first_hover"];
    assert_eq!(rec["state"], json!("failed"), "{reason}");
    let lands = s.sent_cmds(MavCmd::MAV_CMD_DO_SET_MODE).iter().filter(|p| p[1] == 9.0).count();
    if operator {
        // Sticks: the engine hands back. A pilot action already ended tuning.
        let exited = s.fc.effects.contains(&Effect::ExitTuning { operator: true });
        assert_eq!(exited, reason == "operator", "{reason}");
        assert_eq!(lands, lands_before, "operator takeover: the pilot holds, no landing");
    } else {
        s.until(|s| !s.fc.armed, 30_000);
        let lands = s.sent_cmds(MavCmd::MAV_CMD_DO_SET_MODE).iter().filter(|p| p[1] == 9.0).count();
        assert!(lands > lands_before, "{reason}: abort lands; modes {:?}", s.sent_cmds(MavCmd::MAV_CMD_DO_SET_MODE));
        s.run(1500);
        assert!(s.fc.effects.contains(&Effect::ExitTuning { operator: false }), "{reason}");
    }
}

#[test]
fn each_abort_trigger() {
    abort_case(|s| s.fc.battery_pct = 25, "battery", false);
    abort_case(|s| s.fc.wind = Some((90.0, 9.0)), "wind", false);
    abort_case(|s| s.fc.n = 25.0, "drift", false);
    abort_case(|s| s.fc.inputs.link_lost = true, "link_loss", false);
    abort_case(|s| s.fc.inputs.axes = [0.0, 0.6, 0.0, 0.0], "operator", true);
    abort_case(|s| s.e.set_pilot_tuning(false), "operator_revoked", true);
}

#[test]
fn abort_blockers_name_the_trigger() {
    let mut s = air_ready();
    hover(&mut s);
    s.fc.battery_pct = 20;
    s.run(1500);
    let rec = &s.e.state.air.steps["first_hover"];
    assert_eq!(rec["state"], json!("failed"));
    assert_eq!(rec["blockers"], json!(["setup.blocker.aborted_battery"]), "the trigger is persisted");
    let v = s.e.view(s.now);
    let fh = v.wizards.iter().find(|w| w.id == A).unwrap().steps.iter().find(|x| x.step == "first_hover").unwrap();
    assert_eq!(fh.blockers, vec!["setup.blocker.aborted_battery".to_string()], "and listed after the run ended");
    assert_eq!(s.e.wizard_state(A), WizardState::NotStarted, "a single-step run leaves the wizard state alone");
}

#[test]
fn autotune_statustext_sequence_saves_gains() {
    let mut s = air_ready();
    assert_eq!(s.e.run_step(A, "autotune", s.now)["ok"], json!(true));
    s.run(100);
    s.act(A, "autotune", "start", json!({}));
    s.until(|s| s.e.current_view().is_none(), 180_000);
    assert_eq!(s.step_state(A, "autotune"), StepState::Done, "{}", s.result(A, "autotune"));
    let r = s.result(A, "autotune");
    assert_eq!(r["copter_axes_saved"], json!(["roll", "pitch", "yaw"]));
    let changed: Vec<String> = serde_json::from_value(r["changed"].clone()).unwrap();
    for n in ["ATC_RAT_RLL_P", "ATC_ANG_RLL_P", "ATC_RAT_PIT_P", "ATC_RAT_YAW_P"] {
        assert!(changed.contains(&n.to_string()), "{n} in {changed:?}");
    }
    assert!((s.param("AUTOTUNE_AGGR") - 0.075).abs() < 1e-6);
    // Roll + pitch first, then yaw and yaw D (4.6 firmware).
    assert_eq!(s.param("AUTOTUNE_AXES"), 12.0);
    let modes: Vec<f32> = s.sent_cmds(MavCmd::MAV_CMD_DO_SET_MODE).iter().map(|p| p[1]).collect();
    assert_eq!(modes.iter().filter(|m| **m == 15.0).count(), 2, "{modes:?}");
    // Centred sticks during the tune; throttle down to land in AUTOTUNE.
    assert!(s.fc.sent.iter().any(|m| matches!(m, MavMessage::MANUAL_CONTROL(c) if c.z == 500 && c.x == 0)));
    assert!(s.fc.sent.iter().any(|m| matches!(m, MavMessage::MANUAL_CONTROL(c) if c.z == 0)));
}

#[test]
fn autotune_yaw_waits_for_a_battery_swap() {
    let mut s = air_ready();
    s.e.run_step(A, "autotune", s.now);
    s.run(100);
    s.act(A, "autotune", "start", json!({}));
    // Battery sags below 50 % during roll/pitch.
    s.until(|s| s.fc.alt > 7.5, 20_000);
    s.fc.battery_pct = 45;
    s.until(|s| s.e.current_view().is_some_and(|v| v.actions.iter().any(|a| a.id == "resume_yaw")), 60_000);
    assert!(s.e.data().autotune_yaw_pending);
    // "Tune yaw now" on the pack that flew roll and pitch is refused (it9: the
    // yaw round flew into the FC's battery failsafe).
    assert_eq!(s.e.action(A, "autotune", "resume_yaw", &Map::new(), s.now)["err"], json!("battery_low"));
    s.fc.battery_pct = 98;
    s.run(1100);
    s.act(A, "autotune", "resume_yaw", json!({}));
    s.until(|s| s.e.current_view().is_none(), 120_000);
    assert_eq!(s.result(A, "autotune")["copter_axes_saved"], json!(["roll", "pitch", "yaw"]));
    assert!(!s.e.data().autotune_yaw_pending);
}

#[test]
fn plane_doublets_stay_in_the_box() {
    // Point-mass plane at 18 m/s in a 3 m/s westerly (it8: the skywalker
    // left the 400 m box downwind in 20 s), entering heading east, with a
    // sluggish bank (1 s lag, at most 35°) and a 1.5 m/s sink at neutral
    // pitch, as ArduPlane's FBWA-like AUTOTUNE flies at a fixed throttle.
    let box_m = 400.0;
    let mut d = Doublets::new(0, 45.0, 20.0, box_m);
    let (mut n, mut e, mut hdg, mut h) = (48.0f32, -47.0f32, 1.5f32, 80.0f32);
    let v = 18.0;
    let mut bank = 0.0f32;
    let (mut max_d, mut h_lo, mut h_hi) = (0.0f32, f32::MAX, f32::MIN);
    let mut t = 0;
    let mut saw = (false, false);
    while t < 240_000 {
        let dm = d.demand(t, [n, e], hdg, 80.0 - h);
        saw.0 |= dm.roll.abs() > 0.9;
        saw.1 |= dm.pitch.abs() > 0.5;
        let target = (dm.roll * 45f32).clamp(-35.0, 35.0).to_radians();
        bank += (target - bank) * 0.02;
        hdg += 9.81 * bank.tan() / v * 0.02;
        n += v * hdg.cos() * 0.02;
        e += (v * hdg.sin() + 3.0) * 0.02;
        h += (v * (dm.pitch * 20f32).to_radians().sin() - 1.5) * 0.02;
        max_d = max_d.max((n * n + e * e).sqrt());
        h_lo = h_lo.min(h);
        h_hi = h_hi.max(h);
        t += 20;
    }
    assert!(max_d < 0.8 * box_m, "left the inner tuning box: {max_d}");
    assert!(h_lo > 50.0 && h_hi < 110.0, "height {h_lo}..{h_hi} m around the 80 m loiter");
    assert!(saw.0 && saw.1, "both doublet axes flown");
}

#[test]
fn setup_needed_from_prearm_text() {
    let mut s = Sim::copter();
    assert_eq!(s.e.setup_needed(s.now).as_deref(), Some("Aircraft setup not completed"));
    s.e.state.ground.state = WizardState::Done;
    assert_eq!(s.e.setup_needed(s.now), None);
    let m = MavMessage::STATUSTEXT(ados_protocol::mavlink::ardupilotmega::STATUSTEXT_DATA {
        text: "PreArm: Compass not calibrated".into(),
        ..Default::default()
    });
    s.e.on_fc(1, 1, &m, s.now);
    assert_eq!(s.e.setup_needed(s.now).as_deref(), Some("PreArm: Compass not calibrated"));
    assert_eq!(s.e.setup_needed(s.now + 11_000), None);
}

#[test]
fn stream_frames_and_list_shape() {
    let mut s = Sim::copter();
    s.e.start(G, s.now);
    let frames = s.e.frames(s.now);
    let v: Vec<Value> = frames.iter().map(|f| serde_json::to_value(f).unwrap()).collect();
    assert!(v.iter().any(|f| f["kind"] == json!("wizard") && f["data"]["id"] == json!("ground")));
    assert!(v.iter().any(|f| f["kind"] == json!("step") && f["data"]["step"] == json!("detect")));
    let list = s.e.request("setup.list", &json!({}), s.now);
    for k in ["v", "uid", "firmware", "firmware_version", "vehicle_class", "running", "wizards", "report"] {
        assert!(list.get(k).is_some(), "{k}");
    }
    assert_eq!(list["uid"], json!("00c0ffee12345678"));
    assert_eq!(list["running"]["step"], json!("detect"));
    let step = &list["wizards"][0]["steps"][0];
    for k in ["wizard", "step", "title_key", "body_key", "args", "image", "state", "progress", "live", "actions", "blockers", "result"] {
        assert!(step.get(k).is_some(), "{k}");
    }
    assert_eq!(step["title_key"], json!("setup.detect.title"));
    // A second wizard cannot start while one runs.
    assert_eq!(s.e.request("setup.start", &json!({"wizard": "ground"}), s.now)["err"], json!("wizard_running"));
    assert_eq!(s.e.request("setup.abort", &json!({"wizard": "ground"}), s.now)["ok"], json!(true));
    assert_eq!(s.e.wizard_state(G), WizardState::Aborted);
}

#[test]
fn question_and_body_args_fill_every_placeholder() {
    // Motors: each question names the motor number on the layout image.
    let mut s = Sim::copter();
    s.e.run_step(G, "outputs", s.now);
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(s.e.current_view().unwrap().args["n"].clone());
        s.act(G, "outputs", "test", json!({}));
        s.until(|s| s.e.current_view().unwrap().live.get("question_key") == Some(&json!("setup.question.motor_position")), 5_000);
        s.check_placeholders();
        s.act(G, "outputs", "answer_yes", json!({}));
        s.check_placeholders();
        s.act(G, "outputs", "answer_yes", json!({}));
    }
    // Quad X in test order A..D is motors 1, 4, 2, 3.
    assert_eq!(seen, vec![json!(1), json!(4), json!(2), json!(3)]);
    // Plane level: "nose up about {deg}°".
    let mut p = Sim::new(MavType::MAV_TYPE_FIXED_WING, ap_plane_params());
    p.e.run_step(G, "level", p.now);
    p.check_placeholders();
    assert_eq!(p.e.current_view().unwrap().args["deg"], json!(3.0));
    // Preflight with a low battery: "nothing within {radius} m", "below {pct} %".
    let mut a = Sim::copter();
    a.e.state.ground.state = WizardState::Done;
    a.fc.battery_pct = 60;
    a.run(1500);
    assert_eq!(a.e.start(A, a.now)["ok"], json!(true));
    a.run(200);
    let v = a.e.current_view().unwrap();
    assert!(v.blockers.contains(&"setup.blocker.battery_low".to_string()), "{:?}", v.blockers);
    a.check_placeholders();
    assert_eq!(v.args["radius"], json!(30));
    assert_eq!(v.args["pct"], json!(80));
}

/// Boot a fake FC against an engine on `dir` until the engine files its state
/// under an identity; returns the `GET /api/v1/fc-setup` view.
fn boot(dir: &std::path::Path, mav_type: MavType, uid: u64, uid2: Option<[u8; 18]>) -> super::schema::SetupView {
    let params = if mav_type == MavType::MAV_TYPE_FIXED_WING { ap_plane_params() } else { ap_copter_params() };
    let mut e = Engine::new(dir, 0);
    let mut fc = FakeFc::new(mav_type, params);
    fc.uid = uid;
    fc.uid2 = uid2;
    let mut t = 0;
    while e.uid.is_none() {
        assert!(t < 10_000, "no identity adopted");
        t += TICK;
        fc.step(&mut e, t);
    }
    e.view(t)
}

#[test]
fn identity_from_uid_then_uid2_then_derived() {
    use super::fcstate::IdSource;
    let dir = tempfile::tempdir().unwrap();
    // A board with a 64-bit uid.
    let v = boot(dir.path(), MavType::MAV_TYPE_QUADROTOR, 0x00C0_FFEE_1234_5678, None);
    assert_eq!((v.uid.as_deref(), v.uid_source), (Some("00c0ffee12345678"), Some(IdSource::Uid)));
    // uid 0 with a uid2 (the MAVLink 2 extension, read from the raw frame).
    let mut u2 = [0u8; 18];
    u2[..12].copy_from_slice(&[0x30, 0x00, 0x28, 0x00, 0x0b, 0x51, 0x33, 0x30, 0x38, 0x38, 0x36, 0x37]);
    let v = boot(dir.path(), MavType::MAV_TYPE_QUADROTOR, 0, Some(u2));
    assert_eq!(v.uid_source, Some(IdSource::Uid2));
    assert_eq!(v.uid.as_deref(), Some("300028000b51333038383637000000000000"));
    // uid2 supersedes a non-zero uid.
    let v = boot(dir.path(), MavType::MAV_TYPE_QUADROTOR, 7, Some(u2));
    assert_eq!(v.uid_source, Some(IdSource::Uid2));
    // Neither (SITL): a derived id, stable across a reboot, different per vehicle type.
    let a = boot(dir.path(), MavType::MAV_TYPE_QUADROTOR, 0, None);
    assert_eq!(a.uid_source, Some(IdSource::Derived));
    let id = a.uid.clone().unwrap();
    assert!(id.starts_with('d') && id.len() == 17, "{id}");
    let again = boot(dir.path(), MavType::MAV_TYPE_QUADROTOR, 0, Some([0; 18]));
    assert_eq!(again.uid.as_deref(), Some(id.as_str()), "an all-zero uid2 counts as none");
    let plane = boot(dir.path(), MavType::MAV_TYPE_FIXED_WING, 0, None);
    assert_ne!(plane.uid, a.uid);
    // The wizard starts on a derived id, and its state persists under it.
    let mut e = Engine::new(dir.path(), 0);
    let mut fc = FakeFc::new(MavType::MAV_TYPE_QUADROTOR, ap_copter_params());
    fc.uid = 0;
    let mut t = 0;
    while e.uid.is_none() || !e.params.complete() {
        t += TICK;
        fc.step(&mut e, t);
    }
    assert_eq!(e.start(G, t)["ok"], json!(true));
    let state = std::fs::read_to_string(dir.path().join(&id).join("state.json")).unwrap();
    assert!(state.contains("\"uid_source\": \"derived\""), "{state}");
    assert!(state.contains("\"state\": \"running\""), "{state}");
}

#[test]
fn uid2_parses_from_trimmed_and_full_frames() {
    use super::fcstate::uid2_from_frame;
    let mut payload = vec![0u8; 78];
    payload[60] = 0xAB;
    payload[77] = 0xCD;
    let mut f = vec![0xFD, 78, 0, 0, 0, 1, 1, 148, 0, 0];
    f.extend_from_slice(&payload);
    f.extend_from_slice(&[0, 0]);
    let u = uid2_from_frame(&f).unwrap();
    assert_eq!((u[0], u[17]), (0xAB, 0xCD));
    // Trimmed to the fixed fields (no extension sent): all zero.
    let mut g = vec![0xFD, 60, 0, 0, 0, 1, 1, 148, 0, 0];
    g.extend_from_slice(&[1u8; 60]);
    assert_eq!(uid2_from_frame(&g), Some([0u8; 18]));
    // MAVLink 1 or another message: nothing.
    assert_eq!(uid2_from_frame(&[0xFE, 60, 0, 1, 1, 148]), None);
    let mut h = f.clone();
    h[7] = 0;
    assert_eq!(uid2_from_frame(&h), None);
}

fn plane_with_airspeed() -> Sim {
    let mut p = ap_plane_params();
    p.insert("ARSPD_TYPE".into(), 100.0);
    let mut s = Sim::new(MavType::MAV_TYPE_FIXED_WING, p);
    s.run(1100);
    s
}

fn action_ids(s: &Sim) -> Vec<String> {
    s.e.current_view().map(|v| v.actions.iter().map(|a| a.id.clone()).collect()).unwrap_or_default()
}

#[test]
fn airspeed_cal_runs_with_the_ground_pressure_command() {
    // ArduPilot zeroes the airspeed sensor in PREFLIGHT_CALIBRATION p3 = 1
    // (IN_PROGRESS, then ACCEPTED); p6 = 2 is UNSUPPORTED there.
    let mut s = plane_with_airspeed();
    assert_eq!(s.e.run_step(G, "airspeed", s.now)["ok"], json!(true));
    s.wait_step("airspeed", StepState::WaitingUser);
    s.act(G, "airspeed", "start", json!({}));
    s.until(|s| s.e.current_view().is_none(), 10_000);
    assert_eq!(s.step_state(G, "airspeed"), StepState::Done);
    let sent = s.sent_cmds(MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION);
    assert_eq!(sent.last().map(|p| (p[2], p[5])), Some((1.0, 0.0)));
}

#[test]
fn airspeed_skipped_when_the_fc_reports_no_sensor() {
    let mut s = plane_with_airspeed();
    s.fc.script.sensors_present = Some(ados_protocol::mavlink::ardupilotmega::MavSysStatusSensor::empty());
    s.run(1100);
    s.e.run_step(G, "airspeed", s.now);
    s.until(|s| s.e.current_view().is_none(), 1_000);
    assert_eq!(s.step_state(G, "airspeed"), StepState::Skipped);
    assert_eq!(s.result(G, "airspeed")["reason"], json!("no_airspeed_sensor"));
}

#[test]
fn failed_optional_step_offers_retry_and_skip() {
    let mut s = plane_with_airspeed();
    s.fc.script.airspeed_cal_ok = false;
    s.e.run_step(G, "airspeed", s.now);
    s.wait_step("airspeed", StepState::WaitingUser);
    s.act(G, "airspeed", "start", json!({}));
    s.wait_step("airspeed", StepState::Failed);
    assert_eq!(s.e.current_view().unwrap().blockers, vec!["setup.blocker.calibration_failed".to_string()]);
    assert_eq!(action_ids(&s), ["retry", "skip"]);
    let skip = s.e.current_view().unwrap().actions.into_iter().find(|a| a.id == "skip").unwrap();
    assert_eq!((skip.style, skip.confirm), (super::schema::ActionStyle::Secondary, super::schema::Confirm::None));
    // Retry: back to the step's first screen, which still offers skip.
    s.act(G, "airspeed", "retry", json!({}));
    assert_eq!(s.state(), Some(StepState::WaitingUser));
    assert_eq!(action_ids(&s), ["start", "skip"]);
    s.act(G, "airspeed", "start", json!({}));
    s.wait_step("airspeed", StepState::Failed);
    s.act(G, "airspeed", "skip", json!({}));
    s.until(|s| s.e.current_view().is_none(), 1_000);
    assert_eq!(s.step_state(G, "airspeed"), StepState::Skipped);
    // A required step offers retry only, and retry is refused before a failure.
    let mut c = Sim::copter();
    c.e.run_step(G, "frame", c.now);
    assert_eq!(c.e.action(G, "frame", "retry", &Map::new(), c.now)["err"], json!("not_failed"));
}

fn ritz_params() -> std::collections::BTreeMap<String, f32> {
    let mut p = ap_plane_params();
    for (k, v) in [
        ("Q_ENABLE", 1.0),
        ("Q_TAILSIT_ENABLE", 1.0),
        ("Q_FRAME_CLASS", 10.0),
        ("Q_FRAME_TYPE", 1.0),
        ("SERVO1_FUNCTION", 77.0),
        ("SERVO2_FUNCTION", 78.0),
        ("SERVO3_FUNCTION", 73.0),
        ("SERVO4_FUNCTION", 74.0),
        ("SERVO5_FUNCTION", 75.0),
        ("SERVO6_FUNCTION", 76.0),
        ("SERVO2_REVERSED", 0.0),
    ] {
        p.insert(k.to_string(), v);
    }
    p
}

#[test]
fn tailsitter_frame_keeps_its_class_and_outputs() {
    let mut s = Sim::new(MavType::MAV_TYPE_VTOL_TAILSITTER_DUOROTOR, ritz_params());
    s.e.run_step(G, "frame", s.now);
    let inputs = s.e.current_view().unwrap().args["inputs"].clone();
    assert_eq!(inputs["frame_class"]["value"], json!(10.0));
    assert!(inputs["frame_class"]["options"].as_array().unwrap().contains(&json!(10)));
    assert!(inputs.get("wing").is_none(), "a tailsitter's outputs belong to its mixer");
    s.act(G, "frame", "apply", json!({"frame_class": 10, "frame_type": 1, "prop_in": 9, "motor_kv": 1000, "cells": 4}));
    s.until(|s| s.e.current_view().is_none(), 10_000);
    assert_eq!(s.step_state(G, "frame"), StepState::Done);
    assert_eq!(s.fc.reboots, 0, "nothing changed, nothing to reboot for");
    assert_eq!((s.param("Q_FRAME_CLASS"), s.param("SERVO3_FUNCTION")), (10.0, 73.0));

    // Outputs: both elevons, then the right and left throttles (test order 1 and 3), position only.
    s.e.run_step(G, "outputs", s.now);
    let mut questions = 0;
    while s.e.current_view().is_some() {
        s.until(|s| s.state() == Some(StepState::WaitingUser) || s.e.current_view().is_none(), 10_000);
        let ids = action_ids(&s);
        if ids.iter().any(|a| a == "test") {
            s.act(G, "outputs", "test", json!({}));
        } else if ids.iter().any(|a| a == "answer_yes") {
            questions += 1;
            s.act(G, "outputs", "answer_yes", json!({}));
        } else if s.e.current_view().is_some() {
            panic!("outputs waits with {ids:?}");
        }
    }
    assert_eq!(s.step_state(G, "outputs"), StepState::Done);
    assert_eq!(questions, 4, "two surfaces, two motors without a direction question");
    let seqs: Vec<f32> = s.sent_cmds(MavCmd::MAV_CMD_DO_MOTOR_TEST).iter().map(|p| p[0]).collect();
    assert_eq!(seqs, [1.0, 3.0]);
}

#[test]
fn plane_frame_defaults_to_the_wing_its_outputs_describe() {
    let mut p = ap_plane_params();
    p.insert("SERVO1_FUNCTION".into(), 78.0);
    p.insert("SERVO2_FUNCTION".into(), 77.0);
    let mut s = Sim::new(MavType::MAV_TYPE_FIXED_WING, p);
    s.e.run_step(G, "frame", s.now);
    assert_eq!(s.e.current_view().unwrap().args["inputs"]["wing"]["value"], json!("elevon"));
    s.act(G, "frame", "apply", json!({"wing": "elevon", "cells": 4}));
    s.until(|s| s.e.current_view().is_none(), 10_000);
    // Confirming the wing the outputs already describe keeps their assignment.
    assert_eq!((s.param("SERVO1_FUNCTION"), s.param("SERVO2_FUNCTION")), (78.0, 77.0));
}

#[test]
fn frame_reboot_that_never_returns_fails_with_timeout() {
    let mut s = Sim::copter();
    s.fc.script.reboot_returns = false;
    s.e.run_step(G, "frame", s.now);
    s.act(G, "frame", "apply", json!({"frame_class": 2, "frame_type": 1, "prop_in": 10, "motor_kv": 900, "cells": 4}));
    s.until(|s| s.state() == Some(StepState::Failed), 130_000);
    let v = s.e.current_view().unwrap();
    assert_eq!(v.blockers, vec!["setup.blocker.timeout".to_string()]);
    assert_eq!(action_ids(&s), ["retry"], "frame is required: no skip");
    // The FC is gone: retry returns to the first screen, but nothing that
    // drives the FC "completes" against the cached parameters.
    s.act(G, "frame", "retry", json!({}));
    let args: Map<String, Value> =
        json!({"frame_class": 2, "frame_type": 1, "prop_in": 10, "motor_kv": 900, "cells": 4}).as_object().cloned().unwrap();
    assert_eq!(s.e.action(G, "frame", "apply", &args, s.now)["err"], json!("no_fc"));
}

/// Answer the waiting step the way the bench driver does, retrying while
/// the engine is still taking `tuning`.
fn answer(s: &mut Sim, w: WizardId) {
    let Some(v) = s.e.current_view() else { return };
    if v.state != StepState::WaitingUser {
        return;
    }
    let ids: Vec<String> = v.actions.iter().map(|a| a.id.clone()).collect();
    let Some(a) = ["confirm_area", "apply", "start"].iter().find(|a| ids.iter().any(|i| i == *a)) else {
        panic!("{} waits with {ids:?}", v.step);
    };
    let r = s.e.action(w, &v.step, a, &Map::new(), s.now);
    assert!(r["ok"] == json!(true) || r["err"] == json!("tuning_pending"), "{}/{a}: {r}", v.step);
}

/// Drive the air wizard until `cond`, answering every waiting step.
fn drive_air(s: &mut Sim, cond: impl Fn(&Sim) -> bool, max_ms: u64) {
    let end = s.now + max_ms;
    while !cond(s) {
        assert!(
            s.now < end && s.e.wizard_state(A) == WizardState::Running,
            "air wizard {:?} at {:?}; air steps {:?}",
            s.e.wizard_state(A),
            s.e.current_view().map(|v| (v.step, v.state, v.blockers)),
            s.e.state.air.steps
        );
        answer(s, A);
        s.run(100);
    }
}

#[test]
fn air_wizard_flies_first_hover_then_autotune() {
    // first_hover lands and disarms; autotune arms again under the same
    // `tuning` owner and runs (no operator-takeover abort).
    let mut s = air_ready();
    assert_eq!(s.e.start(A, s.now)["ok"], json!(true));
    drive_air(&mut s, |s| s.step_id().as_deref() == Some("autotune"), 150_000);
    assert_eq!(s.step_state(A, "first_hover"), StepState::Done);
    drive_air(&mut s, |s| s.fc.armed && s.fc.alt > 5.0, 30_000);
    // ArduCopter only enters AUTOTUNE from a hold mode (LOITER here), not GUIDED.
    drive_air(&mut s, |s| s.fc.custom_mode == 15, 20_000);
    let modes: Vec<f32> = s.sent_cmds(MavCmd::MAV_CMD_DO_SET_MODE).iter().map(|p| p[1]).collect();
    let at = modes.iter().position(|m| *m == 15.0).unwrap();
    assert_eq!(modes[..at].last(), Some(&5.0), "LOITER before AUTOTUNE: {modes:?}");
    s.run(3000);
    assert_eq!(s.e.wizard_state(A), WizardState::Running, "{:?}", s.e.state.air.steps.get("autotune"));
    assert_eq!(s.step_id().as_deref(), Some("autotune"));
}

#[test]
fn refused_autotune_mode_fails_fast_and_lands() {
    let mut s = air_ready();
    s.fc.script.refuse_autotune = true;
    assert_eq!(s.e.run_step(A, "autotune", s.now)["ok"], json!(true));
    s.run(100);
    s.act(A, "autotune", "start", json!({}));
    s.until(|s| s.state() == Some(StepState::Failed), 60_000);
    assert_eq!(s.e.current_view().unwrap().blockers, vec!["setup.blocker.tuning_refused".to_string()]);
    s.until(|s| !s.fc.armed, 30_000);
}

#[test]
fn autotune_stops_when_the_fc_leaves_the_tune_mode() {
    // A failsafe takes the FC out of AUTOTUNE: the step fails and never sends AUTOTUNE again.
    let mut s = air_ready();
    s.fc.script.autotune_s = 600;
    assert_eq!(s.e.run_step(A, "autotune", s.now)["ok"], json!(true));
    s.run(100);
    s.act(A, "autotune", "start", json!({}));
    s.until(|s| s.fc.custom_mode == 15, 30_000);
    s.run(2000);
    s.fc.custom_mode = 6; // RTL
    s.until(|s| s.state() == Some(StepState::Failed), 5_000);
    assert_eq!(s.e.current_view().unwrap().blockers, vec!["setup.blocker.autotune_failed".to_string()]);
    let sent = s.sent_cmds(MavCmd::MAV_CMD_DO_SET_MODE).len();
    s.run(10_000);
    assert_eq!(s.fc.custom_mode, 6, "the FC keeps its own mode");
    assert_eq!(s.sent_cmds(MavCmd::MAV_CMD_DO_SET_MODE).len(), sent, "no mode forced over the FC");
}

#[test]
fn plane_tecs_stays_above_its_floor_and_autotune_reaches_the_loiter() {
    let mut p = ap_plane_params();
    for (k, v) in [
        ("THR_MAX", 100.0),
        ("TKOFF_ALT", 50.0),
        ("TRIM_THROTTLE", 45.0),
        ("AIRSPEED_MIN", 12.0),
        ("AIRSPEED_MAX", 25.0),
        ("AIRSPEED_CRUISE", 18.0),
        ("TECS_CLMB_MAX", 5.0),
        ("TECS_SINK_MAX", 5.0),
        ("TECS_SINK_MIN", 2.0),
        ("SERVO1_TRIM", 1500.0),
        ("SERVO2_TRIM", 1500.0),
        ("SERVO4_TRIM", 1500.0),
    ] {
        p.insert(k.into(), v);
    }
    let mut s = Sim::new(MavType::MAV_TYPE_FIXED_WING, p);
    s.e.state.ground.state = WizardState::Done;
    assert_eq!(s.e.start(A, s.now)["ok"], json!(true));
    drive_air(&mut s, |s| s.step_id().as_deref() == Some("tecs_measure") && s.state() == Some(StepState::Running), 600_000);
    // The sink stages end at the 50 m floor; the idle dive is not carried on below it.
    let mut min_alt = f32::MAX;
    while s.step_id().as_deref() == Some("tecs_measure") {
        assert!(s.e.wizard_state(A) == WizardState::Running, "{:?}", s.e.state.air.steps);
        answer(&mut s, A);
        s.run(100);
        min_alt = min_alt.min(s.fc.alt);
    }
    assert!(min_alt > 40.0, "tecs_measure sank to {min_alt} m");
    // The TECS stages left an offboard GUIDED height; the autotune loiter overrides it.
    drive_air(&mut s, |s| s.fc.custom_mode == 8, 90_000);
    assert!(s.fc.alt > 70.0, "autotune at {} m", s.fc.alt);
}

#[test]
fn vtol_first_hover_climbs_to_the_transition_height() {
    // ArduPlane ignores local position setpoints (the fake does too): after the 45 s
    // hover the tailsitter must climb through its GUIDED target height (it10 held 3 m
    // for 650 s waiting for a climb that local setpoints never started).
    let mut p = ritz_params();
    p.insert("Q_GUIDED_MODE".into(), 1.0);
    let mut s = Sim::new(MavType::MAV_TYPE_VTOL_TAILSITTER_DUOROTOR, p);
    s.e.state.ground.state = WizardState::Done;
    // A box kept from an earlier session, far away: this run starts its own.
    s.e.state.data.box_center = Some([500.0, 500.0]);
    s.e.state.data.box_latlon = Some((474_100_000, 85_100_000));
    assert_eq!(s.e.run_step(A, "first_hover", s.now)["ok"], json!(true));
    s.run(100);
    s.act(A, "first_hover", "start", json!({}));
    s.until(|s| s.fc.alt > 2.9, 20_000);
    assert_eq!(s.e.state.data.box_center, Some([0.0, 0.0]), "the box is this take-off's");
    // The box centre was fixed before the GPS settled, 9 m off (it11: "VTOL Position1 d=8.0"):
    // the climb holds the current position rather than flying the tailsitter there.
    s.e.state.data.box_latlon = Some((474_000_800, 85_000_000));
    s.until(|s| s.fc.alt > 28.0, 75_000);
    let climb_at = s.fc.sent.iter().rev().find_map(|m| match m {
        MavMessage::COMMAND_INT(c) if c.command == MavCmd::MAV_CMD_DO_REPOSITION => Some(c.x),
        _ => None,
    });
    assert_eq!(climb_at, Some(474_000_000), "climb in place");
    // Forward flight: the tailsitter circles the plane's box, not the hover's 20 m.
    s.fc.vtol_state = ados_protocol::mavlink::ardupilotmega::MavVtolState::MAV_VTOL_STATE_FW;
    s.fc.n = 150.0;
    s.run(3000);
    assert_eq!(s.step_id().as_deref(), Some("first_hover"), "{:?}", s.e.state.air.steps);
    assert_eq!(s.state(), Some(StepState::Running), "no drift abort in forward flight");
}
