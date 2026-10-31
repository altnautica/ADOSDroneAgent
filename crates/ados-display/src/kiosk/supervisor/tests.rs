use tokio::io::AsyncReadExt;

use super::*;

fn sh(script: &str) -> ChildSpec {
    ChildSpec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        ..ChildSpec::default()
    }
}

fn fast(spec: ChildSpec) -> KioskSupervisor {
    let mut sup = KioskSupervisor::new(spec);
    sup.timing.retry = Duration::ZERO;
    sup.timing.grace = Duration::from_millis(200);
    sup
}

async fn stderr_of(sup: &KioskSupervisor) -> String {
    let mut child = sup.spawn().unwrap();
    let mut out = String::new();
    child
        .take_stderr()
        .unwrap()
        .read_to_string(&mut out)
        .await
        .unwrap();
    let _ = child.wait().await;
    out
}

#[tokio::test]
async fn spawn_overlays_the_env_on_the_service_env() {
    let mut spec = sh(r#"printf '%s|%s' "$DISPLAY" "${PATH:+set}" >&2"#);
    spec.env = vec![("DISPLAY".into(), ":0".into())];
    assert_eq!(stderr_of(&KioskSupervisor::new(spec)).await, ":0|set");
}

#[tokio::test]
async fn env_unset_wins_over_the_overlay() {
    let mut spec = sh(r#"printf '%s|%s' "${DISPLAY-unset}" "$WLR_RENDERER" >&2"#);
    spec.env = vec![
        ("WLR_RENDERER".into(), "pixman".into()),
        ("DISPLAY".into(), ":0".into()),
    ];
    spec.env_unset = vec!["DISPLAY".into(), "WAYLAND_DISPLAY".into()];
    assert_eq!(stderr_of(&KioskSupervisor::new(spec)).await, "unset|pixman");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn spawn_drops_to_the_run_as_uid() {
    let uid = nix::unistd::getuid().as_raw();
    let mut spec = sh("id -u >&2");
    spec.run_as_uid = Some(uid);
    assert_eq!(
        stderr_of(&KioskSupervisor::new(spec)).await.trim(),
        uid.to_string()
    );
}

#[tokio::test]
async fn a_missing_binary_returns_3() {
    let mut sup = fast(ChildSpec {
        argv: vec!["/nonexistent/ados-kiosk-test-cage".into(), "--".into()],
        ..ChildSpec::default()
    });
    assert_eq!(sup.run().await, 3);
}

#[tokio::test]
async fn another_spawn_failure_returns_4() {
    let mut sup = fast(ChildSpec::default());
    assert_eq!(sup.run().await, 4);
}

#[tokio::test]
async fn a_stop_terminates_the_child_gracefully() {
    let mut sup = fast(sh("sleep 30"));
    let stop = sup.stop_handle();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.request();
    });
    let rc = tokio::time::timeout(Duration::from_secs(5), sup.run())
        .await
        .unwrap();
    assert_eq!(rc, 0);
}

#[tokio::test]
async fn a_child_ignoring_sigterm_is_killed() {
    let mut sup = fast(sh("trap '' TERM; sleep 30"));
    let stop = sup.stop_handle();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.request();
    });
    let rc = tokio::time::timeout(Duration::from_secs(5), sup.run())
        .await
        .expect("SIGKILL after the grace");
    assert_eq!(rc, 0);
}

#[tokio::test]
async fn five_crashes_in_the_window_stop_the_restart_loop() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("spawns");
    let mut sup = fast(sh(&format!("echo x >> '{}'; exit 2", log.display())));
    let rc = tokio::time::timeout(Duration::from_secs(10), sup.run())
        .await
        .unwrap();
    assert_eq!(rc, 2);
    assert!(sup.crash_looped);
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 5);
}

#[tokio::test]
async fn a_crash_loop_of_clean_exits_ends_with_a_restartable_code() {
    let mut sup = fast(sh("exit 0"));
    let rc = tokio::time::timeout(Duration::from_secs(10), sup.run())
        .await
        .unwrap();
    assert!(sup.crash_looped);
    assert_ne!(rc, 0);
}

#[test]
fn the_fifth_crash_in_the_window_trips_the_guard() {
    let mut sup = KioskSupervisor::new(ChildSpec::default());
    for _ in 0..4 {
        assert!(sup.record_crash());
    }
    assert!(!sup.record_crash());
}

#[test]
fn tail_keeps_the_trailing_window_on_a_char_boundary() {
    let s = format!("{}éMARKER", "A".repeat(100));
    assert_eq!(tail_str(&s, 6), "MARKER");
    assert_eq!(tail_str(&s, 7), "MARKER", "never splits a char");
    assert_eq!(tail_str("", 10), "");
}

#[tokio::test]
async fn child_stderr_reaches_the_tail_while_the_child_runs() {
    let tail = Arc::new(Mutex::new(String::new()));
    let stderr: &[u8] = b"ERROR:gl_factory.cc:110] Requested GL implementation not found\n\
        ERROR:viz_main_impl.cc:190] Exiting GPU process due to errors\n";
    stream_stderr(stderr, tail.clone()).await;
    assert!(tail.lock().unwrap().contains("Exiting GPU process"));
}

#[tokio::test]
async fn the_browser_watch_is_inert_on_the_windowed_path() {
    let fired = tokio::time::timeout(
        Duration::from_millis(200),
        watch_browser(false, &Timing::default()),
    )
    .await;
    assert!(fired.is_err(), "the child is the browser; nothing to watch");
}

#[tokio::test]
async fn a_stop_is_sticky_and_wakes_every_waiter() {
    let stop = Stop::new();
    let waiter = {
        let stop = stop.clone();
        tokio::spawn(async move { stop.wait().await })
    };
    tokio::task::yield_now().await;
    stop.request();
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(stop.wait_timeout(Duration::from_millis(1)).await);
}
