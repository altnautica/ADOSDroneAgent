//! State reconciliation: keeping the served sockets and live tokens equal to
//! what the lifecycle controller wrote.
//!
//! The plugin host daemon and the lifecycle controller are different processes.
//! The controller (the Python supervisor behind the REST path, or this crate's
//! supervisor behind the cloud relay) installs, enables, disables and
//! grants — all of which are writes to the plugin state file plus `systemctl`
//! calls. The daemon is what actually *serves* a plugin: it binds
//! `<socket_dir>/<id>/host.sock` and writes the 0600 token credential the
//! plugin's unit loads.
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use ados_protocol::plugin::TOKEN_TTL_SECONDS;
use tokio::task::JoinHandle;

use crate::handlers::SharedTopic;
use crate::host::HostServices;
use crate::manifest::{PluginManifest, SharedTopicDecl};
use crate::server::PluginIpcServer;
use crate::state::{self, PluginStatus};
use crate::supervisor::{InstallVerifier, TAMPERED_REASON};
use crate::token_secret::TokenMint;

mod facts;
mod lifecycle;
mod units;

pub use lifecycle::spawn_loops;

/// The unit operations the reconciler cannot do itself: it serves sockets
/// and tokens, while the service manager runs the units. The daemon backs
/// this with its lifecycle controller.
pub trait UnitControl: Send + Sync {
    /// Re-render every installed plugin's units (the FC port mask moved).
    fn refresh_units(&self);
    /// Stop every unit of a plugin that failed its tamper check.
    fn stop_plugin_units(&self, plugin_id: &str);
}

/// How long one read of the router state socket may take inside a poll.
const FC_STATE_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// How often the state file is re-read when nothing pokes the control socket.
///
/// Fixed, not backed off: this is a recovery loop, and a recovery loop that
/// widens its interval turns a transient miss into a plugin that stays inert
/// for minutes. Each pass parses the state file and `stat`s every enabled
/// plugin's manifest; a manifest is re-parsed only when its mtime moved.
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
    /// The grant set baked into each served plugin's last minted token. A pass
    /// that finds state granting a different set re-mints and pushes, so a grant
    /// or revoke takes effect within one poll even when the controller's
    /// control-socket poke never arrived.
    minted: Mutex<BTreeMap<String, BTreeSet<String>>>,
    /// What the reconciler reads off each plugin's manifest, keyed by the
    /// manifest mtime it was read at, so an unchanged manifest is not re-parsed
    /// every poll.
    manifests: Mutex<BTreeMap<String, (SystemTime, ManifestFacts)>>,
    /// The single tamper check, cached by the stat stamp of the files it reads.
    verifier: InstallVerifier,
    /// The unit side, the router state socket and the run dir the live FC
    /// device is recorded in. `None` in a host that runs no units.
    units: Option<(Arc<dyn UnitControl>, PathBuf, PathBuf)>,
}

/// The parts of a manifest the reconciler acts on.
#[derive(Clone, Default)]
struct ManifestFacts {
    /// Whether it declares a subprocess agent half (the only kind served).
    subprocess: bool,
    /// The topics it shares on the event bus.
    shared_topics: Vec<SharedTopicDecl>,
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
            minted: Mutex::new(BTreeMap::new()),
            manifests: Mutex::new(BTreeMap::new()),
            verifier: InstallVerifier::default(),
            units: None,
        }
    }

    /// Give the reconciler the unit side: it stops a tampered plugin's units,
    /// and re-renders every unit when the router's live FC device (read from
    /// `state_socket`) moves, recording it under `run_dir`.
    pub fn with_unit_control(
        mut self,
        units: Arc<dyn UnitControl>,
        state_socket: PathBuf,
        run_dir: PathBuf,
    ) -> Self {
        self.units = Some((units, state_socket, run_dir));
        self
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
    /// can run every two seconds without churning live connections. A state
    /// file that exists but cannot be read or parsed skips the pass entirely:
    /// "unreadable" is not "nothing enabled", and tearing every plugin down on
    /// one failed read would remove the sockets and tokens a restarting unit
    /// needs.
    pub fn reconcile(&self) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        let installs = match state::load_state_checked(Some(&self.state_path)) {
            Ok(installs) => installs,
            Err(e) => {
                tracing::warn!(error = %e, "plugin state unreadable; keeping the served set");
                report.serving = self.serving().len();
                return report;
            }
        };

        // The set that SHOULD be served: enabled or running, with a subprocess
        // agent half. A built-in / inprocess / gcs-only plugin has no runner
        // socket, and a disabled one must not have a bound socket or a live
        // token.
        let mut wanted: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut tampered: Vec<(String, String)> = Vec::new();
        for install in &installs {
            if !matches!(
                install.status,
                PluginStatus::Enabled | PluginStatus::Running
            ) {
                continue;
            }
            if !self.is_subprocess_agent(&install.plugin_id) {
                continue;
            }
            if let Err(detail) = self.verifier.verify(&self.install_dir, install) {
                tampered.push((install.plugin_id.clone(), detail));
                continue;
            }
            wanted.insert(install.plugin_id.clone(), state::granted_caps(install));
        }
        self.verifier.retain(|id| wanted.contains_key(id));
        if !tampered.is_empty() {
            self.mark_tampered(&tampered);
        }
        if let Ok(mut cache) = self.manifests.lock() {
            cache.retain(|id, _| wanted.contains_key(id));
        }
        self.refresh_shared_topics(wanted.keys());

        // ---- stop what should no longer be served ----------------------
        let stale: Vec<String> = self
            .serving()
            .into_iter()
            .filter(|id| !wanted.contains_key(id))
            .collect();
        for id in stale {
            if let Ok(mut map) = self.served.lock() {
                if let Some(handle) = map.remove(&id) {
                    handle.abort();
                }
            }
            if let Ok(mut minted) = self.minted.lock() {
                minted.remove(&id);
            }
            self.server.stop_plugin(&id);
            self.mint.forget(&id);
            report.stopped += 1;
            tracing::info!(plugin_id = %id, "stopped serving plugin socket");
        }

        // ---- drop accept loops that ended on their own -----------------
        // A finished handle is a socket nobody accepts on; forgetting it lets
        // the start loop below bind it again instead of counting it as served.
        if let Ok(mut map) = self.served.lock() {
            map.retain(|id, handle| {
                let alive = !handle.is_finished();
                if !alive {
                    tracing::warn!(plugin_id = %id, "plugin accept loop ended; re-serving");
                }
                alive
            });
        }

        // ---- start what should be served -------------------------------
        for (id, caps) in &wanted {
            let already = self
                .served
                .lock()
                .map(|m| m.contains_key(id))
                .unwrap_or(false);
            if already {
                // Served already: re-mint only when the grant set moved since
                // the token the session holds was minted.
                let current = self.minted.lock().ok().and_then(|m| m.get(id).cloned());
                if current.as_ref() != Some(caps) {
                    match self.rotate_token(id) {
                        Ok(delivered) => tracing::info!(
                            plugin_id = %id,
                            delivered,
                            "grant set changed in state; re-minted the plugin token"
                        ),
                        Err(e) => tracing::warn!(plugin_id = %id, detail = %e, "re-mint failed"),
                    }
                }
                continue;
            }
            match self.server.serve_plugin(id) {
                Ok((path, handle)) => {
                    if let Ok(mut map) = self.served.lock() {
                        map.insert(id.clone(), handle);
                    }
                    // Mint before the plugin can connect. The token credential
                    // is what the unit's `LoadCredential=` reads, so writing it
                    // here (rather than only at daemon boot) is what makes an
                    // enable effective without a restart.
                    match self.mint.mint_current(id) {
                        Some(token) => self.record_minted(id, token.granted_caps),
                        None => tracing::warn!(
                            plugin_id = %id,
                            "served the socket but could not mint a token; the plugin \
                             will keep retrying until one appears"
                        ),
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

    fn record_minted(&self, plugin_id: &str, caps: BTreeSet<String>) {
        if let Ok(mut minted) = self.minted.lock() {
            minted.insert(plugin_id.to_string(), caps);
        }
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
        self.record_minted(plugin_id, token.granted_caps.clone());
        Ok(self.server.refresh_registry().push(plugin_id, token))
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

#[cfg(test)]
mod tests;
