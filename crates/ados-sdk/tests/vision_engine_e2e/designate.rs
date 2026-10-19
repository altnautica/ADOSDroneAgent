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
            ados_vision::ring::FrameTiming::host(1_700_000_000_000),
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
