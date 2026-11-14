use ados_protocol::pilot::{Owner, Request};

use ados_protocol::mavlink::ardupilotmega::{
    MavMessage, MavSeverity, HOME_POSITION_DATA, STATUSTEXT_DATA,
};

use super::harness::{World, FC_COMP, FC_SYS};
use crate::core::Reply;
use crate::fc::{MASK_POS_VEL_YAW, MASK_VELOCITY};
use crate::modes::copter;

#[test]
fn disarmed_is_idle_and_sends_no_setpoints() {
    let mut w = World::new();
    w.run(200);
    assert_eq!(w.p.owner(), Owner::Idle);
    assert!(w.last_setpoint().is_none());
    assert_eq!(w.p.state(w.now).owner_label, "READY");
}

#[test]
fn heartbeat_is_sent_at_2_hz() {
    let mut w = World::new();
    w.run(1000);
    let hb = w
        .sent
        .iter()
        .filter(|m| {
            matches!(
                m,
                ados_protocol::mavlink::ardupilotmega::MavMessage::HEARTBEAT(_)
            )
        })
        .count();
    assert!((2..=3).contains(&hb), "{hb} heartbeats in 1 s");
}

#[test]
fn take_control_enters_guided_and_holds() {
    let mut w = World::new();
    w.airborne(copter::LOITER);
    assert_eq!(w.p.owner(), Owner::FcMode);
    assert!(matches!(w.action("take_control"), Reply::Action(a) if a.ok));
    w.run(100);
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);
    let sp = w.last_setpoint().unwrap();
    assert_eq!(sp.type_mask.bits(), MASK_POS_VEL_YAW);
    assert_eq!(sp.z, -5.0);
}

#[test]
fn external_guided_is_adopted_with_a_gcs_banner() {
    let mut w = World::new();
    w.airborne(copter::LOITER);
    w.mode = copter::GUIDED;
    w.run(40);
    assert_eq!(w.p.owner(), Owner::Pilot);
    let b = w.p.state(w.now).banner.unwrap();
    assert_eq!(b.text_key, "pilot.banner.ados_engaged");
    assert_eq!(b.args["source"], "gcs");
}

/// In `fc_manual` and `elrs` a ground station's reposition (the FC goes to
/// GUIDED and flies to the point) stays the FC's: the pilot never takes it
/// over and holds the vehicle where it is.
#[test]
fn external_guided_outside_ados_stays_with_the_fc() {
    for mode in ["fc_manual", "elrs"] {
        let mut w = World::new();
        w.airborne(copter::LOITER);
        assert!(matches!(w.action(&format!("control_mode:{mode}")), Reply::Action(a) if a.ok));
        w.run(40);
        w.mode = copter::GUIDED;
        w.run(400);
        assert_eq!(w.p.owner(), Owner::FcMode, "{mode}");
        assert_eq!(w.mode, copter::GUIDED, "{mode}");
        assert!(
            w.last_setpoint().is_none(),
            "{mode}: the pilot sent a setpoint"
        );
    }
}

/// In `fc_manual` and `elrs`, after Take control, a ground station's
/// reposition finds the FC already in GUIDED, so the pilot sees no mode
/// change. It still flies: the pilot sends its hold once and then leaves the
/// FC's target alone, and the FC's accepted reposition hands it the vehicle.
#[test]
fn reposition_after_take_control_outside_ados_is_the_fcs() {
    use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavResult, COMMAND_ACK_DATA};
    let setpoints = |w: &World| {
        w.sent
            .iter()
            .filter(|m| matches!(m, MavMessage::SET_POSITION_TARGET_LOCAL_NED(_)))
            .count()
    };
    for mode in ["fc_manual", "elrs"] {
        let mut w = World::new();
        w.airborne(copter::LOITER);
        assert!(matches!(w.action(&format!("control_mode:{mode}")), Reply::Action(a) if a.ok));
        assert!(matches!(w.action("take_control"), Reply::Action(a) if a.ok));
        w.run(300);
        assert_eq!(w.mode, copter::GUIDED, "{mode}");
        assert_eq!(w.p.owner(), Owner::Pilot, "{mode}");
        assert!(setpoints(&w) >= 1, "{mode}: the hold was never sent");
        w.sent.clear();
        w.run(1000);
        assert_eq!(
            setpoints(&w),
            0,
            "{mode}: the pilot streams its hold over the FC's GUIDED target"
        );
        let ack = MavMessage::COMMAND_ACK(COMMAND_ACK_DATA {
            command: MavCmd::MAV_CMD_DO_REPOSITION,
            result: MavResult::MAV_RESULT_ACCEPTED,
        });
        w.p.on_fc(FC_SYS, FC_COMP, &ack, w.now);
        w.run(400);
        assert_eq!(w.p.owner(), Owner::FcMode, "{mode}");
        assert_eq!(w.mode, copter::GUIDED, "{mode}");
        assert_eq!(setpoints(&w), 0, "{mode}");
    }
}

/// Under `ados` the pilot keeps streaming its hold: a ground station's
/// reposition is redirected to it rather than reaching the FC.
#[test]
fn hold_keeps_streaming_under_ados() {
    let mut w = World::new();
    w.airborne(copter::LOITER);
    assert!(matches!(w.action("take_control"), Reply::Action(a) if a.ok));
    w.run(300);
    w.sent.clear();
    w.run(1000);
    let n = w
        .sent
        .iter()
        .filter(|m| matches!(m, MavMessage::SET_POSITION_TARGET_LOCAL_NED(_)))
        .count();
    assert!(n >= 5, "{n} hold setpoints in 1 s");
}

#[test]
fn stick_flight_moves_and_centred_sticks_hold() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600); // pass the gate
    w.stick([0.0, 1.0, 0.0, 0.0], 500);
    let sp = w.last_setpoint().unwrap();
    assert_eq!(sp.type_mask.bits(), MASK_VELOCITY);
    assert!(sp.vx > 0.5, "forward velocity {}", sp.vx);
    w.stick([0.0; 4], 6000);
    assert_eq!(
        w.last_setpoint().unwrap().type_mask.bits(),
        MASK_POS_VEL_YAW
    );
}

#[test]
fn gate_blocks_deflected_sticks_until_centred() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0, 0.5, 0.0, 0.0], 1000);
    assert!(w.p.state(w.now).gate.unwrap().pending);
    assert_eq!(
        w.last_setpoint().unwrap().type_mask.bits(),
        MASK_POS_VEL_YAW,
        "no stick effect"
    );
    w.stick([0.0; 4], 600);
    assert!(w.p.state(w.now).gate.is_none());
}

#[test]
fn second_session_waits_for_take() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.input([0.0; 4]);
    let other = ados_protocol::pilot::Input {
        src: "android".into(),
        session: 7,
        seq: 0,
        t_ms: 0,
        axes: [0.0; 4],
        buttons: 0,
    };
    w.req(Request::Input(other));
    assert_eq!(w.p.state(w.now).input_session.unwrap().src, "mc");
    w.req(Request::SessionTake {
        src: "android".into(),
        session: 7,
    });
    assert_eq!(w.p.state(w.now).input_session.unwrap().src, "android");
}

#[test]
fn controller_loss_brakes_to_hold() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.stick([0.0; 4], 600);
    w.stick([0.0, 1.0, 0.0, 0.0], 300);
    w.run(400);
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("controller-lost"));
    assert_eq!(s.banner.unwrap().text_key, "pilot.banner.controller_lost");
}

#[test]
fn link_loss_triggers_rtl_after_ten_seconds() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.req(Request::Hb { src: "mc".into() });
    w.run(5000);
    assert!(w.p.state(w.now).link_loss_in_s.unwrap() <= 5.1);
    w.run(5100);
    assert_eq!(w.mode, copter::RTL);
    assert_eq!(w.p.owner(), Owner::FcMode);
}

#[test]
fn the_pilot_kind_names_the_link_loss_action_and_a_vtol_transition() {
    use ados_protocol::mavlink::ardupilotmega::{
        MavLandedState, MavMessage, MavVtolState, EXTENDED_SYS_STATE_DATA,
    };
    let mut w = World::new();
    assert_eq!(w.p.state(w.now).link_loss_action, "rth");
    w.p.settings.pilot.link_loss_action = crate::config::LinkLossAction::Land;
    assert_eq!(w.p.state(w.now).link_loss_action, "land");
    w.p.settings.pilot.link_loss_action = crate::config::LinkLossAction::Hold;
    assert_eq!(w.p.state(w.now).link_loss_action, "hold");
    assert!(!w.p.state(w.now).vtol_transitioning);
    let transition = MavMessage::EXTENDED_SYS_STATE(EXTENDED_SYS_STATE_DATA {
        vtol_state: MavVtolState::MAV_VTOL_STATE_TRANSITION_TO_FW,
        landed_state: MavLandedState::MAV_LANDED_STATE_IN_AIR,
    });
    w.p.on_fc(
        super::harness::FC_SYS,
        super::harness::FC_COMP,
        &transition,
        w.now,
    );
    assert!(w.p.state(w.now).vtol_transitioning);
}

#[test]
fn setup_engine_actions_do_not_arm_the_link_loss_timer() {
    // The setup wizard's "Use ADOS Pilot" sets the control mode through an
    // action from `setup`; no operator surface spoke, so no link can be lost.
    let mut w = World::new();
    w.req(Request::Action(ados_protocol::pilot::ActionReq {
        name: "control_mode:ados".into(),
        src: "setup".into(),
        args: None,
        id: None,
    }));
    w.run(11_000);
    assert!(!w.p.link_lost(w.now));
}

#[test]
fn tuning_owner_survives_the_landing_between_setup_flight_steps() {
    // first_hover lands and disarms, then autotune arms again: the setup
    // engine still holds `tuning`, so the pilot must not idle it on disarm.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.p.auth.enter_tuning().unwrap();
    w.run(200);
    assert_eq!(w.p.owner(), Owner::Tuning);
    w.armed = false;
    w.run(1500);
    assert_eq!(w.p.owner(), Owner::Tuning);
    // An operator safety action still ends it (and the next disarm idles).
    w.armed = true;
    w.run(1500);
    w.action("land");
    assert_ne!(w.p.owner(), Owner::Tuning);
}

#[test]
fn lease_grant_goal_and_revocations() {
    let mut w = World::new();
    // Avoidance off: the goal is flown as a position setpoint at the target
    // (with avoidance on, the brake caps it to the space the lidar has seen).
    w.p.settings.pilot.avoid = ados_protocol::pilot::AvoidMode::Off;
    w.airborne(copter::GUIDED);
    let id = w.lease();
    assert_eq!(w.p.owner(), Owner::Skill);
    // Busy for a second holder.
    let r = w.req(Request::LeaseAcquire {
        holder: ados_protocol::pilot::Holder {
            plugin_id: "x".into(),
            skill_id: "y".into(),
        },
        kind: "motion".into(),
    });
    assert!(matches!(r, Reply::Lease(l) if l.err.as_deref() == Some("busy")));
    w.goal(id, 10.0, 0.0);
    w.run(300);
    let sp = w.last_setpoint().unwrap();
    assert_eq!((sp.x, sp.y), (10.0, 0.0));
    // pause revokes.
    w.action("pause");
    assert_eq!(w.revoked.last().unwrap(), &(id, "pause".to_owned()));
    assert_eq!(w.p.owner(), Owner::Pilot);
}

#[test]
fn lease_revoked_by_silence_mode_change_and_takeover() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.goal(id, 5.0, 0.0);
    w.run(1200);
    assert_eq!(w.revoked.last().unwrap(), &(id, "goal-timeout".to_owned()));

    let id = w.lease();
    w.mode = copter::LOITER;
    w.run(40);
    assert_eq!(w.revoked.last().unwrap(), &(id, "fc-mode".to_owned()));

    w.mode = copter::GUIDED;
    w.run(40);
    let id = w.lease();
    for _ in 0..60 {
        w.goal(id, 5.0, 0.0);
        w.input([0.0, 1.0, 0.0, 0.0]);
        w.run(20);
    }
    assert_eq!(w.revoked.last().unwrap(), &(id, "takeover".to_owned()));
    assert_eq!(
        w.p.state(w.now).banner.unwrap().text_key,
        "pilot.banner.pilot_took_control"
    );
}

#[test]
fn lease_refused_while_disarmed() {
    let mut w = World::new();
    let r = w.req(Request::LeaseAcquire {
        holder: ados_protocol::pilot::Holder {
            plugin_id: "p".into(),
            skill_id: "s".into(),
        },
        kind: "motion".into(),
    });
    assert!(matches!(r, Reply::Lease(l) if l.err.as_deref() == Some("disarmed")));
}

#[test]
fn mission_yields_and_stick_preempts_into_guided() {
    let mut w = World::new();
    w.airborne(copter::AUTO);
    w.run(40);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    assert!(w.last_setpoint().is_none());
    w.stick([0.0, 0.5, 0.0, 0.0], 400);
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert!(matches!(w.action("resume"), Reply::Action(a) if a.ok));
    assert_eq!(w.mode, copter::AUTO);
}

#[test]
fn rth_and_land_hand_to_the_fc() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.action("rth");
    assert_eq!(w.mode, copter::RTL);
    w.run(40);
    assert_eq!(w.p.owner(), Owner::FcMode);
    assert!(w.p.state(w.now).owner_label.starts_with("RTH"));
    w.mode = copter::GUIDED;
    w.run(40);
    w.action("land");
    w.agl = Some(0.5);
    w.pos[2] = -0.5;
    w.run(100);
    assert_eq!(w.mode, copter::LAND);
}

#[test]
fn takeoff_arms_climbs_and_holds() {
    let mut w = World::new();
    w.action("takeoff");
    w.run(500);
    assert!(w.armed);
    assert_eq!(w.mode, copter::GUIDED);
    w.run(1500);
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert_eq!(w.p.state(w.now).sequence.unwrap().step, "done");
}

#[test]
fn fc_profile_mismatch_blocks_arming() {
    let mut w = World::new();
    w.p.fc.params.insert("FS_GCS_ENABLE".into(), 0.0);
    assert!(
        matches!(w.action("arm"), Reply::Action(a) if a.err.as_deref() == Some("not_ready:fc_profile"))
    );
}

#[test]
fn goto_flies_and_arrives_into_hold() {
    let mut w = World::new();
    w.p.settings.pilot.avoid = ados_protocol::pilot::AvoidMode::Off;
    w.airborne(copter::GUIDED);
    w.req(Request::Goto {
        n: 0.5,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    w.run(40);
    assert_eq!(w.p.owner(), Owner::Pilot, "arrived at once: within 1 m");
    w.req(Request::Goto {
        n: 20.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    w.run(300);
    assert_eq!(w.p.owner(), Owner::Goto);
    assert_eq!(w.last_setpoint().unwrap().x, 20.0);
}

#[test]
fn geofence_clamps_outward_velocity() {
    let v = crate::core::control::geofence([500.0, 0.0], [0.0, 0.0], 500.0, [3.0, 2.0]);
    assert_eq!(v, [0.0, 2.0]);
}

#[test]
fn control_mode_change_revokes_and_persists() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.action("control_mode:fc_manual");
    assert_eq!(w.revoked.last().unwrap(), &(id, "control-mode".to_owned()));
    assert_eq!(
        w.p.state(w.now).control_mode,
        ados_protocol::pilot::ControlMode::FcManual
    );
}

#[test]
fn golden_pilot_fixture_round_trips() {
    let text = include_str!("../../../../ados-control/tests/fixtures/pilot-frame.json");
    let v: serde_json::Value = serde_json::from_str(text).unwrap();
    let s: ados_protocol::pilot::PilotState = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
    let sensors = s.sensors.as_ref().expect("the fixture carries sensors");
    assert_eq!(sensors.flow.state, ados_protocol::pilot::FlowState::Low);
    assert_eq!(sensors.range.height_m, Some(4.2));
    assert_eq!(
        sensors.lidar.state,
        ados_protocol::pilot::LidarState::Degraded
    );
    let text = include_str!("../../../../ados-control/tests/fixtures/obstacles-frame.json");
    let v: serde_json::Value = serde_json::from_str(text).unwrap();
    let o: ados_protocol::pilot::ObstaclesFrame = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&o).unwrap(), v);
}

/// Without a live FC link the frame does not claim an arm or landed state:
/// both serialise as JSON `null`, never skipped.
#[test]
fn arm_and_landed_are_null_without_an_fc_link() {
    let p = crate::core::Pilot::new(crate::config::Settings::default());
    let v = serde_json::to_value(p.state(1000)).unwrap();
    assert_eq!(v.get("armed"), Some(&serde_json::Value::Null));
    assert_eq!(v.get("landed"), Some(&serde_json::Value::Null));

    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let s = w.p.state(w.now);
    assert_eq!((s.armed, s.landed), (Some(true), Some(false)));
    // The heartbeat goes quiet for longer than the link window.
    let v = serde_json::to_value(w.p.state(w.now + 3000)).unwrap();
    assert_eq!(v.get("armed"), Some(&serde_json::Value::Null));
    assert_eq!(v.get("landed"), Some(&serde_json::Value::Null));
}

/// Before the FC's first heartbeat the frame does not claim a mode: the
/// mode and its name serialise as JSON `null` (unknown, never mode 0 /
/// STABILIZE), and carry the FC's mode once its heartbeat arrives.
#[test]
fn fc_mode_is_null_until_the_fc_heartbeat() {
    let p = crate::core::Pilot::new(crate::config::Settings::default());
    let v = serde_json::to_value(p.state(1000)).unwrap();
    assert_eq!(v.get("fc_mode"), Some(&serde_json::Value::Null));
    assert_eq!(v.get("fc_mode_name"), Some(&serde_json::Value::Null));

    let mut w = World::new();
    w.airborne(copter::LOITER);
    let v = serde_json::to_value(w.p.state(w.now)).unwrap();
    assert_eq!(v.get("fc_mode"), Some(&serde_json::json!(copter::LOITER)));
    assert_eq!(v.get("fc_mode_name"), Some(&serde_json::json!("LOITER")));
    // The heartbeat goes quiet for longer than the link window.
    let v = serde_json::to_value(w.p.state(w.now + 3000)).unwrap();
    assert_eq!(v.get("fc_mode"), Some(&serde_json::Value::Null));
    assert_eq!(v.get("fc_mode_name"), Some(&serde_json::Value::Null));
}

fn home_at(w: &mut World, lat: f64, lon: f64) {
    let m = MavMessage::HOME_POSITION(HOME_POSITION_DATA {
        latitude: (lat * 1e7) as i32,
        longitude: (lon * 1e7) as i32,
        ..Default::default()
    });
    w.p.on_fc(FC_SYS, FC_COMP, &m, w.now);
}

#[test]
fn surface_goto_flies_to_a_map_point_relative_to_home() {
    let mut w = World::new();
    assert_eq!(w.p.state(w.now).surface_ops, vec!["goto".to_owned()]);
    let args = serde_json::json!({"lat": 12.0009, "lon": 77.0, "alt_m": 6.0});
    home_at(&mut w, 12.0, 77.0);
    assert!(
        matches!(w.action_args("goto", args.clone()), Reply::Action(a) if a.err.as_deref() == Some("not_armed"))
    );
    w.airborne(copter::GUIDED);
    let bad = serde_json::json!({"lat": 12.0});
    assert!(
        matches!(w.action_args("goto", bad), Reply::Action(a) if a.err.as_deref() == Some("bad_args"))
    );
    assert!(matches!(w.action_args("goto", args), Reply::Action(a) if a.ok));
    w.run(300);
    assert_eq!(w.p.owner(), Owner::Goto);
    let g = w.p.state(w.now).goal_ned.unwrap();
    assert!((g[0] - 100.2).abs() < 0.5, "{g:?}");
    assert!(g[1].abs() < 0.1 && (g[2] + 6.0).abs() < 0.01, "{g:?}");
}

#[test]
fn surface_goto_without_home_is_refused() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let args = serde_json::json!({"lat": 12.0, "lon": 77.0, "alt_m": 6.0});
    assert!(
        matches!(w.action_args("goto", args), Reply::Action(a) if a.err.as_deref() == Some("no_home"))
    );
}

/// Outside `ados` the pilot neither offers nor flies a surface's map point:
/// the surface repositions the FC itself.
#[test]
fn surface_goto_is_only_offered_under_ados() {
    for mode in ["fc_manual", "elrs"] {
        let mut w = World::new();
        home_at(&mut w, 12.0, 77.0);
        w.airborne(copter::LOITER);
        assert!(matches!(w.action(&format!("control_mode:{mode}")), Reply::Action(a) if a.ok));
        assert!(w.p.state(w.now).surface_ops.is_empty(), "{mode}");
        let args = serde_json::json!({"lat": 12.0009, "lon": 77.0, "alt_m": 6.0});
        assert!(
            matches!(w.action_args("goto", args), Reply::Action(a) if a.err.as_deref() == Some("control_mode")),
            "{mode}"
        );
        w.run(200);
        assert_eq!(w.p.owner(), Owner::FcMode, "{mode}");
        assert_eq!(w.mode, copter::LOITER, "{mode}");
    }
}

/// While the FC flies an RTL or land the pilot did not command (an RC switch,
/// a radio, battery or EKF failsafe), a surface's map point is refused: the
/// operator changes mode first. After Hand to FC (LOITER) it flies.
#[test]
fn surface_goto_refused_during_an_fc_failsafe() {
    let args = serde_json::json!({"lat": 12.0009, "lon": 77.0, "alt_m": 6.0});
    for mode in [copter::RTL, copter::SMART_RTL, copter::LAND] {
        let mut w = World::new();
        home_at(&mut w, 12.0, 77.0);
        w.airborne(copter::GUIDED);
        w.run(100);
        w.mode = mode;
        w.run(100);
        let r = w.action_args("goto", args.clone());
        assert!(
            matches!(&r, Reply::Action(a) if a.err.as_deref() == Some("failsafe")),
            "mode {mode}: {r:?}"
        );
        w.run(100);
        assert_eq!(w.mode, mode, "the FC keeps flying its failsafe");
        let hand = serde_json::json!({"mode": "loiter", "control_mode": "ados"});
        assert!(matches!(w.action_args("hand_to_fc", hand), Reply::Action(a) if a.ok));
        w.run(100);
        assert!(matches!(w.action_args("goto", args.clone()), Reply::Action(a) if a.ok));
        w.run(100);
        assert_eq!(w.p.owner(), Owner::Goto, "mode {mode}");
    }
}

/// During an FC failsafe RTL or land a skill's motion lease is refused with
/// `failsafe` and the FC keeps flying its failsafe. After the operator's mode
/// change (Hand to FC) the lease is granted again.
#[test]
fn skill_lease_refused_during_an_fc_failsafe() {
    for mode in [copter::RTL, copter::SMART_RTL, copter::LAND] {
        let mut w = World::new();
        home_at(&mut w, 12.0, 77.0);
        w.airborne(copter::GUIDED);
        w.run(100);
        w.mode = mode;
        w.run(100);
        let r = w.req(Request::LeaseAcquire {
            holder: ados_protocol::pilot::Holder {
                plugin_id: "com.example.follow".into(),
                skill_id: "follow-me".into(),
            },
            kind: "motion".into(),
        });
        assert!(
            matches!(&r, Reply::Lease(l) if !l.ok && l.err.as_deref() == Some("failsafe")),
            "mode {mode}: {r:?}"
        );
        w.run(200);
        assert_eq!(w.mode, mode, "the FC keeps flying its failsafe");
        assert_ne!(w.p.owner(), Owner::Skill, "mode {mode}");
        let hand = serde_json::json!({"mode": "loiter", "control_mode": "ados"});
        assert!(matches!(w.action_args("hand_to_fc", hand), Reply::Action(a) if a.ok));
        w.run(100);
        w.lease();
        assert_eq!(w.p.owner(), Owner::Skill, "mode {mode}");
    }
}

/// An RTL the operator asked for is the operator's own mode change: a map
/// point after it flies.
#[test]
fn surface_goto_after_an_operator_rth_flies() {
    let mut w = World::new();
    home_at(&mut w, 12.0, 77.0);
    w.airborne(copter::GUIDED);
    w.run(100);
    assert!(matches!(w.action("rth"), Reply::Action(a) if a.ok));
    w.run(100);
    assert_eq!(w.mode, copter::RTL);
    let args = serde_json::json!({"lat": 12.0009, "lon": 77.0, "alt_m": 6.0});
    assert!(matches!(w.action_args("goto", args), Reply::Action(a) if a.ok));
}

#[test]
fn surface_goto_refused_while_armed_on_the_ground() {
    let mut w = World::new();
    home_at(&mut w, 12.0, 77.0);
    w.armed = true;
    w.run(200);
    let args = serde_json::json!({"lat": 12.0009, "lon": 77.0, "alt_m": 6.0});
    let r = w.action_args("goto", args);
    assert!(
        matches!(&r, Reply::Action(a) if a.err.as_deref() == Some("landed")),
        "{r:?}"
    );
    assert_ne!(w.p.owner(), Owner::Goto);
}

#[test]
fn surface_goto_refused_while_tuning() {
    let mut w = World::new();
    home_at(&mut w, 12.0, 77.0);
    w.airborne(copter::GUIDED);
    w.p.auth.enter_tuning().unwrap();
    w.run(200);
    let args = serde_json::json!({"lat": 12.0009, "lon": 77.0, "alt_m": 6.0});
    let r = w.action_args("goto", args);
    assert!(
        matches!(&r, Reply::Action(a) if a.err.as_deref() == Some("tuning")),
        "{r:?}"
    );
    assert_eq!(w.p.owner(), Owner::Tuning);
}

#[test]
fn the_pilot_kind_names_the_setup_step_only_while_tuning() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.p.setup_step = Some("autotune");
    w.run(100);
    let v = serde_json::to_value(w.p.state(w.now)).unwrap();
    assert_eq!(
        v["setup_step"],
        serde_json::Value::Null,
        "owner {:?}",
        w.p.owner()
    );
    assert!(v.as_object().unwrap().contains_key("setup_step"));

    w.p.auth.enter_tuning().unwrap();
    w.run(100);
    let s = w.p.state(w.now);
    assert_eq!(s.setup_step.as_deref(), Some("autotune"));
    assert_eq!(serde_json::to_value(&s).unwrap()["setup_step"], "autotune");

    w.p.setup_step = None;
    w.run(100);
    assert_eq!(w.p.owner(), Owner::Tuning);
    assert_eq!(w.p.state(w.now).setup_step, None);

    w.p.setup_step = Some("first_hover");
    w.action("land");
    assert_ne!(w.p.owner(), Owner::Tuning);
    assert_eq!(w.p.state(w.now).setup_step, None);
}

#[test]
fn surface_goto_offsets_from_a_home_away_from_the_local_origin() {
    let mut w = World::new();
    let m = MavMessage::HOME_POSITION(HOME_POSITION_DATA {
        latitude: 120_000_000,
        longitude: 770_000_000,
        x: 5.0,
        y: -3.0,
        z: -2.0,
        ..Default::default()
    });
    w.p.on_fc(FC_SYS, FC_COMP, &m, w.now);
    w.airborne(copter::GUIDED);
    let args = serde_json::json!({"lat": 12.0009, "lon": 77.0009, "alt_m": 6.0});
    let r = w.action_args("goto", args);
    assert!(matches!(&r, Reply::Action(a) if a.ok), "{r:?}");
    w.run(300);
    assert_eq!(w.p.owner(), Owner::Goto);
    let g = w.p.state(w.now).goal_ned.unwrap();
    let east = 100.2 * 12.0_f64.to_radians().cos();
    assert!((g[0] - (5.0 + 100.2)).abs() < 0.5, "{g:?}");
    assert!((g[1] - (-3.0 + east)).abs() < 0.5, "{g:?}");
    assert!((g[2] - (-2.0 - 6.0)).abs() < 0.01, "{g:?}");
}

#[test]
fn surface_goto_waits_for_a_climbing_takeoff() {
    let mut w = World::new();
    home_at(&mut w, 12.0, 77.0);
    let r = w.action_args(
        "launch",
        serde_json::json!({"after": "hold", "alt_m": 10.0}),
    );
    assert!(matches!(&r, Reply::Action(a) if a.ok), "{r:?}");
    let mut waited = 0;
    while w.landed && waited < 5000 {
        w.run(20);
        waited += 20;
    }
    assert!(!w.landed, "the take-off command went out");
    // Mid-climb: in the air, well short of the take-off height.
    w.pos[2] = -3.0;
    w.agl = Some(3.0);
    w.run(200);
    assert_eq!(w.p.state(w.now).sequence.unwrap().step, "takeoff");
    let args = serde_json::json!({"lat": 12.0009, "lon": 77.0, "alt_m": 10.0});
    let r = w.action_args("goto", args.clone());
    assert!(
        matches!(&r, Reply::Action(a) if a.err.as_deref() == Some("launching")),
        "{r:?}"
    );
    assert_ne!(w.p.owner(), Owner::Goto);
    // At the top of the climb the launch hands over and a goto is taken.
    w.pos[2] = -10.0;
    w.agl = Some(10.0);
    w.run(1000);
    assert_eq!(w.p.state(w.now).sequence.unwrap().step, "done");
    assert!(matches!(w.action_args("goto", args), Reply::Action(a) if a.ok));
    w.run(300);
    assert_eq!(w.p.owner(), Owner::Goto);
}

#[test]
fn agl_reports_its_source() {
    let mut w = World::new();
    w.agl = Some(3.0);
    w.run(40);
    let s = w.p.state(w.now);
    assert_eq!(
        (s.agl_m, s.agl_source.as_deref()),
        (Some(3.0), Some("rangefinder"))
    );
    w.agl = None;
    w.pos = [0.0, 0.0, -12.0];
    w.run(600);
    let s = w.p.state(w.now);
    assert_eq!(
        (s.agl_m, s.agl_source.as_deref()),
        (Some(12.0), Some("home"))
    );
}

#[test]
fn fc_warnings_reach_the_pilot_kind_newest_first_and_age_out() {
    let mut w = World::new();
    let text = |sev: MavSeverity, t: &str| {
        MavMessage::STATUSTEXT(STATUSTEXT_DATA {
            severity: sev,
            text: t.into(),
        })
    };
    w.p.on_fc(
        FC_SYS,
        FC_COMP,
        &text(MavSeverity::MAV_SEVERITY_INFO, "EKF3 IMU0 is using GPS"),
        w.now,
    );
    w.p.on_fc(
        FC_SYS,
        FC_COMP,
        &text(
            MavSeverity::MAV_SEVERITY_WARNING,
            "PreArm: Compass not calibrated",
        ),
        w.now,
    );
    w.run(1000);
    w.p.on_fc(
        FC_SYS,
        FC_COMP,
        &text(MavSeverity::MAV_SEVERITY_CRITICAL, "Crash: Disarming"),
        w.now,
    );
    let s = w.p.state(w.now);
    let got: Vec<_> = s
        .statustext
        .iter()
        .map(|t| (t.severity, t.text.as_str(), t.age_ms))
        .collect();
    assert_eq!(
        got,
        vec![
            (2, "Crash: Disarming", 0),
            (4, "PreArm: Compass not calibrated", 1000)
        ]
    );
    w.run(29_500);
    assert_eq!(w.p.state(w.now).statustext.len(), 1);
    for i in 0..20 {
        w.p.on_fc(
            FC_SYS,
            FC_COMP,
            &text(MavSeverity::MAV_SEVERITY_ERROR, &format!("e{i}")),
            w.now,
        );
    }
    let s = w.p.state(w.now);
    assert_eq!(s.statustext.len(), crate::fc::STATUSTEXT_RING);
    assert_eq!(s.statustext[0].text, "e19");
}

#[test]
fn skill_goal_stops_at_the_geofence() {
    let mut w = World::new();
    w.p.settings.pilot.max_dist_m = 20.0;
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.pos = [19.9, 0.0, -4.0];
    // A goal past the fence, with a feed-forward carrying the vehicle out.
    let mut goal = ados_protocol::pilot::Goal::new(
        ados_protocol::pilot::GoalKind::Position,
        [60.0, 0.0, -4.0],
        5.0,
    );
    goal.vn = 3.0;
    for _ in 0..4 {
        w.req(Request::Goal {
            lease_id: id,
            goal: goal.clone(),
        });
        w.run(100);
        if w.pos[0] < 20.0 {
            w.pos[0] = 20.0; // the FC's lag carries it onto the fence
        }
    }
    let sp = w.last_setpoint().unwrap();
    assert!(sp.vx <= 1e-3, "outward velocity {}", sp.vx);
    assert!(
        sp.x <= 20.0 + 1e-3,
        "position setpoint {} past the fence",
        sp.x
    );
}

#[test]
fn goto_stops_at_the_geofence() {
    let mut w = World::new();
    w.p.settings.pilot.max_dist_m = 20.0;
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.pos = [20.0, 0.0, -5.0];
    // Past the fence and off to the side: the fence point is 9 m away.
    w.req(Request::Goto {
        n: 60.0,
        e: 30.0,
        d: -5.0,
        yaw: None,
    });
    w.run(400);
    assert_eq!(w.p.owner(), Owner::Goto);
    let sp = w.last_setpoint().unwrap();
    assert!(sp.vx <= 1e-3, "outward velocity {}", sp.vx);
    assert!(
        sp.x <= 20.0 + 1e-3,
        "position setpoint {} past the fence",
        sp.x
    );
}

#[test]
fn a_goto_past_the_fence_arrives_at_the_fence() {
    let mut w = World::new();
    w.p.settings.pilot.max_dist_m = 20.0;
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.pos = [20.0, 0.0, -5.0];
    w.req(Request::Goto {
        n: 60.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    w.run(400);
    assert_eq!(w.p.owner(), Owner::Pilot, "arrived and holds");
}

#[test]
fn a_goto_above_the_ceiling_arrives_at_the_ceiling() {
    let mut w = World::new();
    w.p.settings.pilot.max_alt_agl_m = 5.0;
    w.airborne(copter::GUIDED); // d = -5, 5 m above the ground: at the ceiling
    w.action("take_control");
    w.req(Request::Goto {
        n: 0.0,
        e: 0.0,
        d: -30.0,
        yaw: None,
    });
    w.run(400);
    assert_eq!(w.p.owner(), Owner::Pilot, "arrived and holds");
}

#[test]
fn a_rangefinder_stuck_low_holds_the_height_rather_than_climbing() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.agl = Some(0.3); // a stuck reading under the 0.5 m floor, at d = -5
    let id = w.lease();
    w.goal_d(id, 0.0, 0.0, -5.0);
    w.run(400);
    let sp = w.last_setpoint().unwrap();
    assert!(sp.z >= -5.0 - 1e-3, "climbs to d {}", sp.z);
}

#[test]
fn skill_goal_altitude_is_held_under_the_ceiling() {
    let mut w = World::new();
    w.p.settings.pilot.max_alt_agl_m = 6.0;
    w.airborne(copter::GUIDED);
    // Ground 4 m below the vehicle at d = -5 (rising terrain, inside the
    // rangefinder's height-reference band): the ceiling is at d = -7.
    w.agl = Some(4.0);
    let id = w.lease();
    w.goal_d(id, 0.0, 0.0, -30.0); // asks 30 m up
    w.run(400);
    let sp = w.last_setpoint().unwrap();
    assert!(sp.z >= -7.0 - 1e-3, "d {} above the 6 m ceiling", sp.z);
}

#[test]
fn skill_goal_altitude_is_held_over_the_floor() {
    let mut w = World::new();
    w.airborne(copter::GUIDED); // d = -5, 5 m above the ground
    let id = w.lease();
    w.goal_d(id, 0.0, 0.0, 3.0); // asks for a point under the ground
    w.run(400);
    let sp = w.last_setpoint().unwrap();
    assert!(sp.z <= -0.5 + 1e-3, "d {} under the floor", sp.z);
}

#[test]
fn a_vantage_goal_reports_enroute_then_arrived_to_the_holder() {
    use ados_protocol::pilot::VantageState;
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    // Clear grid: the vehicle already sees the prediction, so it is the
    // vantage point; still moving, it is on the way.
    w.vel = [1.0, 0.0, 0.0];
    w.hold_vantage(id, [7.0, 0.0], [0.0, 0.0], 400);
    let first = w.goal_status.first().expect("status").clone();
    assert_eq!(first.lease_id, id);
    assert_eq!(first.state, VantageState::Enroute);
    assert_eq!(first.vantage_ne, Some([w.pos[0], w.pos[1]]));
    w.vel = [0.0; 3];
    w.hold_vantage(id, [7.0, 0.0], [0.0, 0.0], 1000);
    let last = w.goal_status.last().unwrap();
    assert_eq!(last.state, VantageState::Arrived);
    assert!(last.coverage > 0.99);
}

#[test]
fn a_vantage_goal_faces_the_predicted_subject() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.hold_vantage(id, [0.0, 7.0], [0.0, 0.0], 600);
    let sp = w.last_setpoint().expect("a setpoint");
    assert!(
        (sp.yaw - std::f32::consts::FRAC_PI_2).abs() < 0.05,
        "yaw {}",
        sp.yaw
    );
    assert!(sp.vx.hypot(sp.vy) < 0.1, "no feed-forward: {sp:?}");
}

#[test]
fn a_vantage_goal_without_a_spec_holds_and_says_so() {
    use ados_protocol::pilot::{Goal, GoalKind, VantageState};
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    for _ in 0..3 {
        let goal = Goal::new(GoalKind::Vantage, [9.0, 0.0, -5.0], 2.0);
        w.req(Request::Goal { lease_id: id, goal });
        w.run(200);
    }
    assert_eq!(w.goal_status.last().unwrap().state, VantageState::NoVantage);
    assert!(w.goal_status.iter().all(|s| s.vantage_ne.is_none()));
}

#[test]
fn only_a_vantage_goal_publishes_goal_status() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    for _ in 0..3 {
        w.goal(id, 5.0, 0.0);
        w.run(200);
    }
    assert!(w.goal_status.is_empty());
}

/// Feed `scan` every tick for `ms`, re-sending a vantage goal for a
/// subject predicted at `pred` every 200 ms.
fn vantage_with_scan(
    w: &mut World,
    lease_id: u64,
    pred: [f32; 2],
    scan: &serde_json::Value,
    ms: u64,
) {
    let end = w.now + ms;
    while w.now < end {
        w.vantage_goal(lease_id, pred, [0.0, 0.0]);
        for _ in 0..10 {
            w.p.on_feed("plugin.world-engine.scan", scan, w.now);
            w.run(20);
        }
    }
}

/// A trunk hides the prediction; the vantage beside it is reached by a
/// straight, free way, so the plan is clear flight and the frame draws the
/// straight line to the point. Only the brake's clearance speed near the
/// trunk may read as avoiding, as on any goal; nothing holds.
#[test]
fn a_vantage_goal_on_a_free_straight_way_plans_straight() {
    use super::safety::trunk_scan;
    use ados_protocol::pilot::{AvoidState, VantageState};
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    vantage_with_scan(&mut w, id, [7.0, 0.0], &trunk_scan(4.0), 1000);
    let st = w.goal_status.last().unwrap();
    assert_eq!(st.state, VantageState::Enroute);
    let v = st.vantage_ne.expect("a vantage beside the trunk");
    assert!(v[1].abs() > 1.0, "{v:?}");
    let s = w.p.state(w.now);
    assert_ne!(s.avoid_state, AvoidState::Blocked);
    assert_eq!(s.hold_reason, None);
    assert_eq!(w.p.plan.state, AvoidState::Clear);
    assert_eq!(s.plan_ned.len(), 2, "{:?}", s.plan_ned);
    let end = s.plan_ned[1];
    let near = |a: f64, b: f32| (a - f64::from(b)).abs() < 0.05;
    assert!(near(end[0], v[0]) && near(end[1], v[1]), "{end:?} {v:?}");
}

/// The hard stop acts on a vantage goal as on any goal: an obstacle inside
/// the stop distance stops the ask and only pushes the vehicle back out.
#[test]
fn a_vantage_goal_holds_at_the_hard_stop() {
    use super::safety::wall_scan;
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    vantage_with_scan(&mut w, id, [10.0, 0.0], &wall_scan(0.8), 1000);
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("obstacle-close"));
    let sp = w.last_setpoint().unwrap();
    let max = crate::core::control::PUSH_MAX_MPS + 1e-3;
    assert!(
        (-max..0.05).contains(&sp.vx) && sp.vy.abs() < 0.05,
        "only the push back out: {sp:?}"
    );
}

/// No point inside the search bound sees the prediction behind a wall:
/// the vehicle holds, and the frame names why (`no-vantage`) rather than
/// claiming an obstacle blocks it, and draws no goal.
#[test]
fn a_vantage_goal_with_no_view_holds_as_no_vantage_not_blocked() {
    use super::safety::wall_scan;
    use ados_protocol::pilot::{AvoidState, Goal, GoalKind, VantageSpec, VantageState};
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    let end = w.now + 1000;
    while w.now < end {
        let mut goal = Goal::new(GoalKind::Vantage, [0.0, 0.0, -5.0], 2.0);
        // Subject 10 m north behind a wall 4 m ahead; the search may not
        // leave 0.5 m of where the recovery started.
        goal.vantage = Some(VantageSpec::new(
            [10.0, 0.0],
            [0.5, 0.0, 0.5],
            1.0,
            5.0,
            0.5,
            [0.0, 0.0],
        ));
        w.req(Request::Goal { lease_id: id, goal });
        for _ in 0..10 {
            w.p.on_feed("plugin.world-engine.scan", &wall_scan(4.0), w.now);
            w.run(20);
        }
    }
    assert_eq!(w.goal_status.last().unwrap().state, VantageState::NoVantage);
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("no-vantage"));
    assert_eq!(s.avoid_state, AvoidState::Clear);
    assert_eq!(s.goal_ned, None);
    assert!(w.last_setpoint().unwrap().vx.abs() < 0.05, "holds");
}

/// The link-loss failsafe counts a skill as flying: ten seconds after the
/// last ground heartbeat it flies the action (RTH) and the lease ends.
#[test]
fn link_loss_during_a_skill_lease_flies_the_action_and_revokes() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.req(Request::Hb { src: "mc".into() });
    let id = w.lease();
    let end = w.now + 10_200;
    while w.now < end && w.mode == copter::GUIDED {
        w.goal(id, 5.0, 0.0);
        w.run(200);
    }
    assert_eq!(w.mode, copter::RTL);
    assert_eq!(w.p.owner(), Owner::FcMode);
    assert!(w.p.auth.lease().is_none(), "the lease ended");
    assert!(w.revoked.iter().any(|(l, _)| *l == id), "{:?}", w.revoked);
}

/// Low battery under a skill lease: a 10 s countdown, then RTH, and the
/// lease ends.
#[test]
fn low_battery_rth_revokes_a_skill_lease_after_the_countdown() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    let id = w.lease();
    w.p.fc.battery_pct = Some(15.0);
    let start = w.now;
    while w.now < start + 9_000 {
        w.goal(id, 5.0, 0.0);
        w.run(200);
    }
    assert_eq!(w.mode, copter::GUIDED, "still counting down");
    assert_eq!(w.p.owner(), Owner::Skill);
    while w.now < start + 11_000 && w.mode == copter::GUIDED {
        w.goal(id, 5.0, 0.0);
        w.run(200);
    }
    assert_eq!(w.mode, copter::RTL);
    assert!(w.p.auth.lease().is_none(), "the lease ended");
    assert!(w.revoked.iter().any(|(l, _)| *l == id), "{:?}", w.revoked);
}
