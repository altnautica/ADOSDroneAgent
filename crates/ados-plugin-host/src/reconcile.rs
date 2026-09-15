//! State reconciliation: keeping the served sockets and live tokens equal to
//! what the lifecycle controller wrote.
//!
//! The plugin host daemon and the lifecycle controller are different processes.
//! The controller (the Python supervisor behind the REST path, or this crate's
//! supervisor behind the cloud relay) installs, enables, disables and
//! grants — all of which are writes to the plugin state file plus `systemctl`
//! calls. The daemon is what actually *serves* a plugin: it binds
//! `<socket_dir>/<id>.sock` and writes the 0600 token env file the plugin's
//! unit reads.
//!
//! The daemon used to do that exactly once, at boot. Three failures came
//! straight out of that:
//!
//! * **A freshly enabled plugin was inert.** `enable` started the unit; nothing
//!   bound a socket or wrote a token, so the runner found neither and fell
//!   through to a null IPC client. systemd said active, the GCS said running,
//!   and the plugin did nothing at all — with no error surfaced anywhere. The
//!   worst failure shape there is: the operator sees success.
//! * **Tokens died at ten minutes.** The TTL is 600 s and nothing re-minted, so
//!   every gated call from every plugin began failing `token_expired` ten
//!   minutes after the daemon started serving.
//! * **A revoke was advisory.** Granted caps were baked into the token once per
//!   daemon start, so revoking `mavlink.write` from a misbehaving plugin
//!   reported success while the plugin kept commanding the flight controller.
//!
//! [`PluginReconciler`] is the answer to all three, because all three are the
//! same bug: a snapshot where a continuously-maintained equality was needed.
//! It is driven three ways, and none of them is a restart:
//!
//! 1. **On demand.** The lifecycle controllers poke `plugin.reconcile` /
//!    `token.rotate` on the control socket the moment they finish a write, so
//!    an enable or a revoke is effective before the CLI returns.
//! 2. **On a fixed poll.** [`POLL_INTERVAL`] re-reads the state file
//!    unconditionally. This is the backstop for a controller that could not
//!    reach the control socket (the daemon was restarting, a different
//!    lifecycle path wrote state), and it never gives up or backs off.
//! 3. **Ahead of expiry.** [`ROTATE_INTERVAL`] re-mints every served plugin's
//!    token at half the TTL, so the steady state never reaches an expired
//!    token at all. The on-expiry re-mint in `server.rs` is the second line
//!    under that, for a plugin whose session predates a daemon hiccup.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_protocol::plugin::TOKEN_TTL_SECONDS;
use tokio::task::JoinHandle;

use crate::host::HostServices;
use crate::manifest::PluginManifest;
use crate::server::PluginIpcServer;
use crate::state::{self, PluginStatus};
use crate::token_secret::TokenMint;

/// How often the state file is re-read when nothing pokes the control socket.
///
/// Fixed, not backed off: this is a recovery loop, and a recovery loop that
/// widens its interval turns a transient miss into a plugin that stays inert
/// for minutes. Two seconds costs one `stat` and, when the file changed, one
/// small JSON parse.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How often every served plugin's token is re-minted.
///
/// Half the TTL, so a token is always replaced with roughly five minutes of
/// life left — enough slack that a missed tick (a busy box, a suspended VM)
/// still lands before expiry.
pub const ROTATE_INTERVAL: Duration = Duration::from_secs((TOKEN_TTL_SECONDS / 2) as u64);

/// What a reconcile pass changed. Returned so the caller (and the control
/// socket's reply) can say something true about the outcome instead of "ok".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Plugins whose socket was bound by this pass.
    pub started: usize,
    /// Plugins whose socket was unbound by this pass.
    pub stopped: usize,
    /// Plugins served after this pass.
    pub serving: usize,
}

/// Keeps the daemon's served sockets and minted tokens equal to the state file.
pub struct PluginReconciler<H: HostServices> {
    server: Arc<PluginIpcServer<H>>,
    mint: Arc<TokenMint>,
    state_path: PathBuf,
    install_dir: PathBuf,
    /// Accept-loop handles for the plugins currently served, keyed by plugin id.
    /// Holding them is what makes a disable able to stop accepting, and what
    /// shutdown aborts.
    served: Mutex<BTreeMap<String, JoinHandle<()>>>,
}

impl<H: HostServices> PluginReconciler<H> {
    pub fn new(
        server: Arc<PluginIpcServer<H>>,
        mint: Arc<TokenMint>,
        state_path: PathBuf,
        install_dir: PathBuf,
    ) -> Self {
        PluginReconciler {
            server,
            mint,
            state_path,
            install_dir,
            served: Mutex::new(BTreeMap::new()),
        }
    }

    /// Plugin ids currently served.
    pub fn serving(&self) -> Vec<String> {
        self.served
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Bring the served set in line with on-disk state.
    ///
    /// Must run inside a tokio runtime (binding a socket spawns its accept
    /// task). Idempotent: a plugin already served is left alone, so the poll
    /// can run every two seconds without churning live connections.
    pub fn reconcile(&self) -> ReconcileReport {
        let installs = state::load_state(Some(&self.state_path));
        let mut report = ReconcileReport::default();

        // The set that SHOULD be served: enabled or running, with a subprocess
        // agent half. A built-in / inprocess / gcs-only plugin has no runner
        // socket, and a disabled one must not have a bound socket or a live
        // token.
        let mut wanted: Vec<String> = Vec::new();
        for install in &installs {
            if !matches!(
                install.status,
                PluginStatus::Enabled | PluginStatus::Running
            ) {
                continue;
            }
            let Some(manifest) = read_plugin_manifest(&self.install_dir, &install.plugin_id) else {
                continue;
            };
            if !manifest.is_subprocess_agent() {
                continue;
            }
            wanted.push(install.plugin_id.clone());
        }

        // ---- stop what should no longer be served ----------------------
        let stale: Vec<String> = self
            .serving()
            .into_iter()
            .filter(|id| !wanted.contains(id))
            .collect();
        for id in stale {
            if let Ok(mut map) = self.served.lock() {
                if let Some(handle) = map.remove(&id) {
                    handle.abort();
                }
            }
            self.server.stop_plugin(&id);
            self.mint.forget(&id);
            report.stopped += 1;
            tracing::info!(plugin_id = %id, "stopped serving plugin socket");
        }

        // ---- start what should be served -------------------------------
        for id in &wanted {
            let already = self
                .served
                .lock()
                .map(|m| m.contains_key(id))
                .unwrap_or(false);
            if already {
                continue;
            }
            match self.server.serve_plugin(id) {
                Ok((path, handle)) => {
                    if let Ok(mut map) = self.served.lock() {
                        map.insert(id.clone(), handle);
                    }
                    // Mint before the plugin can connect. The token env file is
                    // what the unit's `EnvironmentFile=` reads, so writing it
                    // here (rather than only at daemon boot) is what makes an
                    // enable effective without a restart.
                    if self.mint.mint_current(id).is_none() {
                        tracing::warn!(
                            plugin_id = %id,
                            "served the socket but could not mint a token; the plugin \
                             will keep retrying until one appears"
                        );
                    }
                    report.started += 1;
                    tracing::info!(
                        plugin_id = %id,
                        socket = %path.display(),
                        "serving plugin socket"
                    );
                }
                Err(e) => {
                    tracing::warn!(plugin_id = %id, error = %e, "failed to bind plugin socket");
                }
            }
        }

        report.serving = self.serving().len();
        report
    }

    /// Re-mint one plugin's token and push it into its live session.
    ///
    /// This is what makes a grant or revoke immediate. `mint_current` reads the
    /// grant set off state, so the fresh token cannot carry a capability that
    /// was just revoked; pushing it into the open connection means the next
    /// request re-gates against the new set with no restart and no dropped
    /// session.
    ///
    /// `Ok(false)` means the token was re-minted to the env file but no session
    /// was open to push it to, which is the correct outcome for an enabled
    /// plugin that has not connected yet.
    pub fn rotate_token(&self, plugin_id: &str) -> Result<bool, String> {
        let Some(token) = self.mint.mint_current(plugin_id) else {
            return Err(format!(
                "plugin {plugin_id} is not installed or is not enabled; nothing to rotate"
            ));
        };
        Ok(self
            .server
            .refresh_registry()
            .push(plugin_id, token))
    }

    /// Re-mint every served plugin's token. Driven by [`ROTATE_INTERVAL`] so
    /// the steady state never reaches expiry.
    pub fn rotate_all(&self) -> usize {
        let mut rotated = 0;
        for id in self.serving() {
            match self.rotate_token(&id) {
                Ok(_) => rotated += 1,
                Err(e) => tracing::debug!(plugin_id = %id, detail = %e, "token rotation skipped"),
            }
        }
        rotated
    }

    /// Abort every accept loop and unlink every served socket.
    pub fn shutdown(&self) {
        let served: Vec<(String, JoinHandle<()>)> = self
            .served
            .lock()
            .map(|mut m| std::mem::take(&mut *m).into_iter().collect())
            .unwrap_or_default();
        for (id, handle) in served {
            handle.abort();
            self.server.stop_plugin(&id);
        }
    }
}

impl<H: HostServices> crate::control::LifecycleControl for PluginReconciler<H> {
    fn rotate_token(&self, plugin_id: &str) -> Result<bool, String> {
        PluginReconciler::rotate_token(self, plugin_id)
    }

    fn reconcile(&self) -> (usize, usize, usize) {
        let report = PluginReconciler::reconcile(self);
        (report.started, report.stopped, report.serving)
    }
}

/// Read a plugin's manifest off its unpacked install dir. `None` when the
/// manifest is missing or unparseable, in which case the plugin is skipped
/// rather than the whole pass failing.
fn read_plugin_manifest(install_dir: &Path, plugin_id: &str) -> Option<PluginManifest> {
    let path = install_dir.join(plugin_id).join("manifest.yaml");
    let text = std::fs::read_to_string(&path).ok()?;
    match PluginManifest::from_yaml_text(&text) {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!(
                plugin_id,
                path = %path.display(),
                error = %e.0,
                "skipping plugin with an unparseable manifest"
            );
            None
        }
    }
}

/// Spawn the two background loops: the state poll and the token rotation.
///
/// Returns their join handles so shutdown can abort them. Both are infinite and
/// unconditional — no attempt cap, no backoff, no terminal state an operator
/// has to clear.
pub fn spawn_loops<H: HostServices + 'static>(
    reconciler: Arc<PluginReconciler<H>>,
) -> (JoinHandle<()>, JoinHandle<()>) {
    let poll_target = reconciler.clone();
    let poll = tokio::spawn(async move {
        // `interval` fires its first tick immediately; start one period out
        // instead. The caller has already run the first pass synchronously, and
        // an immediate duplicate would re-read state for nothing.
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + POLL_INTERVAL,
            POLL_INTERVAL,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let report = poll_target.reconcile();
            if report.started > 0 || report.stopped > 0 {
                tracing::info!(
                    started = report.started,
                    stopped = report.stopped,
                    serving = report.serving,
                    "reconciled plugin sockets from state"
                );
            }
        }
    });

    let rotate_target = reconciler;
    let rotate = tokio::spawn(async move {
        // Same offset, and here it is load-bearing rather than tidy: an
        // immediate first tick would rotate tokens that were minted seconds
        // ago in the first reconcile pass, pushing a pointless `token.refresh`
        // at every plugin the instant it connects.
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + ROTATE_INTERVAL,
            ROTATE_INTERVAL,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let rotated = rotate_target.rotate_all();
            if rotated > 0 {
                tracing::debug!(rotated, "rotated plugin capability tokens ahead of expiry");
            }
        }
    });


    (poll, rotate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_lands_well_inside_the_token_ttl() {
        // The whole point of the proactive rotation: a token must be replaced
        // with real life left on it, so a missed tick on a busy box does not
        // produce the expiry this loop exists to prevent.
        assert!(
            (ROTATE_INTERVAL.as_secs() as i64) * 2 <= TOKEN_TTL_SECONDS,
            "rotation interval {}s does not leave slack inside the {TOKEN_TTL_SECONDS}s TTL",
            ROTATE_INTERVAL.as_secs()
        );
    }

    #[test]
    fn the_state_poll_is_a_fixed_short_interval() {
        // A recovery loop that backs off turns a transient miss into a plugin
        // inert for minutes, so this is pinned rather than left to taste.
        assert!(POLL_INTERVAL >= Duration::from_secs(2));
        assert!(POLL_INTERVAL <= Duration::from_secs(5));
    }
}
