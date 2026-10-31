use std::time::Duration;

use serde_json::json;

use super::*;

/// A supervisor stand-in: crash-loops (`run` returns 5) or blocks until
/// stopped (returns 0).
struct Fake {
    crash: bool,
    stop: Arc<Stop>,
}

impl Fake {
    fn new(crash: bool) -> Self {
        Self {
            crash,
            stop: Stop::new(),
        }
    }
}

impl Runner for Fake {
    fn stop_handle(&self) -> Arc<Stop> {
        self.stop.clone()
    }
    fn crash_looped(&self) -> bool {
        self.crash
    }
    fn stderr_tail(&self) -> String {
        "EGL_NOT_INITIALIZED".into()
    }
    async fn run(&mut self) -> i32 {
        if self.crash {
            return 5;
        }
        self.stop.wait().await;
        0
    }
}

fn plan(renderer: Renderer) -> Plan {
    Plan {
        url: "http://x".into(),
        session: None,
        renderer,
        lib_dir: Some("/opt/ados/gpu/mali".into()),
    }
}

fn watch_at(path: std::path::PathBuf, enabled: bool) -> SettingsWatch {
    let mut watch = SettingsWatch::new(
        path.clone(),
        KioskSettings {
            enabled,
            output_mode: None,
        },
        config_signature(&path),
    );
    watch.interval = Duration::from_millis(10);
    watch
}

#[test]
fn the_panel_assigned_elsewhere_stands_the_service_down() {
    for selection in ["lcd", "none"] {
        let cfg =
            json!({"ground_station": {"display": {"type": selection}, "kiosk": {"enabled": true}}});
        assert!(
            matches!(preflight(&cfg), Preflight::StandDown(_)),
            "{selection} must stand down without probing the display"
        );
    }
}

#[test]
fn a_disabled_kiosk_stands_down_and_an_enabled_one_launches() {
    let off = json!({"ground_station": {"display": {"type": "auto"}, "kiosk": {"enabled": false}}});
    assert!(matches!(preflight(&off), Preflight::StandDown(_)));
    let on = json!({"ground_station": {"display": {"type": "hdmi"}, "kiosk": {"enabled": true, "resolution": "720p"}}});
    assert_eq!(
        preflight(&on),
        Preflight::Launch(KioskSettings {
            enabled: true,
            output_mode: Some("1280x720")
        })
    );
}

#[tokio::test]
async fn a_crash_looping_gpu_cage_launch_is_retried_once_in_software() {
    let dir = tempfile::tempdir().unwrap();
    let mut watch = watch_at(dir.path().join("config.yaml"), true);
    let mut p = plan(Renderer::Gpu);
    let mut renderers = Vec::new();
    let rc = supervise(
        &mut p,
        &mut watch,
        &Stop::new(),
        &Mutex::new(None),
        |plan, _| {
            renderers.push(plan.renderer);
            // The software retry crash-loops too: no further downgrade.
            Ok::<_, String>(Fake::new(true))
        },
    )
    .await;
    assert_eq!(renderers, [Renderer::Gpu, Renderer::Software]);
    assert_eq!(p.lib_dir, None);
    assert_eq!(rc, 5);
}

#[tokio::test]
async fn a_windowed_crash_loop_is_not_downgraded() {
    let dir = tempfile::tempdir().unwrap();
    let mut watch = watch_at(dir.path().join("config.yaml"), true);
    let mut p = plan(Renderer::Gpu);
    p.session = Some(crate::kiosk::session::DesktopSession {
        uid: 1000,
        session_type: "wayland".into(),
        display: None,
        wayland_display: Some("wayland-0".into()),
    });
    let mut made = 0;
    let rc = supervise(
        &mut p,
        &mut watch,
        &Stop::new(),
        &Mutex::new(None),
        |_, _| {
            made += 1;
            Ok::<_, String>(Fake::new(true))
        },
    )
    .await;
    assert_eq!((made, rc), (1, 5));
}

#[tokio::test]
async fn a_missing_browser_returns_3() {
    let dir = tempfile::tempdir().unwrap();
    let mut watch = watch_at(dir.path().join("config.yaml"), true);
    let rc = supervise(
        &mut plan(Renderer::Software),
        &mut watch,
        &Stop::new(),
        &Mutex::new(None),
        |_, _| Err::<Fake, _>("no browser".to_string()),
    )
    .await;
    assert_eq!(rc, 3);
}

#[tokio::test]
async fn a_config_edit_restarts_the_child_then_disabling_stands_down() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "ground_station:\n  kiosk:\n    enabled: true\n").unwrap();
    let mut watch = watch_at(path.clone(), true);
    let rewrite = |body: &str| {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, body).unwrap();
        std::fs::rename(&tmp, &path).unwrap(); // a new inode, as the writers do
    };
    let mut modes = Vec::new();
    let mut stops: Vec<Arc<Stop>> = Vec::new();
    let current = Mutex::new(None);
    let operator = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        rewrite("ground_station:\n  kiosk:\n    enabled: true\n    resolution: 1080p\n");
        tokio::time::sleep(Duration::from_millis(100)).await;
        rewrite("ground_station:\n  kiosk:\n    enabled: false\n    resolution: 1080p\n");
    };
    let mut p = plan(Renderer::Software);
    let no_shutdown = Stop::new();
    let service = supervise(&mut p, &mut watch, &no_shutdown, &current, |_, mode| {
        modes.push(mode);
        let fake = Fake::new(false);
        stops.push(fake.stop_handle());
        Ok::<_, String>(fake)
    });
    let (rc, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(service, operator)
    })
    .await
    .unwrap();
    assert_eq!(rc, 0);
    assert_eq!(modes, [None, Some("1920x1080")]);
    assert!(stops.iter().all(|s| s.is_set()));
    assert!(current.lock().unwrap().is_none());
}

#[tokio::test]
async fn a_shutdown_stops_the_current_child_and_exits_clean() {
    let dir = tempfile::tempdir().unwrap();
    let mut watch = watch_at(dir.path().join("config.yaml"), true);
    let shutdown = Stop::new();
    let current: Mutex<Option<Arc<Stop>>> = Mutex::new(None);
    let signal = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.request();
        if let Some(stop) = current.lock().unwrap().as_ref() {
            stop.request();
        }
    };
    let mut p = plan(Renderer::Software);
    let service = supervise(&mut p, &mut watch, &shutdown, &current, |_, _| {
        Ok::<_, String>(Fake::new(false))
    });
    let (rc, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(service, signal)
    })
    .await
    .unwrap();
    assert_eq!(rc, 0);
}

#[test]
fn exit_codes() {
    assert_eq!(exit_code(0), ExitCode::SUCCESS);
    assert_eq!(exit_code(3), ExitCode::from(3));
    assert_eq!(exit_code(-1), ExitCode::FAILURE);
}
