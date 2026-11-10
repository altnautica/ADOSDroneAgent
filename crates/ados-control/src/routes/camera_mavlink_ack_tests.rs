//! Camera write ACK correlation: the answering component and resends.

use super::*;

/// A fake vehicle that lists two cameras, `CAM` and `CAM + 1`, and answers
/// every other command with an ACCEPTED ACK from `ack_from`, the component
/// that is not necessarily the one addressed.
fn start_vehicle(
    path: &std::path::Path,
    ack_from: u8,
) -> Arc<parking_lot::Mutex<Vec<COMMAND_LONG_DATA>>> {
    let listener = UnixListener::bind(path).unwrap();
    let commands = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let seen = commands.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (mut rd, mut wr) = stream.into_split();
            let seen = seen.clone();
            tokio::spawn(async move {
                loop {
                    let mut hdr = [0u8; 4];
                    if rd.read_exact(&mut hdr).await.is_err() {
                        return;
                    }
                    let mut body = vec![0u8; u32::from_be_bytes(hdr) as usize];
                    if rd.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    let Ok((_h, MavMessage::COMMAND_LONG(long))) = mavlink::parse_v2(&body) else {
                        continue;
                    };
                    seen.lock().push(long.clone());
                    let replies = if long.command == MavCmd::MAV_CMD_REQUEST_MESSAGE {
                        vec![
                            camera_information(SYS, CAM, photo_and_zoom()),
                            camera_information(SYS, CAM + 1, photo_and_zoom()),
                        ]
                    } else {
                        vec![ack_frame(
                            ack_from,
                            long.command,
                            MavResult::MAV_RESULT_ACCEPTED,
                        )]
                    };
                    for frame in replies {
                        if wr
                            .write_all(&encode_record(1, &frame).unwrap())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    let _ = wr.flush().await;
                }
            });
        }
    });
    commands
}

#[tokio::test]
async fn an_ack_from_another_camera_component_does_not_answer_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let _seen = start_vehicle(&sock, CAM + 1);
    let state = state_with_socket(dir.path(), sock);
    for resp in [
        capture(&state, CAM).await,
        zoom(&state, CAM, ZoomType::Step, 1.0).await,
    ] {
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_json(resp).await,
            json!({ "ok": false, "result": "TIMEOUT", "component_id": CAM })
        );
    }
    // The addressed component's own ACK still answers.
    assert_eq!(
        body_json(capture(&state, CAM + 1).await).await,
        json!({ "ok": true, "result": "ACCEPTED", "component_id": CAM + 1 })
    );
}

#[tokio::test]
async fn a_capture_is_never_resent_so_a_lost_ack_cannot_shoot_twice() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink.sock");
    let vehicle = FakeVehicle::start(&sock, vec![(CAM, photo_and_zoom())], None);
    let mut state = state_with_socket(dir.path(), sock);
    let mut resending = timing();
    resending.ack.retries = 2;
    state.mavlink_cameras = MavlinkCameras::with_timing(resending);
    let resp = capture(&state, CAM).await;
    assert_eq!(
        body_json(resp).await,
        json!({ "ok": false, "result": "TIMEOUT", "component_id": CAM })
    );
    let shots = vehicle
        .commands()
        .iter()
        .filter(|c| c.command == MavCmd::MAV_CMD_IMAGE_START_CAPTURE)
        .count();
    assert_eq!(shots, 1);
    // A zoom is idempotent and keeps the configured resends.
    let _ = zoom(&state, CAM, ZoomType::Step, 1.0).await;
    let zooms = vehicle
        .commands()
        .iter()
        .filter(|c| c.command == MavCmd::MAV_CMD_SET_CAMERA_ZOOM)
        .count();
    assert_eq!(zooms, 3);
}
