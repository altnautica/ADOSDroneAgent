use super::*;

#[test]
fn gpu_failure_markers() {
    assert!(looks_gpu_failure(
        "EGL_NOT_INITIALIZED: DRI2 failed to create screen"
    ));
    assert!(looks_gpu_failure("failed to create renderer"));
    assert!(!looks_gpu_failure("clean shutdown, no errors"));
}

#[test]
fn the_browser_probe_errs_toward_running_when_it_cannot_tell() {
    assert!(browser_running_from(&RunOutcome::SpawnFailed(
        "no ps".into()
    )));
    assert!(browser_running_from(&RunOutcome::Exited {
        code: Some(1),
        stdout: String::new(),
        stderr: String::new(),
    }));
    assert!(browser_running_from(&RunOutcome::TimedOut));
}

#[test]
fn the_browser_probe_matches_the_executable_not_the_command_line() {
    let only_cage = "/usr/bin/cage -- /usr/bin/chromium --kiosk http://localhost:8080/\n";
    assert!(!browser_in_ps(only_cage));
    let with_browser = format!("{only_cage}/usr/lib/chromium/chromium --type=renderer\n");
    assert!(browser_in_ps(&with_browser));
    assert!(browser_running_from(&RunOutcome::Exited {
        code: Some(0),
        stdout: with_browser,
        stderr: String::new(),
    }));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_compositor_whose_arguments_name_the_browser_is_not_a_browser() {
    let mut fake_cage = std::process::Command::new("sh")
        .args([
            "-c",
            "sleep 30",
            "cage",
            "--",
            "/usr/bin/chromium",
            "--kiosk",
        ])
        .spawn()
        .unwrap();
    let running = browser_running().await;
    let _ = fake_cage.kill();
    let _ = fake_cage.wait();
    assert!(!running);
}

#[tokio::test]
async fn output_mode_with_no_connected_output_returns_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let drm = DrmPaths {
        sysfs: dir.path().join("sys"),
        dev: dir.path().join("dev"),
    };
    tokio::time::timeout(
        Duration::from_secs(2),
        apply_output_mode("1920x1080", dir.path(), &[], &drm),
    )
    .await
    .expect("nothing pending, nothing to wait for");
}
