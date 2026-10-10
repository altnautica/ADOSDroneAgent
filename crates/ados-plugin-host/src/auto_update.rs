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

fn notice(install: &PluginInstall, latest: &str, reason: &str) -> Value {
    json!({
        "plugin_id": install.plugin_id,
        "current_version": install.version,
        "latest_version": latest,
        "reason": reason,
        "timestamp_ms": now_ms(),
    })
}

/// The notice for an update whose signer is not the installed version's.
fn signer_change_notice(install: &PluginInstall, latest: &str, offered: Option<&str>) -> Value {
    let mut n = notice(install, latest, SIGNER_CHANGE_REASON);
    n["current_signer"] = json!(install.signer_id);
    n["offered_signer"] = json!(offered);
    n
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

impl CodeSurface {
    fn of(manifest: &PluginManifest) -> Self {
        let agent = manifest.agent.as_ref();
        CodeSurface {
            risk: manifest.risk.clone(),
            services: agent
                .and_then(|a| a.extra.get("contributes"))
                .and_then(|c| c.get("services"))
                .cloned(),
            spawn: agent
                .map(|a| a.subprocess_spawn.clone())
                .unwrap_or_default(),
            binaries: agent.map(|a| a.binaries.clone()).unwrap_or_default(),
            payloads: agent
                .map(|a| a.payloads.iter().map(|p| p.path.clone()).collect())
                .unwrap_or_default(),
        }
    }
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

fn record(sup: &mut PluginSupervisor, id: &str, version: &str, outcome: &str, error: Option<&str>) {
    if let Err(e) = sup.note_update_attempt(id, version, outcome, error) {
        tracing::warn!(plugin_id = id, error = %e, "auto_update_record_failed");
    }
}

fn notify(
    sup: &mut PluginSupervisor,
    source: &dyn UpdateSource,
    id: &str,
    version: &str,
    n: Value,
) -> Outcome {
    source.notify(&n);
    record(sup, id, version, "notify", None);
    tracing::info!(plugin_id = id, notice = %n, "auto_update_notify");
    Outcome::Notify
}

fn fail(sup: &mut PluginSupervisor, id: &str, version: &str, e: String) -> Outcome {
    tracing::warn!(plugin_id = id, error = %e, "auto_update_install_failed");
    record(sup, id, version, "failed", Some(&e));
    Outcome::Failed
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

/// Query the registry for a planned update and download its newer archive.
/// Takes no lock: the download can be large.
pub fn fetch_planned(
    planned: &PlannedUpdate,
    source: &dyn UpdateSource,
) -> Result<FetchedArchive, FetchStop> {
    let install = &planned.install;
    let id = install.plugin_id.as_str();
    if install.pinned_version.is_some() || !install.auto_update {
        return Err(FetchStop::Skipped);
    }
    let row = match source.latest(id) {
        Ok(Some(row)) => row,
        Ok(None) => return Err(FetchStop::Skipped),
        Err(e) => {
            tracing::warn!(plugin_id = id, error = %e, "auto_update_registry_query_failed");
            return Err(FetchStop::Failed {
                version: "unknown".to_string(),
                error: e,
            });
        }
    };
    let (Some(latest), Some(current)) = (semver(&row.version), semver(&install.version)) else {
        return Err(FetchStop::Skipped);
    };
    if latest <= current {
        return Err(FetchStop::Skipped);
    }
    // A plugin id is bound to the key that installed it. The registry row
    // names the publishing key, so another key (or none, for a signed
    // install) is refused before anything is downloaded.
    let row_signer = (!row.signer_key_id.is_empty()).then_some(row.signer_key_id.as_str());
    if row_signer != install.signer_id.as_deref() {
        return Err(FetchStop::Notify {
            notice: signer_change_notice(install, &row.version, row_signer),
            version: row.version,
        });
    }
    if row.archive_sha256.trim().is_empty() {
        return Err(FetchStop::Failed {
            version: row.version,
            error: "registry row carries no archive_sha256; refusing an unpinned update"
                .to_string(),
        });
    }
    match fetch_archive(source, &row) {
        Ok(contents) => Ok(FetchedArchive {
            install: install.clone(),
            row,
            contents,
        }),
        Err(error) => Err(FetchStop::Failed {
            version: row.version,
            error,
        }),
    }
}

/// Record a planned update that ended before it had an archive.
fn conclude_stop(
    supervisor: &mut PluginSupervisor,
    id: &str,
    stop: FetchStop,
    source: &dyn UpdateSource,
) -> Outcome {
    match stop {
        FetchStop::Skipped => Outcome::Skipped,
        FetchStop::Notify { version, notice } => notify(supervisor, source, id, &version, notice),
        FetchStop::Failed { version, error } => fail(supervisor, id, &version, error),
    }
}

/// Check a fetched archive against the install and act: notify, or install
/// it silently. Runs under the supervisor lock, and first re-checks that the
/// install is still the version (and preference) the plan saw; anything that
/// changed during the download is left for the next cycle.
pub fn install_planned(
    supervisor: &mut PluginSupervisor,
    fetched: FetchedArchive,
    source: &dyn UpdateSource,
    board: Option<&BoardId>,
) -> Outcome {
    let FetchedArchive {
        install: planned,
        row,
        contents,
    } = fetched;
    let id = planned.plugin_id.as_str();
    if let Err(e) = supervisor.refresh() {
        tracing::warn!(error = %e, "auto_update_state_unreadable");
        return Outcome::Skipped;
    }
    let Some(install) = supervisor.find_install(id).cloned() else {
        return Outcome::Skipped;
    };
    if install.version != planned.version
        || install.pinned_version.is_some()
        || !install.auto_update
        || !matches!(
            install.status,
            PluginStatus::Enabled | PluginStatus::Running
        )
    {
        tracing::info!(plugin_id = id, "auto_update_install_changed_during_fetch");
        return Outcome::Skipped;
    }
    let (Some(latest), Some(current)) = (semver(&row.version), semver(&install.version)) else {
        return Outcome::Skipped;
    };
    // Every check below reads the manifest that would be installed.
    let remote = &contents.manifest;
    if remote.id != id {
        return fail(
            supervisor,
            id,
            &row.version,
            format!("archive is plugin {}, not {id}", remote.id),
        );
    }
    if remote.version != row.version {
        return fail(
            supervisor,
            id,
            &row.version,
            format!(
                "archive is version {} but the registry row is {}",
                remote.version, row.version
            ),
        );
    }
    // The registry's claim is not proof: the archive's own verified signer is
    // what the install would record, so it is checked before the running
    // version is touched.
    let signer = match supervisor.verified_signer(&contents) {
        Ok(signer) => signer,
        Err(e) => return fail(supervisor, id, &row.version, format!("signature: {e}")),
    };
    if signer.as_deref() != install.signer_id.as_deref() {
        let n = signer_change_notice(&install, &row.version, signer.as_deref());
        return notify(supervisor, source, id, &row.version, n);
    }
    if latest.0 > current.0 {
        let n = notice(&install, &row.version, "major_bump");
        return notify(supervisor, source, id, &row.version, n);
    }
    if let Some(board) = board {
        if !remote.compatibility.supports_board(board) {
            let n = notice(&install, &row.version, "board_mismatch");
            return notify(supervisor, source, id, &row.version, n);
        }
    }
    let current_manifest = supervisor.installed_manifest(id).ok();
    // Any change to the declared permission set, added or removed, is the
    // operator's to approve.
    let current_declared = current_manifest
        .as_ref()
        .map(PluginManifest::declared_permissions)
        .unwrap_or_default();
    let remote_declared = remote.declared_permissions();
    if remote_declared != current_declared {
        let mut n = notice(&install, &row.version, "permission_delta");
        n["new_permissions"] = json!(remote_declared
            .difference(&current_declared)
            .collect::<Vec<_>>());
        n["removed_permissions"] = json!(current_declared
            .difference(&remote_declared)
            .collect::<Vec<_>>());
        return notify(supervisor, source, id, &row.version, n);
    }
    if current_manifest.as_ref().map(CodeSurface::of) != Some(CodeSurface::of(remote)) {
        let n = notice(&install, &row.version, "code_surface_change");
        return notify(supervisor, source, id, &row.version, n);
    }

    match silent_install(supervisor, &install, contents, &row) {
        Ok(()) => {
            record(supervisor, id, &row.version, "success", None);
            tracing::info!(plugin_id = id, from = %install.version, to = %row.version, "auto_update_installed");
            Outcome::SilentInstall
        }
        Err(e) => fail(supervisor, id, &row.version, e),
    }
}

/// Check one install against the registry and act, holding the supervisor
/// throughout (a single-owner caller). Never fails: every error becomes a
/// recorded `Failed` outcome.
pub fn check_one(
    supervisor: &mut PluginSupervisor,
    install: &PluginInstall,
    source: &dyn UpdateSource,
    board: Option<&BoardId>,
) -> Outcome {
    let planned = PlannedUpdate {
        install: install.clone(),
    };
    match fetch_planned(&planned, source) {
        Ok(fetched) => install_planned(supervisor, fetched, source, board),
        Err(stop) => conclude_stop(supervisor, &install.plugin_id, stop, source),
    }
}

/// Download the row's archive and parse it.
fn fetch_archive(source: &dyn UpdateSource, row: &VersionRow) -> Result<ArchiveContents, String> {
    if row.download_url.is_empty() {
        return Err("no download_url on registry row".to_string());
    }
    let bytes = source
        .download(&row.download_url, &row.archive_sha256)
        .map_err(|e| format!("download: {e}"))?;
    parse_archive_bytes(bytes).map_err(|e| format!("archive: {e}"))
}

/// Stop, install the checked archive over, restore grants, re-enable. A
/// refused archive leaves the old version installed, so the rollback is a
/// re-enable. A plugin left stopped either way is marked for the enable retry.
fn silent_install(
    supervisor: &mut PluginSupervisor,
    install: &PluginInstall,
    contents: ArchiveContents,
    row: &VersionRow,
) -> Result<(), String> {
    let id = install.plugin_id.as_str();
    let granted: BTreeSet<String> = state::granted_caps(install);
    supervisor
        .disable(id)
        .map_err(|e| format!("disable: {e}"))?;
    if let Err(e) = supervisor.install_contents(contents, Path::new(&row.download_url)) {
        if let Err(re) = supervisor.enable(id) {
            tracing::warn!(plugin_id = id, error = %re, "auto_update_rollback_enable_failed");
            mark_reenable_pending(supervisor, id);
        }
        return Err(format!("install: {e}"));
    }
    for perm in &granted {
        if let Err(e) = supervisor.grant_permission(id, perm) {
            tracing::warn!(plugin_id = id, permission = %perm, error = %e, "auto_update_regrant_failed");
        }
    }
    if let Err(e) = supervisor.enable(id) {
        mark_reenable_pending(supervisor, id);
        return Err(format!("enable: {e}"));
    }
    Ok(())
}

fn mark_reenable_pending(supervisor: &mut PluginSupervisor, id: &str) {
    if let Err(e) = supervisor.note_reenable_pending(id) {
        tracing::warn!(plugin_id = id, error = %e, "auto_update_reenable_mark_failed");
    }
}

/// Try to start every plugin an update left stopped. A success clears the
/// mark (inside `enable`); a failure keeps it for the next attempt.
pub fn retry_pending_enables(supervisor: &mut PluginSupervisor) {
    if let Err(e) = supervisor.refresh() {
        tracing::warn!(error = %e, "auto_update_state_unreadable");
        return;
    }
    let pending: Vec<String> = supervisor
        .installs()
        .iter()
        .filter(|i| i.reenable_pending)
        .map(|i| i.plugin_id.clone())
        .collect();
    for id in pending {
        match supervisor.enable(&id) {
            Ok(()) => tracing::info!(plugin_id = %id, "auto_update_reenabled"),
            Err(e) => tracing::warn!(plugin_id = %id, error = %e, "auto_update_reenable_failed"),
        }
    }
}

fn stamp_check(supervisor: &mut PluginSupervisor, plugin_id: &str) {
    if let Err(e) = supervisor.note_update_check(plugin_id, now_ms()) {
        tracing::warn!(plugin_id, error = %e, "auto_update_check_stamp_failed");
    }
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
mod tests {
    use super::*;
    use crate::backend::RecordingBackend;
    use crate::supervisor::Paths;
    use std::io::Write;

    fn paths_in(dir: &Path) -> Paths {
        Paths {
            install_dir: dir.join("plugins"),
            unit_dir: dir.join("units"),
            state_path: dir.join("state/plugin-state.json"),
            log_dir: dir.join("logs"),
            control_dir: dir.join("plugin-host"),
            loopback_guard_state: dir.join("plugin-loopback-guard.json"),
            socket_dir: dir.join("sockets"),
            token_secret: dir.join("secrets/plugin-token-secret"),
            runner: dir.join("bin/ados-plugin-runner"),
            run_dir: dir.join("run"),
            data_root: dir.join("plugin-data"),
            device_id_file: dir.join("device-id"),
        }
    }

    fn manifest(version: &str, perms: &[&str]) -> String {
        let mut m = format!(
            "id: com.example.thermal\nversion: {version}\nrisk: high\ncompatibility:\n  \
             ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n"
        );
        for p in perms {
            m.push_str(&format!("    - {p}\n"));
        }
        m
    }

    fn archive(manifest_yaml: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("manifest.yaml", opts).unwrap();
            w.write_all(manifest_yaml.as_bytes()).unwrap();
            w.start_file("agent/py/x.py", opts).unwrap();
            w.write_all(b"print('hi')").unwrap();
            w.finish().unwrap();
        }
        buf
    }

    struct FakeSource {
        row: VersionRow,
        archive: Vec<u8>,
        notices: Mutex<Vec<Value>>,
        queried: Mutex<u32>,
        downloads: Mutex<u32>,
        /// How long each download takes (a slow mirror).
        delay: Duration,
    }

    /// A vehicle that reads disarmed.
    struct Disarmed;

    impl ArmedProbe for Disarmed {
        fn treated_as_armed(&self) -> bool {
            false
        }
    }

    /// An armed verdict the test flips.
    struct Toggle(std::sync::atomic::AtomicBool);

    impl ArmedProbe for Toggle {
        fn treated_as_armed(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl UpdateSource for FakeSource {
        fn ready(&self) -> bool {
            true
        }
        fn latest(&self, _: &str) -> Result<Option<VersionRow>, String> {
            *self.queried.lock().unwrap() += 1;
            Ok(Some(self.row.clone()))
        }
        fn download(&self, _: &str, _: &str) -> Result<Vec<u8>, String> {
            *self.downloads.lock().unwrap() += 1;
            std::thread::sleep(self.delay);
            Ok(self.archive.clone())
        }
        fn notify(&self, notice: &Value) {
            self.notices.lock().unwrap().push(notice.clone());
        }
    }

    fn source(version: &str, perms: &[&str]) -> FakeSource {
        FakeSource {
            row: VersionRow {
                version: version.to_string(),
                download_url: "https://example.com/p.adosplug".to_string(),
                archive_sha256: "ab".repeat(32),
                signer_key_id: String::new(),
            },
            archive: archive(&manifest(version, perms)),
            notices: Mutex::new(Vec::new()),
            queried: Mutex::new(0),
            downloads: Mutex::new(0),
            delay: Duration::ZERO,
        }
    }

    /// A backend whose `enable_start` fails while `failing` is set.
    struct FlakyBackend {
        inner: RecordingBackend,
        failing: std::sync::atomic::AtomicBool,
    }

    impl crate::backend::ServiceBackend for FlakyBackend {
        fn install(
            &self,
            name: &str,
            spec: &crate::backend::UnitSpec,
        ) -> Result<bool, crate::errors::SupervisorError> {
            self.inner.install(name, spec)
        }
        fn uninstall(&self, name: &str) -> Result<(), crate::errors::SupervisorError> {
            self.inner.uninstall(name)
        }
        fn enable_start(&self, name: &str) -> Result<(), crate::errors::SupervisorError> {
            if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::errors::SupervisorError(
                    "unit failed to start".into(),
                ));
            }
            self.inner.enable_start(name)
        }
        fn stop_disable(&self, name: &str) -> Result<(), crate::errors::SupervisorError> {
            self.inner.stop_disable(name)
        }
        fn restart(&self, name: &str) -> Result<(), crate::errors::SupervisorError> {
            self.inner.restart(name)
        }
        fn is_active(&self, name: &str) -> Result<bool, crate::errors::SupervisorError> {
            self.inner.is_active(name)
        }
        fn probe(
            &self,
            probe: &crate::backend::ProbeSpec,
        ) -> Result<(), crate::errors::SupervisorError> {
            self.inner.probe(probe)
        }
        fn enforces_sandbox(&self) -> bool {
            self.inner.enforces_sandbox()
        }
    }

    /// A throwaway Ed25519 key enrolled as `signer` in `keys_dir`.
    fn enrol(keys_dir: &Path, signer: &str) -> ed25519_dalek::SigningKey {
        use ed25519_dalek::pkcs8::{spki::der::pem::LineEnding, EncodePublicKey};
        use sha2::Digest;
        let key =
            ed25519_dalek::SigningKey::from_bytes(&sha2::Sha256::digest(signer.as_bytes()).into());
        std::fs::create_dir_all(keys_dir).unwrap();
        let pem = key
            .verifying_key()
            .to_public_key_pem(LineEnding::LF)
            .unwrap();
        std::fs::write(keys_dir.join(format!("{signer}.pem")), pem).unwrap();
        key
    }

    /// `archive` with a `SIGNATURE` entry by `signer` over its payload hash.
    fn signed(archive: Vec<u8>, signer: &str, key: &ed25519_dalek::SigningKey) -> Vec<u8> {
        use base64::Engine;
        use ed25519_dalek::Signer;
        let hash = parse_archive_bytes(archive.clone()).unwrap().payload_hash;
        let signature =
            base64::engine::general_purpose::STANDARD.encode(key.sign(&hash).to_bytes());
        let mut w = zip::ZipWriter::new_append(std::io::Cursor::new(archive)).unwrap();
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        w.start_file("SIGNATURE", opts).unwrap();
        w.write_all(format!("{signer}\n{signature}\n").as_bytes())
            .unwrap();
        w.finish().unwrap().into_inner()
    }

    fn running_v1(dir: &Path) -> PluginSupervisor {
        let mut sup = PluginSupervisor::new(paths_in(dir), false, None, "1.0.0")
            .with_backend(Arc::new(RecordingBackend::default()));
        let contents =
            crate::archive::parse_archive_bytes(archive(&manifest("1.0.0", &["hardware.spi"])))
                .unwrap();
        sup.install_contents(contents, Path::new("/tmp/v1.adosplug"))
            .unwrap();
        sup.grant_permission("com.example.thermal", "hardware.spi")
            .unwrap();
        sup.enable("com.example.thermal").unwrap();
        sup
    }

    #[test]
    fn a_registry_row_from_another_signer_notifies_without_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = running_v1(dir.path());
        let mut src = source("1.1.0", &["hardware.spi"]);
        src.row.signer_key_id = "someone-else".to_string();
        assert_eq!(
            run_cycle(&mut sup, &src, None, &Disarmed)[0].1,
            Outcome::Notify
        );
        let n = &src.notices.lock().unwrap()[0];
        assert_eq!(n["reason"], "signer_change");
        assert_eq!(n["offered_signer"], "someone-else");
        assert_eq!(*src.downloads.lock().unwrap(), 0);
        let rec = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.version, "1.0.0");
        assert_eq!(rec.status, PluginStatus::Running);
    }

    #[test]
    fn an_archive_whose_verified_signer_differs_is_refused_before_the_install_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let keys = dir.path().join("keys");
        let alice = enrol(&keys, "alice");
        let mallory = enrol(&keys, "mallory");
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "1.0.0")
            .with_backend(Arc::new(RecordingBackend::default()))
            .with_trusted_keys_dir(&keys);
        let v1 = signed(
            archive(&manifest("1.0.0", &["hardware.spi"])),
            "alice",
            &alice,
        );
        sup.install_contents(
            parse_archive_bytes(v1).unwrap(),
            Path::new("/tmp/v1.adosplug"),
        )
        .unwrap();
        sup.grant_permission("com.example.thermal", "hardware.spi")
            .unwrap();
        sup.enable("com.example.thermal").unwrap();

        // The row claims the installed key, but the archive it serves is
        // signed by another enrolled key, or not signed at all.
        let v11 = archive(&manifest("1.1.0", &["hardware.spi"]));
        for (served, offered) in [
            (signed(v11.clone(), "mallory", &mallory), json!("mallory")),
            (v11.clone(), Value::Null),
        ] {
            let mut src = source("1.1.0", &["hardware.spi"]);
            src.row.signer_key_id = "alice".to_string();
            src.archive = served;
            assert_eq!(
                run_cycle(&mut sup, &src, None, &Disarmed)[0].1,
                Outcome::Notify
            );
            let n = &src.notices.lock().unwrap()[0];
            assert_eq!(n["reason"], "signer_change");
            assert_eq!(n["current_signer"], "alice");
            assert_eq!(n["offered_signer"], offered);
            let rec = sup.find_install("com.example.thermal").unwrap();
            assert_eq!(rec.version, "1.0.0");
            assert_eq!(rec.status, PluginStatus::Running);
        }

        // The same key's build still updates silently.
        let mut src = source("1.1.0", &["hardware.spi"]);
        src.row.signer_key_id = "alice".to_string();
        src.archive = signed(v11, "alice", &alice);
        assert_eq!(
            run_cycle(&mut sup, &src, None, &Disarmed)[0].1,
            Outcome::SilentInstall
        );
        let rec = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.version, "1.1.0");
        assert_eq!(rec.signer_id.as_deref(), Some("alice"));
    }

    #[test]
    fn a_minor_bump_with_the_same_permissions_installs_and_keeps_the_grants() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = running_v1(dir.path());
        let src = source("1.1.0", &["hardware.spi"]);
        let results = run_cycle(&mut sup, &src, None, &Disarmed);
        assert_eq!(
            results,
            vec![("com.example.thermal".to_string(), Outcome::SilentInstall)]
        );
        let rec = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.version, "1.1.0");
        assert_eq!(rec.status, PluginStatus::Running);
        assert!(state::granted_caps(rec).contains("hardware.spi"));
        assert_eq!(
            rec.last_update_attempt.as_ref().unwrap()["outcome"],
            "success"
        );
        assert!(rec.last_update_check_at.is_some());
    }

    #[test]
    fn a_major_bump_or_a_permission_change_only_notifies() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = running_v1(dir.path());
        let major = source("2.0.0", &["hardware.spi"]);
        assert_eq!(
            run_cycle(&mut sup, &major, None, &Disarmed)[0].1,
            Outcome::Notify
        );
        assert_eq!(major.notices.lock().unwrap()[0]["reason"], "major_bump");

        let widened = source("1.2.0", &["hardware.spi", "mavlink.write"]);
        assert_eq!(
            run_cycle(&mut sup, &widened, None, &Disarmed)[0].1,
            Outcome::Notify
        );
        let n = &widened.notices.lock().unwrap()[0];
        assert_eq!(n["reason"], "permission_delta");
        assert_eq!(n["new_permissions"], json!(["mavlink.write"]));
        assert_eq!(
            sup.find_install("com.example.thermal").unwrap().version,
            "1.0.0"
        );
    }

    #[test]
    fn every_check_reads_the_archive_that_would_be_installed() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = running_v1(dir.path());

        // The archive is another plugin: refused, nothing touched.
        let mut other = source("1.1.0", &["hardware.spi"]);
        other.archive = archive(
            &manifest("1.1.0", &["hardware.spi"])
                .replace("com.example.thermal", "com.example.other"),
        );
        assert_eq!(
            run_cycle(&mut sup, &other, None, &Disarmed)[0].1,
            Outcome::Failed
        );

        // The archive fetches a payload binary the installed version did not,
        // under the same permission set: only the code-surface check sees it.
        let fetching = manifest("1.1.0", &["hardware.spi"])
            + "  payloads:\n    - path: bin/tool\n      \
               source: https://github.com/o/r/releases/download/v1/tool\n      \
               sha256: "
            + &"ab".repeat(32)
            + "\n      size_bytes: 10\n";
        let mut widened = source("1.1.0", &["hardware.spi"]);
        widened.archive = archive(&fetching);
        assert_eq!(
            run_cycle(&mut sup, &widened, None, &Disarmed)[0].1,
            Outcome::Notify
        );
        assert_eq!(
            widened.notices.lock().unwrap()[0]["reason"],
            "code_surface_change"
        );

        // A new risk label alone is a code-surface change too.
        let mut riskier = source("1.1.0", &["hardware.spi"]);
        riskier.archive =
            archive(&manifest("1.1.0", &["hardware.spi"]).replace("risk: high", "risk: critical"));
        assert_eq!(
            run_cycle(&mut sup, &riskier, None, &Disarmed)[0].1,
            Outcome::Notify
        );
        assert_eq!(
            riskier.notices.lock().unwrap()[0]["reason"],
            "code_surface_change"
        );

        // A row with no archive pin is refused before any download.
        let mut unpinned = source("1.1.0", &["hardware.spi"]);
        unpinned.row.archive_sha256 = String::new();
        assert_eq!(
            run_cycle(&mut sup, &unpinned, None, &Disarmed)[0].1,
            Outcome::Failed
        );
        assert_eq!(*unpinned.downloads.lock().unwrap(), 0);

        let rec = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.version, "1.0.0");
        assert_eq!(rec.status, PluginStatus::Running);
    }

    #[test]
    fn a_plugin_an_update_left_stopped_is_started_again_once_it_can() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FlakyBackend {
            inner: RecordingBackend::default(),
            failing: std::sync::atomic::AtomicBool::new(false),
        });
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "1.0.0")
            .with_backend(backend.clone());
        let v1 = parse_archive_bytes(archive(&manifest("1.0.0", &["hardware.spi"]))).unwrap();
        sup.install_contents(v1, Path::new("/tmp/v1.adosplug"))
            .unwrap();
        sup.enable("com.example.thermal").unwrap();

        // The new version installs but its unit will not start.
        backend
            .failing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let src = source("1.1.0", &["hardware.spi"]);
        assert_eq!(
            run_cycle(&mut sup, &src, None, &Disarmed)[0].1,
            Outcome::Failed
        );
        let rec = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.version, "1.1.0");
        assert!(rec.reenable_pending);
        // The daily cycle skips a stopped plugin, but the retry does not.
        retry_pending_enables(&mut sup);
        assert!(
            sup.find_install("com.example.thermal")
                .unwrap()
                .reenable_pending
        );

        backend
            .failing
            .store(false, std::sync::atomic::Ordering::SeqCst);
        retry_pending_enables(&mut sup);
        let rec = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.status, PluginStatus::Running);
        assert!(!rec.reenable_pending);
    }

    #[test]
    fn an_operator_disable_cancels_the_pending_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = running_v1(dir.path());
        sup.disable("com.example.thermal").unwrap();
        sup.note_reenable_pending("com.example.thermal").unwrap();
        sup.disable("com.example.thermal").unwrap();
        retry_pending_enables(&mut sup);
        let rec = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.status, PluginStatus::Disabled);
        assert!(!rec.reenable_pending);
    }

    #[test]
    fn a_pinned_plugin_is_never_queried() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = running_v1(dir.path());
        let mut install = sup.find_install("com.example.thermal").unwrap().clone();
        install.pinned_version = Some("1.0.0".to_string());
        let src = source("1.1.0", &["hardware.spi"]);
        assert_eq!(check_one(&mut sup, &install, &src, None), Outcome::Skipped);
        assert_eq!(*src.queried.lock().unwrap(), 0);
    }

    #[test]
    fn a_refused_archive_rolls_back_to_the_running_old_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = running_v1(dir.path());
        let mut src = source("1.1.0", &["hardware.spi"]);
        src.archive = b"not a zip".to_vec();
        assert_eq!(
            run_cycle(&mut sup, &src, None, &Disarmed)[0].1,
            Outcome::Failed
        );
        let rec = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.version, "1.0.0");
        assert_eq!(rec.status, PluginStatus::Running);
        assert_eq!(
            rec.last_update_attempt.as_ref().unwrap()["outcome"],
            "failed"
        );
    }

    #[test]
    fn the_registry_payload_yields_its_newest_row() {
        let payload = json!({"plugin": {}, "versions": [
            {"version": "1.2.0", "manifest_yaml": "x", "supported_boards": ["rpi4"],
             "download_url": "https://example.com/a", "archive_sha256": "ab"},
            {"version": "1.1.0"}]});
        let row = latest_version_row(&payload).unwrap().unwrap();
        assert_eq!(row.version, "1.2.0");
        assert_eq!(row.archive_sha256, "ab");
        assert!(latest_version_row(&json!({"error": "nope"})).is_err());
        assert_eq!(
            latest_version_row(&json!({"plugin": {}, "versions": []})).unwrap(),
            None
        );
    }

    #[test]
    fn auto_update_defers_while_armed_and_runs_after_disarm() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Mutex::new(running_v1(dir.path()));
        let src = source("1.1.0", &["hardware.spi"]);
        let probe = Toggle(std::sync::atomic::AtomicBool::new(true));

        // Armed: no query, no download, nothing stopped.
        assert!(run_cycle_shared(&sup, &src, None, &probe).is_none());
        assert!(run_cycle(&mut sup.lock().unwrap(), &src, None, &probe).is_empty());
        assert_eq!(*src.queried.lock().unwrap(), 0);
        assert_eq!(*src.downloads.lock().unwrap(), 0);
        {
            let guard = sup.lock().unwrap();
            let rec = guard.find_install("com.example.thermal").unwrap();
            assert_eq!(rec.version, "1.0.0");
            assert_eq!(rec.status, PluginStatus::Running);
        }

        // Disarmed: the deferred update runs.
        probe.0.store(false, std::sync::atomic::Ordering::SeqCst);
        let results = run_cycle_shared(&sup, &src, None, &probe).unwrap();
        assert_eq!(
            results,
            vec![("com.example.thermal".to_string(), Outcome::SilentInstall)]
        );
        let guard = sup.lock().unwrap();
        assert_eq!(
            guard.find_install("com.example.thermal").unwrap().version,
            "1.1.0"
        );
    }

    #[test]
    fn lock_free_during_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let sup = Arc::new(Mutex::new(running_v1(dir.path())));
        let mut src = source("1.1.0", &["hardware.spi"]);
        src.delay = Duration::from_millis(800);
        let src = Arc::new(src);
        let (cycle_sup, cycle_src) = (Arc::clone(&sup), Arc::clone(&src));
        let cycle = std::thread::spawn(move || {
            run_cycle_shared(&cycle_sup, cycle_src.as_ref(), None, &Disarmed)
        });
        // Wait until the download is under way.
        let started = std::time::Instant::now();
        while *src.downloads.lock().unwrap() == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "download never started"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Another lifecycle caller gets the supervisor within 100 ms.
        let asked = std::time::Instant::now();
        let guard = sup.lock().unwrap();
        assert!(
            asked.elapsed() < Duration::from_millis(100),
            "lock took {:?}",
            asked.elapsed()
        );
        drop(guard);
        let results = cycle.join().unwrap().unwrap();
        assert_eq!(results[0].1, Outcome::SilentInstall);
    }
}
