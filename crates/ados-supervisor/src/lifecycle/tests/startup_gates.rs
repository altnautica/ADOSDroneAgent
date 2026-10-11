//! Startup gate tests: which units the hardware-detect pass starts, and the
//! cleanup it does for the units it leaves off.

use super::*;

#[tokio::test]
async fn vision_enabled_starts_the_vision_engine() {
    // Regression: a `vision.enabled: true` config must actually bring the
    // vision engine up. The unit is not in any install-time enable set and
    // the monitor never starts a Stopped service, so the hardware-detect
    // pass is the ONLY thing that starts it — if it does not, the vision →
    // world-model pipeline silently never comes up.
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    // Drone profile (the vision unit's profile gate) with vision enabled and
    // a network camera source, so the camera-source precondition holds
    // without a local /dev/video node (host-independent).
    let mut config = cfg("drone");
    config.vision_enabled = true;
    config.video_network_source = Some("rtsp://cam/scene".to_string());
    let mut sup = Supervisor::with_process_manager(config, bind, mock.clone());

    sup.detect_and_start_hardware().await;

    assert!(
        mock.calls().iter().any(|c| c == "start:ados-vision"),
        "vision.enabled=true must reach a start of ados-vision; calls={:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn vision_disabled_does_not_start_the_vision_engine() {
    // The mirror of the above: with vision off, the engine must stay down
    // even though a camera source is present.
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut config = cfg("drone");
    config.vision_enabled = false;
    config.video_network_source = Some("rtsp://cam/scene".to_string());
    let mut sup = Supervisor::with_process_manager(config, bind, mock.clone());

    sup.detect_and_start_hardware().await;

    assert!(
        !mock.calls().iter().any(|c| c == "start:ados-vision"),
        "ados-vision must not start when vision is disabled; calls={:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn vision_enabled_without_a_camera_source_is_surfaced_not_started() {
    // A genuine hard precondition: vision enabled but no camera source at
    // all. The engine is not started (there are no frames to feed it); the
    // code path logs a loud warning rather than skipping in silence.
    //
    // `has_video_source` = a local /dev/video node OR a configured network
    // URL. This test forces the network URL absent; the assertion is only
    // meaningful when the test host also has no local camera node, so it
    // guards on the real probe rather than assuming the host state (a dev
    // box with a webcam would legitimately have a source and start it).
    if crate::hardware::has_camera().await {
        return;
    }
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut config = cfg("drone");
    config.vision_enabled = true;
    config.video_network_source = None;
    let mut sup = Supervisor::with_process_manager(config, bind, mock.clone());

    sup.detect_and_start_hardware().await;

    assert!(
        !mock.calls().iter().any(|c| c == "start:ados-vision"),
        "ados-vision must not start without a camera source; calls={:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn vision_enabled_on_a_non_drone_profile_is_gated_off_not_started() {
    // A genuine hard precondition: vision runs on the drone profile only
    // (the prebuilt catalog fetches the ados-vision binary there), so on any
    // other profile the unit's binary is absent and starting it would
    // crash-loop. Even with vision enabled AND a camera source present, the
    // engine must NOT be started on a non-drone node. The detect pass
    // surfaces the reason loudly; the behavioral contract locked here is that
    // it never reaches a start of the gated-off unit.
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    // Workstation profile (ados-vision gates off) with vision enabled and a
    // network camera source, so the camera-source precondition is satisfied
    // and the ONLY thing keeping the engine down is the profile gate.
    let mut config = cfg("workstation");
    config.vision_enabled = true;
    config.video_network_source = Some("rtsp://cam/scene".to_string());
    let mut sup = Supervisor::with_process_manager(config, bind, mock.clone());

    sup.detect_and_start_hardware().await;

    assert!(
        !mock.calls().iter().any(|c| c == "start:ados-vision"),
        "ados-vision must not start on a non-drone profile; calls={:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn vision_enabled_headless_drone_is_gated_off_not_started() {
    // The lean headless core excludes vision/AI (ados-vision is not in the
    // KEEP set). Even on a drone with vision enabled and a camera source, a
    // headless node must NOT start the engine — the headless gate is
    // subtractive. Locks that the world-model capture path stays off in the
    // zero-Python profile rather than crash-looping a binary the lean profile
    // never provisions.
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut config = cfg_headless();
    config.vision_enabled = true;
    config.video_network_source = Some("rtsp://cam/scene".to_string());
    let mut sup = Supervisor::with_process_manager(config, bind, mock.clone());

    sup.detect_and_start_hardware().await;

    assert!(
        !mock.calls().iter().any(|c| c == "start:ados-vision"),
        "ados-vision must not start on a headless node; calls={:?}",
        mock.calls()
    );
}

#[tokio::test]
async fn startup_clears_a_stale_failure_on_a_unit_it_leaves_off() {
    // A previous process can leave an intentionally-off unit `failed`
    // (a stop that landed mid-start). Nothing starts it again, so without a
    // reset the status surface reports a failure for a unit meant to be off.
    let pm = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(cfg("drone"), bind, pm.clone());

    sup.start().await;

    let calls = pm.calls();
    assert!(
        !calls.contains(&"start:ados-vision".to_string()),
        "vision is off in this config; calls={calls:?}"
    );
    assert!(
        calls.contains(&"reset_failed:ados-vision".to_string()),
        "the stale failed state of a unit left off was not cleared; calls={calls:?}"
    );
}
