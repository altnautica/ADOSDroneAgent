//! HDMI kiosk: a full-screen browser on the ground station's HDMI output,
//! pointed at the agent-served cockpit (`ados-kiosk.service`).
//!
//! Lifecycle ([`run`]):
//! 1. Honour the config: `ground_station.display.type` of `lcd` / `none`, or
//!    `ground_station.kiosk.enabled: false`, stands the service down with exit
//!    0 (the unit's "nothing to run").
//! 2. Wait for a DRM display, polling at a fixed interval for as long as it
//!    takes, so a monitor plugged in long after boot still gets the cockpit.
//! 3. Resolve the target URL (config, then `ADOS_KIOSK_URL`, then the default)
//!    and wait, bounded, for it to be served.
//! 4. Launch adaptively: inside a running desktop session, a full-screen
//!    browser window as the session user (cage cannot run there: the desktop
//!    compositor holds the DRM master); otherwise under cage, which owns the
//!    display. On the cage path a configured `720p` / `1080p` sets the output
//!    mode with `wlr-randr`.
//! 5. Supervise the child with a fixed retry interval. Five exits in 60 s
//!    downgrade a GPU cage launch to software once, else end the service
//!    non-zero so systemd restarts it. The config is re-checked on a fixed
//!    cadence: an `enabled` / `resolution` change restarts the child (or stands
//!    the service down).
//! 6. On SIGTERM: `SIGTERM` the child's process group, `SIGKILL` after 10 s;
//!    on the cage path also sweep orphaned cage / browser processes (never
//!    inside a desktop, where that would kill the operator's own browser).

use std::path::Path;

pub mod compositor;
pub mod drm;
pub mod launcher;
mod service;
pub mod session;
pub mod settings;
pub mod supervisor;

pub use service::run;

use drm::{resolve_drm_device, DrmPaths};
use launcher::{
    cage_argv, cage_env, cage_runtime_dir, cage_storage_dir, windowed_argv, Renderer,
    KIOSK_STORAGE_SUBDIR,
};
use session::{session_env, DesktopSession, RUN_USER_ROOT};
use supervisor::ChildSpec;

/// The child spec for a (session, renderer) combination.
///
/// Windowed (a live desktop): the session's display-server env, run as the
/// session user (keeping the browser sandbox), storage under that user's
/// runtime dir, no orphan sweep. The resolved renderer is honoured: a desktop
/// with its own GL and video stack must not be forced to software, and a GPU
/// launch that cannot bring up GL still downgrades through the crash path.
///
/// Cage (the appliance): `DISPLAY` / `WAYLAND_DISPLAY` / `XAUTHORITY` removed,
/// renderer, DRM device and scoped GPU libraries pinned, orphan sweep on.
pub fn make_spec(
    url: &str,
    session: Option<&DesktopSession>,
    renderer: Renderer,
    lib_dir: Option<&str>,
    output_mode: Option<&str>,
    browser: &str,
    drm: &DrmPaths,
) -> ChildSpec {
    if let Some(session) = session {
        let storage = format!("{RUN_USER_ROOT}/{}/{KIOSK_STORAGE_SUBDIR}", session.uid);
        return ChildSpec {
            argv: windowed_argv(browser, url, &session.session_type, renderer, &storage),
            env: session_env(session),
            env_unset: Vec::new(),
            run_as_uid: Some(session.uid),
            sweep_orphans: false,
            output_mode: None,
        };
    }
    let existing_ld = std::env::var("LD_LIBRARY_PATH").ok();
    ChildSpec {
        argv: cage_argv(browser, url, renderer),
        env: cage_env(
            renderer,
            lib_dir,
            &resolve_drm_device(drm),
            existing_ld.as_deref(),
        ),
        env_unset: ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY"]
            .map(str::to_string)
            .to_vec(),
        run_as_uid: None,
        sweep_orphans: true,
        output_mode: output_mode.map(str::to_string),
    }
}

/// Create the cage path's tmpfs dirs: the browser storage (the browser will
/// not start when its profile's parent is missing) and cage's runtime dir,
/// mode 0700 as the XDG spec requires.
pub fn prepare_cage_dirs() {
    let storage = cage_storage_dir();
    if let Err(error) = std::fs::create_dir_all(&storage) {
        tracing::warn!(path = %storage.display(), %error, "kiosk_storage_dir_unavailable");
    }
    let runtime = cage_runtime_dir();
    if let Err(error) = create_private_dir(&runtime) {
        tracing::warn!(path = %runtime.display(), %error, "kiosk_runtime_dir_unavailable");
    }
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
