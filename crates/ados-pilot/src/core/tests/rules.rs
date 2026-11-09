use ados_protocol::pilot::{Owner, Request};

use super::harness::World;
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
    let text = include_str!("../../../../ados-control/tests/fixtures/obstacles-frame.json");
    let v: serde_json::Value = serde_json::from_str(text).unwrap();
    let o: ados_protocol::pilot::ObstaclesFrame = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&o).unwrap(), v);
}
