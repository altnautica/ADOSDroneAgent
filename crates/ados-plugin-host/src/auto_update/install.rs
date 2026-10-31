//! Fetching, verifying and installing a planned update.

use super::*;

pub(super) fn record(
    sup: &mut PluginSupervisor,
    id: &str,
    version: &str,
    outcome: &str,
    error: Option<&str>,
) {
    if let Err(e) = sup.note_update_attempt(id, version, outcome, error) {
        tracing::warn!(plugin_id = id, error = %e, "auto_update_record_failed");
    }
}

pub(super) fn notify(
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

pub(super) fn fail(sup: &mut PluginSupervisor, id: &str, version: &str, e: String) -> Outcome {
    tracing::warn!(plugin_id = id, error = %e, "auto_update_install_failed");
    record(sup, id, version, "failed", Some(&e));
    Outcome::Failed
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
pub(super) fn conclude_stop(
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
pub(super) fn fetch_archive(
    source: &dyn UpdateSource,
    row: &VersionRow,
) -> Result<ArchiveContents, String> {
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
pub(super) fn silent_install(
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

pub(super) fn mark_reenable_pending(supervisor: &mut PluginSupervisor, id: &str) {
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

pub(super) fn stamp_check(supervisor: &mut PluginSupervisor, plugin_id: &str) {
    if let Err(e) = supervisor.note_update_check(plugin_id, now_ms()) {
        tracing::warn!(plugin_id, error = %e, "auto_update_check_stamp_failed");
    }
}
