//! Air-wizard safety against the fake FC: an FC failsafe mode ends the wizard
//! with no further command and every value it changed put back, the FC's
//! battery warning lands it, the altitude floors and ground contact abort a
//! step, an aborted tune discards only the round it cut short, and an
//! aircraft that is neither flying nor landed gets no take-off.

use std::collections::BTreeMap;

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, MavType};
use serde_json::{json, Value};

use super::{air_ready, ap_plane_params, Sim, A};
use crate::setup::fake_fc::PlaneTune;
use crate::setup::out::Effect;
use crate::setup::schema::{StepState, WizardState};

fn plane_params() -> BTreeMap<String, f32> {
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
        ("AUTOTUNE_LEVEL", 6.0),
        // The ADOS profile's low-battery action (RTL).
        ("BATT_FS_LOW_ACT", 1.0),
    ] {
        p.insert(k.into(), v);
    }
    p
}

fn plane() -> Sim {
    let mut s = Sim::new(MavType::MAV_TYPE_FIXED_WING, plane_params());
    s.e.state.ground.state = WizardState::Done;
    s
}

/// A lone air step from the ground: run it and press start.
fn start_step(s: &mut Sim, step: &str) {
    assert_eq!(s.e.run_step(A, step, s.now)["ok"], json!(true), "{step}");
    s.run(200);
    s.act(A, step, "start", json!({}));
}

/// Values of the PARAM_SETs for `name` from frame `from` on.
fn param_sets(s: &Sim, from: usize, name: &str) -> Vec<f32> {
    s.fc.sent[from..]
        .iter()
        .filter_map(|m| match m {
            MavMessage::PARAM_SET(p)
                if p.param_id.to_str().unwrap_or("").trim_end_matches('\0') == name =>
            {
                Some(p.param_value)
            }
            _ => None,
        })
        .collect()
}

/// Mode commands (DO_SET_MODE custom modes) from frame `from` on.
fn modes_from(s: &Sim, from: usize) -> Vec<f32> {
    s.fc.sent[from..]
        .iter()
        .filter_map(|m| match m {
            MavMessage::COMMAND_LONG(c) if c.command == MavCmd::MAV_CMD_DO_SET_MODE => {
                Some(c.param2)
            }
            _ => None,
        })
        .collect()
}

/// Navigation commands (GUIDED repositions, height changes, take-offs) from frame `from` on.
fn nav_from(s: &Sim, from: usize) -> Vec<MavCmd> {
    s.fc.sent[from..]
        .iter()
        .filter_map(|m| match m {
            MavMessage::COMMAND_INT(c) => Some(c.command),
            MavMessage::COMMAND_LONG(c) if c.command == MavCmd::MAV_CMD_NAV_TAKEOFF => {
                Some(c.command)
            }
            _ => None,
        })
        .collect()
}

fn record(s: &Sim, step: &str) -> Value {
    s.e.state.air.steps[step].clone()
}

#[test]
fn an_fc_failsafe_ends_the_wizard_with_no_command_over_it() {
    // The FC enters a failsafe mode with no battery text: RTL during the TECS
    // climb and the idle glide (THR_MAX 0 on the FC), the short failsafe's
    // CIRCLE and the long failsafe's FBWA glide. The wizard ended at once,
    // sent no mode or navigation command and no THR_MAX 0, and put back every
    // value it changed, the measured climb limit too.
    for (glide, mode) in [(false, 11u32), (true, 11), (true, 1), (false, 5)] {
        let at = format!("glide {glide}, mode {mode}");
        let mut s = plane();
        start_step(&mut s, "tecs_measure");
        if glide {
            s.until(|s| s.fc.params["THR_MAX"] == 0.0, 240_000);
        } else {
            s.until(|s| s.fc.params["TECS_CLMB_MAX"] == 20.0, 240_000);
        }
        let mark = s.fc.sent.len();
        s.fc.custom_mode = mode;
        s.run(30_000);
        assert!(
            !param_sets(&s, mark, "THR_MAX").contains(&0.0),
            "{at}: THR_MAX 0 written over the failsafe"
        );
        assert_eq!(modes_from(&s, mark), Vec::<f32>::new(), "{at}");
        assert_eq!(nav_from(&s, mark), Vec::<MavCmd>::new(), "{at}");
        assert_eq!(
            record(&s, "tecs_measure")["blockers"],
            json!(["setup.blocker.aborted_fc_failsafe"]),
            "{at}"
        );
        assert!(s.e.current_view().is_none(), "{at}: wizard ended");
        assert!(s
            .fc
            .effects
            .contains(&Effect::ExitTuning { operator: false }));
        assert_eq!(s.fc.params["THR_MAX"], 100.0, "{at}");
        assert_eq!(s.fc.params["TECS_SINK_MAX"], 5.0, "{at}");
        assert_eq!(s.fc.params["TECS_CLMB_MAX"], 5.0, "{at}");
        assert_eq!(s.fc.custom_mode, mode, "{at}: the FC keeps its mode");
    }
}

#[test]
fn a_battery_warning_alone_lands_the_wizard_and_puts_its_limits_back() {
    // "Battery 1 is low" with only the critical action set: the FC does
    // nothing, so this is no FC failsafe. The wizard aborts with its own
    // reason, puts THR_MAX back and flies its own return.
    let mut s = plane();
    s.fc.set_param_notify("BATT_FS_LOW_ACT", 0.0);
    s.fc.set_param_notify("BATT_FS_CRT_ACT", 1.0);
    start_step(&mut s, "tecs_measure");
    s.until(|s| s.fc.params["THR_MAX"] == 0.0, 240_000);
    let mark = s.fc.sent.len();
    s.fc.push_text("Battery 1 is low 6.77V used 304 mAh");
    s.run(5000);
    assert_eq!(
        record(&s, "tecs_measure")["blockers"],
        json!(["setup.blocker.aborted_battery_warning"])
    );
    assert!(!param_sets(&s, mark, "THR_MAX").contains(&0.0));
    assert_eq!(s.fc.params["THR_MAX"], 100.0);
    assert_eq!(s.fc.params["TECS_SINK_MAX"], 5.0);
    let modes = modes_from(&s, mark);
    assert!(modes.contains(&11.0), "the wizard's own return: {modes:?}");
}

#[test]
fn tecs_limits_go_back_when_the_fc_leaves_guided() {
    // The pilot's switch to LOITER during the idle glide: THR_MAX 0 is only
    // for the GUIDED flight the stage commanded.
    let mut s = plane();
    start_step(&mut s, "tecs_measure");
    s.until(|s| s.fc.params["THR_MAX"] == 0.0, 240_000);
    let mark = s.fc.sent.len();
    s.fc.custom_mode = 12;
    s.run(5000);
    assert_eq!(
        record(&s, "tecs_measure")["blockers"],
        json!(["setup.blocker.mode_changed"])
    );
    assert_eq!(s.fc.params["THR_MAX"], 100.0);
    assert_eq!(s.fc.params["TECS_SINK_MAX"], 5.0);
    assert_eq!(
        s.fc.params["TECS_CLMB_MAX"], 5.0,
        "measured climb limit put back"
    );
    assert_eq!(modes_from(&s, mark), Vec::<f32>::new());
    assert_eq!(s.fc.custom_mode, 12);
}

#[test]
fn a_measured_limit_still_pending_at_a_failsafe_goes_back() {
    // The glide's TECS_SINK_MIN lands on the FC but its reply is lost, so the
    // write is still pending when the FC's RTL arrives in the dive.
    let mut s = plane();
    s.fc.script.unacked_params = vec!["TECS_SINK_MIN".into()];
    start_step(&mut s, "tecs_measure");
    s.until(|s| s.fc.params["TECS_SINK_MIN"] != 2.0, 400_000);
    s.run(500);
    s.fc.custom_mode = 11;
    s.run(10_000);
    assert_eq!(
        record(&s, "tecs_measure")["blockers"],
        json!(["setup.blocker.aborted_fc_failsafe"])
    );
    assert_eq!(s.fc.params["TECS_SINK_MIN"], 2.0, "measured sink put back");
    assert_eq!(s.fc.params["TECS_SINK_MAX"], 5.0);
    assert_eq!(s.fc.params["THR_MAX"], 100.0);
}

#[test]
fn a_cruise_phase_below_its_floor_aborts_and_returns() {
    let mut s = plane();
    start_step(&mut s, "tecs_measure");
    s.until(|s| s.fc.params["TECS_CLMB_MAX"] == 20.0, 240_000);
    let mark = s.fc.sent.len();
    s.fc.alt = 25.0;
    s.run(3000);
    assert_eq!(
        record(&s, "tecs_measure")["blockers"],
        json!(["setup.blocker.aborted_altitude_floor"])
    );
    let modes = modes_from(&s, mark);
    assert!(modes.contains(&11.0), "RTL below the floor: {modes:?}");
}

#[test]
fn a_hover_tune_below_its_floor_aborts_and_lands() {
    let mut s = air_ready();
    start_step(&mut s, "autotune");
    s.until(|s| s.fc.custom_mode == 15, 60_000);
    s.run(2000);
    let mark = s.fc.sent.len();
    s.fc.alt = 1.5;
    s.run(3000);
    assert_eq!(
        record(&s, "autotune")["blockers"],
        json!(["setup.blocker.aborted_altitude_floor"])
    );
    let modes = modes_from(&s, mark);
    assert!(modes.contains(&9.0), "LAND below the floor: {modes:?}");
}

/// Fly a lone plane AUTOTUNE until the FC is in AUTOTUNE.
fn plane_tuning(tune: PlaneTune) -> Sim {
    let mut s = plane();
    s.fc.script.plane_tune = Some(tune);
    start_step(&mut s, "autotune");
    s.until(|s| s.fc.custom_mode == 8, 180_000);
    s
}

/// Runs until the autotune step leaves `Running`; returns the lowest height.
fn fly_tune(s: &mut Sim) -> f32 {
    let mut min_alt = f32::MAX;
    let end = s.now + 300_000;
    while s
        .e
        .current_view()
        .is_some_and(|v| v.state == StepState::Running)
        && s.now < end
    {
        s.run(100);
        if s.fc.custom_mode == 8 {
            min_alt = min_alt.min(s.fc.alt);
        }
    }
    min_alt
}

#[test]
fn plane_autotune_aborts_at_its_floor_and_discards_the_gains() {
    // A deficit nothing in the tune can recover: the abort at the floor, a
    // return, and the gains from before.
    let mut s = plane_tuning(PlaneTune {
        sink: 3.0,
        bank_loss: 0.0,
        pitch_gain: 1.0,
        thr_gain: 0.0,
    });
    s.fc.set_param_notify("RLL_RATE_P", 0.386);
    let mark = s.fc.sent.len();
    let min_alt = fly_tune(&mut s);
    assert_eq!(
        record(&s, "autotune")["blockers"],
        json!(["setup.blocker.aborted_altitude_floor"])
    );
    assert!(min_alt > 38.0, "sank to {min_alt} m before the abort");
    s.run(3000);
    let modes = modes_from(&s, mark);
    assert!(modes.contains(&11.0), "RTL after the floor: {modes:?}");
    assert_eq!(s.fc.params["RLL_RATE_P"], 0.08, "tuned roll gain discarded");
    assert_eq!(
        record(&s, "autotune")["result"]["gains_discarded"],
        json!(["RLL_RATE_P"])
    );
}

#[test]
fn ground_contact_fails_the_tune_and_discards_its_gains() {
    let mut s = plane_tuning(PlaneTune {
        sink: 0.0,
        bank_loss: 0.0,
        pitch_gain: 0.0,
        thr_gain: 0.0,
    });
    s.run(2000);
    s.fc.set_param_notify("RLL_RATE_P", 0.386);
    s.run(1000);
    // Skimming the ground: under a metre, the FC not yet reporting it landed.
    s.fc.alt = 0.5;
    s.run(3000);
    let rec = record(&s, "autotune");
    assert_eq!(rec["state"], json!("failed"), "{rec}");
    assert_eq!(
        rec["blockers"],
        json!(["setup.blocker.aborted_ground_contact"])
    );
    assert!(s.e.current_view().is_none(), "nothing runs after it");
    assert_eq!(s.fc.params["RLL_RATE_P"], 0.08, "tuned roll gain discarded");
}

#[test]
fn verify_sends_no_take_off_to_a_plane_in_the_air() {
    // A plane at 0.2 m and 16 m/s is flying: verify climbs it to the loiter.
    // At 0.2 m and 3 m/s it is neither flying nor landed: verify stops.
    for (speed, flying) in [(16.0, true), (3.0, false)] {
        let mut s = plane();
        assert_eq!(s.e.run_step(A, "verify", s.now)["ok"], json!(true));
        s.run(200);
        s.fc.armed = true;
        s.fc.alt = 0.2;
        s.fc.groundspeed = speed;
        s.fc.custom_mode = 8;
        s.run(1500);
        let mark = s.fc.sent.len();
        s.act(A, "verify", "start", json!({}));
        s.run(5000);
        assert!(
            !modes_from(&s, mark).contains(&13.0),
            "{speed} m/s: TAKEOFF mode sent: {:?}",
            modes_from(&s, mark)
        );
        assert!(
            !nav_from(&s, mark).contains(&MavCmd::MAV_CMD_NAV_TAKEOFF),
            "{speed} m/s: NAV_TAKEOFF sent"
        );
        if flying {
            assert!(nav_from(&s, mark).contains(&MavCmd::MAV_CMD_DO_REPOSITION));
            assert!(s.fc.alt > 2.0, "climbing to the loiter");
        } else {
            assert_eq!(
                s.e.current_view().map(|v| v.blockers),
                Some(vec!["setup.blocker.flight_state".to_string()])
            );
        }
    }
}

#[test]
fn a_plane_on_the_ground_in_wind_still_takes_off() {
    // Armed on the ground with 6 m/s on the speed reading: landed, not flying.
    let mut s = plane();
    assert_eq!(s.e.run_step(A, "verify", s.now)["ok"], json!(true));
    s.run(200);
    s.fc.armed = true;
    s.fc.groundspeed = 6.0;
    s.run(1500);
    let mark = s.fc.sent.len();
    s.act(A, "verify", "start", json!({}));
    s.run(3000);
    let modes = modes_from(&s, mark);
    assert!(modes.contains(&13.0), "TAKEOFF from the ground: {modes:?}");
}

#[test]
fn a_copter_neither_flying_nor_landed_gets_no_take_off() {
    let mut s = air_ready();
    assert_eq!(s.e.run_step(A, "autotune", s.now)["ok"], json!(true));
    s.run(200);
    s.fc.armed = true;
    s.fc.alt = 0.3;
    s.fc.custom_mode = 5;
    s.run(1500);
    let mark = s.fc.sent.len();
    s.act(A, "autotune", "start", json!({}));
    s.run(3000);
    assert_eq!(modes_from(&s, mark), Vec::<f32>::new());
    assert!(!nav_from(&s, mark).contains(&MavCmd::MAV_CMD_NAV_TAKEOFF));
    assert_eq!(
        s.e.current_view().map(|v| v.blockers),
        Some(vec!["setup.blocker.flight_state".to_string()])
    );
}

/// A copter tune whose roll and pitch round finished and saved, now waiting
/// for the battery swap before yaw.
fn copter_after_roll_pitch() -> (Sim, f32, f32) {
    let mut s = air_ready();
    let before = s.fc.params["ATC_RAT_RLL_P"];
    start_step(&mut s, "autotune");
    s.until(|s| s.fc.alt > 7.5, 20_000);
    s.fc.battery_pct = 45;
    s.until(
        |s| {
            s.e.current_view()
                .is_some_and(|v| v.actions.iter().any(|a| a.id == "resume_yaw"))
        },
        60_000,
    );
    let tuned = s.fc.params["ATC_RAT_RLL_P"];
    assert!(tuned != before, "the round saved its roll gain");
    (s, before, tuned)
}

#[test]
fn an_abort_keeps_the_gains_of_rounds_already_saved() {
    // Abort while waiting for the battery swap: nothing is flying, and the
    // saved roll and pitch gains stay.
    let (mut s, _, tuned) = copter_after_roll_pitch();
    s.e.abort(A, s.now);
    s.run(3000);
    assert_eq!(
        s.fc.params["ATC_RAT_RLL_P"], tuned,
        "abort in the swap wait"
    );

    // The yaw round sinks below its floor: its own gains go back, the saved
    // roll gain stays.
    let (mut s, _, tuned) = copter_after_roll_pitch();
    s.fc.battery_pct = 98;
    s.run(1100);
    s.act(A, "autotune", "resume_yaw", json!({}));
    let mark = s.fc.sent.len();
    s.until(
        |s| s.fc.custom_mode == 15 && modes_from(s, mark).contains(&15.0),
        60_000,
    );
    s.run(2000);
    s.fc.alt = 1.5;
    s.run(3000);
    let rec = record(&s, "autotune");
    assert_eq!(
        rec["blockers"],
        json!(["setup.blocker.aborted_altitude_floor"]),
        "{rec}"
    );
    assert_eq!(
        s.fc.params["ATC_RAT_RLL_P"], tuned,
        "abort in the yaw round"
    );
    let discarded = rec["result"]["gains_discarded"].to_string();
    assert!(!discarded.contains("RLL"), "{discarded}");
}
