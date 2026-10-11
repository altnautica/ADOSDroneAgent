//! Archive install: verify, gate, fetch payloads, unpack, write the unit.

use super::*;

impl PluginSupervisor {
    /// Install a `.adosplug` archive from a path. The caller is responsible for
    /// prompting the operator to approve permissions before this call; every
    /// requested permission is recorded `granted=false` initially and the
    /// operator-side flow then calls [`grant_permission`](Self::grant_permission)
    /// per approved permission.
    pub fn install_archive(
        &mut self,
        archive_path: &Path,
    ) -> Result<InstallResult, LifecycleError> {
        let contents = open_archive(archive_path)?;
        self.install_contents(contents, archive_path)
    }

    /// Install a `.adosplug` archive, permitting a version lower than the one
    /// already installed.
    ///
    /// The plain [`install_archive`](Self::install_archive) refuses a
    /// downgrade: a signed older version silently replacing a newer install is
    /// a rollback attack on any plugin whose key is held, and the operator sees
    /// only a successful install. A deliberate rollback (a bad release) is a
    /// real need, so it gets its own explicit entry point rather than a flag on
    /// the safe path.
    pub fn install_archive_allowing_downgrade(
        &mut self,
        archive_path: &Path,
    ) -> Result<InstallResult, LifecycleError> {
        let contents = open_archive(archive_path)?;
        self.install_contents_with(contents, archive_path, true)
    }

    /// Install from already-parsed archive contents. Splits the parse from the
    /// install so tests can build contents in memory without a temp `.adosplug`.
    pub fn install_contents(
        &mut self,
        contents: ArchiveContents,
        source_path: &Path,
    ) -> Result<InstallResult, LifecycleError> {
        self.install_contents_with(contents, source_path, false)
    }

    /// Install a built-in plugin: one the agent ships as part of itself (its
    /// code lives in the agent package), described by `manifest_yaml`. It has no
    /// archive signature to verify; every other install gate applies, only the
    /// manifest is written to its install dir, and it records as a local install
    /// from `builtin:<id>`.
    pub fn install_builtin(
        &mut self,
        manifest_yaml: &str,
    ) -> Result<InstallResult, LifecycleError> {
        let contents = builtin_contents(manifest_yaml)?;
        let source = format!("builtin:{}", contents.manifest.id);
        self.install_verified(contents, None, None, Path::new(&source), false)
    }

    /// Install from parsed contents, choosing whether a version downgrade is
    /// permitted.
    ///
    /// Order: signature and revocation, compatibility, the first-party gates
    /// and the service block, then the payload downloads (outside the state
    /// lock, so a large fetch never blocks another lifecycle call), then under
    /// the lock the downgrade and shared-topic checks, the staged unpack with
    /// payloads and the attestation placed into it, the swap, the unit and the
    /// record.
    pub fn install_contents_with(
        &mut self,
        contents: ArchiveContents,
        source_path: &Path,
        allow_downgrade: bool,
    ) -> Result<InstallResult, LifecycleError> {
        let signer = self.verified_signer(&contents)?;
        let digest = hex::encode(Sha256::digest(&contents.raw_archive_bytes));
        // A revoked archive or release is refused whoever signed it; an
        // unreadable list has already refused a signed archive in
        // `verified_signer`.
        if let Ok(revocations) = self.revocations() {
            let manifest = &contents.manifest;
            let refusal = if revocations.archive_revoked(&digest) {
                Some((ARCHIVE_REVOKED_REASON, format!("archive {digest}")))
            } else if revocations.version_revoked(&manifest.id, &manifest.version) {
                Some((
                    VERSION_REVOKED_REASON,
                    format!("version {}", manifest.version),
                ))
            } else {
                None
            };
            if let Some((reason, what)) = refusal {
                return Err(SignatureError::new(
                    SignatureErrorKind::Revoked,
                    format!(
                        "refused: {reason}: {what} of plugin {} is on the revocation list",
                        manifest.id
                    ),
                )
                .into());
            }
        }
        self.install_verified(contents, signer, Some(digest), source_path, allow_downgrade)
    }

    /// The signer whose Ed25519 signature over the archive's payload hash
    /// verifies against an enrolled key and is not revoked. Every trust
    /// decision (the first-party gates, the recorded signer) keys on this,
    /// never on the id the `SIGNATURE` entry declares.
    ///
    /// With signing required, an unsigned archive or a signature that does not
    /// verify is refused. With it relaxed, either installs as unsigned: the
    /// archive is accepted, but a declared id it cannot prove buys it nothing.
    pub fn verified_signer(
        &self,
        contents: &ArchiveContents,
    ) -> Result<Option<String>, SignatureError> {
        let plugin_id = &contents.manifest.id;
        let (Some(signer_id), Some(sig_b64)) = (
            contents.signer_id.as_deref(),
            contents.signature_b64.as_deref(),
        ) else {
            if self.require_signed {
                return Err(SignatureError::new(
                    SignatureErrorKind::Missing,
                    format!("plugin {plugin_id}: archive is unsigned"),
                ));
            }
            return Ok(None);
        };
        let trusted = load_trusted_keys(Some(&self.trusted_keys_dir));
        // An unreadable list never silently re-trusts a revoked signer.
        let revocations = match self.revocations() {
            Ok(r) => r.signers,
            Err(e) => {
                tracing::error!(plugin_id = %plugin_id, error = %e, "plugin_revocation_list_unreadable");
                return Err(SignatureError::new(
                    SignatureErrorKind::Revoked,
                    format!("refused: {REVOCATION_LIST_UNREADABLE_REASON}: {e}"),
                ));
            }
        };
        match verify_archive_signature(
            &contents.payload_hash,
            sig_b64,
            signer_id,
            &trusted,
            &revocations,
        ) {
            Ok(()) => Ok(Some(signer_id.to_string())),
            // A revoked signer is refused even with signing relaxed: it must
            // not slip in as an unsigned install.
            Err(e) if self.require_signed || e.kind == SignatureErrorKind::Revoked => Err(e),
            Err(e) => {
                tracing::warn!(
                    plugin_id = %plugin_id,
                    declared_signer = signer_id,
                    error = %e,
                    "plugin signature does not verify; installing as unsigned"
                );
                Ok(None)
            }
        }
    }

    /// The install past the signature check: compatibility, the first-party
    /// gates and the service block, the payloads, then the locked unpack, unit
    /// and record. `signer` is the verified signer, `None` when unsigned.
    pub(super) fn install_verified(
        &mut self,
        contents: ArchiveContents,
        signer: Option<String>,
        archive_sha256: Option<String>,
        source_path: &Path,
        allow_downgrade: bool,
    ) -> Result<InstallResult, LifecycleError> {
        let manifest = &contents.manifest;
        self.check_compatibility(manifest)?;
        let first_party = self.is_first_party(signer.as_deref());
        self.reject_inline_for_third_party(manifest, signer.as_deref(), first_party)?;
        self.reject_heavy_for_third_party(manifest, first_party)?;
        self.reject_unsandboxed_third_party(manifest, first_party)?;
        // A malformed service block (a foreign slice, an off-box ready_check,
        // an unrenderable command, an undeclared listener) is refused here, as
        // the Python manifest model refuses it at parse, rather than surfacing
        // at enable.
        for service in declared_services(manifest)? {
            service_argv(&service.command)?;
        }

        let payload_dir = self.paths.install_dir.join(format!(
            ".{}.payloads-{}-{}",
            manifest.id,
            std::process::id(),
            now_ms()
        ));
        let result = self
            .fetch_payloads(manifest, &payload_dir)
            .and_then(|fetched| {
                self.install_locked(
                    &contents,
                    signer,
                    archive_sha256,
                    source_path,
                    allow_downgrade,
                    &payload_dir,
                    &fetched,
                )
            });
        let _ = std::fs::remove_dir_all(&payload_dir);
        let result = result?;

        tracing::info!(
            plugin_id = %result.plugin_id,
            version = %result.version,
            signer_id = ?result.signer_id,
            "plugin_installed"
        );
        Ok(result)
    }

    /// Download every payload that applies to this node (its profile and
    /// `<arch>-<os>`) into `dir`, each streamed, capped at
    /// [`PAYLOAD_MAX_BYTES`], and checked against the pinned size and sha256.
    /// Returns `(path, sha256)` per fetched payload.
    pub(super) fn fetch_payloads(
        &self,
        manifest: &PluginManifest,
        dir: &Path,
    ) -> Result<Vec<(String, String)>, LifecycleError> {
        let applicable = manifest.applicable_payloads(&self.current_profile, &host_arch_os());
        if applicable.is_empty() {
            return Ok(Vec::new());
        }
        let source: Arc<dyn DownloadSource> = match &self.download {
            Some(s) => s.clone(),
            None => Arc::new(HttpDownloadSource::new()),
        };
        let mut fetched = Vec::with_capacity(applicable.len());
        for payload in applicable {
            fetch_to_file(
                source.as_ref(),
                &payload.source,
                &dir.join(&payload.path),
                PAYLOAD_MAX_BYTES,
                Some(payload.size_bytes),
                &payload.sha256,
            )
            .map_err(|e| {
                SupervisorError(format!(
                    "plugin {}: payload {}: {e}",
                    manifest.id, payload.path
                ))
            })?;
            tracing::info!(
                plugin_id = %manifest.id,
                path = %payload.path,
                bytes = payload.size_bytes,
                "plugin_payload_fetched"
            );
            fetched.push((payload.path.clone(), payload.sha256.clone()));
        }
        Ok(fetched)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn install_locked(
        &mut self,
        contents: &ArchiveContents,
        signer: Option<String>,
        archive_sha256: Option<String>,
        source_path: &Path,
        allow_downgrade: bool,
        payload_dir: &Path,
        fetched: &[(String, String)],
    ) -> Result<InstallResult, LifecycleError> {
        let manifest = &contents.manifest;
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
        if !allow_downgrade {
            self.reject_downgrade(manifest)?;
        }
        self.reject_shared_topic_collision(manifest)?;
        self.reject_signer_change(manifest, signer.as_deref())?;
        self.reject_unit_name_collision(manifest)?;

        // Unpack and check into a staging dir beside the target. Every check
        // runs against the staging copy, so a bad archive leaves an existing
        // install exactly as it was; only a validated tree replaces it.
        let target = plugin_dir_under(&self.paths.install_dir, &manifest.id)?;
        let staging = self
            .paths
            .install_dir
            .join(format!(".{}.staging", manifest.id));
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        let staged = unpack_to(
            &contents.raw_archive_bytes,
            &staging,
            &Executables::of(manifest),
        )
        .map_err(LifecycleError::from)
        .and_then(|()| place_payloads(manifest, payload_dir, &staging, fetched))
        .and_then(|()| {
            // A manifest that declares a GCS half (or a rust agent binary)
            // must actually ship the file it points at. Fail here rather
            // than letting a missing bundle surface later as an empty
            // iframe or a unit dying with 203/EXEC — the operator cannot
            // diagnose either.
            crate::archive::verify_entrypoints_present(
                manifest,
                &crate::archive::unpacked_paths(&staging),
                &host_arch_os(),
            )
            .map_err(LifecycleError::from)
        })
        .and_then(|()| {
            Attestation::new(
                contents.signature_text.clone(),
                &contents.file_digests,
                fetched,
            )
            .write_to(&staging)
            .map_err(LifecycleError::from)
        });
        // The plugin's system user and data dir, before anything replaces the
        // old install: the unit runs as that user and binds the dir writable,
        // so both must exist before the unit can start.
        let staged = staged
            .and_then(|()| self.ensure_account(manifest))
            .and_then(|()| self.ensure_data_dir(manifest));
        if let Err(e) = staged {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }

        let previous = find_install(&self.installs, &manifest.id).cloned();
        if let Some(previous) = &previous {
            // Stop what runs from the files about to be replaced: a later start
            // on the still-active unit is a no-op, so the old version would
            // keep running while state reports the new one.
            self.stop_for_replacement(previous);
        }
        let swap = swap_into_place(&staging, &target)?;

        let install = PluginInstall {
            plugin_id: manifest.id.clone(),
            version: manifest.version.clone(),
            source: PluginSource::LocalFile,
            source_uri: Some(source_path.display().to_string()),
            signer_id: signer.clone(),
            // Filled from the swapped-in tree.
            manifest_hash: String::new(),
            status: PluginStatus::Installed,
            installed_at: now_ms(),
            enabled_at: None,
            failure_reason: None,
            permissions: std::collections::BTreeMap::new(),
            // The operator's update preferences outlive a reinstall.
            auto_update: previous.as_ref().is_none_or(|p| p.auto_update),
            pinned_version: previous.as_ref().and_then(|p| p.pinned_version.clone()),
            last_update_check_at: None,
            last_update_attempt: None,
            reenable_pending: false,
            model_status: None,
            service_status: None,
            revocation_pending: false,
            archive_sha256,
        };
        // Everything from here to the saved record either completes or rolls
        // back: a half-applied install (new tree, old record) would fail every
        // later lifecycle call on a manifest-hash mismatch.
        if let Err(e) = self.write_unit_and_record(manifest, &target, install) {
            // Swap the previous tree back before touching its unit, so the
            // restored unit reads the files its record describes.
            drop(swap);
            self.restore_previous_unit(&manifest.id, previous.as_ref());
            return Err(e);
        }
        swap.commit();

        Ok(InstallResult {
            plugin_id: manifest.id.clone(),
            version: manifest.version.clone(),
            signer_id: signer,
            risk: manifest.risk.clone(),
            permissions_requested: manifest.declared_permissions().into_iter().collect(),
        })
    }

    /// The post-swap half of an install: write the unit for a subprocess agent
    /// half (a fresh install has granted nothing yet, so it renders with the
    /// most restrictive sandbox; grant/revoke re-renders it), then save the
    /// record with the swapped-in manifest's hash.
    pub(super) fn write_unit_and_record(
        &mut self,
        manifest: &PluginManifest,
        target: &Path,
        mut install: PluginInstall,
    ) -> Result<(), LifecycleError> {
        if let Some(spec) = build_unit_spec(
            manifest,
            &self.paths,
            &self.current_profile,
            &BTreeSet::new(),
            false,
        )? {
            self.backend.install(&unit_name_for(&manifest.id), &spec)?;
        }
        let manifest_bytes = std::fs::read(target.join(MANIFEST_FILENAME))?;
        install.manifest_hash = hex::encode(Sha256::digest(&manifest_bytes));
        let installs = upsert_install(self.installs.clone(), install);
        save_state(&installs, Some(&self.paths.state_path))?;
        self.installs = installs;
        Ok(())
    }

    /// After a failed install rolled the tree back: re-install the previous
    /// version's unit and start it again if it was running, or drop the unit a
    /// fresh install wrote. Best-effort, logged.
    pub(super) fn restore_previous_unit(&self, plugin_id: &str, previous: Option<&PluginInstall>) {
        let Some(previous) = previous else {
            if let Err(e) = self.backend.uninstall(&unit_name_for(plugin_id)) {
                tracing::warn!(plugin_id, error = %e, "plugin_install_rollback_unit_cleanup_failed");
            }
            tracing::warn!(plugin_id, "plugin_install_rolled_back");
            return;
        };
        let manifest = match self.manifest_for(plugin_id) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(plugin_id, error = %e, "plugin_install_rollback_manifest_unreadable");
                return;
            }
        };
        if let Err(e) = self.refresh_unit(plugin_id, &manifest) {
            tracing::error!(plugin_id, error = %e, "plugin_install_rollback_unit_failed");
        }
        if manifest.is_subprocess_agent()
            && matches!(
                previous.status,
                PluginStatus::Running | PluginStatus::Enabled
            )
        {
            match self.backend.enable_start(&unit_name_for(plugin_id)) {
                Ok(()) => self.start_declared_services(plugin_id, &manifest),
                Err(e) => {
                    tracing::error!(plugin_id, error = %e, "plugin_install_rollback_restart_failed")
                }
            }
        }
        tracing::warn!(plugin_id, "plugin_install_rolled_back");
    }
}
