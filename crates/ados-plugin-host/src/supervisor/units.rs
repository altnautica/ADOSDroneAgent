//! Host files, declared service units, and on-disk placement helpers.

use super::*;

impl PluginSupervisor {
    /// Create an `agent.http` plugin's socket dir, owned by the account its
    /// units run as. The dir lives on the run-dir tmpfs, so it is re-created
    /// at every enable and at daemon start.
    pub(super) fn ensure_http_dir(&self, manifest: &PluginManifest) -> Result<(), LifecycleError> {
        if !manifest.agent.as_ref().is_some_and(|a| a.http) {
            return Ok(());
        }
        let dir = plugin_http_dir(&self.paths.run_dir, &manifest.id);
        std::fs::create_dir_all(&dir)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770))?;
        }
        // A sandboxed unit runs as the plugin's own user; the dir must be its
        // to create the socket in.
        self.hand_over(&dir, &manifest.id)?;
        Ok(())
    }

    /// Create a subprocess plugin's data dir, `<data root>/<id>`: 0700 and
    /// handed to the plugin's own user, which the unit binds writable and
    /// nothing else under the data root. Run at install, every enable and
    /// daemon start, like the HTTP dir.
    ///
    /// A base created by this call is still the host's, so the per-drone dir
    /// the runner's `ADOS_PLUGIN_DATA_DIR` names on a paired node is made in it
    /// too, and the chain is handed over deepest first. An existing base is the
    /// plugin account's: it is handed over again (a leftover root-owned one is
    /// fixed) but never entered, so a root process never creates or chowns
    /// through a path the plugin controls. The runner makes a missing per-drone
    /// dir under its own base.
    pub(super) fn ensure_data_dir(&self, manifest: &PluginManifest) -> Result<(), LifecycleError> {
        if !manifest.is_subprocess_agent() {
            return Ok(());
        }
        let base = plugin_dir_under(&self.paths.data_root, &manifest.id)?;
        // The root stays traversable (0755 by the default umask): a unit
        // reaches its bound dir through it.
        std::fs::create_dir_all(&self.paths.data_root)?;
        let mut fresh: Vec<PathBuf> = Vec::new();
        if create_private_dir(&base)? {
            let agent_id = device_id_at(&self.paths.device_id_file, None);
            if is_one_component(&agent_id) {
                let leaf = plugin_data_dir(&self.paths.data_root, &manifest.id, &agent_id);
                for dir in [leaf.parent().map(Path::to_path_buf), Some(leaf)]
                    .into_iter()
                    .flatten()
                {
                    create_private_dir(&dir)?;
                    fresh.push(dir);
                }
            } else if !agent_id.is_empty() {
                tracing::warn!(
                    plugin_id = %manifest.id,
                    device_id = %agent_id,
                    "the device id is not a directory name; not creating a per-drone data dir"
                );
            }
        }
        for dir in fresh.iter().rev() {
            self.hand_over(dir, &manifest.id)?;
        }
        self.hand_over(&base, &manifest.id)?;
        Ok(())
    }

    /// Give `dir` to `plugin_id`'s own user. Only a sandboxing backend runs
    /// units as another account; elsewhere they run as this process's user,
    /// who already owns it.
    pub(super) fn hand_over(&self, dir: &Path, plugin_id: &str) -> Result<(), SupervisorError> {
        if self.backend.enforces_sandbox() {
            self.accounts.hand_over(dir, plugin_id)?;
        }
        Ok(())
    }

    /// Make sure everything an enabled plugin's units need from the plugin
    /// host exists before one starts: its socket dir (bound without the
    /// optional prefix, so a missing one fails the start in namespace setup),
    /// the host socket in it, and the token credential the unit loads.
    ///
    /// The host creates all three when it reconciles the Enabled record just
    /// saved, so it is asked to, synchronously. A host that cannot be reached
    /// leaves the socket to its next poll; the dir and a token minted from the
    /// shared secret are put in place here, so the unit still starts and its
    /// runner waits for the socket.
    pub(super) fn prepare_host_files(&self, plugin_id: &str) -> Result<(), LifecycleError> {
        if let Err(e) = crate::reconcile_via_control(&self.paths.control_dir) {
            tracing::warn!(
                plugin_id,
                detail = %e,
                "plugin host control socket unreachable; preparing the plugin's socket dir \
                 and token here"
            );
        }
        let socket_dir = plugin_socket_dir(&self.paths.socket_dir, plugin_id);
        if !socket_dir.is_dir() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(&socket_dir)?;
            std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o755))?;
        }
        if !token_credential_path(plugin_id, Some(&self.paths.socket_dir)).is_file() {
            let granted = self
                .find_install(plugin_id)
                .map(state::granted_caps)
                .unwrap_or_default();
            write_token_credential(
                &shared_issuer(&self.paths.token_secret)?,
                plugin_id,
                &granted,
                &plugin_socket_path(&self.paths.socket_dir, plugin_id),
                &self.paths.data_root,
                &device_id_at(&self.paths.device_id_file, None),
                Some(&self.paths.socket_dir),
            )?;
        }
        Ok(())
    }

    /// Stop a plugin whose files are about to be replaced by a reinstall.
    ///
    /// Best-effort: an old manifest that no longer reads, or a unit that is
    /// already gone, must not block the install. The reinstalled record lands as
    /// `Installed`, so the operator's next enable starts the NEW version.
    pub(super) fn stop_for_replacement(&self, previous: &PluginInstall) {
        if !matches!(
            previous.status,
            PluginStatus::Running | PluginStatus::Enabled
        ) {
            return;
        }
        let manifest = match self.manifest_for(&previous.plugin_id) {
            Ok(m) if m.is_subprocess_agent() => m,
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(
                    plugin_id = %previous.plugin_id,
                    error = %e,
                    "plugin_reinstall_old_manifest_unreadable"
                );
                return;
            }
        };
        // The declared services are stopped, disabled and deleted: the new
        // version may declare different ones, and enable() renders them afresh.
        self.stop_declared_services(&previous.plugin_id, &manifest);
        self.delete_declared_service_units(&previous.plugin_id, &manifest);
        if let Err(e) = self
            .backend
            .stop_disable(&unit_name_for(&previous.plugin_id))
        {
            tracing::warn!(
                plugin_id = %previous.plugin_id,
                error = %e,
                "plugin_reinstall_stop_failed"
            );
        }
    }

    // ------------------------------------------------------------------
    // Declared extra services (additive to the main runner unit)
    // ------------------------------------------------------------------

    /// Every service `manifest` declares. A block that no longer validates
    /// (install refused it, so only a tampered or pre-validation install can
    /// carry one) is logged and treated as declaring none.
    pub(super) fn services_of(
        &self,
        plugin_id: &str,
        manifest: &PluginManifest,
    ) -> Vec<ServiceSpec> {
        declared_services(manifest).unwrap_or_else(|e| {
            tracing::warn!(plugin_id, error = %e, "plugin_services_unreadable");
            Vec::new()
        })
    }

    /// The declared services that run on this node's profile.
    pub(super) fn active_services_of(
        &self,
        plugin_id: &str,
        manifest: &PluginManifest,
    ) -> Vec<ServiceSpec> {
        self.services_of(plugin_id, manifest)
            .into_iter()
            .filter(|s| s.applies_to(&self.current_profile))
            .collect()
    }

    /// Render, enable and start one unit per declared service that runs on
    /// this node. Best-effort per service: a failure is logged and surfaces as
    /// not-ready on the readiness probe; it never aborts the enable of the
    /// plugin's main half.
    pub(super) fn start_declared_services(&self, plugin_id: &str, manifest: &PluginManifest) {
        let granted = self
            .find_install(plugin_id)
            .map(state::granted_caps)
            .unwrap_or_default();
        let guard =
            ados_protocol::plugin_loopback_guard::read_state_at(&self.paths.loopback_guard_state);
        for service in self.active_services_of(plugin_id, manifest) {
            let name = service_unit_name_for(plugin_id, &service.name);
            let result = build_service_spec(
                manifest,
                &service,
                &self.paths,
                &self.current_profile,
                &granted,
                guard.active,
            )
            .and_then(|spec| self.backend.install(&name, &spec))
            .and_then(|_| self.backend.enable_start(&name));
            if let Err(e) = result {
                tracing::warn!(plugin_id, service = %service.name, error = %e, "plugin_service_start_failed");
            }
        }
    }

    /// Stop and disable each declared service unit. Best-effort.
    pub(super) fn stop_declared_services(&self, plugin_id: &str, manifest: &PluginManifest) {
        for service in self.services_of(plugin_id, manifest) {
            let name = service_unit_name_for(plugin_id, &service.name);
            if let Err(e) = self.backend.stop_disable(&name) {
                tracing::warn!(plugin_id, service = %service.name, error = %e, "plugin_service_stop_failed");
            }
        }
    }

    /// Delete each declared service's unit. Best-effort.
    pub(super) fn delete_declared_service_units(&self, plugin_id: &str, manifest: &PluginManifest) {
        for service in self.services_of(plugin_id, manifest) {
            let name = service_unit_name_for(plugin_id, &service.name);
            if let Err(e) = self.backend.uninstall(&name) {
                tracing::warn!(plugin_id, service = %service.name, error = %e, "plugin_service_delete_failed");
            }
        }
    }
}

/// Move each fetched payload from `payload_dir` into the staged tree at its
/// manifest path. A payload may not overwrite a file the archive ships. A
/// payload the manifest runs is made executable.
pub(super) fn place_payloads(
    manifest: &PluginManifest,
    payload_dir: &Path,
    staging: &Path,
    fetched: &[(String, String)],
) -> Result<(), LifecycleError> {
    let executables = Executables::of(manifest);
    for (path, _) in fetched {
        let dest = staging.join(path);
        if dest.exists() {
            return Err(SupervisorError(format!(
                "plugin {}: payload {path} collides with a file in the archive",
                manifest.id
            ))
            .into());
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(payload_dir.join(path), &dest)?;
        use std::os::unix::fs::PermissionsExt;
        let mode = if executables.contains(path) {
            0o755
        } else {
            0o644
        };
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// The default signature-enforcement policy for [`PluginSupervisor::production`]:
/// ON unless `ADOS_PLUGIN_REQUIRE_SIGNED` is explicitly one of the falsey tokens
/// (`0` / `false` / `no` / `off`, case-insensitive). An absent or unrecognized
/// value keeps enforcement ON, so the secure posture is the default a misconfig
/// cannot weaken.
pub fn require_signed_default() -> bool {
    match std::env::var("ADOS_PLUGIN_REQUIRE_SIGNED") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// The archive contents of a built-in plugin: a one-entry archive holding its
/// manifest, parsed and validated like any other so the install path, the
/// attestation and the recorded manifest hash cover exactly these bytes.
pub(super) fn builtin_contents(manifest_yaml: &str) -> Result<ArchiveContents, LifecycleError> {
    use std::io::Write;
    let mut raw: Vec<u8> = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut raw));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file(MANIFEST_FILENAME, stored)
            .and_then(|()| zip.write_all(manifest_yaml.as_bytes()).map_err(Into::into))
            .and_then(|()| zip.finish().map(|_| ()))
            .map_err(|e| SupervisorError(format!("built-in manifest archive: {e}")))?;
    }
    parse_archive_bytes(raw)
}

/// A plugin's own dir under one of the host's roots (its unpacked tree under
/// the install dir, its data under the data root): a direct child of `root`
/// named by the id.
///
/// The host swaps, deletes and hands these dirs over as root, so an id that
/// is absolute, carries a separator, or walks up with `..` would aim all of
/// that at an arbitrary host directory. The manifest parser already refuses
/// such an id; this is the check at the point of use, so no id that reaches
/// here some other way can widen the blast radius past one plugin dir.
pub(super) fn plugin_dir_under(root: &Path, plugin_id: &str) -> Result<PathBuf, SupervisorError> {
    if is_one_component(plugin_id) {
        Ok(root.join(plugin_id))
    } else {
        Err(SupervisorError(format!(
            "plugin id {plugin_id:?} does not name a directory under {}",
            root.display()
        )))
    }
}

/// Whether `name` is exactly one normal path component: not empty, not
/// absolute, not `.` or `..`, no separator.
pub(super) fn is_one_component(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// Create `dir` 0700, returning whether this call created it. An existing
/// directory is left as it is; anything else there (a symlink included) is an
/// error.
pub(super) fn create_private_dir(dir: &Path) -> Result<bool, LifecycleError> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(dir)?.is_dir() {
                Ok(false)
            } else {
                Err(SupervisorError(format!("{} is not a directory", dir.display())).into())
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// A swapped-in install tree that is not final yet: the previous tree stays at
/// `.<id>.retired` until [`commit`](Self::commit). Dropped uncommitted, it
/// removes the new tree and renames the previous one back.
#[must_use = "an uncommitted swap rolls back when dropped"]
pub(super) struct SwapGuard {
    target: PathBuf,
    retired: Option<PathBuf>,
    committed: bool,
}

impl SwapGuard {
    /// Keep the new tree and delete the retired one.
    pub(super) fn commit(mut self) {
        self.committed = true;
        if let Some(retired) = &self.retired {
            let _ = std::fs::remove_dir_all(retired);
        }
    }
}

impl Drop for SwapGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.target) {
            tracing::error!(dir = %self.target.display(), error = %e, "plugin_install_rollback_failed");
            return;
        }
        if let Some(retired) = &self.retired {
            if let Err(e) = std::fs::rename(retired, &self.target) {
                tracing::error!(dir = %self.target.display(), error = %e, "plugin_install_rollback_failed");
            }
        }
    }
}

/// Replace `target` with the validated `staging` tree by renames, so the
/// install dir only ever holds a complete old tree or a complete new one. The
/// returned guard keeps the old tree until committed.
pub(super) fn swap_into_place(staging: &Path, target: &Path) -> Result<SwapGuard, LifecycleError> {
    if !target.exists() {
        std::fs::rename(staging, target)?;
        return Ok(SwapGuard {
            target: target.to_path_buf(),
            retired: None,
            committed: false,
        });
    }
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let retired = target.with_file_name(format!(".{name}.retired"));
    if retired.exists() {
        std::fs::remove_dir_all(&retired)?;
    }
    std::fs::rename(target, &retired)?;
    std::fs::rename(staging, target)?;
    Ok(SwapGuard {
        target: target.to_path_buf(),
        retired: Some(retired),
        committed: false,
    })
}

/// Bounded semver-range parser for the constraint vocabulary.
///
/// Supports comma-separated atoms each of the form `<op><semver>` where op is
/// one of `>=`, `<=`, `>`, `<`, `==`, `=`. Atoms are AND-ed. A bare `<semver>`
/// means `==<semver>`. An unparseable semver is a [`SupervisorError`]. Byte-for-
/// byte the same comparison logic as the Python `_semver_in_range`.
pub fn semver_in_range(version: &str, constraint: &str) -> Result<bool, SupervisorError> {
    let cur = semver_tuple(version)?;
    for atom in constraint
        .split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        let (op, target) = split_op(atom);
        let tt = semver_tuple(target)?;
        let ok = match op {
            "==" | "=" => cur == tt,
            ">=" => cur >= tt,
            "<=" => cur <= tt,
            ">" => cur > tt,
            "<" => cur < tt,
            _ => false,
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn split_op(atom: &str) -> (&str, &str) {
    for op in [">=", "<=", "==", ">", "<", "="] {
        if let Some(rest) = atom.strip_prefix(op) {
            return (op, rest.trim());
        }
    }
    ("==", atom)
}

pub(super) fn semver_tuple(v: &str) -> Result<(u64, u64, u64), SupervisorError> {
    // Strip pre-release / build metadata, then take the first three dotted ints.
    let base = v.split('-').next().unwrap_or(v);
    let base = base.split('+').next().unwrap_or(base);
    let mut parts: Vec<&str> = base.split('.').collect();
    while parts.len() < 3 {
        parts.push("0");
    }
    let parse = |s: &str| -> Result<u64, SupervisorError> {
        s.parse::<u64>()
            .map_err(|_| SupervisorError(format!("unparseable semver {v}")))
    };
    Ok((parse(parts[0])?, parse(parts[1])?, parse(parts[2])?))
}
