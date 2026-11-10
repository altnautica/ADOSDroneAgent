use std::collections::HashMap;

use super::*;

const BROWSER: &str = "/usr/bin/chromium";

fn names_a_gl_implementation(argv: &[String]) -> bool {
    argv.iter().any(|a| {
        a.starts_with("--use-gl") || a.starts_with("--gl=") || a.starts_with("--use-angle")
    })
}

fn env_map(env: Vec<(String, String)>) -> HashMap<String, String> {
    env.into_iter().collect()
}

#[test]
fn cage_argv_runs_the_browser_under_cage() {
    let argv = cage_argv(BROWSER, "http://target/hud", Renderer::Software);
    assert_eq!(&argv[..3], ["cage", "--", BROWSER]);
    for flag in ["--kiosk", "--ozone-platform=wayland", "--no-sandbox"] {
        assert!(argv.iter().any(|a| a == flag), "{flag}");
    }
    assert_eq!(argv.last().unwrap(), "http://target/hud");
}

#[test]
fn software_disables_the_gpu_and_gpu_names_no_gl_implementation() {
    let sw = cage_argv(BROWSER, "http://x", Renderer::Software);
    assert!(sw.iter().any(|a| a == "--disable-gpu"));
    assert!(!names_a_gl_implementation(&sw));
    assert!(!sw.iter().any(|a| a == "--enable-gpu-rasterization"));
    let gpu = cage_argv(BROWSER, "http://x", Renderer::Gpu);
    assert!(!names_a_gl_implementation(&gpu));
    assert!(!gpu.iter().any(|a| a == "--disable-gpu"));
    assert_eq!(render_flags(Renderer::Software), ["--disable-gpu"]);
    assert!(render_flags(Renderer::Gpu).is_empty());
}

#[test]
fn cage_argv_persists_the_profile_and_keeps_the_cache_in_tmpfs() {
    let argv = cage_argv(BROWSER, "http://target/cockpit", Renderer::Software);
    let storage = cage_storage_dir().to_string_lossy().into_owned();
    for flag in [
        "--user-data-dir=/var/ados/kiosk/profile".to_string(),
        format!("--disk-cache-dir={storage}/cache"),
        format!("--disk-cache-size={DISK_CACHE_BYTES}"),
    ] {
        assert!(argv.contains(&flag), "{flag}");
    }
    assert_eq!(
        cage_storage_dir(),
        ados_protocol::paths::run_dir().join("kiosk")
    );
    assert!(!storage.contains("/root/"));
}

#[test]
fn windowed_argv_matches_the_session_and_scopes_storage() {
    let cache = format!("/run/user/1000/{KIOSK_STORAGE_SUBDIR}");
    let profile = windowed_profile_dir(1000).to_string_lossy().into_owned();
    assert_eq!(profile, "/var/ados/kiosk/profile-1000");
    let argv = windowed_argv(
        BROWSER,
        "http://target/cockpit",
        "wayland",
        Renderer::Software,
        &profile,
        &cache,
    );
    assert!(!argv.iter().any(|a| a == "cage"));
    assert_eq!(argv[0], BROWSER);
    assert!(argv.iter().any(|a| a == "--kiosk"));
    assert!(argv.iter().any(|a| a == "--ozone-platform=wayland"));
    assert!(argv.contains(&format!("--user-data-dir={profile}")));
    assert!(argv.contains(&format!("--disk-cache-dir={cache}/cache")));
    assert!(!argv.iter().any(|a| a.starts_with("--user-data-dir=/home")));
    assert_eq!(argv.last().unwrap(), "http://target/cockpit");
    let x11 = windowed_argv(
        BROWSER,
        "http://t",
        "x11",
        Renderer::Software,
        &profile,
        "/run/user/1000",
    );
    assert!(x11.iter().any(|a| a == "--ozone-platform=x11"));
}

#[cfg(unix)]
#[test]
fn clearing_the_singleton_locks_removes_dangling_symlinks_only() {
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join("SingletonLock");
    // Chromium writes `<hostname>-<pid>` as the link target: it never exists.
    std::os::unix::fs::symlink("ados-gs-4242", &lock).unwrap();
    std::os::unix::fs::symlink("/nonexistent/socket", dir.path().join("SingletonSocket")).unwrap();
    std::fs::write(dir.path().join("Preferences"), "{}").unwrap();
    assert!(std::fs::symlink_metadata(&lock).is_ok());
    clear_singleton_locks(dir.path()).unwrap();
    for name in SINGLETON_FILES {
        assert!(
            std::fs::symlink_metadata(dir.path().join(name)).is_err(),
            "{name} left behind"
        );
    }
    assert!(dir.path().join("Preferences").exists(), "the profile kept");
    clear_singleton_locks(dir.path()).unwrap();
}

#[cfg(unix)]
#[test]
fn preparing_a_profile_creates_it_private_and_clears_the_locks() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("kiosk/profile");
    prepare_profile_dir(&profile, None).unwrap();
    let mode = std::fs::metadata(&profile).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    std::os::unix::fs::symlink("ados-gs-4242", profile.join("SingletonLock")).unwrap();
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    prepare_profile_dir(&profile, Some((uid, Some(gid)))).unwrap();
    assert!(std::fs::symlink_metadata(profile.join("SingletonLock")).is_err());
}

#[test]
fn renderer_aliases() {
    for v in ["gpu", "GLES2", "egl"] {
        assert_eq!(normalise_renderer(v), Some(Renderer::Gpu), "{v}");
    }
    for v in ["software", "pixman", "cpu"] {
        assert_eq!(normalise_renderer(v), Some(Renderer::Software), "{v}");
    }
    assert_eq!(normalise_renderer("nonsense"), None);
}

#[test]
fn render_marker_parses_renderer_and_lib_dir() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("kiosk-render.conf");
    std::fs::write(
        &marker,
        "# provisioned by installer\nrenderer: gpu\nlib_dir: /opt/ados/gpu/mali\n",
    )
    .unwrap();
    assert_eq!(
        read_render_marker(&marker),
        (Some(Renderer::Gpu), Some("/opt/ados/gpu/mali".into()))
    );
    assert_eq!(
        parse_render_marker("renderer = software\n").0,
        Some(Renderer::Software)
    );
    assert_eq!(
        read_render_marker(&dir.path().join("absent.conf")),
        (None, None)
    );
}

#[test]
fn render_plan_precedence() {
    assert_eq!(
        resolve_render_plan(None, (None, None)),
        (Renderer::Software, None)
    );
    assert_eq!(
        resolve_render_plan(Some("software"), (Some(Renderer::Gpu), Some("/x".into()))),
        (Renderer::Software, None)
    );
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().to_string_lossy().into_owned();
    assert_eq!(
        resolve_render_plan(None, (Some(Renderer::Gpu), Some(lib.clone()))),
        (Renderer::Gpu, Some(lib))
    );
    // A marker whose scoped libraries are gone falls back to software.
    assert_eq!(
        resolve_render_plan(
            None,
            (Some(Renderer::Gpu), Some("/opt/ados/gpu/gone".into()))
        ),
        (Renderer::Software, None)
    );
}

#[test]
fn cage_env_overrides_the_units_runtime_dir() {
    for renderer in [Renderer::Software, Renderer::Gpu] {
        let env = env_map(cage_env(renderer, None, "/dev/dri/card0", None));
        assert_ne!(env["XDG_RUNTIME_DIR"], "/run/user/0");
        assert_eq!(env["XDG_RUNTIME_DIR"], cage_runtime_dir().to_string_lossy());
        assert!(cage_runtime_dir().starts_with(ados_protocol::paths::run_dir()));
    }
}

#[test]
fn cage_env_per_renderer() {
    let sw = env_map(cage_env(Renderer::Software, None, "/dev/dri/card1", None));
    assert_eq!(sw["WLR_RENDERER"], "pixman");
    assert_eq!(sw["WLR_DRM_DEVICES"], "/dev/dri/card1");
    assert_eq!(sw["WLR_NO_HARDWARE_CURSORS"], "1");
    assert!(!sw.contains_key("LD_LIBRARY_PATH"));

    let gpu = env_map(cage_env(
        Renderer::Gpu,
        Some("/opt/ados/gpu/mali"),
        "/dev/dri/card0",
        None,
    ));
    assert_eq!(gpu["WLR_RENDERER"], "gles2");
    assert_eq!(gpu["LD_LIBRARY_PATH"], "/opt/ados/gpu/mali");
    assert!(!gpu.contains_key("EGL_PLATFORM"));
    assert!(!gpu.contains_key("WLR_NO_HARDWARE_CURSORS"));

    let prepended = env_map(cage_env(
        Renderer::Gpu,
        Some("/opt/ados/gpu/mali"),
        "/dev/dri/card0",
        Some("/usr/lib/existing"),
    ));
    assert_eq!(
        prepended["LD_LIBRARY_PATH"],
        "/opt/ados/gpu/mali:/usr/lib/existing"
    );

    let system_gpu = env_map(cage_env(Renderer::Gpu, None, "/dev/dri/card0", None));
    assert_eq!(system_gpu["WLR_RENDERER"], "gles2");
    assert!(!system_gpu.contains_key("LD_LIBRARY_PATH"));
}

#[cfg(unix)]
#[test]
fn browser_resolution_takes_the_first_candidate_on_path() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let only = dir.path().join("chromium");
    std::fs::write(&only, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&only, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        resolve_browser_in(dir.path().as_os_str()).unwrap(),
        only.to_string_lossy()
    );
}

#[test]
fn a_missing_browser_names_every_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let err = resolve_browser_in(dir.path().as_os_str()).unwrap_err();
    for name in BROWSER_CANDIDATES {
        assert!(err.contains(name), "{err}");
    }
}
