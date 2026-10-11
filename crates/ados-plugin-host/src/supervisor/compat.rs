//! Install-time compatibility, trust, and revocation gates.

use super::*;

impl PluginSupervisor {
    pub(super) fn require_install_ref(
        &self,
        plugin_id: &str,
    ) -> Result<&PluginInstall, SupervisorError> {
        find_install(&self.installs, plugin_id)
            .ok_or_else(|| SupervisorError(format!("plugin {plugin_id} is not installed")))
    }

    pub(super) fn require_install_mut(
        &mut self,
        plugin_id: &str,
    ) -> Result<&mut PluginInstall, SupervisorError> {
        state::find_install_mut(&mut self.installs, plugin_id)
            .ok_or_else(|| SupervisorError(format!("plugin {plugin_id} is not installed")))
    }

    /// The on-disk manifest bytes of a plugin, re-checked against the recorded
    /// `manifest_hash` (tamper detection).
    pub(super) fn manifest_bytes_checked(
        &self,
        plugin_id: &str,
    ) -> Result<Vec<u8>, SupervisorError> {
        let manifest_path =
            plugin_dir_under(&self.paths.install_dir, plugin_id)?.join(MANIFEST_FILENAME);
        if !manifest_path.exists() {
            return Err(SupervisorError(format!(
                "plugin {plugin_id} manifest missing at {}",
                manifest_path.display()
            )));
        }
        let manifest_bytes = std::fs::read(&manifest_path).map_err(|e| {
            SupervisorError(format!("read of {} failed: {e}", manifest_path.display()))
        })?;
        if let Some(install) = find_install(&self.installs, plugin_id) {
            if !install.manifest_hash.is_empty() {
                let current = hex::encode(Sha256::digest(&manifest_bytes));
                if current != install.manifest_hash {
                    return Err(SupervisorError(format!(
                        "plugin {plugin_id} manifest hash mismatch; on-disk file has \
                         been modified since install"
                    )));
                }
            }
        }
        Ok(manifest_bytes)
    }

    /// Resolve the manifest for a plugin off its unpacked dir, re-checking the
    /// recorded `manifest_hash` against the on-disk bytes (tamper detection).
    pub(super) fn manifest_for(&self, plugin_id: &str) -> Result<PluginManifest, SupervisorError> {
        let manifest_bytes = self.manifest_bytes_checked(plugin_id)?;
        let text = std::str::from_utf8(&manifest_bytes)
            .map_err(|e| SupervisorError(format!("manifest is not valid UTF-8: {e}")))?;
        PluginManifest::from_yaml_text(text).map_err(|e: ManifestError| SupervisorError(e.0))
    }

    /// Run the version + board + tier + profile + binary + inprocess gates.
    pub(super) fn check_compatibility(
        &self,
        manifest: &PluginManifest,
    ) -> Result<(), LifecycleError> {
        let constraint = manifest.compatibility.ados_version.trim();
        if constraint.is_empty() {
            return Err(ManifestError(format!(
                "plugin {} has empty compatibility.ados_version",
                manifest.id
            ))
            .into());
        }
        if !semver_in_range(&self.agent_version, constraint)? {
            return Err(SupervisorError(format!(
                "plugin {} requires ADOS version {constraint}; running {}",
                manifest.id, self.agent_version
            ))
            .into());
        }
        if let Some(board) = &self.current_board_id {
            if !manifest.compatibility.supports_board(board) {
                return Err(SupervisorError(format!(
                    "refused: {BOARD_MISMATCH_REASON}: plugin {} does not support board {board}",
                    manifest.id
                ))
                .into());
            }
        }
        // The compute-tier floor. Lenient when either the floor or the board
        // tier is unknown, matching `supported_boards`: a node that cannot tell
        // you its tier does not get to refuse an install over a guess.
        if let (Some(min_tier), Some(tier)) =
            (manifest.compatibility.min_tier, self.current_board_tier)
        {
            if tier < min_tier {
                return Err(SupervisorError(format!(
                    "plugin {} requires compute tier {min_tier}; this board is tier {tier}",
                    manifest.id
                ))
                .into());
            }
        }
        if let Some(agent) = &manifest.agent {
            // A GCS-only plugin has no agent half to place, so this gate
            // applies to the agent half alone.
            if !agent.targets_profile(&self.current_profile) {
                return Err(SupervisorError(format!(
                    "incompatible: target_profiles excludes {}",
                    self.current_profile
                ))
                .into());
            }
            // Every binary this node would run must exist for its
            // architecture: the entrypoint and each service that runs here.
            let arch_os = host_arch_os();
            let mut needed: Vec<&str> = Vec::new();
            if let Some(name) = bin_reference(&agent.entrypoint) {
                needed.push(name);
            }
            let services = declared_services(manifest)?;
            for service in services
                .iter()
                .filter(|s| s.applies_to(&self.current_profile))
            {
                if let Some(first) = service.command.split_whitespace().next() {
                    if let Some(name) = bin_reference(first) {
                        needed.push(name);
                    }
                }
            }
            if needed
                .iter()
                .any(|name| agent.binary_path(name, &arch_os).is_none())
            {
                return Err(
                    SupervisorError(format!("incompatible: no binary for {arch_os}")).into(),
                );
            }
            // Nothing executes an in-process agent half: every plugin runs as
            // its own unit. Accepting one would install a plugin that reads
            // enabled and never runs, whoever signed it.
            if agent.isolation == AgentIsolation::Inprocess {
                return Err(SupervisorError(format!(
                    "plugin {} requests inprocess isolation, which this agent does not run; \
                     use subprocess isolation",
                    manifest.id
                ))
                .into());
            }
        }
        Ok(())
    }

    /// Refuse an install whose version is lower than the one already installed.
    ///
    /// A signed older archive silently replacing a newer install is a rollback
    /// attack on any plugin whose signing key is held: the signature verifies,
    /// the operator sees a successful install, and the plugin is back to a
    /// version with a known hole. Version ordering is the only thing that
    /// catches it, so it is checked before anything is unpacked.
    ///
    /// A deliberate rollback goes through
    /// [`install_archive_allowing_downgrade`](Self::install_archive_allowing_downgrade).
    /// An unparseable version on either side is not treated as a downgrade —
    /// the semver gate above has already had its say, and refusing on a parse
    /// failure here would block a legitimate install over a formatting detail.
    pub(super) fn reject_downgrade(&self, manifest: &PluginManifest) -> Result<(), LifecycleError> {
        let Some(install) = find_install(&self.installs, &manifest.id) else {
            return Ok(());
        };
        let (Ok(incoming), Ok(installed)) = (
            semver_tuple(&manifest.version),
            semver_tuple(&install.version),
        ) else {
            return Ok(());
        };
        if incoming < installed {
            return Err(SupervisorError(format!(
                "plugin {} archive is version {} but {} is installed; refusing a \
                 downgrade (a signed older version is a rollback attack). Use the \
                 explicit allow-downgrade path to force it",
                manifest.id, manifest.version, install.version
            ))
            .into());
        }
        Ok(())
    }

    /// Reject `inline` GCS isolation from a non-first-party signer.
    pub(super) fn reject_inline_for_third_party(
        &self,
        manifest: &PluginManifest,
        signer_id: Option<&str>,
        first_party: bool,
    ) -> Result<(), LifecycleError> {
        if let Some(gcs) = &manifest.gcs {
            if gcs.isolation == GcsIsolation::Inline && !first_party {
                return Err(SupervisorError(format!(
                    "plugin {} requests inline GCS isolation but signer {} is not first-party",
                    manifest.id,
                    signer_id.unwrap_or("<none>")
                ))
                .into());
            }
        }
        Ok(())
    }

    /// Reject `resources.class: heavy` from a non-first-party signer: the
    /// lifted bounds let one plugin take most of a node.
    pub(super) fn reject_heavy_for_third_party(
        &self,
        manifest: &PluginManifest,
        first_party: bool,
    ) -> Result<(), LifecycleError> {
        let heavy = manifest
            .agent
            .as_ref()
            .is_some_and(|a| a.resources.class == ResourceClass::Heavy);
        if heavy && !first_party {
            return Err(
                SupervisorError("refused: heavy_resources_first_party_only".to_string()).into(),
            );
        }
        Ok(())
    }

    /// Reject an agent half from a non-first-party signer on a service backend
    /// that enforces no sandbox. Every sandbox-enforced capability would be
    /// granted in name only there, so only a first-party plugin, whose
    /// behaviour is known, may run.
    pub(super) fn reject_unsandboxed_third_party(
        &self,
        manifest: &PluginManifest,
        first_party: bool,
    ) -> Result<(), LifecycleError> {
        if manifest.is_subprocess_agent() && !self.backend.enforces_sandbox() && !first_party {
            return Err(SupervisorError(
                "refused: sandbox_unavailable_first_party_only".to_string(),
            )
            .into());
        }
        Ok(())
    }

    /// Refuse a shared topic another installed plugin already owns: two
    /// plugins whose ids end in the same segment would otherwise both publish
    /// under one `plugin.<leaf>.` namespace.
    pub(super) fn reject_shared_topic_collision(
        &self,
        manifest: &PluginManifest,
    ) -> Result<(), LifecycleError> {
        let Some(agent) = &manifest.agent else {
            return Ok(());
        };
        let wanted: BTreeSet<String> = agent.shared_topics().into_iter().map(|t| t.topic).collect();
        if wanted.is_empty() {
            return Ok(());
        }
        for install in &self.installs {
            if install.plugin_id == manifest.id {
                continue;
            }
            let Ok(other) = self.manifest_for(&install.plugin_id) else {
                continue;
            };
            let Some(other_agent) = other.agent else {
                continue;
            };
            if let Some(topic) = other_agent
                .shared_topics()
                .into_iter()
                .find(|t| wanted.contains(&t.topic))
            {
                return Err(SupervisorError(format!(
                    "plugin {}: shared topic {} is already owned by {}",
                    manifest.id, topic.topic, install.plugin_id
                ))
                .into());
            }
        }
        Ok(())
    }

    /// Refuse an archive whose verified signer differs from the signer of the
    /// install it would replace. A plugin id is bound to the key that first
    /// installed it: an archive signed by another enrolled key would otherwise
    /// take over the id, its operator-approved grants and its data. A signed
    /// install is never replaced by an unsigned archive, and an unsigned one is
    /// never silently promoted; either is a remove and a fresh install.
    pub(super) fn reject_signer_change(
        &self,
        manifest: &PluginManifest,
        signer: Option<&str>,
    ) -> Result<(), LifecycleError> {
        let Some(previous) = find_install(&self.installs, &manifest.id) else {
            return Ok(());
        };
        if previous.signer_id.as_deref() == signer {
            return Ok(());
        }
        Err(SupervisorError(format!(
            "refused: {SIGNER_CHANGE_REASON}: plugin {} is installed signed by {} but the \
             archive is signed by {}; remove the plugin to install another publisher's build",
            manifest.id,
            previous.signer_id.as_deref().unwrap_or("<unsigned>"),
            signer.unwrap_or("<unsigned>"),
        ))
        .into())
    }

    /// Every unit name `manifest` renders on any profile: the main unit and one
    /// per declared service.
    pub(super) fn unit_names_of(manifest: &PluginManifest) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        if manifest.is_subprocess_agent() {
            names.insert(unit_name_for(&manifest.id));
            for service in declared_services(manifest).unwrap_or_default() {
                names.insert(service_unit_name_for(&manifest.id, &service.name));
            }
        }
        names
    }

    /// Refuse an install whose rendered unit names are already owned by a
    /// different installed plugin id, so one plugin can never overwrite,
    /// stop or delete another's unit.
    pub(super) fn reject_unit_name_collision(
        &self,
        manifest: &PluginManifest,
    ) -> Result<(), LifecycleError> {
        let wanted = Self::unit_names_of(manifest);
        if wanted.is_empty() {
            return Ok(());
        }
        for install in &self.installs {
            if install.plugin_id == manifest.id {
                continue;
            }
            let Ok(other) = self.manifest_for(&install.plugin_id) else {
                continue;
            };
            if let Some(name) = Self::unit_names_of(&other).intersection(&wanted).next() {
                return Err(SupervisorError(format!(
                    "refused: unit_name_collision: plugin {} would render unit {name}, which \
                     plugin {} already owns",
                    manifest.id, install.plugin_id
                ))
                .into());
            }
        }
        Ok(())
    }

    /// Create the plugin's system user on a sandboxing backend.
    pub(super) fn ensure_account(&self, manifest: &PluginManifest) -> Result<(), LifecycleError> {
        if manifest.is_subprocess_agent() && self.backend.enforces_sandbox() {
            self.accounts.ensure(&manifest.id)?;
        }
        Ok(())
    }

    /// Why `install` may not run under `revocations`: its signer, its archive
    /// digest or its release (`<plugin_id>@<version>`) is revoked.
    pub(super) fn revoked_reason(
        install: &PluginInstall,
        revocations: &Revocations,
    ) -> Option<&'static str> {
        if install
            .signer_id
            .as_deref()
            .is_some_and(|s| revocations.signers.contains(s))
        {
            Some(SIGNER_REVOKED_REASON)
        } else if install
            .archive_sha256
            .as_deref()
            .is_some_and(|d| revocations.archive_revoked(d))
        {
            Some(ARCHIVE_REVOKED_REASON)
        } else if revocations.version_revoked(&install.plugin_id, &install.version) {
            Some(VERSION_REVOKED_REASON)
        } else {
            None
        }
    }

    /// The refusal reason for enabling `install` now: revoked, or a signed
    /// plugin while the revocation list cannot be read.
    pub(super) fn revocation_refusal(&self, install: &PluginInstall) -> Option<&'static str> {
        match self.revocations() {
            Ok(revocations) => Self::revoked_reason(install, &revocations),
            Err(e) if install.signer_id.is_some() => {
                tracing::error!(plugin_id = %install.plugin_id, error = %e, "plugin_revocation_list_unreadable");
                Some(REVOCATION_LIST_UNREADABLE_REASON)
            }
            Err(_) => None,
        }
    }

    /// Apply `revocations` to the installed plugins.
    ///
    /// Disarmed: every plugin signed by a revoked signer or installed from a
    /// revoked archive is disabled with the reason recorded, and its
    /// `revocation_pending` cleared. Armed (`armed` is the fail-closed verdict:
    /// unknown or stale counts as armed): nothing is stopped mid-flight; each
    /// such plugin is marked `revocation_pending` and listed as deferred, and
    /// the next disarmed pass applies the disable. New installs and enables of a
    /// revoked plugin are refused at once in either state.
    pub fn apply_revocations(
        &mut self,
        revocations: &Revocations,
        armed: bool,
    ) -> Result<RevocationSweep, LifecycleError> {
        let mut sweep = RevocationSweep::default();
        if revocations.signers.is_empty() && revocations.archives.is_empty() {
            return Ok(sweep);
        }
        let targets: Vec<(String, &'static str)> = {
            let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
            self.reload_installs()?;
            self.installs
                .iter()
                .filter_map(|i| Self::revoked_reason(i, revocations).map(|r| (i, r)))
                .filter(|(i, reason)| {
                    i.status != PluginStatus::Disabled
                        || i.failure_reason.as_deref() != Some(*reason)
                        || i.revocation_pending
                })
                .map(|(i, reason)| (i.plugin_id.clone(), reason))
                .collect()
        };
        for (plugin_id, reason) in targets {
            if armed {
                self.update_record(&plugin_id, |install| install.revocation_pending = true)?;
                tracing::warn!(plugin_id, reason, "plugin_revocation_deferred_armed");
                sweep.deferred.push(plugin_id);
                continue;
            }
            if let Err(e) = self.disable(&plugin_id) {
                tracing::error!(plugin_id, error = %e, "revoked plugin could not be stopped");
            }
            self.update_record(&plugin_id, |install| {
                install.status = PluginStatus::Disabled;
                install.enabled_at = None;
                install.failure_reason = Some(reason.to_string());
                install.revocation_pending = false;
            })?;
            tracing::warn!(plugin_id, reason, "plugin_disabled_revoked");
            sweep.disabled.push(plugin_id);
        }
        Ok(sweep)
    }
}
