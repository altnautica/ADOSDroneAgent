//! Discovery, reload, and install-record bookkeeping.

use super::*;

impl PluginSupervisor {
    /// Read on-disk state and filter each install's granted permissions down to
    /// what its manifest currently declares (defends against a tampered state
    /// file).
    pub fn discover(&mut self) -> Result<(), LifecycleError> {
        self.installs = load_state(Some(&self.paths.state_path));
        self.restore_interrupted_swaps();
        self.filter_installs_against_manifests();
        tracing::info!(
            installed_count = self.installs.len(),
            "plugin_supervisor_discovered"
        );
        Ok(())
    }

    /// Put back an install tree a crash left missing between the two renames
    /// of a swap: the retired tree (it matches the record) first, else the
    /// validated staging tree.
    pub(super) fn restore_interrupted_swaps(&self) {
        for install in &self.installs {
            let Ok(target) = plugin_dir_under(&self.paths.install_dir, &install.plugin_id) else {
                continue;
            };
            if target.exists() {
                continue;
            }
            for kind in ["retired", "staging"] {
                let candidate = self
                    .paths
                    .install_dir
                    .join(format!(".{}.{kind}", install.plugin_id));
                if !candidate.is_dir() {
                    continue;
                }
                match std::fs::rename(&candidate, &target) {
                    Ok(()) => {
                        tracing::warn!(
                            plugin_id = %install.plugin_id,
                            from = kind,
                            "plugin_install_restored"
                        );
                        break;
                    }
                    Err(e) => tracing::error!(
                        plugin_id = %install.plugin_id,
                        from = kind,
                        error = %e,
                        "plugin_install_restore_failed"
                    ),
                }
            }
        }
    }

    /// Re-read the install list from disk before a read-modify-write.
    ///
    /// This controller is long-lived (the cloud relay holds one for its whole
    /// life) and the state file has other writers: the LAN install path and the
    /// CLI. Every mutator runs this under [`StateLock`] so it modifies what is on
    /// disk now, not the list it read at boot; saving the boot-time copy erased
    /// every install another writer had recorded since. An unreadable file is
    /// an error, never an empty list: writing over state that could not be read
    /// would drop every install in it.
    pub(super) fn reload_installs(&mut self) -> Result<(), LifecycleError> {
        self.installs = state::load_state_checked(Some(&self.paths.state_path)).map_err(|e| {
            SupervisorError(format!(
                "plugin state is unreadable ({e}); refusing to overwrite it"
            ))
        })?;
        self.restore_interrupted_swaps();
        self.filter_installs_against_manifests();
        Ok(())
    }

    /// Filter every install's grants down to what its manifest declares.
    pub(super) fn filter_installs_against_manifests(&mut self) {
        // Collect the ids up-front so the manifest lookups can borrow `self`
        // immutably while the filter mutates each install.
        let ids: Vec<String> = self.installs.iter().map(|i| i.plugin_id.clone()).collect();
        for id in ids {
            let declared = match self.manifest_for(&id) {
                Ok(m) => m.declared_permissions(),
                Err(_) => continue,
            };
            if let Some(install) = self.installs.iter_mut().find(|i| i.plugin_id == id) {
                filter_permissions_against_manifest(install, &declared);
            }
        }
    }

    /// Current in-memory install list.
    pub fn installs(&self) -> &[PluginInstall] {
        &self.installs
    }

    /// The install record for `plugin_id`, if installed.
    pub fn find_install(&self, plugin_id: &str) -> Option<&PluginInstall> {
        find_install(&self.installs, plugin_id)
    }

    /// The tamper-checked manifest of an installed or built-in plugin.
    pub fn installed_manifest(&self, plugin_id: &str) -> Result<PluginManifest, SupervisorError> {
        self.manifest_for(plugin_id)
    }

    /// The installed `manifest.yaml` bytes, exactly as the attestation digest
    /// and the recorded manifest hash cover them. Refused when the file no
    /// longer matches the hash recorded at install.
    pub fn installed_manifest_bytes(&self, plugin_id: &str) -> Result<Vec<u8>, SupervisorError> {
        self.require_install_ref(plugin_id)?;
        self.manifest_bytes_checked(plugin_id)
    }

    /// The install attestation of an installed plugin: its `SIGNATURE` entry
    /// and the digest of every file (see [`crate::attestation`]).
    pub fn attestation(&self, plugin_id: &str) -> Result<Attestation, SupervisorError> {
        self.require_install_ref(plugin_id)?;
        let dir = plugin_dir_under(&self.paths.install_dir, plugin_id)?;
        Attestation::read_from(&dir)
    }

    /// The plugin that declares capability `cap_id` and its declaration, so an
    /// install dialog can list a plugin-declared capability with its declared
    /// risk. `None` when no installed plugin declares it.
    pub fn declared_capability(&self, cap_id: &str) -> Option<(String, DeclaredCapability)> {
        self.installs.iter().find_map(|install| {
            let manifest = self.manifest_for(&install.plugin_id).ok()?;
            let cap = manifest
                .agent?
                .declared_capabilities
                .into_iter()
                .find(|c| c.id == cap_id)?;
            Some((install.plugin_id.clone(), cap))
        })
    }

    /// Re-read state from disk, so a long-lived holder sees installs another
    /// writer recorded since it last looked.
    pub fn refresh(&mut self) -> Result<(), LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()
    }

    /// Record the most recent auto-update attempt on an install:
    /// `{version, outcome, error}`. A plugin removed meanwhile is a no-op.
    pub fn note_update_attempt(
        &mut self,
        plugin_id: &str,
        version: &str,
        outcome: &str,
        error: Option<&str>,
    ) -> Result<(), LifecycleError> {
        self.update_record(plugin_id, |install| {
            install.last_update_attempt = Some(serde_json::json!({
                "version": version,
                "outcome": outcome,
                "error": error,
            }));
        })
    }

    /// Stamp the auto-update check time on an install, whatever the outcome,
    /// so the GCS sees the check ran.
    pub fn note_update_check(&mut self, plugin_id: &str, at_ms: i64) -> Result<(), LifecycleError> {
        self.update_record(plugin_id, |install| {
            install.last_update_check_at = Some(at_ms)
        })
    }

    /// Record that an auto-update stopped this plugin and could not start it
    /// again, so the update loop keeps retrying the enable. Any enable or
    /// disable clears it.
    pub fn note_reenable_pending(&mut self, plugin_id: &str) -> Result<(), LifecycleError> {
        self.update_record(plugin_id, |install| install.reenable_pending = true)
    }

    /// Record the model-delivery status of an installed plugin (one entry per
    /// declared model, as the vision model manager resolved it), so the host's
    /// `vision.read_model` and the heartbeat surface it.
    pub fn set_model_status(
        &mut self,
        plugin_id: &str,
        status: serde_json::Value,
    ) -> Result<(), LifecycleError> {
        self.update_installed(plugin_id, |install| install.model_status = Some(status))
    }

    /// Hold an installed plugin at `version` for auto-update (`None` lifts the
    /// pin).
    pub fn set_pinned_version(
        &mut self,
        plugin_id: &str,
        version: Option<String>,
    ) -> Result<(), LifecycleError> {
        self.update_installed(plugin_id, |install| install.pinned_version = version)
    }

    /// Whether auto-update may update an installed plugin.
    pub fn set_auto_update(
        &mut self,
        plugin_id: &str,
        enabled: bool,
    ) -> Result<(), LifecycleError> {
        self.update_installed(plugin_id, |install| install.auto_update = enabled)
    }

    /// [`update_record`](Self::update_record) for an operator write: a plugin
    /// that is not installed is an error, not a no-op.
    pub(super) fn update_installed(
        &mut self,
        plugin_id: &str,
        f: impl FnOnce(&mut PluginInstall),
    ) -> Result<(), LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
        f(self.require_install_mut(plugin_id)?);
        save_state(&self.installs, Some(&self.paths.state_path))
    }

    pub(super) fn update_record(
        &mut self,
        plugin_id: &str,
        f: impl FnOnce(&mut PluginInstall),
    ) -> Result<(), LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
        let Some(install) = state::find_install_mut(&mut self.installs, plugin_id) else {
            return Ok(());
        };
        f(install);
        save_state(&self.installs, Some(&self.paths.state_path))
    }

    // ------------------------------------------------------------------
    // Install / enable / disable / remove
    // ------------------------------------------------------------------
}
