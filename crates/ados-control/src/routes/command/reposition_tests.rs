//! `reposition`: the frame, its argument checks and the failsafe refusal.

use super::*;

#[test]
fn reposition_is_a_command_int_do_reposition_at_full_precision() {
    let (c, body) =
        reposition_command(&[json!(12.971_598_7), json!(-77.594_562_3), json!(25.5)]).unwrap();
    let c = addressed_int(c, target());
    assert_eq!(c.command, MavCmd::MAV_CMD_DO_REPOSITION);
    assert_eq!(c.frame, MavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT);
    assert_eq!(
        (c.param1, c.param2, c.param3),
        (-1.0, 0.0, 0.0),
        "default speed, no mode change"
    );
    assert!(c.param4.is_nan(), "yaw is kept");
    assert_eq!((c.x, c.y, c.z), (129_715_987, -775_945_623, 25.5));
    assert_eq!((c.target_system, c.target_component), target());
    assert_eq!(body["cmd"], json!("reposition"));
    // A numeric string reads the same as a number.
    let (s, _) =
        reposition_command(&[json!("12.9715987"), json!("-77.5945623"), json!("25.5")]).unwrap();
    assert_eq!((s.x, s.y, s.z), (c.x, c.y, c.z));
    // The frame round-trips through the v2 encoding the send path uses.
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        sequence: 0,
    };
    let frame = mavlink::serialize_v2(header, &c.message(1)).unwrap();
    let (_h, back) = mavlink::parse_v2(&frame).unwrap();
    let MavMessage::COMMAND_INT(back) = back else {
        panic!("expected COMMAND_INT");
    };
    assert_eq!(
        (back.x, back.y, back.command),
        (c.x, c.y, MavCmd::MAV_CMD_DO_REPOSITION)
    );
}

#[test]
fn reposition_refuses_every_unflyable_point() {
    let ok = [json!(12.97), json!(77.59), json!(20)];
    for (i, bad) in [
        (0, json!("nan")),
        (0, json!("inf")),
        (0, json!(90.5)),
        (0, json!(-91)),
        (1, json!(180.01)),
        (1, json!(-181)),
        (1, json!(null)),
        (2, json!(0.5)),
        (2, json!(121)),
        (2, json!(-5)),
        (2, json!("high")),
        (2, json!(1e39)),
    ] {
        let mut args = ok.to_vec();
        args[i] = bad.clone();
        let err = reposition_command(&args).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST, "arg {i} = {bad}");
    }
    assert_eq!(
        reposition_command(&ok[..2]).unwrap_err().status,
        StatusCode::BAD_REQUEST
    );
    // The spec's launch range bounds it, both ends included.
    for alt in [1, 120] {
        assert!(
            reposition_command(&[json!(12.97), json!(77.59), json!(alt)]).is_ok(),
            "{alt} m"
        );
    }
}

#[tokio::test]
async fn reposition_reaches_the_latched_fc_as_a_command_int() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.unwrap();
        let (mut rd, mut wr) = stream.into_split();
        let mut hdr = [0u8; 4];
        rd.read_exact(&mut hdr).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(hdr) as usize];
        rd.read_exact(&mut body).await.unwrap();
        let ack = ack_frame(
            7,
            MavCmd::MAV_CMD_DO_REPOSITION,
            MavResult::MAV_RESULT_ACCEPTED,
        );
        wr.write_all(&encode_record(1, &ack).unwrap())
            .await
            .unwrap();
        let _ = wr.flush().await;
        tokio::spawn(async move {
            let mut sink = [0u8; 256];
            while rd.read(&mut sink).await.is_ok_and(|n| n > 0) {}
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        body
    });

    let state = state_with_socket(dir.path(), path);
    state.state.set_snapshot_for_test(json!({
        "fc_connected": true,
        "autopilot": 3,
        "fc_system_id": 7,
        "fc_component_id": 1,
        "mode": "GUIDED",
    }));
    let resp = execute_command(
        State(state),
        Json(CommandRequest {
            cmd: "Reposition".to_string(),
            args: vec![json!(47.397_742), json!(8.545_594), json!(30)],
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (_h, decoded) = mavlink::parse_v2(&server.await.unwrap()).unwrap();
    let MavMessage::COMMAND_INT(sent) = decoded else {
        panic!("expected COMMAND_INT on the socket");
    };
    assert_eq!((sent.target_system, sent.target_component), (7, 1));
    assert_eq!((sent.x, sent.y, sent.z), (473_977_420, 85_455_940, 30.0));
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["status"], json!("ok"), "{body}");
    assert_eq!(body["ack"]["accepted"], json!(true), "{body}");
}

#[tokio::test]
async fn reposition_refuses_a_bad_point_before_any_send() {
    let dir = tempfile::tempdir().unwrap();
    // No socket at all: a refusal must come before the send would 503.
    let state = state_with_socket(dir.path(), dir.path().join("absent.sock"));
    state
        .state
        .set_snapshot_for_test(json!({"fc_connected": true, "autopilot": 3, "mode": "GUIDED"}));
    let resp = execute_command(
        State(state),
        Json(CommandRequest {
            cmd: "reposition".to_string(),
            args: vec![json!(95.0), json!(8.5), json!(30)],
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn reposition_refuses_the_null_point() {
    let err = reposition_command(&[json!(0), json!(0), json!(20)]).unwrap_err();
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
    // A point on one zero line is a real place.
    assert!(reposition_command(&[json!(0), json!(77.59), json!(20)]).is_ok());
    assert!(reposition_command(&[json!(12.97), json!(0), json!(20)]).is_ok());
}

/// A fake FC that records the first command it reads and ACCEPTS a reposition.
fn reposition_fc(listener: UnixListener) -> tokio::task::JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.unwrap();
        let (mut rd, mut wr) = stream.into_split();
        let mut hdr = [0u8; 4];
        rd.read_exact(&mut hdr).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(hdr) as usize];
        rd.read_exact(&mut body).await.unwrap();
        let ack = ack_frame(
            7,
            MavCmd::MAV_CMD_DO_REPOSITION,
            MavResult::MAV_RESULT_ACCEPTED,
        );
        wr.write_all(&encode_record(1, &ack).unwrap())
            .await
            .unwrap();
        let _ = wr.flush().await;
        tokio::spawn(async move {
            let mut sink = [0u8; 256];
            while rd.read(&mut sink).await.is_ok_and(|n| n > 0) {}
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        body
    })
}

async fn reposition_in(state: AppState) -> Response {
    execute_command(
        State(state),
        Json(CommandRequest {
            cmd: "reposition".to_string(),
            args: vec![json!(47.397_742), json!(8.545_594), json!(30)],
        }),
    )
    .await
}

async fn json_of(resp: Response) -> Value {
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn reposition_is_refused_while_a_fresh_pilot_reports_a_failsafe() {
    let dir = tempfile::tempdir().unwrap();
    // No socket: a refusal must come before the send would 503.
    let state = state_with_socket(dir.path(), dir.path().join("absent.sock"));
    state
        .state
        .set_snapshot_for_test(json!({"fc_connected": true, "autopilot": 3, "mode": "GUIDED"}));
    state.nav.ingest_host(
        crate::routes::nav::NavKind::Pilot,
        1,
        json!({"control_mode": "fc", "fc_failsafe": true}),
    );
    let resp = reposition_in(state).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json_of(resp).await["code"], json!("failsafe"));
}

#[tokio::test]
async fn reposition_is_refused_while_the_fc_returns_or_lands() {
    // Every return and landing mode the cockpit's shared list names, the
    // Copter battery failsafe's AUTO_RTL and Plane's LOITER_ALT_QLAND included.
    for mode in [
        "RTL",
        "LAND",
        "SMART_RTL",
        "QRTL",
        "QLAND",
        "AUTO_RTL",
        "LOITER_ALT_QLAND",
        "AUTO.RTL",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_socket(dir.path(), dir.path().join("absent.sock"));
        state
            .state
            .set_snapshot_for_test(json!({"fc_connected": true, "autopilot": 3, "mode": mode}));
        let resp = reposition_in(state).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{mode}");
        assert_eq!(json_of(resp).await["code"], json!("failsafe"), "{mode}");
    }
}

#[tokio::test]
async fn reposition_in_guided_is_sent_without_a_mode_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let server = reposition_fc(UnixListener::bind(&path).unwrap());
    let state = state_with_socket(dir.path(), path);
    state.state.set_snapshot_for_test(json!({
        "fc_connected": true,
        "autopilot": 3,
        "fc_system_id": 7,
        "fc_component_id": 1,
        "mode": "GUIDED",
    }));
    // A pilot frame without a failsafe does not refuse.
    state.nav.ingest_host(
        crate::routes::nav::NavKind::Pilot,
        1,
        json!({"control_mode": "fc", "fc_failsafe": false}),
    );
    let resp = reposition_in(state).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json_of(resp).await["ack"]["accepted"], json!(true));
    let (_h, decoded) = mavlink::parse_v2(&server.await.unwrap()).unwrap();
    let MavMessage::COMMAND_INT(sent) = decoded else {
        panic!("expected COMMAND_INT on the socket");
    };
    assert_eq!(sent.param2, 0.0);
}

#[tokio::test]
async fn reposition_is_refused_while_the_heartbeat_reports_critical_or_emergency() {
    for system_status in [5, 6] {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_socket(dir.path(), dir.path().join("absent.sock"));
        state.state.set_snapshot_for_test(json!({
            "fc_connected": true,
            "autopilot": 3,
            "mode": "GUIDED",
            "system_status": system_status,
        }));
        let resp = reposition_in(state).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{system_status}");
        assert_eq!(
            json_of(resp).await["code"],
            json!("failsafe"),
            "{system_status}"
        );
    }
}

#[tokio::test]
async fn reposition_is_refused_outside_ardupilot_guided() {
    // (autopilot, mode): ArduPilot in a non-guided mode, no mode yet, and PX4,
    // which this route never repositions.
    for (autopilot, mode) in [
        (3, json!("LOITER")),
        (3, json!("AUTO")),
        (3, json!("STABILIZE")),
        (3, Value::Null),
        (12, json!("POSCTL")),
        (12, json!("AUTO.LOITER")),
        (12, json!("GUIDED")),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_socket(dir.path(), dir.path().join("absent.sock"));
        state.state.set_snapshot_for_test(
            json!({"fc_connected": true, "autopilot": autopilot, "mode": mode}),
        );
        let resp = reposition_in(state).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{autopilot} {mode}");
        let body = json_of(resp).await;
        assert_eq!(body["code"], json!("bad_state"), "{autopilot} {mode}");
        assert!(body["detail"].is_string());
    }
}
