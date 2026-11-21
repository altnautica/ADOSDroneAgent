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

/// A pilot that lost the FC publishes `armed`/`landed` as nil: the vehicle
/// may still be flying, so its death still sets the loss mode, and a goto is
/// refused meanwhile.
#[test]
fn unknown_arm_state_keeps_the_pilot_flying() {
    let (tx, _rx) = mpsc::channel(4);
    let link = PilotLink::new(ControlMode::Ados, tx);
    link.on_state(&pilot_state("pilot", true, false, "multirotor"));
    assert!(link.goto_ok.load(Ordering::Relaxed));
    let rmpv::Value::Map(mut unlinked) = pilot_state("pilot", true, false, "multirotor") else {
        unreachable!()
    };
    for (k, v) in &mut unlinked {
        if matches!(k.as_str(), Some("armed" | "landed")) {
            *v = rmpv::Value::Nil;
        }
    }
    link.on_state(&rmpv::Value::Map(unlinked));
    assert!(!link.goto_ok.load(Ordering::Relaxed));
    let t = link.alive_ms.load(Ordering::Relaxed);
    assert_eq!(
        link.death_mode_at(t + DEATH_MS, || LossAction::Rth),
        Some(6),
        "still treated as flying"
    );
}

/// A bench take in lockstep with the simulator: the pilot published its
/// state at sim 59.84 s, then the simulator took 1.13 s of wall time to step
/// to 59.94 s. A router judging the silence on wall time set RTL 1.1 s after
/// that state and ended the follow; on the simulator's time the pilot was
/// silent for 100 ms. Its death is still found when sim time passes
/// without its state.
#[tokio::test(start_paused = true)]
async fn a_slow_simulator_step_is_not_a_dead_pilot() {
    let (clock, feed) = crate::clock::tests::sim_clock();
    let (tx, _rx) = mpsc::channel(4);
    let link = PilotLink::with_clock(ControlMode::Ados, tx, clock);
    let (tx, _rx) = mpsc::channel(4);
    let wall = PilotLink::new(ControlMode::Ados, tx);
    crate::clock::tests::advance(&feed, 1, 59_840);
    let flying = pilot_state("skill", true, false, "multirotor");
    link.on_state(&flying);
    wall.on_state(&flying);
    tokio::time::advance(Duration::from_millis(1_130)).await;
    assert_eq!(
        wall.death_mode(|| LossAction::Rth),
        Some(6),
        "on wall time the step reads as a dead pilot"
    );
    assert_eq!(link.death_mode(|| LossAction::Rth), None);
    assert!(link.gating(), "still the pilot's motion");
    crate::clock::tests::advance(&feed, 2, 59_940);
    link.on_state(&flying);
    // The pilot gone while the simulator runs on: a second of sim time.
    crate::clock::tests::advance(&feed, 3, 60_939);
    assert_eq!(link.death_mode(|| LossAction::Rth), None);
    crate::clock::tests::advance(&feed, 4, 60_940);
    assert_eq!(link.death_mode(|| LossAction::Rth), Some(6), "copter RTL");
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
    assert_eq!(
        ack_of(&link, r).1,
        MavResult::MAV_RESULT_TEMPORARILY_REJECTED
    );
    link.on_state(&pilot_state("tuning", true, false, "multirotor"));
    assert_eq!(
        ack_of(&link, r).1,
        MavResult::MAV_RESULT_TEMPORARILY_REJECTED
    );

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

/// `SET_MODE` as a ground station sends it: `base_mode` is the custom-mode
/// flag (1, or 129 armed), which the typed decoder refuses, so the frame is
/// built by hand. `v2` selects the MAVLink 2 header.
fn set_mode_frame(custom_mode: u32, base_mode: u8, v2: bool) -> Vec<u8> {
    let mut payload = custom_mode.to_le_bytes().to_vec();
    payload.extend([1, base_mode]);
    let mut f = if v2 {
        vec![0xFD, payload.len() as u8, 0, 0, 0, 255, 190, 11, 0, 0]
    } else {
        vec![0xFE, payload.len() as u8, 0, 255, 190, 11]
    };
    f.extend(payload);
    f.extend([0, 0]);
    f
}

fn command_long(command: MavCmd, param1: f32, param2: f32) -> MavMessage {
    MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
        param1,
        param2,
        param3: 0.0,
        param4: 0.0,
        param5: 0.0,
        param6: 0.0,
        param7: 0.0,
        command,
        target_system: 1,
        target_component: 1,
        confirmation: 0,
    })
}

fn modes_in(data: &[u8]) -> Vec<ModeCommand> {
    let mut v = Vec::new();
    mode_commands(data, |c| v.push(c));
    v
}

/// Every way a ground station asks the FC for a mode is seen, in one write
/// with other frames around it; a mode without the custom flag is not one.
#[test]
fn client_mode_commands_are_found_in_a_write() {
    let mut data = frame(&heartbeat());
    data.extend(set_mode_frame(6, 1, true));
    data.extend(set_mode_frame(9, 129, false));
    data.extend(set_mode_frame(5, 0, true));
    data.extend(frame(&command_long(MavCmd::MAV_CMD_DO_SET_MODE, 1.0, 6.0)));
    data.extend(frame(&command_long(MavCmd::MAV_CMD_DO_SET_MODE, 0.0, 6.0)));
    data.extend(frame(&command_long(
        MavCmd::MAV_CMD_NAV_RETURN_TO_LAUNCH,
        0.0,
        0.0,
    )));
    data.extend(frame(&command_long(MavCmd::MAV_CMD_NAV_LAND, 0.0, 0.0)));
    data.extend(frame(&reposition_long()));
    assert_eq!(
        modes_in(&data),
        vec![
            ModeCommand::Custom(6),
            ModeCommand::Custom(9),
            ModeCommand::Custom(6),
            ModeCommand::Rtl,
            ModeCommand::Land,
        ]
    );
}

/// The pilot hears each client mode command as the `client.mode` feed.
#[tokio::test]
async fn client_mode_commands_reach_the_pilot_as_a_feed() {
    let (tx, mut rx) = mpsc::channel(4);
    let link = PilotLink::new(ControlMode::FcManual, tx);
    let mut data = set_mode_frame(6, 1, true);
    data.extend(frame(&command_long(MavCmd::MAV_CMD_NAV_LAND, 0.0, 0.0)));
    link.note_client_modes(&data);
    for want in [serde_json::json!(6), serde_json::json!("land")] {
        match ados_protocol::pilot::decode_request(&rx.recv().await.unwrap()).unwrap() {
            ados_protocol::pilot::Request::Feed { topic, payload } => {
                assert_eq!(topic, ados_protocol::pilot::CLIENT_MODE_TOPIC);
                let p = serde_json::to_value(&payload).unwrap();
                assert_eq!(p["mode"], want);
            }
            other => panic!("not a feed: {other:?}"),
        }
    }
    assert!(rx.try_recv().is_err());
}

/// While the FC flies a failsafe RTL or land the pilot ignores a redirected
/// reposition, and the sender is told so instead of seeing it accepted.
#[test]
fn a_reposition_during_an_fc_failsafe_is_rejected() {
    let (tx, _rx) = mpsc::channel(4);
    let link = PilotLink::new(ControlMode::Ados, tx);
    let out = filter(&frame(&reposition_long()), Some(GEO));
    let r = out.repositions[0];
    let mut state = pilot_state("fc_mode", true, false, "multirotor");
    if let rmpv::Value::Map(m) = &mut state {
        m.push((rmpv::Value::from("fc_failsafe"), rmpv::Value::from(true)));
    }
    link.on_state(&state);
    assert_eq!(
        ack_of(&link, r).1,
        MavResult::MAV_RESULT_TEMPORARILY_REJECTED
    );
    // An RTL the operator chose is not a failsafe: the goto flies.
    link.on_state(&pilot_state("fc_mode", true, false, "multirotor"));
    assert_eq!(ack_of(&link, r).1, MavResult::MAV_RESULT_ACCEPTED);
}

/// The pilot's per-tick command, its stick command and the shadow planner's
/// command are onboard diagnostics: the radio downlink never carries them,
/// so they cost no aux frame space and never push the plan polyline out.
#[test]
fn the_downlink_drops_the_command_and_shadow_diagnostics() {
    let mut state = pilot_state("skill", true, false, "multirotor");
    if let rmpv::Value::Map(m) = &mut state {
        for key in ["command", "shadow", "stick"] {
            let body = rmpv::Value::Map(vec![(
                rmpv::Value::from("t_ms"),
                rmpv::Value::from(1_700_000_000_000u64),
            )]);
            m.push((rmpv::Value::from(key), body));
        }
    }
    let body = downlink_body(&state).expect("fits one frame");
    let back = rmpv::decode::read_value(&mut body.as_slice()).unwrap();
    let keys: Vec<&str> = back
        .as_map()
        .unwrap()
        .iter()
        .filter_map(|(k, _)| k.as_str())
        .collect();
    assert!(keys.contains(&"owner"), "{keys:?}");
    assert!(
        !keys.contains(&"command") && !keys.contains(&"shadow") && !keys.contains(&"stick"),
        "{keys:?}"
    );
}

/// A state that stays over one aux frame after every trim is refused with its
/// trimmed size, which the downlink logs and counts (`pilot_downlink`).
#[test]
fn an_oversized_state_is_refused_with_its_size() {
    let mut state = pilot_state("pilot", true, false, "multirotor");
    if let rmpv::Value::Map(m) = &mut state {
        m.push((
            rmpv::Value::from("hold_reason"),
            rmpv::Value::from("x".repeat(DOWNLINK_MAX_BODY)),
        ));
    }
    let bytes = downlink_body(&state).expect_err("cannot fit");
    assert!(bytes > DOWNLINK_MAX_BODY, "{bytes}");
    let stats = DownlinkStats::default();
    stats.oversize(bytes);
    stats.oversize(bytes);
    let snap = stats.snapshot();
    assert_eq!(snap["oversize_dropped"], 2);
    assert_eq!(snap["last_bytes"], bytes as u64);
    assert_eq!(snap["sent"], 0);
}

/// A state over one aux frame goes down as the fields a ground reads: the
/// onboard-only ones (`sensors`, the full profile row) stay behind before the
/// plan polyline or the readiness list is cut.
#[test]
fn an_oversized_state_goes_down_as_the_fields_a_ground_reads() {
    fn s(x: impl Into<rmpv::Value>) -> rmpv::Value {
        x.into()
    }
    let mut state = pilot_state("pilot", true, false, "multirotor");
    let plan: Vec<rmpv::Value> = (0..10)
        .map(|i| rmpv::Value::Array(vec![s(i as f64), s(-(i as f64))]))
        .collect();
    let mut limits = vec![(s("profile"), s("normal"))];
    for k in ["v_h_mps", "v_up_mps", "v_down_mps", "yaw_dps", "a_acc_mps2"] {
        limits.push((s(k), s(1.5f64)));
    }
    for k in ["jerk_mps3", "a_v_mps2", "expo_h", "stop_m", "margin_m"] {
        limits.push((s(k), s(1.5f64)));
    }
    if let rmpv::Value::Map(m) = &mut state {
        m.insert(0, (s("op"), s("state")));
        m.push((s("plan_ned"), rmpv::Value::Array(plan)));
        m.push((s("profile_limits"), rmpv::Value::Map(limits)));
        m.push((s("owner_label"), s("NOT READY · ".repeat(25))));
        m.push((s("sensors"), s("x".repeat(600))));
    }
    let mut full = Vec::new();
    rmpv::encode::write_value(&mut full, &state).unwrap();
    assert!(full.len() > DOWNLINK_MAX_BODY, "{}", full.len());

    let body = downlink_body(&state).expect("fits one frame once reduced");
    assert!(body.len() <= DOWNLINK_MAX_BODY);
    let back = rmpv::decode::read_value(&mut body.as_slice()).unwrap();
    let map = back.as_map().unwrap();
    let field = |name: &str| map.iter().find(|(k, _)| k.as_str() == Some(name));
    let text = |name: &str| field(name).and_then(|(_, v)| v.as_str());
    assert_eq!(text("op"), Some("state"));
    assert_eq!(text("owner"), Some("pilot"));
    assert!(field("sensors").is_none());
    let plan = field("plan_ned").and_then(|(_, v)| v.as_array());
    assert_eq!(plan.map(Vec::len), Some(10));
    let (_, limits) = field("profile_limits").unwrap();
    let limits = limits.as_map().unwrap();
    let keys: Vec<&str> = limits.iter().filter_map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["profile", "v_h_mps", "margin_m"]);
}
