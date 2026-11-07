use super::*;
use crate::pilot::{decode_distance, encode_distance, GoalKind};

#[test]
fn input_round_trips_through_encode_op() {
    let input = Input { src: "mc".into(), session: 7, seq: 42, t_ms: 1000, axes: [0.1, -0.5, 1.0, 0.0], buttons: 0b101 };
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
    let Request::Goal { lease_id, goal } = req else { panic!("not a goal") };
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
