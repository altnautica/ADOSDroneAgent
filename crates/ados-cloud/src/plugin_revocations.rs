//! The registry's plugin revocation list, merged into the local one daily.
//!
//! The plugin host enforces the local list at `/etc/ados/plugin-revocations.json`
//! (revoked signer key ids and revoked archive hashes). Once a day, alongside
//! the plugin update check, this service fetches `{registry}/v1/revoked.json`
//! from the same base the plugin registry queries use and merges it in
//! add-only: an entry the registry lists is added, and nothing the operator or
//! an earlier fetch put in the local list is ever removed. A failed fetch is
//! logged and retried at the next cycle.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_plugin_host::signing::{merge_revocations, parse_revocations};
use tokio::sync::watch;

/// The local revocation list the plugin host enforces.
pub const LOCAL_REVOCATIONS: &str = "/etc/ados/plugin-revocations.json";

/// Delay before the first fetch after start.
pub const FIRST_FETCH_DELAY: Duration = Duration::from_secs(60);

/// Interval between fetches.
pub const FETCH_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Bound on one fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on the list body; a revocation list is a few kilobytes.
const MAX_LIST_BYTES: usize = 1024 * 1024;

/// The registry's revocation list URL for a registry base.
pub fn revoked_url(registry_base: &str) -> String {
    format!("{}/v1/revoked.json", registry_base.trim_end_matches('/'))
}

/// Fetch the list at `url` and merge it add-only into `local`. Returns whether
/// the local list changed.
pub async fn fetch_and_merge(
    http: &reqwest::Client,
    url: &str,
    local: &Path,
) -> Result<bool, String> {
    let resp = http
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("revocation list fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "revocation list fetch answered HTTP {}",
            resp.status()
        ));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("revocation list read failed: {e}"))?;
    if body.len() > MAX_LIST_BYTES {
        return Err(format!(
            "revocation list is {} bytes, over the {MAX_LIST_BYTES} byte cap",
            body.len()
        ));
    }
    let incoming = parse_revocations(&body)?;
    let local = local.to_path_buf();
    tokio::task::spawn_blocking(move || merge_revocations(&local, &incoming))
        .await
        .map_err(|e| format!("revocation merge task failed: {e}"))?
}

/// Fetch and merge after [`FIRST_FETCH_DELAY`] and then every
/// [`FETCH_INTERVAL`] until `shutdown` turns true. Idle when there is no
/// registry base (local mode).
pub async fn run_daily_loop(
    registry_base: String,
    local: PathBuf,
    mut shutdown: watch::Receiver<bool>,
) {
    if registry_base.is_empty() {
        tracing::info!("plugin revocation fetch idle (no registry url)");
        return;
    }
    let http = match reqwest::Client::builder()
        .use_preconfigured_tls(crate::tls::client_config())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "plugin revocation fetch: http client unavailable");
            return;
        }
    };
    let url = revoked_url(&registry_base);
    let mut wait = FIRST_FETCH_DELAY;
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { return; }
                continue;
            }
            _ = tokio::time::sleep(wait) => {}
        }
        wait = FETCH_INTERVAL;
        match fetch_and_merge(&http, &url, &local).await {
            Ok(true) => tracing::info!("plugin_revocations_merged"),
            Ok(false) => tracing::debug!("plugin_revocations_unchanged"),
            Err(e) => tracing::warn!(error = %e, "plugin_revocations_fetch_failed"),
        }
    }
}

#[cfg(test)]
mod tests;
