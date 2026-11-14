use super::*;
use crate::pilot::{decode_distance, encode_distance, GoalKind};

#[test]
fn input_round_trips_through_encode_op() {
    let input = Input {
        src: "mc".into(),
        session: 7,
        seq: 42,
        t_ms: 1000,
        axes: [0.1, -0.5, 1.0, 0.0],
        buttons: 0b101,
    };
    let body = encode_op("input", &input).unwrap();
    assert_eq!(decode_request(&body).unwrap(), Request::Input(input));
}

#[test]
fn json_goal_frame_decodes_with_defaults() {
    let frame = serde_json::json!({
        "op": "goal", "lease_id": 3,
        "goal": {"kind": "orbit", "n": 1.0, "e": 2.0, "d": -4.0, "max_speed_mps": 5.0,
                 "subject_ne": [3.0, 4.0], "margin_m": 2.0}
    });
    let (req, _) = json_to_request_body(&frame).unwrap();
    let Request::Goal { lease_id, goal } = req else {
        panic!("not a goal")
    };
    assert_eq!(lease_id, 3);
    assert_eq!(goal.kind, GoalKind::Orbit);
    assert_eq!(goal.vn, 0.0);
    assert_eq!(goal.yaw, None);
    assert_eq!(goal.margin_m, Some(2.0));
}

#[test]
fn unknown_and_setup_ops_pass_through() {
    let body = encode_op("setup.start", &serde_json::json!({"wizard": "ground"})).unwrap();
    match decode_request(&body).unwrap() {
        Request::Other { op, .. } => assert_eq!(op, "setup.start"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn missing_op_is_rejected() {
    let body = rmp_serde::to_vec_named(&serde_json::json!({"x": 1})).unwrap();
    assert!(matches!(decode_request(&body), Err(PilotWireError::NoOp)));
}

#[test]
fn malformed_input_names_the_op() {
    let body = encode_op("input", &serde_json::json!({"src": "mc"})).unwrap();
    match decode_request(&body) {
        Err(PilotWireError::Malformed { op, .. }) => assert_eq!(op, "input"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn avoid_state_wire_strings() {
    use crate::pilot::AvoidState;
    for (s, wire) in [
        (AvoidState::Clear, "clear"),
        (AvoidState::Detour, "detour"),
        (AvoidState::Blocked, "blocked"),
        (AvoidState::Off, "off"),
        (AvoidState::Stale, "stale"),
        (AvoidState::Unavailable, "unavailable"),
    ] {
        assert_eq!(serde_json::to_value(s).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<AvoidState>(serde_json::json!(wire)).unwrap(),
            s
        );
    }
}

#[test]
fn distance_code_covers_the_range() {
    assert_eq!(decode_distance(0), None);
    assert!((decode_distance(1).unwrap() - 0.2).abs() < 1e-4);
    assert!((decode_distance(255).unwrap() - 60.0).abs() < 0.05);
    for d in [0.3f32, 1.0, 2.0, 5.0, 12.0, 40.0] {
        let back = decode_distance(encode_distance(d)).unwrap();
        assert!((back - d).abs() / d < 0.02, "{d} -> {back}");
    }
}

#[tokio::test]
async fn framing_round_trips() {
    let (mut a, mut b) = tokio::io::duplex(1024);
    let body = encode_op("subscribe", &()).unwrap();
    write_msg(&mut a, &body).await.unwrap();
    drop(a);
    let got = read_msg(&mut b).await.unwrap().unwrap();
    assert_eq!(decode_request(&got).unwrap(), Request::Subscribe);
    assert!(read_msg(&mut b).await.unwrap().is_none());
}

fn decode_json(v: serde_json::Value) -> Request {
    json_to_request_body(&v).unwrap().0
}

#[test]
fn every_client_op_decodes() {
    use serde_json::json;
    assert_eq!(
        decode_json(json!({"op": "release", "src": "mc", "session": 4})),
        Request::Release {
            src: "mc".into(),
            session: 4
        }
    );
    assert_eq!(
        decode_json(json!({"op": "session.take", "src": "android", "session": 9})),
        Request::SessionTake {
            src: "android".into(),
            session: 9
        }
    );
    assert_eq!(
        decode_json(json!({"op": "hb", "src": "kiosk"})),
        Request::Hb {
            src: "kiosk".into()
        }
    );
    match decode_json(json!({"op": "lease.acquire", "holder": {"plugin_id": "p", "skill_id": "s"}}))
    {
        Request::LeaseAcquire { kind, .. } => assert_eq!(kind, "motion"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        decode_json(json!({"op": "lease.release", "lease_id": 3})),
        Request::LeaseRelease { lease_id: 3 }
    );
    assert_eq!(
        decode_json(json!({"op": "goto", "n": 1.0, "e": 2.0, "d": -3.0})),
        Request::Goto {
            n: 1.0,
            e: 2.0,
            d: -3.0,
            yaw: None
        }
    );
    assert_eq!(
        decode_json(json!({"op": "swarm.goal", "vn": 1.0, "ve": 0.5, "vd": 0.0, "yaw": 1.5})),
        Request::SwarmGoal {
            vn: 1.0,
            ve: 0.5,
            vd: 0.0,
            yaw: Some(1.5)
        }
    );
    match decode_json(
        json!({"op": "view_demand", "cameras": {"main": ["obstacles"]},
        "models": {"main": {"frame_w": 1280, "frame_h": 720, "hfov_deg": 80.0, "vfov_deg": 50.0,
                            "tilt_down_deg": 10.0, "yaw_deg": 0.0}}}),
    ) {
        Request::ViewDemand(v) => {
            assert_eq!(v.cameras["main"], vec!["obstacles".to_owned()]);
            assert_eq!(v.models["main"].hfov_deg, 80.0);
        }
        other => panic!("{other:?}"),
    }
    match decode_json(json!({"op": "feed", "topic": "t", "payload": {"a": 1}})) {
        Request::Feed { topic, .. } => assert_eq!(topic, "t"),
        other => panic!("{other:?}"),
    }
    match decode_json(
        json!({"op": "action", "name": "launch", "src": "mc", "id": "x1",
        "args": {"after": "hold", "alt_m": 3}}),
    ) {
        Request::Action(a) => {
            assert_eq!(a.id.as_deref(), Some("x1"));
            assert_eq!(a.args.unwrap()["after"], "hold");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn string_session_is_rejected() {
    let frame = serde_json::json!({"op": "input", "src": "mc", "session": "abc", "seq": 1, "t_ms": 1,
        "axes": [0.0, 0.0, 0.0, 0.0], "buttons": 0});
    assert!(matches!(
        json_to_request_body(&frame),
        Err(PilotWireError::Malformed { .. })
    ));
}

#[tokio::test]
async fn zero_length_and_oversized_frames_are_errors() {
    let zero = 0u32.to_be_bytes();
    assert!(read_msg(&mut &zero[..]).await.is_err());
    let big = (PILOT_MAX_FRAME as u32 + 1).to_be_bytes();
    assert!(read_msg(&mut &big[..]).await.is_err());
    let truncated = [0u8, 0, 0, 8, 1, 2];
    assert!(read_msg(&mut &truncated[..]).await.is_err());
}

#[test]
fn wire_floats_are_short_decimals() {
    let v = serde_json::to_value(crate::pilot::wire(2.4)).unwrap();
    assert_eq!(v.to_string(), "2.4");
}

#[test]
fn the_registry_version_records_the_nullable_fc_state() {
    // `state.armed`, `state.landed` (v2), `state.fc_mode` and
    // `state.fc_mode_name` (v3) are null while the FC link is down: changes
    // of the state message's shape, so the contract version moved with them
    // and the registry describes them.
    assert_eq!(crate::pilot::PILOT_V1_VERSION, 3);
    let meta = crate::contracts::CONTRACTS
        .iter()
        .find(|c| c.id == "pilot.v1")
        .expect("pilot.v1 is registered");
    for text in [
        "armed and landed are null",
        "fc_mode and fc_mode_name are null",
    ] {
        assert!(meta.description.contains(text), "{}", meta.description);
    }
}

#[test]
fn speed_cap_is_omitted_until_set() {
    let mut s = crate::pilot::PilotState::default();
    let v = serde_json::to_value(&s).unwrap();
    assert!(v.get("speed_cap").is_none(), "{v}");
    s.speed_cap = Some(crate::pilot::SpeedCap::new("gnss-denied", 2.0));
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(
        v["speed_cap"],
        serde_json::json!({"reason": "gnss-denied", "v_h_mps": 2.0})
    );
    let back: crate::pilot::PilotState = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
}

#[test]
fn sensors_serialise_with_the_contract_shape() {
    use crate::pilot::{
        FlowState, LidarDriverHealth, LidarState, PilotState, RangeState, SensorHealth,
    };
    let s = PilotState::default();
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v.get("sensors"), Some(&serde_json::Value::Null));
    let mut h = SensorHealth::default();
    h.flow.state = FlowState::Low;
    h.flow.quality = Some(90);
    h.flow.rate_hz = Some(20.0);
    h.flow.age_ms = Some(40);
    h.range.state = RangeState::OutOfRange;
    h.range.age_ms = Some(30);
    h.lidar.state = LidarState::Degraded;
    h.lidar.rate_hz = Some(6.5);
    h.lidar.health = Some(LidarDriverHealth::Warning);
    h.lidar.age_ms = Some(100);
    let s = PilotState {
        sensors: Some(h),
        ..PilotState::default()
    };
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(
        v["sensors"],
        serde_json::json!({
            "flow": {"state": "low", "quality": 90, "rate_hz": 20.0, "age_ms": 40},
            "range": {"state": "out_of_range", "height_m": null, "age_ms": 30},
            "lidar": {"state": "degraded", "rate_hz": 6.5, "health": "warning", "age_ms": 100}
        })
    );
    let absent = serde_json::to_value(SensorHealth::default()).unwrap();
    assert_eq!(
        absent,
        serde_json::json!({
            "flow": {"state": "absent", "quality": null, "rate_hz": null, "age_ms": null},
            "range": {"state": "absent", "height_m": null, "age_ms": null},
            "lidar": {"state": "absent", "rate_hz": null, "health": null, "age_ms": null}
        })
    );
    let back: PilotState = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
}

#[test]
fn a_vantage_goal_decodes_with_its_spec_and_hypotheses() {
    let frame = serde_json::json!({
        "op": "goal", "lease_id": 4,
        "goal": {"kind": "vantage", "n": 0.0, "e": 0.0, "d": -5.0, "max_speed_mps": 2.0,
                 "vantage": {"predicted_ne": [7.0, 1.0], "cov_ne": [0.5, 0.0, 0.5],
                             "sigma_k": 1.0, "standoff_m": 5.0, "max_radius_m": 12.0,
                             "anchor_ne": [0.0, 0.0], "hypotheses_ne": [[3.0, 0.0]]}}
    });
    let (req, _) = json_to_request_body(&frame).unwrap();
    let Request::Goal { goal, .. } = req else {
        panic!("not a goal")
    };
    assert_eq!(goal.kind, GoalKind::Vantage);
    let v = goal.vantage.expect("vantage spec");
    assert_eq!(v.predicted_ne, [7.0, 1.0]);
    assert_eq!(v.hypotheses_ne, vec![[3.0, 0.0]]);
}

#[test]
fn a_vantage_spec_without_hypotheses_decodes_empty() {
    let v: crate::pilot::VantageSpec = serde_json::from_value(serde_json::json!({
        "predicted_ne": [1.0, 2.0], "cov_ne": [0.1, 0.0, 0.1], "sigma_k": 2.0,
        "standoff_m": 4.0, "max_radius_m": 10.0, "anchor_ne": [0.0, 0.0]
    }))
    .unwrap();
    assert!(v.hypotheses_ne.is_empty());
}

#[test]
fn goal_status_states_are_kebab_case_on_the_wire() {
    let mut s = GoalStatus::new(9, VantageState::NoVantage, 0.0);
    s.occluder_width_m = Some(2.5);
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["state"], "no-vantage");
    assert_eq!(v["lease_id"], 9);
    let body = encode_op("goal.status", &s).unwrap();
    let (op, _) = peek_op(&body).unwrap();
    assert_eq!(op, "goal.status");
    let back: GoalStatus = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
}
