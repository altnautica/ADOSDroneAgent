//! DRM display probing and the cockpit-URL readiness wait.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::settings::SettingsWatch;

/// Poll interval for a display to appear. The KMS device can show up seconds
/// after boot and a monitor can be plugged in at any time, so the wait has no
/// deadline.
pub const DISPLAY_POLL: Duration = Duration::from_secs(5);

/// How long to wait for the cockpit URL to be served before launching anyway.
/// A browser started before the agent front serves the page sticks on an error
/// page with no retry.
pub const URL_WAIT: Duration = Duration::from_secs(90);
pub const URL_POLL: Duration = Duration::from_secs(1);
pub const URL_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Fallback DRM device when no connected connector names its card.
pub const DRM_DEVICE: &str = "/dev/dri/card0";

/// Where the DRM connector status and card nodes live.
#[derive(Debug, Clone)]
pub struct DrmPaths {
    /// `/sys/class/drm` (holds `card*-*/status` and `modes`).
    pub sysfs: PathBuf,
    /// `/dev/dri` (holds the `card*` nodes).
    pub dev: PathBuf,
}

impl Default for DrmPaths {
    fn default() -> Self {
        Self {
            sysfs: PathBuf::from("/sys/class/drm"),
            dev: PathBuf::from("/dev/dri"),
        }
    }
}

/// Connector directories (`card1-HDMI-A-1`) under `sysfs`, sorted.
fn connectors(sysfs: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(sysfs) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("card") && n.contains('-'))
        })
        .collect();
    dirs.sort();
    dirs
}

fn is_connected(connector: &Path) -> bool {
    std::fs::read_to_string(connector.join("status")).is_ok_and(|s| s.trim() == "connected")
}

/// True when a DRM display is available: a connector on any card reports
/// `connected` (on some boards the display is `card1`, not `card0`), or, when
/// no status reads connected, any `card*` node exists (DRM is up but the
/// connector state is unreadable).
pub fn hdmi_present(paths: &DrmPaths) -> bool {
    if connectors(&paths.sysfs).iter().any(|c| is_connected(c)) {
        return true;
    }
    std::fs::read_dir(&paths.dev).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("card"))
    })
}

/// The DRM card that drives a display: the card of the first connected
/// connector whose `/dev/dri` node exists, else [`DRM_DEVICE`]. On a Pi 4 the
/// render node is `card0` and the display `card1`; a compositor handed `card0`
/// there has nothing to modeset.
pub fn resolve_drm_device(paths: &DrmPaths) -> String {
    for connector in connectors(&paths.sysfs) {
        if !is_connected(&connector) {
            continue;
        }
        let name = connector.file_name().unwrap_or_default().to_string_lossy();
        let card = name.split('-').next().unwrap_or_default();
        let node = paths.dev.join(card);
        if node.exists() {
            return node.to_string_lossy().into_owned();
        }
    }
    DRM_DEVICE.to_string()
}

/// Connected connectors (by the name wlroots uses, without the card prefix:
/// `card1-HDMI-A-1` → `HDMI-A-1`) and the `WIDTHxHEIGHT` modes each advertises.
pub fn connected_outputs(paths: &DrmPaths) -> BTreeMap<String, BTreeSet<String>> {
    let mut outputs = BTreeMap::new();
    for connector in connectors(&paths.sysfs) {
        if !is_connected(&connector) {
            continue;
        }
        let Ok(modes) = std::fs::read_to_string(connector.join("modes")) else {
            continue;
        };
        let name = connector.file_name().unwrap_or_default().to_string_lossy();
        let Some((_, output)) = name.split_once('-') else {
            continue;
        };
        outputs.insert(
            output.to_string(),
            modes.split_whitespace().map(str::to_string).collect(),
        );
    }
    outputs
}

/// Wait for a display, re-probing every `poll` with no deadline. Returns
/// `true` once one is present, `false` when the kiosk is disabled meanwhile
/// (the config is re-checked on the same tick).
pub async fn wait_for_display(watch: &mut SettingsWatch, paths: &DrmPaths, poll: Duration) -> bool {
    if hdmi_present(paths) {
        return true;
    }
    tracing::info!(poll_s = poll.as_secs_f64(), "kiosk_waiting_for_display");
    loop {
        tokio::time::sleep(poll).await;
        if watch.poll() && !watch.settings.enabled {
            return false;
        }
        if hdmi_present(paths) {
            return true;
        }
    }
}

/// Whether an HTTP status means the page is served. 2xx/3xx serve; 401, 403
/// and 405 still prove the server is up and routing.
pub fn serving_status(status: u16) -> bool {
    (200..400).contains(&status) || matches!(status, 401 | 403 | 405)
}

/// True when `url` answers with a serving status (redirects followed). A
/// refused connection or an error status is not-ready.
pub fn url_serving(url: &str) -> bool {
    let agent = ureq::AgentBuilder::new().timeout(URL_PROBE_TIMEOUT).build();
    match agent.get(url).call() {
        Ok(resp) => serving_status(resp.status()),
        Err(ureq::Error::Status(code, _)) => serving_status(code),
        Err(ureq::Error::Transport(_)) => false,
    }
}

/// Wait up to `wait` (probing every `poll` with `probe`) for the URL to be
/// served. `false` after the deadline: the caller launches anyway, so a
/// genuinely wrong URL still shows something.
pub async fn wait_for_url_with<F>(url: &str, wait: Duration, poll: Duration, probe: F) -> bool
where
    F: Fn(String) -> bool + Clone + Send + 'static,
{
    let check = |probe: F| {
        let url = url.to_string();
        async move {
            tokio::task::spawn_blocking(move || probe(url))
                .await
                .unwrap_or(false)
        }
    };
    if check(probe.clone()).await {
        return true;
    }
    tracing::info!(url, timeout_s = wait.as_secs_f64(), "kiosk_waiting_for_url");
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        tokio::time::sleep(poll).await;
        if check(probe.clone()).await {
            return true;
        }
    }
    tracing::warn!(url, "kiosk_url_wait_timeout");
    false
}

/// [`wait_for_url_with`] over a real HTTP probe and the shipped timings.
pub async fn wait_for_url(url: &str) -> bool {
    wait_for_url_with(url, URL_WAIT, URL_POLL, |u| url_serving(&u)).await
}

#[cfg(test)]
mod tests;
