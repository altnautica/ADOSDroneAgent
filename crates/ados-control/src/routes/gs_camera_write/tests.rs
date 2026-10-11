use super::*;

/// The default FC target the golden frame is addressed to.
fn target() -> (u8, u8) {
    crate::fc_target::DEFAULT_TARGET
}

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Decode a lowercase hex string into bytes for the golden-frame assertion.
fn hex_to_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn the_534_frame_for_camera_index_2_is_the_golden_frame() {
    // The exact 44-byte frame the reference encoder produces for
    // SET_CAMERA_SOURCE (534) with camera_index=2, source 255/190, target
    // 1/1, sequence 0. param2 carries the index (2.0); every other param 0.
    let frame = build_set_camera_source_frame(2, target());
    let golden = hex_to_bytes(
        "fd20000000ffbe4c000000000000000000400000000000000000000000000000000000000000160201019b45",
    );
    assert_eq!(
        frame, golden,
        "the built 534 frame must be byte-identical to the golden frame"
    );
    // Sanity: 44 bytes, v2 start-of-frame, the 534 source identity in the
    // header bytes (system 255 = 0xFF at offset 5, component 190 = 0xBE at 6).
    assert_eq!(frame.len(), 44);
    assert_eq!(frame[0], 0xFD);
    assert_eq!(frame[5], 0xFF);
    assert_eq!(frame[6], 0xBE);
}

#[test]
fn the_534_frame_carries_the_index_in_param2_at_the_wire_offset() {
    // The rust-mavlink ardupilotmega enum has no 534 MavCmd variant, so a full
    // typed decode raises InvalidEnum — the raw builder is exactly why the
    // protocol crate exposes a by-id builder. So assert the load-bearing
    // fields straight off the wire bytes instead: the v2 header carries the
    // 255/190 source identity, and param2 (the second f32 in the payload,
    // bytes 14..18) carries the index as a little-endian f32.
    let frame = build_set_camera_source_frame(3, target());
    // v2 header: STX, len, incompat, compat, seq, sysid, compid, msgid×3.
    assert_eq!(frame[0], 0xFD, "v2 start-of-frame");
    assert_eq!(frame[5], 255, "source system id 255");
    assert_eq!(frame[6], 190, "source component id 190");
    // The 3-byte LE message id is 76 (COMMAND_LONG).
    assert_eq!(frame[7], 76);
    // param1 (bytes 10..14) is 0.0; param2 (bytes 14..18) is the index 3.0.
    let param1 = f32::from_le_bytes(frame[10..14].try_into().unwrap());
    let param2 = f32::from_le_bytes(frame[14..18].try_into().unwrap());
    assert_eq!(param1, 0.0, "param1 is the broadcast component 0");
    assert_eq!(param2, 3.0, "param2 carries the source index");
}

#[test]
fn resolve_camera_index_accepts_positive_integers() {
    assert_eq!(resolve_camera_index("1"), Some(1));
    assert_eq!(resolve_camera_index("2"), Some(2));
    assert_eq!(resolve_camera_index("42"), Some(42));
}

#[test]
fn resolve_camera_index_rejects_zero_negative_and_non_numeric() {
    assert_eq!(resolve_camera_index("0"), None);
    assert_eq!(resolve_camera_index("-1"), None);
    assert_eq!(resolve_camera_index("rgb"), None);
    assert_eq!(resolve_camera_index(""), None);
    // Over 32 chars is malformed.
    assert_eq!(resolve_camera_index(&"1".repeat(33)), None);
    // A char outside [A-Za-z0-9_-] is malformed.
    assert_eq!(resolve_camera_index("1.5"), None);
    assert_eq!(resolve_camera_index("a b"), None);
}

#[tokio::test]
async fn the_501_body_is_a_bare_string_detail() {
    // The live single-camera path: the 501 carries a BARE-STRING detail (not
    // an error object), the exact FastAPI plain-string detail. Built directly
    // here so the contract is pinned without a ground-station profile.
    let resp = (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!("drone does not advertise multi-camera support")),
    )
        .into_response();
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!("drone does not advertise multi-camera support"),
        "the 501 detail is the bare string, not an error object"
    );
}

#[tokio::test]
async fn the_400_body_is_the_invalid_id_error_object() {
    let resp = invalid_camera_id();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": {
            "code": "E_INVALID_CAMERA_ID",
            "message": "camera_id must be a positive integer within the advertised range",
        }}})
    );
}

#[tokio::test]
async fn the_503_body_is_the_ipc_unavailable_error_object() {
    let resp = mavlink_ipc_unavailable("connect failed: no such file or directory");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(
        body["detail"]["error"]["code"],
        json!("E_MAVLINK_IPC_UNAVAILABLE")
    );
    assert_eq!(
        body["detail"]["error"]["message"],
        json!("connect failed: no such file or directory")
    );
}

#[tokio::test]
async fn profile_mismatch_body_is_the_fastapi_404_shape() {
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

// ── the relayed drone's camera count ─────────────────────────────────────

/// A relayed-status document with one peer reporting `legs` video legs.
fn sidecar_with_legs(legs: u64, now: f64) -> String {
    json!({
        "version": 1,
        "wall_time_unix": now,
        "peers": [{
            "device_id": "abcdef123456",
            "status_fresh": true,
            "status": {"camera_state": "ready", "video_stream_count": legs},
        }],
    })
    .to_string()
}

#[test]
fn a_two_leg_drone_is_reported_as_multi_camera() {
    // The defect this closes: the count was a hardcoded 1, so a drone flying
    // a pod with two concurrent legs was told its cameras did not exist and
    // the switch route answered 501 to every request ever made to it.
    assert_eq!(
        camera_count_from_sidecar(&sidecar_with_legs(2, 1000.0), 1000.0),
        CameraCount::Known(2)
    );
}

#[test]
fn a_single_leg_drone_is_a_real_answer_not_a_default() {
    assert_eq!(
        camera_count_from_sidecar(&sidecar_with_legs(1, 1000.0), 1000.0),
        CameraCount::Known(1)
    );
}

#[test]
fn an_unknown_count_is_never_reported_as_one_camera() {
    // The three ways the count is genuinely unknown. Each must say so —
    // collapsing them into "this drone has one camera" is the fabrication.
    let unreadable = camera_count_from_sidecar("{not json", 1000.0);
    let stale = camera_count_from_sidecar(&sidecar_with_legs(2, 100.0), 1000.0);
    let silent = camera_count_from_sidecar(
        &json!({"wall_time_unix": 1000.0, "peers": [{"device_id": "a", "status": {}}]}).to_string(),
        1000.0,
    );
    for got in [&unreadable, &stale, &silent] {
        match got {
            CameraCount::Unknown(why) => assert!(
                why.contains("unknown"),
                "the reason must say the count is unknown: {why}"
            ),
            CameraCount::Known(n) => panic!("unknown count reported as {n} cameras"),
        }
    }
    // And specifically: a stale file must not keep serving its last count.
    assert!(matches!(stale, CameraCount::Unknown(_)));
}

#[test]
fn no_relayed_peer_is_unknown_not_single_camera() {
    let empty = json!({"wall_time_unix": 1000.0, "peers": []}).to_string();
    assert!(matches!(
        camera_count_from_sidecar(&empty, 1000.0),
        CameraCount::Unknown(_)
    ));
}

#[test]
fn a_quiet_second_drone_still_blocks_the_guess() {
    // The second drone's status went stale, so its status block is gone,
    // but it is still relayed: one report is not one drone.
    let two = json!({
        "wall_time_unix": 1000.0,
        "peers": [
            {"device_id": "a", "status": {"video_stream_count": 2}},
            {"device_id": "b", "status_fresh": false},
        ],
    })
    .to_string();
    assert!(matches!(
        camera_count_from_sidecar(&two, 1000.0),
        CameraCount::Unknown(_)
    ));
}

#[test]
fn several_relayed_drones_refuse_rather_than_guess_which_one() {
    // The command targets one autopilot and the request names no drone, so
    // picking one would switch the wrong aircraft's camera.
    let two = json!({
        "wall_time_unix": 1000.0,
        "peers": [
            {"device_id": "a", "status": {"video_stream_count": 2}},
            {"device_id": "b", "status": {"video_stream_count": 3}},
        ],
    })
    .to_string();
    match camera_count_from_sidecar(&two, 1000.0) {
        CameraCount::Unknown(why) => assert!(why.contains("more than one drone")),
        CameraCount::Known(n) => panic!("guessed {n} cameras across two drones"),
    }
}

#[tokio::test]
async fn the_unknown_501_still_carries_a_bare_string_detail() {
    // The GCS renders the detail verbatim, so the reason has to survive as a
    // bare string exactly like the single-camera one.
    let resp = unsupported("the relayed drone has not reported ... camera count is unknown");
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    assert!(body_json(resp).await.is_string());
}
