//! PUT validation, reply classification and the video-cmd socket round-trip.

use super::*;

// --- PUT validation + reply classification + socket round-trip ---

#[test]
fn validate_cameras_accepts_a_well_formed_list() {
    let cams = vec![
        json!({"id": "eo", "source": "/dev/video0", "role": "primary", "orientation": "forward", "purpose": ["feed", "detect"], "rotation": 90, "hflip": true, "vflip": false, "encoder": "omx", "keyframe_interval": 5}),
        json!({"id": "belly", "source": "/dev/video1", "orientation": "down", "purpose": ["precision-landing"], "rotation": 270, "hflip": false, "vflip": true}),
    ];
    assert!(validate_cameras(&cams).is_ok());
}

#[test]
fn validate_cameras_rejects_bad_rotation_values() {
    // rotation must be one of 0 / 90 / 180 / 270 (mirrors the encoder
    // schema; distinct from the coarse-mount `orientation` metadata). A
    // degenerate value is a clear 400 before the socket round-trip.
    for bad in [45, 360, -90, 190] {
        assert!(
            validate_cameras(&[json!({"id": "eo", "source": "/dev/video0", "rotation": bad})])
                .is_err(),
            "rotation {bad} must be rejected"
        );
    }
    // A non-integer rotation is rejected too (it would break the reload).
    assert!(
        validate_cameras(&[json!({"id": "eo", "source": "/dev/video0", "rotation": "90"})])
            .is_err()
    );
    // The four supported values are accepted.
    for good in [0, 90, 180, 270] {
        assert!(
            validate_cameras(&[json!({"id": "eo", "source": "/dev/video0", "rotation": good})])
                .is_ok(),
            "rotation {good} must be accepted"
        );
    }
}

#[test]
fn the_primary_leg_cannot_be_disabled_but_a_secondary_can() {
    let off_primary = [
        json!({"id": "main", "source": "/dev/video0", "role": "primary", "enabled": false}),
        json!({"id": "thermal", "source": "/dev/video2"}),
    ];
    assert!(validate_cameras(&off_primary)
        .unwrap_err()
        .contains("primary camera cannot be disabled"));
    // With no role, the first leg is the primary.
    let off_first = [json!({"id": "eo", "source": "/dev/video0", "enabled": false})];
    assert!(validate_cameras(&off_first).is_err());
    let off_secondary = [
        json!({"id": "main", "source": "/dev/video0", "role": "primary"}),
        json!({"id": "thermal", "source": "/dev/video2", "enabled": false}),
    ];
    assert!(validate_cameras(&off_secondary).is_ok());
}

#[test]
fn validate_cameras_rejects_bad_lists() {
    // Empty.
    assert!(validate_cameras(&[]).is_err());
    // Missing id/source.
    assert!(validate_cameras(&[json!({"id": "eo"})]).is_err());
    // Unsafe id char.
    assert!(validate_cameras(&[json!({"id": "a b", "source": "/dev/video0"})]).is_err());
    assert!(validate_cameras(&[json!({"id": "a/b", "source": "/dev/video0"})]).is_err());
    // Duplicate ids.
    assert!(validate_cameras(&[
        json!({"id": "eo", "source": "/dev/video0"}),
        json!({"id": "eo", "source": "/dev/video1"}),
    ])
    .is_err());
    // A non-primary leg claiming the reserved "main" id.
    assert!(validate_cameras(&[
        json!({"id": "eo", "source": "/dev/video0", "role": "primary"}),
        json!({"id": "main", "source": "/dev/video1", "role": "ir"}),
    ])
    .is_err());
    // Unknown orientation / purpose.
    assert!(validate_cameras(&[
        json!({"id": "eo", "source": "/dev/video0", "orientation": "sideways"})
    ])
    .is_err());
    assert!(validate_cameras(&[
        json!({"id": "eo", "source": "/dev/video0", "purpose": ["weaponise"]})
    ])
    .is_err());
    // A non-array purpose is rejected (it would break the reload otherwise).
    assert!(
        validate_cameras(&[json!({"id": "eo", "source": "/dev/video0", "purpose": "detect"})])
            .is_err()
    );
    // An explicit null purpose is accepted (treated as no purpose).
    assert!(
        validate_cameras(&[json!({"id": "eo", "source": "/dev/video0", "purpose": null})]).is_ok()
    );
}

#[test]
fn classify_video_reply_maps_status_codes() {
    let ok = classify_video_reply(
        json!({"ok": true, "count": 2, "persisted": true, "restarted": true})
            .as_object()
            .unwrap()
            .clone(),
    );
    assert_eq!(ok.status(), StatusCode::OK);

    let args = classify_video_reply(
        json!({"ok": false, "error": "E_ARGS", "reason": "bad"})
            .as_object()
            .unwrap()
            .clone(),
    );
    assert_eq!(args.status(), StatusCode::BAD_REQUEST);

    let persist = classify_video_reply(
        json!({"ok": false, "error": "E_PERSIST"})
            .as_object()
            .unwrap()
            .clone(),
    );
    assert_eq!(persist.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // Saved but the restart failed (no error field, ok:false).
    let restart = classify_video_reply(
        json!({"ok": false, "count": 1, "persisted": true, "restarted": false})
            .as_object()
            .unwrap()
            .clone(),
    );
    assert_eq!(restart.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn an_unknown_failure_with_nothing_persisted_is_not_reported_saved() {
    let unknown = classify_video_reply(
        json!({"ok": false, "error": "E_BUSY", "persisted": false})
            .as_object()
            .unwrap()
            .clone(),
    );
    assert_eq!(unknown.status(), StatusCode::BAD_GATEWAY);
    let bytes = axum::body::to_bytes(unknown.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let msg = body["detail"].as_str().unwrap();
    assert!(!msg.contains("saved"), "{msg}");
    assert!(msg.contains("E_BUSY"), "{msg}");
}

#[tokio::test]
async fn video_cmd_round_trips_a_reply() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("video-cmd.sock");
    // A stub server: accept one connection, read the request line, reply once.
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        // The request carries the operator op + owner.
        let req: Value =
            serde_json::from_slice(buf[..n].split(|b| *b == b'\n').next().unwrap()).unwrap();
        assert_eq!(req["op"], "video.cameras.set");
        assert_eq!(req["owner"], "operator");
        stream
            .write_all(b"{\"ok\":true,\"count\":1,\"persisted\":true,\"restarted\":true}\n")
            .await
            .unwrap();
        stream.flush().await.unwrap();
    });

    let req = json!({"op": "video.cameras.set", "owner": "operator", "cameras": [{"id": "eo", "source": "/dev/video0"}]});
    match video_cmd(&req, &sock).await {
        VideoCmd::Reply(reply) => {
            assert_eq!(reply.get("ok"), Some(&Value::Bool(true)));
            assert_eq!(reply.get("count"), Some(&json!(1)));
        }
        VideoCmd::Unavailable => panic!("expected a reply"),
    }
    server.await.unwrap();
}

#[tokio::test]
async fn video_cmd_is_unavailable_when_the_socket_is_absent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("absent.sock");
    let req = json!({"op": "video.cameras.set", "cameras": []});
    assert!(matches!(
        video_cmd(&req, &sock).await,
        VideoCmd::Unavailable
    ));
}
