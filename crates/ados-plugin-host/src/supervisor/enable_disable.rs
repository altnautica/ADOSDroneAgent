//! Enable, disable, and remove.

use super::*;

impl PluginSupervisor {
    /// Enable a plugin. Idempotent: a running plugin is left running. A plugin
    /// with no agent unit (a GCS-only plugin) only flips state; subprocess
    /// plugins are enabled and started on the service backend, with their
    /// declared services, once everything their units bind and read is in
    /// place.
    pub fn enable(&mut self, plugin_id: &str) -> Result<(), LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
        let manifest = self.manifest_for(plugin_id)?;
        // The single tamper check the host's reconciler also runs: a plugin
        // whose manifest or attested files changed since install is refused.
        let verdict = verify_install(
            &self.paths.install_dir,
            self.require_install_ref(plugin_id)?,
        );
        if let Err(detail) = verdict {
            let install = self.require_install_mut(plugin_id)?;
            install.status = PluginStatus::Failed;
            install.failure_reason = Some(format!("{TAMPERED_REASON}: {detail}"));
            save_state(&self.installs, Some(&self.paths.state_path))?;
            return Err(SupervisorError(format!(
                "refused: {TAMPERED_REASON}: plugin {plugin_id} files changed since install ({detail})"
            ))
            .into());
        }
        let is_subprocess = manifest.is_subprocess_agent();
        let already_running = {
            let install = self.require_install_ref(plugin_id)?;
            if let Some(reason) = self.revocation_refusal(install) {
                return Err(SupervisorError(format!(
                    "refused: {reason}: plugin {plugin_id} may not be enabled"
                ))
                .into());
            }
            install.status == PluginStatus::Running
        };
        if already_running {
            // Running is not failed: drop a reason left by an earlier stop.
            let install = self.require_install_mut(plugin_id)?;
            if install.failure_reason.take().is_some() {
                save_state(&self.installs, Some(&self.paths.state_path))?;
            }
            return Ok(());
        }
        if !is_subprocess {
            let install = self.require_install_mut(plugin_id)?;
            install.status = PluginStatus::Enabled;
            install.enabled_at = Some(now_ms());
            install.failure_reason = None;
            install.reenable_pending = false;
            save_state(&self.installs, Some(&self.paths.state_path))?;
            return Ok(());
        }
        self.ensure_account(&manifest)?;
        self.ensure_http_dir(&manifest)?;
        self.ensure_data_dir(&manifest)?;
        // Enabled before the start: the plugin host serves an enabled plugin,
        // and its socket and token must be there before the unit runs.
        let (prior_status, prior_enabled_at) = {
            let install = self.require_install_mut(plugin_id)?;
            let prior = (install.status, install.enabled_at);
            install.status = PluginStatus::Enabled;
            install.enabled_at = Some(now_ms());
            prior
        };
        save_state(&self.installs, Some(&self.paths.state_path))?;
        let started = self.prepare_host_files(plugin_id).and_then(|()| {
            self.backend
                .enable_start(&unit_name_for(plugin_id))
                .map_err(LifecycleError::from)
        });
        if let Err(e) = started {
            let install = self.require_install_mut(plugin_id)?;
            install.status = prior_status;
            install.enabled_at = prior_enabled_at;
            save_state(&self.installs, Some(&self.paths.state_path))?;
            return Err(e);
        }
        // Each declared service under its own unit, additional to the main
        // runner. A failure on one is surfaced as not-ready with its reason,
        // never as a failed enable: the plugin's main half still runs.
        self.start_declared_services(plugin_id, &manifest);
        let service_status = self.readiness_of(plugin_id, &manifest);
        let install = self.require_install_mut(plugin_id)?;
        install.status = PluginStatus::Running;
        install.enabled_at = Some(now_ms());
        // A plugin that runs again has no standing failure to report.
        install.failure_reason = None;
        install.service_status = service_status;
        install.reenable_pending = false;
        save_state(&self.installs, Some(&self.paths.state_path))?;
        tracing::info!(plugin_id, "plugin_enabled");
        Ok(())
    }

    /// Disable a plugin. Idempotent: an already-disabled plugin is a no-op.
    /// Subprocess plugins are stopped and disabled on the service backend.
    pub fn disable(&mut self, plugin_id: &str) -> Result<(), LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
        let manifest = self.manifest_for(plugin_id)?;
        let is_subprocess = manifest.is_subprocess_agent();
        {
            let install = self.require_install_mut(plugin_id)?;
            if install.status == PluginStatus::Disabled {
                // An operator disable outranks a pending re-enable.
                if install.reenable_pending {
                    install.reenable_pending = false;
                    save_state(&self.installs, Some(&self.paths.state_path))?;
                }
                return Ok(());
            }
        }
        if is_subprocess {
            // The declared services first (best-effort, so a missing unit does
            // not block the main teardown), then the main runner.
            self.stop_declared_services(plugin_id, &manifest);
            self.backend.stop_disable(&unit_name_for(plugin_id))?;
        }
        let install = self.require_install_mut(plugin_id)?;
        install.status = PluginStatus::Disabled;
        install.enabled_at = None;
        install.service_status = None;
        install.reenable_pending = false;
        save_state(&self.installs, Some(&self.paths.state_path))?;
        tracing::info!(plugin_id, "plugin_disabled");
        Ok(())
    }

    /// Remove a plugin: disable it (if running/enabled), remove the units,
    /// delete the unpacked dir and drop the state; unless `keep_data`, also the
    /// logs, the data dir and the plugin's system user.
    ///
    /// An install whose manifest cannot be read (missing, tampered, no longer
    /// valid) is still removable: its main unit and every
    /// `ados-plugin-<basename>-*.service` in the unit dir are uninstalled by
    /// name, and the outcome carries the reason as a warning.
    ///
    /// A data dir this process cannot delete (it belongs to the plugin's own
    /// user, and a caller without DAC override such as the cloud relay cannot
    /// enter it) is purged by the plugin host through its control socket. A
    /// data dir that still will not go fails the call with
    /// `data_not_removed:` after everything else is done, so the caller reports
    /// it rather than a clean removal; the user is then kept so the files keep
    /// an owner.
    pub fn remove(
        &mut self,
        plugin_id: &str,
        keep_data: bool,
    ) -> Result<RemoveOutcome, LifecycleError> {
        // disable() takes the lock itself; decide against current state, run it
        // outside the lock below, then re-read under the lock for the removal.
        let status = {
            let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
            self.reload_installs()?;
            self.require_install_ref(plugin_id)?.status
        };
        if matches!(status, PluginStatus::Running | PluginStatus::Enabled) {
            if let Err(e) = self.disable(plugin_id) {
                tracing::warn!(plugin_id, error = %e, "plugin_disable_during_remove_failed");
            }
        }

        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
        self.require_install_ref(plugin_id)?;
        let (manifest, warning) = match self.manifest_for(plugin_id) {
            Ok(m) => (Some(m), None),
            Err(e) => {
                tracing::warn!(plugin_id, error = %e, "plugin_remove_manifest_unreadable");
                (None, Some(e.0))
            }
        };
        match &manifest {
            Some(m) if m.is_subprocess_agent() => {
                // disable() already stopped the declared services; delete their
                // units alongside the main unit.
                self.delete_declared_service_units(plugin_id, m);
                self.backend.uninstall(&unit_name_for(plugin_id))?;
            }
            Some(_) => {}
            None => self.uninstall_units_by_name(plugin_id),
        }
        let http_dir = plugin_http_dir(&self.paths.run_dir, plugin_id);
        if http_dir.exists() {
            let _ = std::fs::remove_dir_all(&http_dir);
        }
        let target = plugin_dir_under(&self.paths.install_dir, plugin_id)?;
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        // State goes before the data dir: the host's purge refuses a plugin
        // that is still installed.
        self.installs = remove_install(std::mem::take(&mut self.installs), plugin_id);
        save_state(&self.installs, Some(&self.paths.state_path))?;
        tracing::info!(plugin_id, keep_data, "plugin_removed");
        let outcome = RemoveOutcome { warning };
        if keep_data {
            return Ok(outcome);
        }
        self.remove_logs(plugin_id, manifest.as_ref())?;
        let data = plugin_dir_under(&self.paths.data_root, plugin_id)?;
        let removed = match std::fs::remove_dir_all(&data) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => crate::control_client::purge_data(&self.paths.control_dir, plugin_id)
                .map_err(|purge| format!("{e}; plugin host purge: {purge}")),
        };
        if let Err(detail) = removed {
            tracing::error!(plugin_id, dir = %data.display(), detail = %detail, "plugin_data_not_removed");
            return Err(SupervisorError(format!(
                "data_not_removed: plugin {plugin_id} was removed but its data dir {} could \
                 not be: {detail}",
                data.display()
            ))
            .into());
        }
        if self.backend.enforces_sandbox() {
            match &manifest {
                Some(m) if m.is_subprocess_agent() => self.accounts.remove(plugin_id)?,
                Some(_) => {}
                // The manifest is gone, so whether it had an account is
                // unknown: remove it if there is one, best-effort.
                None => {
                    if let Err(e) = self.accounts.remove(plugin_id) {
                        tracing::warn!(plugin_id, error = %e, "plugin_account_remove_failed");
                    }
                }
            }
        }
        Ok(outcome)
    }

    /// A plugin's main unit and every `ados-plugin-<basename>-*.service` in
    /// the unit dir, by name alone.
    pub(super) fn unit_names_by_prefix(&self, plugin_id: &str) -> Vec<String> {
        let main = unit_name_for(plugin_id);
        let prefix = format!("{PLUGIN_UNIT_PREFIX}{}-", plugin_unit_basename(plugin_id));
        let mut names = vec![main];
        if let Ok(entries) = std::fs::read_dir(&self.paths.unit_dir) {
            let mut found: Vec<String> = entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.starts_with(&prefix) && n.ends_with(".service"))
                .collect();
            found.sort();
            names.extend(found);
        }
        names
    }

    /// Stop and disable every unit of a plugin whose install failed its tamper
    /// check, without touching state (the caller records the failure).
    /// Best-effort per unit.
    pub fn stop_tampered(&self, plugin_id: &str) {
        for name in self.unit_names_by_prefix(plugin_id) {
            if let Err(e) = self.backend.stop_disable(&name) {
                tracing::warn!(plugin_id, unit = %name, error = %e, "plugin_unit_stop_failed");
            }
        }
    }

    /// Stop and uninstall a plugin's main unit and every
    /// `ados-plugin-<basename>-*.service` in the unit dir, by name alone.
    /// Best-effort: the remove of an unreadable install must not stop here.
    pub(super) fn uninstall_units_by_name(&self, plugin_id: &str) {
        for name in self.unit_names_by_prefix(plugin_id) {
            if let Err(e) = self.backend.stop_disable(&name) {
                tracing::warn!(plugin_id, unit = %name, error = %e, "plugin_unit_stop_failed");
            }
            if let Err(e) = self.backend.uninstall(&name) {
                tracing::warn!(plugin_id, unit = %name, error = %e, "plugin_unit_uninstall_failed");
            }
        }
    }

    /// Delete a plugin's main log, each declared service's log, and any other
    /// `<basename>-*.log` left from a previous version's services.
    pub(super) fn remove_logs(
        &self,
        plugin_id: &str,
        manifest: Option<&PluginManifest>,
    ) -> Result<(), LifecycleError> {
        let log_dir = &self.paths.log_dir;
        let mut paths = vec![crate::systemd::log_path_for(log_dir, plugin_id)];
        if let Some(m) = manifest {
            paths.extend(
                self.services_of(plugin_id, m)
                    .iter()
                    .map(|s| crate::systemd::service_log_path_for(log_dir, plugin_id, &s.name)),
            );
        }
        let prefix = format!("{}-", plugin_unit_basename(plugin_id));
        if let Ok(entries) = std::fs::read_dir(log_dir) {
            paths.extend(
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_name().to_str().is_some_and(|n| {
                            n.starts_with(&prefix) && n.ends_with(PLUGIN_LOG_SUFFIX)
                        })
                    })
                    .map(|e| e.path()),
            );
        }
        for path in paths {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Probe each declared service of an installed plugin that runs on this
    /// node: `[{name, ready, reason}]`, or `None` when it declares none.
    pub fn service_readiness(
        &self,
        plugin_id: &str,
    ) -> Result<Option<serde_json::Value>, SupervisorError> {
        self.require_install_ref(plugin_id)?;
        let manifest = self.manifest_for(plugin_id)?;
        Ok(self.readiness_of(plugin_id, &manifest))
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------
}
