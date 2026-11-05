//! Startup gate tests: which units the hardware-detect pass starts, the
//! cleanup it does for the units it leaves off, and what start-up publishes
//! before the daemon reports ready.

use super::*;

use std::path::{Path, PathBuf};

use ados_config::capture_storage::{self as cs, FsSpace, SysRoots, GIB};

/// A capture volume with room, whatever path it is asked about.
fn roomy(_: &Path) -> Option<FsSpace> {
    Some(FsSpace {
        total_bytes: 1000 * GIB,
        free_bytes: 800 * GIB,
    })
}

/// A drone supervisor whose capture category reads and writes only under
/// `dir`: a capture volume mounted at `<dir>/mnt`, `storage.capture_root` on
/// it, and the run dir at `<dir>/run`. `start()` publishes the capture
/// sidecar, and a test must reach neither the host's volumes nor its run dir.
/// Returns the supervisor and the capture root.
fn drone_with_capture_fixture(dir: &Path, pm: Arc<MockProcessManager>) -> (Supervisor, PathBuf) {
    let mount = dir.join("mnt");
    let root = mount.join("ados/capture");
    std::fs::create_dir_all(dir.join("sys/block/nvme0n1/nvme0n1p1")).unwrap();
    std::fs::create_dir_all(dir.join("sys/block/nvme0n1/queue")).unwrap();
    std::fs::write(dir.join("sys/block/nvme0n1/queue/rotational"), "0\n").unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        dir.join("mounts"),
        format!("/dev/nvme0n1p1 {} ext4 rw 0 0\n", mount.display()),
    )
    .unwrap();
    let config_yaml = dir.join("config.yaml");
    std::fs::write(
        &config_yaml,
        format!("storage:\n  capture_root: {}\n", root.display()),
    )
    .unwrap();
    let mut config = cfg("drone");
    config.run_dir = dir.join("run");
    config.config_yaml = config_yaml;
    let mut sup = Supervisor::with_process_manager(config, Arc::new(BindOrchestrator::new()), pm);
    sup.janitor.capture_sources.sys = SysRoots {
        sys_block: dir.join("sys/block"),
        mounts: dir.join("mounts"),
        fallback_root: dir.join("fallback"),
        sticky_file: dir.join("capture-root"),
        stat: roomy,
    };
    (sup, root)
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn startup_publishes_the_capture_sidecar_before_it_returns() {
    // Plugin units order after the supervisor, so systemd starts them as soon
    // as the daemon reports ready, which it does when `start()` returns. The
    // first monitor pass reaches the janitor seconds later, so a capture
    // plugin asked to start a session right after an agent restart found no
    // sidecar (or the last process's, gone stale) and refused it as
    // `E_STORAGE_FULL`. Start-up itself must leave a fresh, admitting sidecar.
    let tmp = tempfile::tempdir().unwrap();
    let pm = Arc::new(MockProcessManager::new());
    let (mut sup, root) = drone_with_capture_fixture(tmp.path(), pm);
    let sidecar = cs::sidecar_path_in(&tmp.path().join("run"));

    sup.start().await;

    let admitted = cs::admit_from_sidecar(&sidecar, std::time::SystemTime::now())
        .expect("a capture session is admitted as soon as start-up returns");
    assert_eq!(admitted.capture_root, root.display().to_string());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn startup_publishes_no_capture_sidecar_on_a_profile_without_capture() {
    let tmp = tempfile::tempdir().unwrap();
    let pm = Arc::new(MockProcessManager::new());
    let (mut sup, _) = drone_with_capture_fixture(tmp.path(), pm);
    sup.config.profile_wire = "workstation".to_string();

    sup.start().await;

    assert!(!cs::sidecar_path_in(&tmp.path().join("run")).exists());
}

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
async fn a_captured_camera_starts_the_engine_and_the_pipeline_without_a_vision_block() {
    // The config a simulated capture rig is given carries no `vision:` block
    // and no `video.camera.source`: only `cameras[]` with `capture: true`.
    // The engine owns those cameras, so it must run, and the video pipeline
    // must run to encode them off the frame bus.
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(
        &cfg_path,
        include_str!("../../../../ados-config/tests/fixtures/sim-capture-config.yaml"),
    )
    .unwrap();
    let config = crate::config::AgentConfig::load_from(
        &cfg_path,
        &dir.path().join("profile.conf"),
        &dir.path().join("role"),
    );
    assert!(!config.vision_enabled);
    assert_eq!(config.declared_cameras.captured, 2);
    let mock = Arc::new(MockProcessManager::new());
    let bind = Arc::new(BindOrchestrator::new());
    let mut sup = Supervisor::with_process_manager(config, bind, mock.clone());

    sup.detect_and_start_hardware().await;

    let calls = mock.calls();
    assert!(
        calls.iter().any(|c| c == "start:ados-vision"),
        "calls={calls:?}"
    );
    assert!(
        calls.iter().any(|c| c == "start:ados-video"),
        "calls={calls:?}"
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
async fn the_sensor_plane_starts_on_the_profiles_it_serves() {
    // No install step enables ados-clock, ados-sensorhub or ados-gnss, so
    // the hardware pass must start them, clock first, and only where the
    // profile gate allows.
    let started = |profile: &str| {
        let profile = profile.to_owned();
        async move {
            let mock = Arc::new(MockProcessManager::new());
            let bind = Arc::new(BindOrchestrator::new());
            let mut sup = Supervisor::with_process_manager(cfg(&profile), bind, mock.clone());
            sup.detect_and_start_hardware().await;
            mock.calls()
                .into_iter()
                .filter_map(|c| c.strip_prefix("start:").map(str::to_owned))
                .filter(|u| crate::lifecycle::monitor::SENSOR_PLANE_UNITS.contains(&u.as_str()))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        started("visor").await,
        ["ados-clock", "ados-sensorhub", "ados-gnss"]
    );
    assert_eq!(started("drone").await, ["ados-clock", "ados-sensorhub"]);
    assert_eq!(started("ground-station").await, ["ados-clock", "ados-gnss"]);
    assert!(started("workstation").await.is_empty());
}

#[tokio::test]
async fn startup_clears_a_stale_failure_on_a_unit_it_leaves_off() {
    // A previous process can leave an intentionally-off unit `failed`
    // (a stop that landed mid-start). Nothing starts it again, so without a
    // reset the status surface reports a failure for a unit meant to be off.
    let tmp = tempfile::tempdir().unwrap();
    let pm = Arc::new(MockProcessManager::new());
    let (mut sup, _) = drone_with_capture_fixture(tmp.path(), pm.clone());

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
