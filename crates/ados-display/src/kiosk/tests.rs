use std::collections::HashMap;

use super::*;

const BROWSER: &str = "/usr/bin/chromium";

fn wayland_session() -> DesktopSession {
    DesktopSession {
        uid: 1000,
        session_type: "wayland".into(),
        display: None,
        wayland_display: Some("wayland-0".into()),
    }
}

fn pinned_drm(dir: &Path) -> DrmPaths {
    let drm = DrmPaths {
        sysfs: dir.join("sys"),
        dev: dir.join("dev"),
    };
    std::fs::create_dir_all(drm.sysfs.join("card0-HDMI-A-1")).unwrap();
    std::fs::write(drm.sysfs.join("card0-HDMI-A-1/status"), "connected\n").unwrap();
    std::fs::create_dir_all(&drm.dev).unwrap();
    std::fs::write(drm.dev.join("card0"), "").unwrap();
    drm
}

#[test]
fn a_desktop_session_honours_the_resolved_gpu_renderer() {
    let dir = tempfile::tempdir().unwrap();
    let spec = make_spec(
        "http://target/cockpit",
        Some(&wayland_session()),
        Renderer::Gpu,
        None,
        None,
        BROWSER,
        &pinned_drm(dir.path()),
    );
    assert!(!spec.argv.iter().any(|a| a == "--disable-gpu"));
    assert!(!spec.argv.iter().any(|a| a.starts_with("--use-gl")
        || a.starts_with("--gl=")
        || a.starts_with("--use-angle")));
    let sw = make_spec(
        "http://target/cockpit",
        Some(&wayland_session()),
        Renderer::Software,
        None,
        None,
        BROWSER,
        &pinned_drm(dir.path()),
    );
    assert!(sw.argv.iter().any(|a| a == "--disable-gpu"));
}

#[test]
fn the_windowed_path_runs_as_the_session_user_without_a_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let spec = make_spec(
        "http://x",
        Some(&wayland_session()),
        Renderer::Gpu,
        Some("/opt/ados/gpu/mali"),
        Some("1920x1080"),
        BROWSER,
        &pinned_drm(dir.path()),
    );
    assert_eq!(spec.run_as_uid, Some(1000));
    assert!(!spec.sweep_orphans);
    assert!(!spec.argv.iter().any(|a| a == "--no-sandbox"));
    assert_eq!(
        spec.output_mode, None,
        "the desktop owns its display settings"
    );
    assert!(spec.argv.contains(&format!(
        "--user-data-dir=/run/user/1000/{KIOSK_STORAGE_SUBDIR}/profile"
    )));
    let env: HashMap<_, _> = spec.env.into_iter().collect();
    assert_eq!(env["XDG_RUNTIME_DIR"], "/run/user/1000");
}

#[test]
fn the_cage_path_strips_display_and_scopes_the_gpu_libraries() {
    let dir = tempfile::tempdir().unwrap();
    let drm = pinned_drm(dir.path());
    let spec = make_spec(
        "http://x",
        None,
        Renderer::Gpu,
        Some("/opt/ados/gpu/mali"),
        Some("1920x1080"),
        BROWSER,
        &drm,
    );
    assert_eq!(spec.argv[0], "cage");
    assert!(spec.sweep_orphans);
    assert_eq!(spec.run_as_uid, None);
    assert_eq!(spec.output_mode.as_deref(), Some("1920x1080"));
    for key in ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY"] {
        assert!(spec.env_unset.iter().any(|k| k == key), "{key}");
    }
    let env: HashMap<_, _> = spec.env.into_iter().collect();
    assert_eq!(env["WLR_RENDERER"], "gles2");
    assert!(env["LD_LIBRARY_PATH"].starts_with("/opt/ados/gpu/mali"));
    assert_eq!(
        env["WLR_DRM_DEVICES"],
        drm.dev.join("card0").to_string_lossy()
    );
}
