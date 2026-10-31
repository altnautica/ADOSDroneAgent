//! Daily auto-update check for installed plugins.
//!
//! Once a day every enabled install is compared with the newest version the
//! plugin registry publishes, and one of four things happens:
//!
//! * **silent install**: a patch or minor bump with an unchanged permission set
//!   and code surface on a still-supported board, signed by the key the
//!   installed version was, for a plugin that is not pinned and has auto-update
//!   on. The archive is downloaded (it must be pinned by sha256), parsed, and
//!   every check below runs on the manifest inside it, the one that would be
//!   installed, never on the registry's copy. Its signer is verified before the
//!   running version is touched, then it goes through the supervisor's
//!   install, so the signature, compatibility and downgrade gates all apply;
//!   the prior grants are restored and the plugin is re-enabled. A refused
//!   archive rolls back to the still-installed old version.
//! * **notify**: a major bump, a permission change, a change to what the
//!   plugin can run (risk, declared services, spawn allowlist, binaries,
//!   payload files), a board the new version drops, or a different signer
//!   (another key, or an unsigned archive for a signed install). The operator
//!   gets an `update_available` notice and decides; a signer change needs a
//!   remove and a fresh install.
//! * **skipped**: pinned, auto-update off, nothing newer, or no registry row.
//!   The whole cycle is also deferred while the vehicle counts as armed
//!   (unknown or stale state counts as armed): an update stops the plugin,
//!   and a plugin feeding pose or setpoints must not stop in the air. The
//!   deferred cycle is rechecked every [`ENABLE_RETRY_INTERVAL`].
//! * **failed**: registry, download, archive, install or enable error,
//!   recorded on the install so the GCS shows it. The next daily check
//!   retries. When the update stopped the plugin and it could not be started
//!   again, the install is marked `reenable_pending` and the loop retries the
//!   enable every [`ENABLE_RETRY_INTERVAL`] until it succeeds or the operator
//!   enables, disables or removes the plugin.
//!
//! The engine is transport-free: the registry query, the archive download and
//! the notice go through [`UpdateSource`], which the service holding the
//! pairing credentials implements. A cycle is planned under the supervisor
//! lock ([`plan_cycle`]), each archive is fetched without it
//! ([`fetch_planned`]), and the install re-takes it ([`install_planned`]), so
//! a large download never blocks another lifecycle call.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_protocol::armed::ArmedProbe;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::archive::{parse_archive_bytes, ArchiveContents};
use crate::manifest::{BoardId, PluginManifest};
use crate::state::{self, now_ms, PluginInstall, PluginStatus};
use crate::supervisor::{PluginSupervisor, SIGNER_CHANGE_REASON};

mod install;
mod notice;

pub use install::{check_one, fetch_planned, install_planned, retry_pending_enables};
use install::{conclude_stop, stamp_check};
use notice::{notice, signer_change_notice};

/// The fixed interval between checks. A failed check waits for the next one;
/// there is no attempt cap.
pub const DAILY_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Delay before the first check, so it does not race the rest of boot.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);

/// The fixed interval between attempts to start a plugin an update left
/// stopped. There is no attempt cap.
pub const ENABLE_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// The newest published version of a plugin, as the registry reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRow {
    pub version: String,
    pub download_url: String,
    pub archive_sha256: String,
    /// The key the publisher signed the archive with, as the registry
    /// records it; empty for an unsigned row.
    pub signer_key_id: String,
}

/// The transport the engine runs over. Blocking: the engine runs on the
/// blocking pool alongside the supervisor's own filesystem and `systemctl`
/// work.
pub trait UpdateSource: Send + Sync {
    /// Whether the registry can be queried at all (paired, a registry URL
    /// known). `false` skips the whole cycle.
    fn ready(&self) -> bool;
    /// The newest version row for `plugin_id`, `Ok(None)` when the registry
    /// has no row. Implementations fetch the registry payload and hand it to
    /// [`latest_version_row`].
    fn latest(&self, plugin_id: &str) -> Result<Option<VersionRow>, String>;
    /// Download `url` (allowlist + size cap) and verify it against `sha256`.
    fn download(&self, url: &str, sha256: &str) -> Result<Vec<u8>, String>;
    /// Deliver an `update_available` notice to the operator.
    fn notify(&self, notice: &Value);
}

/// What one plugin's check did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    SilentInstall,
    Notify,
    Skipped,
    Failed,
}

/// The newest version row out of a registry `{plugin, versions}` payload
/// (versions are ordered newest first). An `error` body is an error.
pub fn latest_version_row(payload: &Value) -> Result<Option<VersionRow>, String> {
    if payload.get("plugin").is_none() {
        if let Some(err) = payload.get("error") {
            return Err(format!("registry error: {err}"));
        }
    }
    let Some(row) = payload
        .get("versions")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
    else {
        return Ok(None);
    };
    let text = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Some(VersionRow {
        version: text("version"),
        download_url: text("download_url"),
        archive_sha256: text("archive_sha256"),
        signer_key_id: text("signer_key_id"),
    }))
}

fn semver(v: &str) -> Option<(u64, u64, u64)> {
    let base = v.split(['-', '+']).next().unwrap_or(v);
    let mut parts = base.split('.').map(|p| p.parse::<u64>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

/// What a manifest lets the plugin run: its risk label, declared services,
/// spawn allowlist, binaries and payload files. A change to any of it is the
/// operator's to approve, like a permission change.
#[derive(Debug, PartialEq)]
struct CodeSurface {
    risk: String,
    services: Option<serde_norway::Value>,
    spawn: Vec<String>,
    binaries: BTreeMap<String, BTreeMap<String, String>>,
    payloads: BTreeSet<String>,
}

/// One install a cycle will check: a snapshot of its record, taken under the
/// supervisor lock.
#[derive(Debug, Clone)]
pub struct PlannedUpdate {
    pub install: PluginInstall,
}

/// A planned update's newer archive, downloaded and parsed without the lock.
#[derive(Debug)]
pub struct FetchedArchive {
    pub install: PluginInstall,
    pub row: VersionRow,
    pub contents: ArchiveContents,
}

/// Why a planned update ended before there was an archive to install.
#[derive(Debug)]
pub enum FetchStop {
    Skipped,
    Notify { version: String, notice: Value },
    Failed { version: String, error: String },
}

/// The enabled installs to check this cycle. Runs under the supervisor lock.
pub fn plan_cycle(supervisor: &mut PluginSupervisor) -> Vec<PlannedUpdate> {
    if let Err(e) = supervisor.refresh() {
        tracing::warn!(error = %e, "auto_update_state_unreadable");
        return Vec::new();
    }
    supervisor
        .installs()
        .iter()
        .filter(|i| matches!(i.status, PluginStatus::Enabled | PluginStatus::Running))
        .map(|install| PlannedUpdate {
            install: install.clone(),
        })
        .collect()
}

/// One pass over every enabled install by a single-owner caller. Every
/// checked install gets its check time stamped, whatever the outcome. While
/// `probe` says the vehicle counts as armed nothing is checked and the result
/// is empty.
pub fn run_cycle(
    supervisor: &mut PluginSupervisor,
    source: &dyn UpdateSource,
    board: Option<&BoardId>,
    probe: &dyn ArmedProbe,
) -> Vec<(String, Outcome)> {
    if probe.treated_as_armed() {
        tracing::warn!("plugin_auto_update_deferred_armed");
        return Vec::new();
    }
    plan_cycle(supervisor)
        .into_iter()
        .map(|planned| {
            let outcome = check_one(supervisor, &planned.install, source, board);
            stamp_check(supervisor, &planned.install.plugin_id);
            (planned.install.plugin_id, outcome)
        })
        .collect()
}

/// One pass over a supervisor shared with other callers: planned under the
/// lock, each archive fetched without it, each install under it again.
/// `None` when the vehicle counts as armed, before the cycle or before any
/// install it would make: the caller rechecks soon instead of a day later.
pub fn run_cycle_shared(
    supervisor: &Mutex<PluginSupervisor>,
    source: &dyn UpdateSource,
    board: Option<&BoardId>,
    probe: &dyn ArmedProbe,
) -> Option<Vec<(String, Outcome)>> {
    let lock = || supervisor.lock().unwrap_or_else(|p| p.into_inner());
    if probe.treated_as_armed() {
        tracing::warn!("plugin_auto_update_deferred_armed");
        return None;
    }
    let plans = plan_cycle(&mut lock());
    let mut results = Vec::with_capacity(plans.len());
    for planned in plans {
        let fetched = fetch_planned(&planned, source);
        if fetched.is_ok() && probe.treated_as_armed() {
            tracing::warn!(plugin_id = %planned.install.plugin_id, "plugin_auto_update_deferred_armed");
            return None;
        }
        let mut guard = lock();
        let id = planned.install.plugin_id;
        let outcome = match fetched {
            Ok(fetched) => install_planned(&mut guard, fetched, source, board),
            Err(stop) => conclude_stop(&mut guard, &id, stop, source),
        };
        stamp_check(&mut guard, &id);
        results.push((id, outcome));
    }
    Some(results)
}

/// Retry pending enables every [`ENABLE_RETRY_INTERVAL`], and run the check
/// once after [`FIRST_CHECK_DELAY`] and then every [`DAILY_INTERVAL`], until
/// `shutdown` turns true. A cycle whose source is not ready (unpaired) is
/// skipped, not retried early. A cycle deferred because the vehicle counts as
/// armed keeps its due time, so it runs at the first recheck after disarm.
pub async fn run_daily_loop(
    supervisor: Arc<Mutex<PluginSupervisor>>,
    source: Arc<dyn UpdateSource>,
    board: Option<BoardId>,
    probe: Arc<dyn ArmedProbe>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut next_cycle = tokio::time::Instant::now() + FIRST_CHECK_DELAY;
    loop {
        let wake = next_cycle.min(tokio::time::Instant::now() + ENABLE_RETRY_INTERVAL);
        tokio::select! {
            _ = tokio::time::sleep_until(wake) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
                continue;
            }
        }
        let cycle_due = tokio::time::Instant::now() >= next_cycle;
        let run_cycle_now = cycle_due && source.ready();
        if cycle_due && !run_cycle_now {
            tracing::debug!("auto_update_skip_unpaired");
        }
        let (sup, src, board, probe) = (
            Arc::clone(&supervisor),
            Arc::clone(&source),
            board.clone(),
            Arc::clone(&probe),
        );
        let joined = tokio::task::spawn_blocking(move || {
            retry_pending_enables(&mut sup.lock().unwrap_or_else(|p| p.into_inner()));
            run_cycle_now
                .then(|| run_cycle_shared(&sup, src.as_ref(), board.as_ref(), probe.as_ref()))
        })
        .await;
        let deferred = matches!(joined, Ok(Some(None)));
        if cycle_due && !deferred {
            next_cycle = tokio::time::Instant::now() + DAILY_INTERVAL;
        }
        match joined {
            Ok(Some(Some(results))) => {
                tracing::info!(checked = results.len(), "auto_update_cycle_done")
            }
            Ok(Some(None)) | Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "auto_update_cycle_crashed"),
        }
    }
}

#[cfg(test)]
mod tests;
