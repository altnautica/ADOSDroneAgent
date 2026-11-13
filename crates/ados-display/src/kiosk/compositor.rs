//! Probes and actions around the cage compositor: the browser liveness check,
//! the output-mode set, the orphan sweep and the GPU-failure heuristic.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use ados_proc::run::{run, RunOutcome};

use super::drm::{connected_outputs, DrmPaths};
use super::launcher::BROWSER_CANDIDATES;
use super::session::wayland_sockets;
use super::settings::{AUTO_MODE, TV_MODE};

/// How long the output-mode set keeps retrying while cage brings its Wayland
/// socket up, and how often.
pub const OUTPUT_MODE_WAIT: Duration = Duration::from_secs(30);
pub const OUTPUT_MODE_POLL: Duration = Duration::from_secs(1);

const WLR_RANDR_TIMEOUT: Duration = Duration::from_secs(10);
const PKILL_TIMEOUT: Duration = Duration::from_secs(3);
const PS_TIMEOUT: Duration = Duration::from_secs(5);

/// Substrings in a child's stderr that mark a GPU / EGL / renderer init
/// failure.
pub const GPU_FAILURE_MARKERS: &[&str] = &[
    "failed to create renderer",
    "failed to load driver",
    "eglinitialize",
    "egl_not_initialized",
    "dri2",
    "gbm",
    "could not match drm and vulkan",
    "no drm fd",
    "wlr_renderer",
];

/// True when a stderr tail carries a GPU-failure marker. A diagnostic hint
/// logged with the GPU→software downgrade; the downgrade does not depend on it.
pub fn looks_gpu_failure(stderr_tail: &str) -> bool {
    let low = stderr_tail.to_lowercase();
    GPU_FAILURE_MARKERS.iter().any(|m| low.contains(m))
}

/// True when any line of `ps -A -o args=` output is a browser process. Matched
/// on each process's `argv[0]` basename, never the whole command line: cage runs
/// as `cage -- /usr/bin/chromium …`, which names the browser without being one.
pub fn browser_in_ps(stdout: &str) -> bool {
    stdout.lines().any(|line| {
        let argv0 = line.trim().split(' ').next().unwrap_or("");
        let base = argv0.rsplit('/').next().unwrap_or(argv0);
        BROWSER_CANDIDATES.contains(&base)
    })
}

/// Whether a `ps` run shows a live browser. Any uncertainty (ps missing or
/// failing) reads as running: a false "gone" restarts a working kiosk, which
/// is worse than missing one failure.
pub fn browser_running_from(outcome: &RunOutcome) -> bool {
    match outcome {
        RunOutcome::Exited {
            code: Some(0),
            stdout,
            ..
        } => browser_in_ps(stdout),
        _ => true,
    }
}

/// [`browser_running_from`] over a live `ps`.
pub async fn browser_running() -> bool {
    browser_running_from(&run("ps", &["-A", "-o", "args="], &[], PS_TIMEOUT).await)
}

/// Best-effort sweep of orphaned cage and browser processes after cage is torn
/// down: `SIGTERM`, a second, then `SIGKILL`, by name. Only on the cage path,
/// where cage owns the only browser; inside a desktop a broad sweep would kill
/// the operator's own browser windows.
pub async fn sweep_orphans() {
    for name in ["cage", "chromium"] {
        pkill("-TERM", name).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        pkill("-KILL", name).await;
    }
}

async fn pkill(sig: &str, name: &str) {
    if let RunOutcome::SpawnFailed(error) =
        run("pkill", &[sig, "-f", name], &[], PKILL_TIMEOUT).await
    {
        tracing::debug!(sig, name, %error, "kiosk_pkill_skipped");
    }
}

/// The smallest mode a 4K panel advertises: at least this wide and tall.
const UHD_MIN: (u32, u32) = (3840, 2160);

/// The mode to set on one output that advertises `modes`, or `None` to keep
/// the one cage picked. A preset applies when the output offers it. Under
/// [`AUTO_MODE`] a 4K panel (a mode at least [`UHD_MIN`]) that offers
/// [`TV_MODE`] steps down to it; any other panel, QHD and ultrawide included,
/// keeps its preferred mode.
pub fn mode_for_output(requested: &str, modes: &BTreeSet<String>) -> Option<String> {
    if requested != AUTO_MODE {
        return modes.contains(requested).then(|| requested.to_string());
    }
    let uhd = modes
        .iter()
        .filter_map(|m| mode_size(m))
        .any(|(w, h)| w >= UHD_MIN.0 && h >= UHD_MIN.1);
    (uhd && modes.contains(TV_MODE)).then(|| TV_MODE.to_string())
}

/// `WIDTHxHEIGHT`, ignoring anything after the height's digits (`@60`).
fn mode_size(mode: &str) -> Option<(u32, u32)> {
    let (w, rest) = mode.split_once('x')?;
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    Some((w.parse().ok()?, rest[..digits].parse().ok()?))
}

/// Set `mode` (a preset or [`AUTO_MODE`], resolved per output by
/// [`mode_for_output`]) through `wlr-randr` over cage's Wayland socket in
/// `runtime_dir`. Retried every [`OUTPUT_MODE_POLL`] until the socket answers
/// or [`OUTPUT_MODE_WAIT`] passes; an output without a mode to set keeps the
/// one cage picked.
pub async fn apply_output_mode(
    mode: &str,
    runtime_dir: &Path,
    env: &[(String, String)],
    drm: &DrmPaths,
) {
    let mut pending: BTreeMap<String, String> = BTreeMap::new();
    for (name, modes) in connected_outputs(drm) {
        match mode_for_output(mode, &modes) {
            Some(set) => {
                pending.insert(name, set);
            }
            None if mode != AUTO_MODE => {
                tracing::warn!(output = %name, mode, "kiosk_output_mode_unsupported");
            }
            None => {}
        }
    }
    let deadline = Instant::now() + OUTPUT_MODE_WAIT;
    let mut last_error = "compositor socket never appeared".to_string();
    while !pending.is_empty() && Instant::now() < deadline {
        if let Some(socket) = wayland_sockets(runtime_dir).into_iter().next() {
            let mut child_env: Vec<(&str, &str)> = env
                .iter()
                .filter(|(k, _)| k != "WAYLAND_DISPLAY")
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            child_env.push(("WAYLAND_DISPLAY", socket.as_str()));
            for (name, set) in pending.clone() {
                let args = ["--output", name.as_str(), "--mode", set.as_str()];
                match run("wlr-randr", &args, &child_env, WLR_RANDR_TIMEOUT).await {
                    RunOutcome::SpawnFailed(error) => {
                        tracing::warn!(mode, %error, "kiosk_output_mode_unavailable");
                        return;
                    }
                    RunOutcome::Exited { code: Some(0), .. } => {
                        tracing::info!(output = %name, mode = %set, "kiosk_output_mode_applied");
                        pending.remove(&name);
                    }
                    RunOutcome::Exited { stderr, .. } => last_error = stderr.trim().to_string(),
                    RunOutcome::WaitFailed(e) => last_error = e,
                    RunOutcome::TimedOut => last_error = "wlr-randr timed out".into(),
                }
            }
        }
        if !pending.is_empty() {
            tokio::time::sleep(OUTPUT_MODE_POLL).await;
        }
    }
    for (name, set) in pending {
        tracing::warn!(output = %name, mode = %set, error = %last_error, "kiosk_output_mode_failed");
    }
}

#[cfg(test)]
mod tests;
