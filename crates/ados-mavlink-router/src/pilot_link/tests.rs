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
    let (kept, gotos, dropped) = filter(&data, None);
    assert_eq!(dropped, 1);
    assert!(gotos.is_empty());
    assert_eq!(kept, frame(&heartbeat()));
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
    let (kept, gotos, _) = filter(&frame(&cmd), Some(GEO));
    assert!(kept.is_empty());
    let (p, yaw) = gotos[0];
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
