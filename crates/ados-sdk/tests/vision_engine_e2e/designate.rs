//! Operator designation through the SDK locks the real engine's tracker.

use super::*;

#[tokio::test]
async fn a_designated_target_locks_the_engines_tracker() {
    let (h, engine) = harness(&["vision.track.designate"], RealEngine::start).await;
    // Only a camera the engine captures has a tracker to lock.
    engine
        .engine
        .publish_frame(
            CAMERA,
            1,
            ados_vision_engine::ring::FrameTiming::host(1_700_000_000_000),
            4,
            4,
            FrameFormat::Rgb24,
            &[0u8; 48],
        )
        .await
        .expect("publish frame");

    let reply = h
        .ctx
        .vision
        .designate_track(CAMERA, &detection())
        .await
        .expect("designate");

    assert_eq!(field(&reply, "designated"), Some(&Value::Boolean(true)));
    assert_eq!(field(&reply, "camera_id"), Some(&Value::from(CAMERA)));
    assert!(
        matches!(field(&reply, "track_id"), Some(Value::Integer(_))),
        "the tracker assigned the designated box a track id: {reply:?}"
    );
    h.ipc.close().await;
}

#[tokio::test]
async fn a_detection_lease_from_the_sdk_holds_the_engine_on_under_the_plugins_name() {
    let (h, engine) = harness(&["vision.track.designate"], RealEngine::start).await;
    engine
        .engine
        .publish_frame(
            CAMERA,
            1,
            ados_vision_engine::ring::FrameTiming::host(1_700_000_000_000),
            4,
            4,
            FrameFormat::Rgb24,
            &[0u8; 48],
        )
        .await
        .expect("publish frame");

    let reply = h
        .ctx
        .vision
        .request_detection(CAMERA, Duration::from_secs(6))
        .await
        .expect("request detection");
    assert_eq!(reply.camera_id, CAMERA);
    assert!(reply.detecting);
    assert!(reply.until_ms.is_some());
    // The host stamped this plugin as the lease owner.
    let now = ados_protocol::framebus::mono_ns() / 1_000_000;
    let status = engine.engine.detection_status(&[], now);
    let leases = &status.cameras[CAMERA].leases;
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].owner, PLUGIN_ID);

    // A zero hold releases it.
    let reply = h
        .ctx
        .vision
        .request_detection(CAMERA, Duration::ZERO)
        .await
        .expect("release detection");
    assert!(!reply.detecting);
    h.ipc.close().await;
}
