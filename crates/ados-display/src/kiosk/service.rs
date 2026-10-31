//! The kiosk service body: config gate, waits, and the supervise loop.

use std::future::Future;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::compositor::looks_gpu_failure;
use super::drm::{wait_for_display, wait_for_url, DrmPaths, DISPLAY_POLL};
use super::launcher::{resolve_browser_binary, resolve_render_plan_from_env, Renderer};
use super::session::{resolve_desktop_session, DesktopSession};
use super::settings::{
    config_path, config_signature, display_selection, kiosk_settings, load_config,
    resolve_target_url_from_env, KioskSettings, SettingsWatch,
};
use super::supervisor::{KioskSupervisor, Stop};

/// What the service supervises: the real supervisor, or a test double.
pub(crate) trait Runner {
    fn stop_handle(&self) -> Arc<Stop>;
    fn crash_looped(&self) -> bool;
    fn stderr_tail(&self) -> String;
    fn run(&mut self) -> impl Future<Output = i32>;
}

impl Runner for KioskSupervisor {
    fn stop_handle(&self) -> Arc<Stop> {
        KioskSupervisor::stop_handle(self)
    }
    fn crash_looped(&self) -> bool {
        self.crash_looped
    }
    fn stderr_tail(&self) -> String {
        self.last_stderr_tail()
    }
    fn run(&mut self) -> impl Future<Output = i32> {
        KioskSupervisor::run(self)
    }
}

/// The config gate, before anything touches the panel.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Preflight {
    StandDown(&'static str),
    Launch(KioskSettings),
}

/// `lcd` / `none` mean another service owns the screen; a disabled kiosk has
/// nothing to run. Either is a clean stand-down.
pub(crate) fn preflight(config: &Value) -> Preflight {
    let display = display_selection(config);
    if display == "lcd" || display == "none" {
        return Preflight::StandDown(
            "the panel is assigned elsewhere by ground_station.display.type",
        );
    }
    let settings = kiosk_settings(config);
    if !settings.enabled {
        return Preflight::StandDown("ground_station.kiosk.enabled is false");
    }
    Preflight::Launch(settings)
}

/// The launch plan; the renderer changes on a GPU→software downgrade.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub url: String,
    pub session: Option<DesktopSession>,
    pub renderer: Renderer,
    pub lib_dir: Option<String>,
}

/// Run supervisors built by `make` until one ends for good. A settings change
/// restarts the child with the new output mode (or stands down when the kiosk
/// was disabled); a crash-looping GPU cage launch is retried once in software.
pub(crate) async fn supervise<R: Runner>(
    plan: &mut Plan,
    watch: &mut SettingsWatch,
    shutdown: &Stop,
    current: &Mutex<Option<Arc<Stop>>>,
    mut make: impl FnMut(&Plan, Option<&'static str>) -> Result<R, String>,
) -> i32 {
    let mut tried_software = plan.renderer == Renderer::Software;
    loop {
        if shutdown.is_set() {
            return 0;
        }
        let mut sup = match make(plan, watch.settings.output_mode) {
            Ok(sup) => sup,
            Err(error) => {
                tracing::error!(%error, "kiosk_binary_missing");
                return 3;
            }
        };
        let stop = sup.stop_handle();
        *current.lock().unwrap_or_else(|e| e.into_inner()) = Some(stop.clone());
        if shutdown.is_set() {
            stop.request();
        }
        match &plan.session {
            Some(s) => tracing::info!(
                session_type = %s.session_type,
                uid = s.uid,
                renderer = ?plan.renderer,
                "kiosk_desktop_session_detected"
            ),
            None => tracing::info!(renderer = ?plan.renderer, "kiosk_no_desktop_session"),
        }
        let mut changed = false;
        let rc = {
            let run = sup.run();
            tokio::pin!(run);
            tokio::select! {
                rc = &mut run => rc,
                () = watch.changed() => {
                    changed = true;
                    stop.request();
                    run.await
                }
            }
        };
        *current.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if shutdown.is_set() {
            tracing::info!(rc, "kiosk_service_stopped");
            return rc;
        }
        if changed {
            if !watch.settings.enabled {
                tracing::info!("kiosk_disabled_by_config");
                return 0;
            }
            tracing::info!(output_mode = ?watch.settings.output_mode, "kiosk_restarting_for_settings");
            continue;
        }
        if sup.crash_looped()
            && plan.session.is_none()
            && plan.renderer == Renderer::Gpu
            && !tried_software
        {
            let tail = sup.stderr_tail();
            tracing::error!(
                stderr_tail = %tail,
                gpu_failure = looks_gpu_failure(&tail),
                "kiosk_gpu_fallback"
            );
            plan.renderer = Renderer::Software;
            plan.lib_dir = None;
            tried_software = true;
            continue;
        }
        tracing::info!(rc, "kiosk_service_stopped");
        return rc;
    }
}

fn build_supervisor(
    plan: &Plan,
    output_mode: Option<&'static str>,
) -> Result<KioskSupervisor, String> {
    let browser = resolve_browser_binary()?;
    if plan.session.is_none() {
        super::prepare_cage_dirs();
    }
    Ok(KioskSupervisor::new(super::make_spec(
        &plan.url,
        plan.session.as_ref(),
        plan.renderer,
        plan.lib_dir.as_deref(),
        output_mode,
        &browser,
        &DrmPaths::default(),
    )))
}

fn exit_code(rc: i32) -> ExitCode {
    u8::try_from(rc).map_or(ExitCode::FAILURE, ExitCode::from)
}

/// Resolves on SIGTERM or SIGINT.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// The `ados-kiosk` service. See the [module docs](super).
pub async fn run() -> ExitCode {
    // Signature first: an edit landing between it and the load is then seen
    // as a change by the watch and re-read, never missed.
    let path = config_path();
    let signature = config_signature(&path);
    let config = match load_config(&path) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "kiosk_config_unreadable");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("kiosk_service_starting");
    let settings = match preflight(&config) {
        Preflight::StandDown(reason) => {
            tracing::info!(reason, "kiosk_disabled_by_config");
            return ExitCode::SUCCESS;
        }
        Preflight::Launch(settings) => settings,
    };
    let mut watch = SettingsWatch::new(path, settings, signature);

    let shutdown = Stop::new();
    let current: Arc<Mutex<Option<Arc<Stop>>>> = Arc::default();
    {
        let (shutdown, current) = (shutdown.clone(), current.clone());
        tokio::spawn(async move {
            stop_signal().await;
            tracing::info!("kiosk_service_signal_stop");
            shutdown.request();
            if let Some(stop) = current.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                stop.request();
            }
        });
    }

    let drm = DrmPaths::default();
    let shown = tokio::select! {
        shown = wait_for_display(&mut watch, &drm, DISPLAY_POLL) => shown,
        () = shutdown.wait() => return ExitCode::SUCCESS,
    };
    if !shown {
        tracing::info!("kiosk disabled while waiting for a display; stood down");
        return ExitCode::SUCCESS;
    }
    let (url, minimal) = resolve_target_url_from_env(&config);
    tokio::select! {
        _ = wait_for_url(&url) => {}
        () = shutdown.wait() => return ExitCode::SUCCESS,
    }
    let (renderer, lib_dir) = resolve_render_plan_from_env();
    let session = tokio::select! {
        session = resolve_desktop_session() => session,
        () = shutdown.wait() => return ExitCode::SUCCESS,
    };
    tracing::info!(
        url,
        minimal_layer = minimal,
        renderer = ?renderer,
        gpu_lib_dir = ?lib_dir,
        desktop_session = ?session.as_ref().map(|s| s.session_type.as_str()),
        "kiosk_target_resolved"
    );
    let mut plan = Plan {
        url,
        session,
        renderer,
        lib_dir,
    };
    exit_code(supervise(&mut plan, &mut watch, &shutdown, &current, build_supervisor).await)
}

#[cfg(test)]
mod tests;
