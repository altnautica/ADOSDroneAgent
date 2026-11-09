use super::*;
use ados_protocol::mavlink::ardupilotmega::*;
use ados_protocol::mavlink::{serialize_v2, MavHeader};

fn frame(msg: &MavMessage) -> Vec<u8> {
    serialize_v2(
        MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 0,
        },
        msg,
    )
    .unwrap()
}

fn manual_control() -> MavMessage {
    MavMessage::MANUAL_CONTROL(MANUAL_CONTROL_DATA {
        x: 0,
        y: 0,
        z: 500,
        r: 0,
        buttons: 0,
        target: 1,
    })
}

fn heartbeat() -> MavMessage {
    MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_GCS,
        autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_ACTIVE,
        mavlink_version: 3,
    })
}

const GEO: GeoRef = GeoRef {
    lat: 47.0,
    lon: 8.0,
    alt_rel: 5.0,
    local: [10.0, 20.0, -5.0],
};

#[test]
fn sticks_are_dropped_and_other_frames_kept() {
    let mut data = frame(&manual_control());
    data.extend(frame(&heartbeat()));
    let out = filter(&data, None);
    assert_eq!(out.dropped, 1);
    assert!(out.gotos.is_empty());
    assert!(out.repositions.is_empty());
    assert_eq!(out.kept, frame(&heartbeat()));
}

#[test]
fn reposition_becomes_a_local_goto() {
    let cmd = MavMessage::COMMAND_INT(COMMAND_INT_DATA {
        param1: -1.0,
        param2: 0.0,
        param3: 0.0,
        param4: f32::NAN,
        x: 470_001_000,
        y: 80_000_000,
        z: 15.0,
        command: MavCmd::MAV_CMD_DO_REPOSITION,
        target_system: 1,
        target_component: 1,
        frame: MavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT,
        current: 0,
        autocontinue: 0,
    });
    let out = filter(&frame(&cmd), Some(GEO));
    assert!(out.kept.is_empty());
    assert_eq!(
        out.repositions,
        vec![Reposition {
            sys: 255,
            comp: 190,
            redirected: true
        }]
    );
    let (p, yaw) = out.gotos[0];
    assert!((p[0] - (10.0 + 11.13)).abs() < 0.05, "north {}", p[0]);
    assert!((p[1] - 20.0).abs() < 0.01);
    assert!((p[2] - -15.0).abs() < 0.01);
    assert_eq!(yaw, None);
}

#[test]
fn global_target_without_georef_is_dropped() {
    let cmd = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
        param1: 0.0,
        param2: 0.0,
        param3: 0.0,
        param4: 0.0,
        param5: 47.0,
        param6: 8.0,
        param7: 10.0,
        command: MavCmd::MAV_CMD_DO_REPOSITION,
        target_system: 1,
        target_component: 1,
        confirmation: 0,
    });
    assert_eq!(classify(76, Some(&cmd), None), Verdict::Drop);
}

#[test]
fn local_position_target_redirects_and_velocity_only_drops() {
    let sp = |mask: u16| {
        MavMessage::SET_POSITION_TARGET_LOCAL_NED(SET_POSITION_TARGET_LOCAL_NED_DATA {
            time_boot_ms: 0,
            x: 3.0,
            y: 4.0,
            z: -6.0,
            vx: 1.0,
            vy: 0.0,
            vz: 0.0,
            afx: 0.0,
            afy: 0.0,
            afz: 0.0,
            yaw: 1.0,
            yaw_rate: 0.0,
            type_mask: PositionTargetTypemask::from_bits_truncate(mask),
            target_system: 1,
            target_component: 1,
            coordinate_frame: MavFrame::MAV_FRAME_LOCAL_NED,
        })
    };
    assert_eq!(
        classify(84, Some(&sp(2496)), None),
        Verdict::Goto([3.0, 4.0, -6.0], Some(1.0))
    );
    assert_eq!(classify(84, Some(&sp(1479)), None), Verdict::Drop);
}

#[test]
fn gating_needs_ados_mode_and_a_live_pilot() {
    let (tx, _rx) = mpsc::channel(4);
    let link = PilotLink::new(ControlMode::Ados, tx);
    assert!(!link.gating(), "no pilot state yet");
    let state = rmpv::Value::Map(vec![(
        rmpv::Value::from("control_mode"),
        rmpv::Value::from("ados"),
    )]);
    link.on_state(&state);
    assert!(link.gating());
    let state = rmpv::Value::Map(vec![(
        rmpv::Value::from("control_mode"),
        rmpv::Value::from("fc_manual"),
    )]);
    link.on_state(&state);
    assert!(!link.gating(), "fc_manual passes sticks through");
}

fn pilot_state(owner: &str, armed: bool, landed: bool, class: &str) -> rmpv::Value {
    rmpv::Value::Map(
        [
            ("control_mode", rmpv::Value::from("ados")),
            ("owner", rmpv::Value::from(owner)),
            ("armed", rmpv::Value::from(armed)),
            ("landed", rmpv::Value::from(landed)),
            ("vehicle_class", rmpv::Value::from(class)),
        ]
        .into_iter()
        .map(|(k, v)| (rmpv::Value::from(k), v))
        .collect(),
    )
}

#[test]
fn pilot_death_while_flying_sets_the_loss_mode_once() {
    let (tx, _rx) = mpsc::channel(4);
    let link = PilotLink::new(ControlMode::Ados, tx);
    assert_eq!(
        link.death_mode_at(10_000, || LossAction::Rth),
        None,
        "never heard"
    );
    link.on_state(&pilot_state("pilot", true, false, "multirotor"));
    let t = link.alive_ms.load(Ordering::Relaxed);
    assert_eq!(
        link.death_mode_at(t + DEATH_MS - 1, || LossAction::Rth),
        None
    );
    assert_eq!(
        link.death_mode_at(t + DEATH_MS, || LossAction::Rth),
        Some(6),
        "copter RTL"
    );
    assert_eq!(
        link.death_mode_at(t + DEATH_MS + 500, || LossAction::Rth),
        None,
        "fires once"
    );
    // The pilot back and flying again re-arms it; the configured action decides the mode.
    link.on_state(&pilot_state("skill", true, false, "multirotor"));
    let t = link.alive_ms.load(Ordering::Relaxed);
    assert_eq!(
        link.death_mode_at(t + DEATH_MS, || LossAction::Land),
        Some(9)
    );
    link.on_state(&pilot_state("goto", true, false, "fixedwing"));
    let t = link.alive_ms.load(Ordering::Relaxed);
    assert_eq!(
        link.death_mode_at(t + DEATH_MS, || LossAction::Hold),
        Some(12)
    );
    link.on_state(&pilot_state("pilot", true, false, "vtol"));
    let t = link.alive_ms.load(Ordering::Relaxed);
    assert_eq!(
        link.death_mode_at(t + DEATH_MS, || LossAction::Land),
        Some(20)
    );
}

#[test]
fn pilot_death_needs_the_pilot_flying() {
    let (tx, _rx) = mpsc::channel(4);
    let link = PilotLink::new(ControlMode::Ados, tx);
    for s in [
        pilot_state("fc_mode", true, false, "multirotor"),
        pilot_state("mission_fc", true, false, "multirotor"),
        pilot_state("pilot", false, true, "multirotor"),
        pilot_state("pilot", true, true, "multirotor"),
    ] {
        link.on_state(&s);
        let t = link.alive_ms.load(Ordering::Relaxed);
        assert_eq!(
            link.death_mode_at(t + 5 * DEATH_MS, || LossAction::Rth),
            None,
            "{s}"
        );
    }
    let manual = rmpv::Value::Map(vec![
        (
            rmpv::Value::from("control_mode"),
            rmpv::Value::from("fc_manual"),
        ),
        (rmpv::Value::from("owner"), rmpv::Value::from("pilot")),
        (rmpv::Value::from("armed"), rmpv::Value::from(true)),
        (rmpv::Value::from("landed"), rmpv::Value::from(false)),
    ]);
    link.on_state(&manual);
    let t = link.alive_ms.load(Ordering::Relaxed);
    assert_eq!(
        link.death_mode_at(t + 5 * DEATH_MS, || LossAction::Rth),
        None,
        "fc_manual"
    );
}

#[tokio::test]
async fn gotos_and_swarm_goals_reach_the_pilot_socket() {
    let (tx, mut rx) = mpsc::channel(4);
    let link = PilotLink::new(ControlMode::Ados, tx);
    link.send_goto([1.0, 2.0, -3.0], None);
    link.send_swarm([0.5, 0.0, 0.0], Some(1.0));
    let goto = ados_protocol::pilot::decode_request(&rx.recv().await.unwrap()).unwrap();
    assert_eq!(
        goto,
        ados_protocol::pilot::Request::Goto {
            n: 1.0,
            e: 2.0,
            d: -3.0,
            yaw: None
        }
    );
    let swarm = ados_protocol::pilot::decode_request(&rx.recv().await.unwrap()).unwrap();
    assert_eq!(
        swarm,
        ados_protocol::pilot::Request::SwarmGoal {
            vn: 0.5,
            ve: 0.0,
            vd: 0.0,
            yaw: Some(1.0)
        }
    );
}

fn reposition_long() -> MavMessage {
    MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
        param1: -1.0,
        param2: 0.0,
        param3: 0.0,
        param4: f32::NAN,
        param5: 47.0001,
        param6: 8.0,
        param7: 10.0,
        command: MavCmd::MAV_CMD_DO_REPOSITION,
        target_system: 1,
        target_component: 1,
        confirmation: 0,
    })
}

fn ack_of(link: &PilotLink, r: Reposition) -> (MavHeader, MavResult) {
    let bytes = link.reposition_ack(r, 1).unwrap();
    match ados_protocol::mavlink::parse_any(&bytes).unwrap() {
        (h, MavMessage::COMMAND_ACK(a)) => {
            assert_eq!(a.command, MavCmd::MAV_CMD_DO_REPOSITION);
            (h, a.result)
        }
        other => panic!("not an ack: {other:?}"),
    }
}

/// A reposition the router takes off the FC stream gets an answer with its
/// real outcome, so the sender never times out while the aircraft moves.
#[test]
fn an_intercepted_reposition_is_answered_with_its_outcome() {
    let (tx, _rx) = mpsc::channel(4);
    let link = PilotLink::new(ControlMode::Ados, tx);
    let out = filter(&frame(&reposition_long()), Some(GEO));
    assert_eq!(out.gotos.len(), 1);
    let r = out.repositions[0];

    // The pilot is flying a mission: the goto takes over, accepted.
    link.on_state(&pilot_state("mission_fc", true, false, "multirotor"));
    let (h, res) = ack_of(&link, r);
    assert_eq!((h.system_id, h.component_id), (1, 1), "from the FC");
    assert_eq!(res, MavResult::MAV_RESULT_ACCEPTED);
    link.on_state(&pilot_state("skill", true, false, "multirotor"));
    assert_eq!(ack_of(&link, r).1, MavResult::MAV_RESULT_ACCEPTED);

    // On the ground or tuning the pilot ignores a goto: say so.
    link.on_state(&pilot_state("idle", true, true, "multirotor"));
    assert_eq!(ack_of(&link, r).1, MavResult::MAV_RESULT_TEMPORARILY_REJECTED);
    link.on_state(&pilot_state("tuning", true, false, "multirotor"));
    assert_eq!(ack_of(&link, r).1, MavResult::MAV_RESULT_TEMPORARILY_REJECTED);

    // No position to place the target: dropped, and the ack says it failed.
    let out = filter(&frame(&reposition_long()), None);
    assert!(out.gotos.is_empty() && out.kept.is_empty());
    assert!(!out.repositions[0].redirected);
    link.on_state(&pilot_state("pilot", true, false, "multirotor"));
    assert_eq!(
        ack_of(&link, out.repositions[0]).1,
        MavResult::MAV_RESULT_FAILED
    );
}
