//! Renderer plan, cage environment and the browser argv.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Env override of the renderer (`gpu` / `software` and their aliases).
pub const ENV_RENDERER_KEY: &str = "ADOS_KIOSK_RENDERER";

/// The installer-written renderer marker. "software" (pixman, CPU) is the
/// default because it renders on any board; "gpu" (cage `gles2` + browser GL)
/// is used only when the installer provisioned a working GPU userspace and
/// recorded it here. There is deliberately no live EGL probe: on some boards a
/// mismatched driver stack hangs the box when poked, and the kiosk runs on
/// every boot. A GPU launch that still crash-loops downgrades to software.
pub const RENDER_MARKER_PATH: &str = "/etc/ados/kiosk-render.conf";

/// The two renderers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    Gpu,
    Software,
}

/// Browser binary names, in resolution order (the name varies by distro).
pub const BROWSER_CANDIDATES: &[&str] =
    &["chromium-browser", "chromium", "chromium-browser-stable"];

/// Profile/cache subdirectory inside a session user's runtime dir.
pub const KIOSK_STORAGE_SUBDIR: &str = "ados-kiosk";

/// Upper bound on the browser disk cache. It lives in tmpfs, so this is a RAM
/// budget.
pub const DISK_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Browser storage for the cage launch: the agent's tmpfs runtime dir, which
/// exists and is writable by the root-run child. Never `$HOME` (the SD card,
/// rewritten continuously by a live SPA) and never `/run/user/0` (absent for a
/// system service).
pub fn cage_storage_dir() -> PathBuf {
    ados_protocol::paths::run_dir().join("kiosk")
}

/// `XDG_RUNTIME_DIR` for cage. logind creates `/run/user/0` only for a login
/// session, so on an appliance the unit's value does not exist and cage cannot
/// bind its Wayland socket; this tmpfs dir is created (mode 0700) before launch.
pub fn cage_runtime_dir() -> PathBuf {
    cage_storage_dir().join("runtime")
}

/// A renderer name or alias, or `None` when unrecognized.
pub fn normalise_renderer(value: &str) -> Option<Renderer> {
    match value.trim().to_lowercase().as_str() {
        "gpu" | "gles2" | "gles" | "egl" => Some(Renderer::Gpu),
        "software" | "pixman" | "sw" | "cpu" => Some(Renderer::Software),
        _ => None,
    }
}

/// `(renderer, lib_dir)` from the marker text (`key: value` or `key = value`
/// lines, `#` comments). `lib_dir` is the private dir holding the scoped GPU
/// userspace libraries.
pub fn parse_render_marker(text: &str) -> (Option<Renderer>, Option<String>) {
    let mut renderer = None;
    let mut lib_dir = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':').or_else(|| line.split_once('=')) else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_lowercase().as_str() {
            "renderer" => renderer = normalise_renderer(value),
            "lib_dir" if !value.is_empty() => lib_dir = Some(value.to_string()),
            _ => {}
        }
    }
    (renderer, lib_dir)
}

/// The marker at `path`, or `(None, None)` when unreadable.
pub fn read_render_marker(path: &Path) -> (Option<Renderer>, Option<String>) {
    std::fs::read_to_string(path)
        .map(|t| parse_render_marker(&t))
        .unwrap_or((None, None))
}

/// `(renderer, lib_dir)` for the launch: the env override, then the marker,
/// then software. A marker "gpu" whose `lib_dir` no longer exists falls back to
/// software; an env "gpu" keeps the marker's `lib_dir`.
pub fn resolve_render_plan(
    env: Option<&str>,
    marker: (Option<Renderer>, Option<String>),
) -> (Renderer, Option<String>) {
    match env.filter(|v| !v.is_empty()).and_then(normalise_renderer) {
        Some(Renderer::Software) => return (Renderer::Software, None),
        Some(Renderer::Gpu) => return (Renderer::Gpu, marker.1),
        None => {}
    }
    match marker {
        (Some(Renderer::Gpu), Some(dir)) if !Path::new(&dir).is_dir() => (Renderer::Software, None),
        (Some(Renderer::Gpu), lib_dir) => (Renderer::Gpu, lib_dir),
        _ => (Renderer::Software, None),
    }
}

/// [`resolve_render_plan`] from the process env and the shipped marker.
pub fn resolve_render_plan_from_env() -> (Renderer, Option<String>) {
    let env = std::env::var(ENV_RENDERER_KEY).ok();
    resolve_render_plan(
        env.as_deref(),
        read_render_marker(Path::new(RENDER_MARKER_PATH)),
    )
}

/// The environment overlay for the cage launch: the renderer, the DRM device,
/// the runtime dir and, for the GPU with a scoped userspace, `lib_dir`
/// prepended to `existing_ld` (so cage and the browser load the GPU libraries
/// without the system's being replaced). No `EGL_PLATFORM`: cage selects GBM
/// and the browser Wayland, each explicitly. Software disables the hardware
/// cursor, a known wlroots crash on some boards with pixman.
pub fn cage_env(
    renderer: Renderer,
    lib_dir: Option<&str>,
    drm_device: &str,
    existing_ld: Option<&str>,
) -> Vec<(String, String)> {
    let wlr = match renderer {
        Renderer::Gpu => "gles2",
        Renderer::Software => "pixman",
    };
    let mut env = vec![
        ("WLR_RENDERER".to_string(), wlr.to_string()),
        ("WLR_DRM_DEVICES".to_string(), drm_device.to_string()),
        (
            "XDG_RUNTIME_DIR".to_string(),
            cage_runtime_dir().to_string_lossy().into_owned(),
        ),
    ];
    match (renderer, lib_dir) {
        (Renderer::Gpu, Some(dir)) => {
            let ld = match existing_ld.filter(|v| !v.is_empty()) {
                Some(existing) => format!("{dir}:{existing}"),
                None => dir.to_string(),
            };
            env.push(("LD_LIBRARY_PATH".to_string(), ld));
        }
        (Renderer::Software, _) => {
            env.push(("WLR_NO_HARDWARE_CURSORS".to_string(), "1".to_string()));
        }
        _ => {}
    }
    env
}

/// The first browser candidate present in the `PATH`-style `path_var`, or an
/// error naming every candidate tried.
pub fn resolve_browser_in(path_var: &OsStr) -> Result<String, String> {
    for name in BROWSER_CANDIDATES {
        for dir in std::env::split_paths(path_var) {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Ok(candidate.to_string_lossy().into_owned());
            }
        }
    }
    Err(format!(
        "no Chromium browser binary found on PATH; tried: {}",
        BROWSER_CANDIDATES.join(", ")
    ))
}

/// [`resolve_browser_in`] over the process `PATH`.
pub fn resolve_browser_binary() -> Result<String, String> {
    resolve_browser_in(&std::env::var_os("PATH").unwrap_or_default())
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Browser flags for the renderer. The GPU path names no GL implementation:
/// the older `--use-gl=egl` spelling now resolves to "no implementation" and
/// the GPU process exits during init on a loop while the panel stays black,
/// and the default already is the ANGLE/EGL path. Software disables the GPU so
/// nothing opens an EGL the board cannot drive.
pub fn render_flags(renderer: Renderer) -> Vec<String> {
    match renderer {
        Renderer::Gpu => Vec::new(),
        Renderer::Software => vec!["--disable-gpu".to_string()],
    }
}

/// Profile and a bounded cache under `base_dir` (tmpfs): nothing the kiosk
/// shows is worth persisting, and a dedicated profile never collides with the
/// operator's own browser.
pub fn storage_flags(base_dir: &str) -> Vec<String> {
    let base = base_dir.trim_end_matches('/');
    vec![
        format!("--user-data-dir={base}/profile"),
        format!("--disk-cache-dir={base}/cache"),
        format!("--disk-cache-size={DISK_CACHE_BYTES}"),
    ]
}

/// `cage -- <browser> …` for the appliance launch. The browser runs as root
/// under cage, so it needs `--no-sandbox`.
pub fn cage_argv(browser: &str, url: &str, renderer: Renderer) -> Vec<String> {
    let mut argv: Vec<String> = [
        "cage",
        "--",
        browser,
        "--kiosk",
        "--noerrdialogs",
        "--disable-infobars",
        "--no-first-run",
        "--no-sandbox",
        "--ozone-platform=wayland",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    argv.extend(render_flags(renderer));
    argv.extend(storage_flags(&cage_storage_dir().to_string_lossy()));
    argv.push("--autoplay-policy=no-user-gesture-required".into());
    argv.push(url.into());
    argv
}

/// A full-screen browser window inside a running desktop session (no cage),
/// on the Ozone platform matching the session.
pub fn windowed_argv(
    browser: &str,
    url: &str,
    session_type: &str,
    renderer: Renderer,
    storage_dir: &str,
) -> Vec<String> {
    let platform = if session_type == "wayland" {
        "wayland"
    } else {
        "x11"
    };
    let mut argv: Vec<String> = [
        browser,
        "--kiosk",
        "--start-fullscreen",
        "--noerrdialogs",
        "--disable-infobars",
        "--no-first-run",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    argv.push(format!("--ozone-platform={platform}"));
    argv.extend(render_flags(renderer));
    argv.extend(storage_flags(storage_dir));
    argv.push("--autoplay-policy=no-user-gesture-required".into());
    argv.push(url.into());
    argv
}

#[cfg(test)]
mod tests;
