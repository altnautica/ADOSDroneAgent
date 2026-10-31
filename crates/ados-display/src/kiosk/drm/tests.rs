use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::*;
use crate::kiosk::settings::KioskSettings;

/// Fake `/sys/class/drm` connectors and `/dev/dri` card nodes.
fn drm(dir: &Path, connectors: &[(&str, &str)], cards: &[&str]) -> DrmPaths {
    let paths = DrmPaths {
        sysfs: dir.join("sys_drm"),
        dev: dir.join("dev_dri"),
    };
    std::fs::create_dir_all(&paths.sysfs).unwrap();
    std::fs::create_dir_all(&paths.dev).unwrap();
    for (name, status) in connectors {
        let c = paths.sysfs.join(name);
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(c.join("status"), format!("{status}\n")).unwrap();
    }
    for card in cards {
        std::fs::write(paths.dev.join(card), "").unwrap();
    }
    paths
}

fn watch(dir: &Path, enabled: bool) -> SettingsWatch {
    SettingsWatch::new(
        dir.join("config.yaml"),
        KioskSettings {
            enabled,
            output_mode: None,
        },
        None,
    )
}

#[test]
fn a_connected_connector_on_any_card_is_a_display() {
    let dir = tempfile::tempdir().unwrap();
    let p = drm(
        dir.path(),
        &[
            ("card1-HDMI-A-1", "connected"),
            ("card1-HDMI-A-2", "disconnected"),
        ],
        &["card0", "card1"],
    );
    assert!(hdmi_present(&p));
}

#[test]
fn a_card_node_alone_counts_and_nothing_does_not() {
    let dir = tempfile::tempdir().unwrap();
    assert!(hdmi_present(&drm(dir.path(), &[], &["card0"])));
    let dir = tempfile::tempdir().unwrap();
    assert!(!hdmi_present(&drm(
        dir.path(),
        &[("card1-HDMI-A-1", "disconnected")],
        &[]
    )));
}

#[test]
fn the_drm_device_is_the_card_driving_the_display() {
    let dir = tempfile::tempdir().unwrap();
    let p = drm(
        dir.path(),
        &[
            ("card1-HDMI-A-1", "connected"),
            ("card1-HDMI-A-2", "disconnected"),
        ],
        &["card0", "card1"],
    );
    assert_eq!(
        resolve_drm_device(&p),
        p.dev.join("card1").to_string_lossy()
    );
}

#[test]
fn the_drm_device_falls_back_without_a_usable_connector() {
    let dir = tempfile::tempdir().unwrap();
    let p = drm(
        dir.path(),
        &[("card0-HDMI-A-1", "disconnected")],
        &["card0"],
    );
    assert_eq!(resolve_drm_device(&p), DRM_DEVICE);
    // A connected connector whose card node is absent is not a device.
    let dir = tempfile::tempdir().unwrap();
    let p = drm(dir.path(), &[("card9-HDMI-A-1", "connected")], &["card0"]);
    assert_eq!(resolve_drm_device(&p), DRM_DEVICE);
}

#[test]
fn connected_outputs_list_their_modes_by_wlroots_name() {
    let dir = tempfile::tempdir().unwrap();
    let p = drm(
        dir.path(),
        &[
            ("card1-HDMI-A-1", "connected"),
            ("card1-HDMI-A-2", "disconnected"),
        ],
        &["card1"],
    );
    std::fs::write(
        p.sysfs.join("card1-HDMI-A-1/modes"),
        "1920x1080\n1280x720\n",
    )
    .unwrap();
    std::fs::write(p.sysfs.join("card1-HDMI-A-2/modes"), "1920x1080\n").unwrap();
    let outputs = connected_outputs(&p);
    assert_eq!(outputs.len(), 1);
    assert!(outputs["HDMI-A-1"].contains("1280x720"));
}

#[tokio::test]
async fn wait_for_display_returns_at_once_when_present() {
    let dir = tempfile::tempdir().unwrap();
    let p = drm(dir.path(), &[("card0-HDMI-A-1", "connected")], &["card0"]);
    let mut w = watch(dir.path(), true);
    assert!(wait_for_display(&mut w, &p, Duration::from_millis(10)).await);
}

#[tokio::test]
async fn wait_for_display_never_gives_up_while_headless() {
    let dir = tempfile::tempdir().unwrap();
    let p = drm(dir.path(), &[], &[]);
    let mut w = watch(dir.path(), true);
    let waited = tokio::time::timeout(
        Duration::from_millis(300),
        wait_for_display(&mut w, &p, Duration::from_millis(10)),
    )
    .await;
    assert!(waited.is_err(), "a headless box keeps polling");
}

#[tokio::test]
async fn wait_for_display_sees_a_display_that_appears_later() {
    let dir = tempfile::tempdir().unwrap();
    let p = drm(dir.path(), &[], &[]);
    let dev = p.dev.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(dev.join("card0"), "").unwrap();
    });
    let mut w = watch(dir.path(), true);
    let shown = tokio::time::timeout(
        Duration::from_secs(2),
        wait_for_display(&mut w, &p, Duration::from_millis(10)),
    )
    .await
    .unwrap();
    assert!(shown);
}

#[tokio::test]
async fn wait_for_display_stands_down_when_the_kiosk_is_disabled_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let p = drm(dir.path(), &[], &[]);
    std::fs::write(
        dir.path().join("config.yaml"),
        "ground_station:\n  kiosk:\n    enabled: false\n",
    )
    .unwrap();
    let mut w = watch(dir.path(), true);
    let shown = tokio::time::timeout(
        Duration::from_secs(2),
        wait_for_display(&mut w, &p, Duration::from_millis(10)),
    )
    .await
    .unwrap();
    assert!(!shown);
}

#[test]
fn serving_statuses() {
    for code in [200, 204, 307] {
        assert!(serving_status(code), "{code}");
    }
    for code in [401, 403, 405] {
        assert!(serving_status(code), "an auth gate still serves: {code}");
    }
    for code in [404, 500, 502] {
        assert!(!serving_status(code), "{code}");
    }
}

#[test]
fn a_refused_connection_is_not_serving() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert!(!url_serving(&format!("http://127.0.0.1:{port}/cockpit")));
}

#[tokio::test]
async fn wait_for_url_returns_at_once_when_serving() {
    let ms = Duration::from_millis(10);
    assert!(wait_for_url_with("http://x", ms, ms, |_| true).await);
}

#[tokio::test]
async fn wait_for_url_waits_for_the_boot_race() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let probe = move |_: String| counter.fetch_add(1, Ordering::SeqCst) + 1 >= 3;
    let ms = Duration::from_millis(10);
    assert!(wait_for_url_with("http://x", Duration::from_secs(1), ms, probe).await);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn wait_for_url_times_out_and_launches_anyway() {
    let ms = Duration::from_millis(10);
    assert!(!wait_for_url_with("http://x", Duration::from_millis(50), ms, |_| false).await);
}
