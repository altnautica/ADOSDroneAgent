//! Plugin lifecycle controller: install / enable / disable / remove.
//!
//! This is the lifecycle controller, NOT the OS process supervisor. It reads
//! on-disk install state, reconciles it against unpacked archives at
//! `/var/ados/plugins/<id>/`, installs a `.adosplug` archive (verify signature,
//! run compatibility + first-party-isolation gates, unpack, write the systemd
//! unit, persist state), and drives the enable/disable/remove state machine.
//! It does not run plugin code itself; subprocess plugins are started by
//! systemd via the generated unit.
//!
//! Compatibility gates at install time:
//! * `compatibility.ados_version` must include the running agent version.
//! * `compatibility.supported_boards` (if non-empty) must include the current
//!   HAL board id.
//! * `isolation: inprocess` (agent) requires a first-party signer.
//! * `isolation: inline` (GCS) requires a first-party signer.
//!
//! The `systemctl` calls and filesystem ops are real, but the crate stays
//! lib-only: no test invokes systemd (the [`SystemctlRunner`] is injectable and
//! the tests use a no-op recorder).
//!
//! ## Stable consumer contract
//!
//! This controller is consumed in-process as a library by the agent's other
//! long-running services (notably the cloud relay, which drives plugin
//! install/enable/disable/remove from remote commands and a periodic
//! auto-update loop). Those callers depend on the surface below; treat it as
//! stable and change it deliberately:
//!
//! * [`install_archive`](PluginSupervisor::install_archive) `(&Path) -> Result<InstallResult, LifecycleError>`
//! * [`enable`](PluginSupervisor::enable), [`disable`](PluginSupervisor::disable),
//!   [`remove`](PluginSupervisor::remove) `(&str, keep_data: bool)`
//! * [`grant_permission`](PluginSupervisor::grant_permission),
//!   [`revoke_permission`](PluginSupervisor::revoke_permission)
//! * [`installs`](PluginSupervisor::installs),
//!   [`find_install`](PluginSupervisor::find_install)
//!
//! [`InstallResult`] (plugin_id, version, signer_id, risk, permissions_requested)
//! and [`PluginInstall`] (the on-disk install record) are part of the same
//! contract. There is no `configure` method: a batched permission change is the
//! caller's own grant/revoke sequence. [`SystemctlRunner`] is injectable, so a
//! consumer constructs the controller with the real runner in production and a
//! recorder in tests without touching systemd.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::archive::{open_archive, unpack_to, ArchiveContents, MANIFEST_FILENAME};
use crate::errors::{
    LifecycleError, ManifestError, SignatureError, SignatureErrorKind, SupervisorError,
};
use crate::manifest::{AgentIsolation, GcsIsolation, PluginManifest};
use crate::signing::{
    is_first_party_signer, load_revocation_list, load_trusted_keys, verify_archive_signature,
};
use crate::state::{
    self, filter_permissions_against_manifest, find_install, grant_permission, load_state, now_ms,
    remove_install, revoke_permission, save_state, upsert_install, PluginInstall, PluginSource,
    PluginStatus, StateLock,
};
use crate::systemd::{
    render_unit, slice_unit_content, slice_unit_path, unit_name_for, unit_path_for, PLUGIN_LOG_DIR,
    PLUGIN_UNIT_DIR,
};

/// Default install directory for unpacked third-party archives.
pub const PLUGINS_INSTALL_DIR: &str = "/var/ados/plugins";

/// Summary returned from a successful install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub plugin_id: String,
    pub version: String,
    pub signer_id: Option<String>,
    pub risk: String,
    pub permissions_requested: Vec<String>,
}

/// Thin `systemctl` runner. The default [`RealSystemctl`] shells out; tests
/// inject a recorder so no test invokes systemd.
pub trait SystemctlRunner: Send + Sync {
    /// Run `systemctl <args...>`. Returns `Err` with the failure detail on a
    /// non-zero exit, a missing binary, or a timeout.
    fn run(&self, args: &[&str]) -> Result<(), SupervisorError>;
}

/// Production runner: shells out to `systemctl` with a 15s timeout-equivalent
/// (the agent control plane is synchronous and short).
pub struct RealSystemctl;

impl SystemctlRunner for RealSystemctl {
    fn run(&self, args: &[&str]) -> Result<(), SupervisorError> {
        let output = std::process::Command::new("systemctl")
            .args(args)
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SupervisorError("systemctl not found; is this a systemd host?".to_string())
                } else {
                    SupervisorError(format!("systemctl {} failed to spawn: {e}", args.join(" ")))
                }
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SupervisorError(format!(
                "systemctl {} failed: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        Ok(())
    }
}

/// Filesystem + unit-dir layout the controller writes to. Tests point these at
/// a tempdir so the install path runs end-to-end without touching the host.
#[derive(Debug, Clone)]
pub struct Paths {
    pub install_dir: PathBuf,
    pub unit_dir: PathBuf,
    pub state_path: PathBuf,
    pub log_dir: PathBuf,
    /// The plugin host's control dir, where its root-only control socket lives.
    /// The controller reaches the live daemon through it after a grant or
    /// revoke, so a permission change is effective immediately rather than at
    /// the next daemon restart.
    pub control_dir: PathBuf,
    /// The loopback-guard verdict sidecar the plugin-host daemon writes at
    /// startup. A `network.outbound` grant needs it to read active.
    pub loopback_guard_state: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Paths {
            install_dir: PathBuf::from(PLUGINS_INSTALL_DIR),
            unit_dir: PathBuf::from(PLUGIN_UNIT_DIR),
            state_path: PathBuf::from(state::PLUGIN_STATE_PATH),
            log_dir: PathBuf::from(PLUGIN_LOG_DIR),
            control_dir: PathBuf::from(crate::control::DEFAULT_CONTROL_DIR),
            loopback_guard_state: ados_protocol::plugin_loopback_guard::sidecar_path(),
        }
    }
}

/// Plugin lifecycle controller. Constructed once per agent.
pub struct PluginSupervisor {
    paths: Paths,
    require_signed: bool,
    current_board_id: Option<String>,
    /// The current board's compute tier (1-4), for the `min_tier` floor. `None`
    /// leaves the floor unenforced, matching the `supported_boards` posture: a
    /// node that cannot tell you its tier does not get to refuse installs over
    /// a guess.
    current_board_tier: Option<u8>,
    agent_version: String,
    systemctl: Arc<dyn SystemctlRunner>,
    installs: Vec<PluginInstall>,
    /// Capabilities the active host runtime cannot back: their host method
    /// returns the `not_implemented` shape no matter what, so granting one buys
    /// the operator nothing but a surprise error at call time. The controller
    /// refuses to grant these (an honest refuse-at-install). Populated from
    /// [`RealHost::ungrantable_caps`](crate::realhost::RealHost::ungrantable_caps)
    /// by the daemons that wire the default Rust host; empty by default so a
    /// caller with a different host (e.g. tests, or a future fully-wired host)
    /// imposes no refusal.
    ungrantable_caps: BTreeSet<String>,
    /// Built-in plugin manifests keyed by id. The discovery of built-ins from
    /// Python entry-points stays in the Python loader; the Rust controller
    /// accepts them via [`PluginSupervisor::set_builtins`] so the lifecycle
    /// logic (manifest-for, hash tamper check, isolation gate) is identical for
    /// built-in and third-party plugins.
    builtin: std::collections::BTreeMap<String, PluginManifest>,
}

impl PluginSupervisor {
    /// Build a controller. `agent_version` is the running agent semver (the
    /// Python side reads `ados.__version__`); the constraint check compares it
    /// against each plugin's `compatibility.ados_version`.
    pub fn new(
        paths: Paths,
        require_signed: bool,
        current_board_id: Option<String>,
        agent_version: impl Into<String>,
    ) -> Self {
        PluginSupervisor {
            paths,
            require_signed,
            current_board_id,
            agent_version: agent_version.into(),
            systemctl: Arc::new(RealSystemctl),
            installs: Vec::new(),
            builtin: std::collections::BTreeMap::new(),
            ungrantable_caps: BTreeSet::new(),
            current_board_tier: None,
        }
    }

    /// Build a controller for the live agent, with signature enforcement ON by
    /// default. This is the safe constructor every daemon should use so a live
    /// install path can never silently accept an unsigned archive: a caller
    /// cannot accidentally pass `false`.
    ///
    /// The default is overridable for a deliberately-relaxed deployment via the
    /// `ADOS_PLUGIN_REQUIRE_SIGNED` environment variable: any of `0`, `false`,
    /// `no`, `off` (case-insensitive) turns enforcement OFF; everything else
    /// (including the variable being absent) leaves it ON. The relaxed choice is
    /// logged so it is never silent.
    pub fn production(
        paths: Paths,
        current_board_id: Option<String>,
        agent_version: impl Into<String>,
    ) -> Self {
        let require_signed = require_signed_default();
        if !require_signed {
            tracing::warn!(
                "plugin signature enforcement DISABLED via ADOS_PLUGIN_REQUIRE_SIGNED; \
                 unsigned plugin archives will be accepted"
            );
        }
        PluginSupervisor::new(paths, require_signed, current_board_id, agent_version)
    }

    /// Inject a `systemctl` runner (tests use a no-op recorder).
    pub fn with_systemctl(mut self, runner: Arc<dyn SystemctlRunner>) -> Self {
        self.systemctl = runner;
        self
    }

    /// True when this controller enforces a valid first-party signature before
    /// installing an archive. Exposed so a caller / test can assert the live
    /// wiring path is signed.
    pub fn require_signed(&self) -> bool {
        self.require_signed
    }

    /// Declare the capabilities the active host runtime cannot back, so the
    /// controller refuses to grant them ([`grant_permission`](Self::grant_permission)
    /// returns an error naming the cap). The daemons pass
    /// [`RealHost::ungrantable_caps`](crate::realhost::RealHost::ungrantable_caps);
    /// see that method for why a cap shared with a wired surface is never in the
    /// set.
    pub fn with_ungrantable_caps(mut self, caps: BTreeSet<String>) -> Self {
        self.ungrantable_caps = caps;
        self
    }

    /// Declare the current board's compute tier so the `compatibility.min_tier`
    /// floor is enforced. Without it an NPU-dependent plugin installs on a
    /// tier-1 board and crash-loops instead of being refused up front.
    pub fn with_board_tier(mut self, tier: Option<u8>) -> Self {
        self.current_board_tier = tier;
        self
    }

    /// Provide the built-in plugin manifests discovered out-of-band (Python
    /// entry-points). Keyed by plugin id.
    pub fn set_builtins(&mut self, builtins: std::collections::BTreeMap<String, PluginManifest>) {
        self.builtin = builtins;
    }

    /// Read on-disk state and filter each install's granted permissions down to
    /// what its manifest currently declares (defends against a tampered state
    /// file). Built-ins must be set via [`set_builtins`] first if the device
    /// has any built-in installs recorded.
    pub fn discover(&mut self) -> Result<(), LifecycleError> {
        self.installs = load_state(Some(&self.paths.state_path));
        // Collect the ids to reconcile up-front so the manifest lookups can
        // borrow `self` immutably while the filter mutates each install.
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
        tracing::info!(
            builtin_count = self.builtin.len(),
            installed_count = self.installs.len(),
            "plugin_supervisor_discovered"
        );
        Ok(())
    }

    /// Current in-memory install list.
    pub fn installs(&self) -> &[PluginInstall] {
        &self.installs
    }

    /// The install record for `plugin_id`, if installed.
    pub fn find_install(&self, plugin_id: &str) -> Option<&PluginInstall> {
        find_install(&self.installs, plugin_id)
    }

    // ------------------------------------------------------------------
    // Install / enable / disable / remove
    // ------------------------------------------------------------------

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

    /// Install from parsed contents, choosing whether a version downgrade is
    /// permitted.
    pub fn install_contents_with(
        &mut self,
        contents: ArchiveContents,
        source_path: &Path,
        allow_downgrade: bool,
    ) -> Result<InstallResult, LifecycleError> {
        let manifest = contents.manifest.clone();

        if self.require_signed {
            let (Some(signer_id), Some(sig_b64)) = (
                contents.signer_id.as_deref(),
                contents.signature_b64.as_deref(),
            ) else {
                return Err(SignatureError::new(
                    SignatureErrorKind::Missing,
                    format!("plugin {}: archive is unsigned", manifest.id),
                )
                .into());
            };
            let trusted = load_trusted_keys(None);
            let revocations = load_revocation_list(None);
            verify_archive_signature(
                &contents.payload_hash,
                sig_b64,
                signer_id,
                &trusted,
                &revocations,
            )?;
        }

        self.check_compatibility(&manifest, contents.signer_id.as_deref())?;
        self.reject_inline_for_third_party(&manifest, contents.signer_id.as_deref())?;
        if !allow_downgrade {
            self.reject_downgrade(&manifest)?;
        }

        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;

        let target = plugin_install_target(&self.paths.install_dir, &manifest.id)?;
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        unpack_to(&contents.raw_archive_bytes, &target)?;

        // A manifest that declares a GCS half (or a rust agent binary) must
        // actually ship the file it points at. Fail here rather than letting a
        // missing bundle surface later as an empty iframe or a unit dying with
        // 203/EXEC — the operator cannot diagnose either. The half-unpacked
        // dir goes with it so a retry starts clean.
        if let Err(e) = crate::archive::verify_entrypoints_present(
            &manifest,
            &crate::archive::unpacked_paths(&target),
        ) {
            let _ = std::fs::remove_dir_all(&target);
            return Err(e.into());
        }

        // Write the systemd unit for subprocess agent halves. A fresh install
        // has granted nothing yet, so the unit renders with the most
        // restrictive sandbox; grant/revoke re-renders it.
        if manifest.is_subprocess_agent() {
            self.ensure_slice_exists()?;
            let unit_path = unit_path_for(&manifest.id, Some(&self.paths.unit_dir));
            if let Some(unit) =
                render_unit(&manifest, &self.paths.install_dir, &BTreeSet::new(), false)?
            {
                std::fs::write(&unit_path, unit.as_bytes())?;
            }
            self.systemctl.run(&["daemon-reload"])?;
        }

        let manifest_bytes = std::fs::read(target.join(MANIFEST_FILENAME))?;
        let manifest_hash = hex::encode(Sha256::digest(&manifest_bytes));

        let install = PluginInstall {
            plugin_id: manifest.id.clone(),
            version: manifest.version.clone(),
            source: PluginSource::LocalFile,
            source_uri: Some(source_path.display().to_string()),
            signer_id: contents.signer_id.clone(),
            manifest_hash,
            status: PluginStatus::Installed,
            installed_at: now_ms(),
            enabled_at: None,
            failure_reason: None,
            permissions: std::collections::BTreeMap::new(),
            auto_update: true,
            pinned_version: None,
            last_update_check_at: None,
            last_update_attempt: None,
            model_status: None,
            service_status: None,
        };
        self.installs = upsert_install(std::mem::take(&mut self.installs), install);
        save_state(&self.installs, Some(&self.paths.state_path))?;

        tracing::info!(
            plugin_id = %manifest.id,
            version = %manifest.version,
            signer_id = ?contents.signer_id,
            "plugin_installed"
        );

        Ok(InstallResult {
            plugin_id: manifest.id.clone(),
            version: manifest.version.clone(),
            signer_id: contents.signer_id,
            risk: manifest.risk.clone(),
            permissions_requested: manifest.declared_permissions().into_iter().collect(),
        })
    }

    /// Grant a declared permission. Rejects a permission the manifest does not
    /// declare, and a permission whose capability the active host runtime cannot
    /// back (see [`with_ungrantable_caps`](Self::with_ungrantable_caps)) so an
    /// operator never grants a capability that can only error at call time.
    // The explicit `drop(_lock)` below releases the state lock before
    // re-entering the state path. That is load-bearing on Linux, where
    // `StateLock` owns the `Flock` guard; on a non-Linux dev host the struct
    // has no fields and the call is a no-op, which is the build clippy sees.
    #[allow(clippy::drop_non_drop)]
    pub fn grant_permission(
        &mut self,
        plugin_id: &str,
        permission_id: &str,
    ) -> Result<(), LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        let manifest = self.manifest_for(plugin_id)?;
        if !manifest.declared_permissions().contains(permission_id) {
            return Err(SupervisorError(format!(
                "plugin {plugin_id} did not declare permission {permission_id}"
            ))
            .into());
        }
        // Refuse a capability the active host runtime cannot back: its host
        // method returns not_implemented regardless of wiring, so granting it
        // would only surface a surprise error when the plugin first calls the
        // gated method. Refuse honestly at grant time instead.
        if self.ungrantable_caps.contains(permission_id) {
            return Err(SupervisorError(format!(
                "plugin {plugin_id}: capability {permission_id} is not supported by this \
                 agent runtime (its host method is not implemented); refusing to grant a \
                 capability that can only error at call time"
            ))
            .into());
        }
        // Network access is only safe to hold while the loopback guard keeps
        // the plugin off the agent's own listeners.
        if permission_id == crate::sandbox::NETWORK_OUTBOUND_CAP {
            let guard = ados_protocol::plugin_loopback_guard::read_state_at(
                &self.paths.loopback_guard_state,
            );
            if !guard.active {
                return Err(SupervisorError(format!(
                    "plugin {plugin_id}: {permission_id} refused: the plugin loopback guard is \
                     unavailable ({}), so a network-capable plugin could reach the agent's own \
                     loopback services",
                    guard.reason
                ))
                .into());
            }
        }
        let install = self.require_install_mut(plugin_id)?;
        grant_permission(install, permission_id);
        save_state(&self.installs, Some(&self.paths.state_path))?;
        drop(_lock);
        self.apply_permission_change(plugin_id, &manifest)?;
        Ok(())
    }

    /// Revoke a granted permission.
    ///
    /// Takes effect immediately: the sandbox half of the unit is re-rendered
    /// and the plugin's capability token is re-minted from the new grant set
    /// (see [`apply_permission_change`](Self::apply_permission_change)). It
    /// used to take effect only on the next plugin-host restart, so an
    /// operator revoking `mavlink.write` from a misbehaving plugin saw success
    /// in the CLI and the GCS while the plugin kept commanding the flight
    /// controller.
    // The explicit `drop(_lock)` below releases the state lock before
    // re-entering the state path. That is load-bearing on Linux, where
    // `StateLock` owns the `Flock` guard; on a non-Linux dev host the struct
    // has no fields and the call is a no-op, which is the build clippy sees.
    #[allow(clippy::drop_non_drop)]
    pub fn revoke_permission(
        &mut self,
        plugin_id: &str,
        permission_id: &str,
    ) -> Result<(), LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        let manifest = self.manifest_for(plugin_id)?;
        let install = self.require_install_mut(plugin_id)?;
        revoke_permission(install, permission_id);
        save_state(&self.installs, Some(&self.paths.state_path))?;
        drop(_lock);
        self.apply_permission_change(plugin_id, &manifest)?;
        Ok(())
    }

    /// Make a permission change real, right now.
    ///
    /// Two mechanisms enforce a plugin's capabilities and both are snapshots
    /// taken when something was last written, so both have to be refreshed:
    ///
    /// 1. **The sandbox.** `hardware.*`, `network.outbound` and
    ///    `filesystem.host` live in the generated unit (see
    ///    [`crate::sandbox`]), so the unit is re-rendered and, when the grant
    ///    set actually changed the text, the plugin is restarted to pick up the
    ///    new namespace. A restart is the only way: systemd applies
    ///    `DeviceAllow`/`RestrictAddressFamilies` at exec time.
    /// 2. **The capability token.** Every wire-gated capability rides in the
    ///    plugin's HMAC token. The live plugin host re-mints it through the
    ///    control socket, which also pushes the fresh token to the open
    ///    connection so the next request re-gates against the new set.
    ///
    /// A restart is skipped when the unit text is unchanged (a purely
    /// wire-gated capability), because bouncing a running geofence plugin to
    /// apply a token change it can receive live is a needless gap in coverage.
    /// Re-render every installed plugin's unit against the current grant set
    /// and loopback-guard verdict, restarting a running plugin whose sandbox
    /// changed. The plugin-host daemon calls it at startup, after loading the
    /// guard, so a unit rendered while the guard was active cannot keep network
    /// access into a boot where it is not.
    pub fn refresh_all_units(&self) {
        let ids: Vec<String> = self.installs.iter().map(|i| i.plugin_id.clone()).collect();
        for plugin_id in ids {
            let result = self
                .manifest_for(&plugin_id)
                .map_err(LifecycleError::from)
                .and_then(|m| self.refresh_unit(&plugin_id, &m));
            if let Err(e) = result {
                tracing::error!(plugin_id, error = %e, "plugin_unit_refresh_failed");
            }
        }
    }

    /// Rewrite one plugin's unit when its rendered sandbox differs from disk,
    /// then restart it if it is running (systemd applies the sandbox at exec).
    fn refresh_unit(
        &self,
        plugin_id: &str,
        manifest: &PluginManifest,
    ) -> Result<(), LifecycleError> {
        if !manifest.is_subprocess_agent() {
            return Ok(());
        }
        let granted = self
            .find_install(plugin_id)
            .map(state::granted_caps)
            .unwrap_or_default();
        let guard =
            ados_protocol::plugin_loopback_guard::read_state_at(&self.paths.loopback_guard_state);
        let unit_path = unit_path_for(plugin_id, Some(&self.paths.unit_dir));
        if let Some(unit) = render_unit(manifest, &self.paths.install_dir, &granted, guard.active)?
        {
            let previous = std::fs::read_to_string(&unit_path).unwrap_or_default();
            if previous != unit {
                std::fs::write(&unit_path, unit.as_bytes())?;
                self.systemctl.run(&["daemon-reload"])?;
                let running = self
                    .find_install(plugin_id)
                    .is_some_and(|i| matches!(i.status, PluginStatus::Running));
                if running {
                    self.systemctl
                        .run(&["restart", &unit_name_for(plugin_id)])?;
                }
            }
        }
        Ok(())
    }

    fn apply_permission_change(
        &self,
        plugin_id: &str,
        manifest: &PluginManifest,
    ) -> Result<(), LifecycleError> {
        self.refresh_unit(plugin_id, manifest)?;

        // Re-mint the live token. A plugin host that is not up has nothing to
        // re-mint against and will read the new grant set off state when it
        // starts, so an unreachable control socket is logged, not an error.
        match crate::rotate_token_via_control(&self.paths.control_dir, plugin_id) {
            Ok(()) => tracing::info!(plugin_id, "plugin_token_rotated_after_permission_change"),
            Err(e) => tracing::info!(
                plugin_id,
                detail = %e,
                "plugin host control socket unreachable; the new grant set applies \
                 when it next reads state"
            ),
        }
        Ok(())
    }

    /// Enable a plugin. Idempotent: a running plugin is left running. Built-in
    /// / `inprocess` plugins only flip state; subprocess plugins are
    /// `systemctl enable + start`.
    pub fn enable(&mut self, plugin_id: &str) -> Result<(), LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        let manifest = self.manifest_for(plugin_id)?;
        let is_subprocess = manifest.is_subprocess_agent();
        {
            let install = self.require_install_ref(plugin_id)?;
            if install.status == PluginStatus::Running {
                return Ok(());
            }
        }
        if !is_subprocess {
            let install = self.require_install_mut(plugin_id)?;
            install.status = PluginStatus::Enabled;
            install.enabled_at = Some(now_ms());
            save_state(&self.installs, Some(&self.paths.state_path))?;
            return Ok(());
        }
        let unit = unit_name_for(plugin_id);
        self.systemctl.run(&["enable", &unit])?;
        self.systemctl.run(&["start", &unit])?;
        let install = self.require_install_mut(plugin_id)?;
        install.status = PluginStatus::Running;
        install.enabled_at = Some(now_ms());
        save_state(&self.installs, Some(&self.paths.state_path))?;
        tracing::info!(plugin_id, "plugin_enabled");
        Ok(())
    }

    /// Disable a plugin. Idempotent: an already-disabled plugin is a no-op.
    /// Subprocess plugins are `systemctl stop + disable`.
    pub fn disable(&mut self, plugin_id: &str) -> Result<(), LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        let manifest = self.manifest_for(plugin_id)?;
        let is_subprocess = manifest.is_subprocess_agent();
        {
            let install = self.require_install_ref(plugin_id)?;
            if install.status == PluginStatus::Disabled {
                return Ok(());
            }
        }
        if is_subprocess {
            let unit = unit_name_for(plugin_id);
            self.systemctl.run(&["stop", &unit])?;
            self.systemctl.run(&["disable", &unit])?;
        }
        let install = self.require_install_mut(plugin_id)?;
        install.status = PluginStatus::Disabled;
        install.enabled_at = None;
        save_state(&self.installs, Some(&self.paths.state_path))?;
        tracing::info!(plugin_id, "plugin_disabled");
        Ok(())
    }

    /// Remove a plugin: disable it (if running/enabled), remove the unit, delete
    /// the unpacked dir and (unless `keep_data`) the log, and drop the state.
    pub fn remove(&mut self, plugin_id: &str, keep_data: bool) -> Result<(), LifecycleError> {
        // disable() takes the lock itself; run it first, outside the lock below.
        let status = self.require_install_ref(plugin_id)?.status;
        if matches!(status, PluginStatus::Running | PluginStatus::Enabled) {
            if let Err(e) = self.disable(plugin_id) {
                tracing::warn!(plugin_id, error = %e, "plugin_disable_during_remove_failed");
            }
        }

        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        let manifest = self.manifest_for(plugin_id)?;
        if manifest.is_subprocess_agent() {
            let unit_path = unit_path_for(plugin_id, Some(&self.paths.unit_dir));
            if unit_path.exists() {
                std::fs::remove_file(&unit_path)?;
            }
            self.systemctl.run(&["daemon-reload"])?;
        }
        let target = plugin_install_target(&self.paths.install_dir, plugin_id)?;
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        if !keep_data {
            let log_file = self
                .paths
                .log_dir
                .join(format!("{}.log", plugin_id.replace('.', "-")));
            if log_file.exists() {
                std::fs::remove_file(&log_file)?;
            }
        }
        self.installs = remove_install(std::mem::take(&mut self.installs), plugin_id);
        save_state(&self.installs, Some(&self.paths.state_path))?;
        tracing::info!(plugin_id, keep_data, "plugin_removed");
        Ok(())
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn require_install_ref(&self, plugin_id: &str) -> Result<&PluginInstall, SupervisorError> {
        find_install(&self.installs, plugin_id)
            .ok_or_else(|| SupervisorError(format!("plugin {plugin_id} is not installed")))
    }

    fn require_install_mut(
        &mut self,
        plugin_id: &str,
    ) -> Result<&mut PluginInstall, SupervisorError> {
        state::find_install_mut(&mut self.installs, plugin_id)
            .ok_or_else(|| SupervisorError(format!("plugin {plugin_id} is not installed")))
    }

    /// Resolve the manifest for a plugin: built-in first, otherwise read off the
    /// unpacked dir. For a disk-backed manifest the recorded `manifest_hash` is
    /// re-checked against the on-disk bytes (tamper detection).
    fn manifest_for(&self, plugin_id: &str) -> Result<PluginManifest, SupervisorError> {
        if let Some(builtin) = self.builtin.get(plugin_id) {
            return Ok(builtin.clone());
        }
        let manifest_path = self
            .paths
            .install_dir
            .join(plugin_id)
            .join(MANIFEST_FILENAME);
        if !manifest_path.exists() {
            return Err(SupervisorError(format!(
                "plugin {plugin_id} manifest missing at {}",
                manifest_path.display()
            )));
        }
        let manifest_bytes = std::fs::read(&manifest_path).map_err(|e| {
            SupervisorError(format!("read of {} failed: {e}", manifest_path.display()))
        })?;
        // Manifest-hash tamper check against the recorded install hash.
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
        let text = std::str::from_utf8(&manifest_bytes)
            .map_err(|e| SupervisorError(format!("manifest is not valid UTF-8: {e}")))?;
        PluginManifest::from_yaml_text(text).map_err(|e: ManifestError| SupervisorError(e.0))
    }

    /// Run the version + board + tier + inprocess-isolation gates.
    fn check_compatibility(
        &self,
        manifest: &PluginManifest,
        signer_id: Option<&str>,
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
                    "plugin {} does not support board {board}",
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
            if agent.isolation == AgentIsolation::Inprocess
                && signer_id.map(is_first_party_signer) != Some(true)
            {
                return Err(SupervisorError(format!(
                    "plugin {} requests inprocess isolation but signer {} is not first-party",
                    manifest.id,
                    signer_id.unwrap_or("<none>")
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
    fn reject_downgrade(&self, manifest: &PluginManifest) -> Result<(), LifecycleError> {
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
    fn reject_inline_for_third_party(
        &self,
        manifest: &PluginManifest,
        signer_id: Option<&str>,
    ) -> Result<(), LifecycleError> {
        if let Some(gcs) = &manifest.gcs {
            if gcs.isolation == GcsIsolation::Inline
                && signer_id.map(is_first_party_signer) != Some(true)
            {
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

    /// Write the shared slice file if absent, then reload.
    fn ensure_slice_exists(&self) -> Result<(), LifecycleError> {
        let slice_path = slice_unit_path(Some(&self.paths.unit_dir));
        if slice_path.exists() {
            return Ok(());
        }
        if let Some(parent) = slice_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&slice_path, slice_unit_content().as_bytes())?;
        self.systemctl.run(&["daemon-reload"])?;
        Ok(())
    }
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

/// The plugin's unpacked dir: a direct child of `install_dir` named by the id.
///
/// The install path runs `remove_dir_all` on this and then unpacks into it, as
/// root, so an id that is absolute, carries a separator, or walks up with `..`
/// would aim both at an arbitrary host directory. The manifest parser already
/// refuses such an id; this is the check at the point of use, so no id that
/// reaches here some other way can widen the blast radius past one plugin dir.
fn plugin_install_target(install_dir: &Path, plugin_id: &str) -> Result<PathBuf, SupervisorError> {
    let mut components = Path::new(plugin_id).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => Ok(install_dir.join(name)),
        _ => Err(SupervisorError(format!(
            "plugin id {plugin_id:?} does not name a directory under {}",
            install_dir.display()
        ))),
    }
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

fn split_op(atom: &str) -> (&str, &str) {
    for op in [">=", "<=", "==", ">", "<", "="] {
        if let Some(rest) = atom.strip_prefix(op) {
            return (op, rest.trim());
        }
    }
    ("==", atom)
}

fn semver_tuple(v: &str) -> Result<(u64, u64, u64), SupervisorError> {
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

/// A no-op `systemctl` runner that records the calls it received. Lives in the
/// crate (not `#[cfg(test)]`) so integration tests can use it too; it never
/// touches systemd.
#[derive(Default)]
pub struct RecordingSystemctl {
    pub calls: Mutex<Vec<Vec<String>>>,
}

impl SystemctlRunner for RecordingSystemctl {
    fn run(&self, args: &[&str]) -> Result<(), SupervisorError> {
        self.calls
            .lock()
            .expect("systemctl call log not poisoned")
            .push(args.iter().map(|s| s.to_string()).collect());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::parse_archive_bytes;
    use std::collections::BTreeMap;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn paths_in(dir: &Path) -> Paths {
        Paths {
            install_dir: dir.join("plugins"),
            unit_dir: dir.join("units"),
            state_path: dir.join("state/plugin-state.json"),
            log_dir: dir.join("logs"),
            control_dir: dir.join("plugin-host"),
            loopback_guard_state: dir.join("plugin-loopback-guard.json"),
        }
    }

    fn build_unsigned_archive(manifest_yaml: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            w.start_file("manifest.yaml", opts).unwrap();
            w.write_all(manifest_yaml.as_bytes()).unwrap();
            w.start_file("agent/py/x.py", opts).unwrap();
            w.write_all(b"print('hi')").unwrap();
            w.finish().unwrap();
        }
        buf
    }

    const SUBPROC_MANIFEST: &str = "id: com.example.thermal\nversion: 1.0.0\nrisk: high\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - hardware.spi\n";

    #[test]
    fn require_signed_default_is_on_unless_explicitly_falsey() {
        // Serialized env mutation: this is the only test that touches
        // ADOS_PLUGIN_REQUIRE_SIGNED, so it restores the prior value and runs
        // alone within this module's concern.
        let prev = std::env::var("ADOS_PLUGIN_REQUIRE_SIGNED").ok();

        std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED");
        assert!(require_signed_default(), "absent env keeps signing ON");

        for falsey in ["0", "false", "FALSE", "no", "off", " off "] {
            std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", falsey);
            assert!(!require_signed_default(), "{falsey:?} must disable signing");
        }
        for truthy in ["1", "true", "yes", "on", "anything", ""] {
            std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", truthy);
            assert!(require_signed_default(), "{truthy:?} must keep signing ON");
        }

        match prev {
            Some(v) => std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", v),
            None => std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED"),
        }
    }

    #[test]
    fn production_supervisor_requires_signing_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ADOS_PLUGIN_REQUIRE_SIGNED").ok();
        std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED");
        let sup = PluginSupervisor::production(paths_in(dir.path()), None, "0.48.11");
        assert!(sup.require_signed());
        match prev {
            Some(v) => std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", v),
            None => std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED"),
        }
    }

    #[test]
    fn production_supervisor_rejects_unsigned_archive() {
        // The end-to-end F3 guard at the supervisor layer: a production-built
        // controller refuses an unsigned archive on install_contents.
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ADOS_PLUGIN_REQUIRE_SIGNED").ok();
        std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED");
        let mut sup = PluginSupervisor::production(paths_in(dir.path()), None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()));
        let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
        let err = sup
            .install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap_err();
        assert!(
            matches!(err, LifecycleError::Signature(_)),
            "an unsigned archive must fail the signature gate: {err}"
        );
        assert!(format!("{err}").contains("unsigned"), "{err}");
        match prev {
            Some(v) => std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", v),
            None => std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED"),
        }
    }

    #[test]
    fn the_install_target_is_always_a_direct_child_of_the_install_dir() {
        let install_dir = Path::new("/var/ados/plugins");
        for id in [
            "../../etc",
            "/etc",
            "com.example/../../etc",
            "a/b",
            "..",
            ".",
            "",
        ] {
            assert!(
                plugin_install_target(install_dir, id).is_err(),
                "id {id:?} must not resolve to an install target"
            );
        }
        let target = plugin_install_target(install_dir, "com.example.thermal").unwrap();
        assert_eq!(target, install_dir.join("com.example.thermal"));
        assert_eq!(target.parent(), Some(install_dir));
    }

    #[test]
    fn semver_in_range_boundaries() {
        // 0.48.11 is >= 0.9.0 (minor 48 > 9) and < 1.0.0 -> in range.
        assert!(semver_in_range("0.48.11", ">=0.9.0,<1.0.0").unwrap());
        // 0.8.0 falls below the lower bound.
        assert!(!semver_in_range("0.8.0", ">=0.9.0,<1.0.0").unwrap());
        assert!(semver_in_range("0.48.11", ">=0.1.0").unwrap());
        // The upper bound is exclusive.
        assert!(!semver_in_range("1.0.0", ">=0.1.0,<1.0.0").unwrap());
        // The lower bound is inclusive.
        assert!(semver_in_range("0.9.0", ">=0.9.0").unwrap());
        assert!(!semver_in_range("0.9.0", ">0.9.0").unwrap());
        assert!(semver_in_range("0.9.0", "<=0.9.0").unwrap());
        assert!(semver_in_range("0.9.0", "==0.9.0").unwrap());
        // A bare semver means equality.
        assert!(semver_in_range("0.9.0", "0.9.0").unwrap());
        assert!(!semver_in_range("0.9.1", "0.9.0").unwrap());
        // Pre-release metadata is stripped before comparison.
        assert!(semver_in_range("0.48.11-rc1", ">=0.1.0").unwrap());
        // Unparseable -> error.
        assert!(semver_in_range("x.y.z", ">=0.1.0").is_err());
    }

    #[test]
    fn install_unsigned_subprocess_writes_unit_and_state() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingSystemctl::default());
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(rec.clone());

        let archive = build_unsigned_archive(SUBPROC_MANIFEST);
        let contents = parse_archive_bytes(archive).unwrap();
        let res = sup
            .install_contents(contents, Path::new("/tmp/thermal.adosplug"))
            .unwrap();
        assert_eq!(res.plugin_id, "com.example.thermal");
        assert_eq!(res.risk, "high");
        assert_eq!(res.permissions_requested, vec!["hardware.spi".to_string()]);

        // Unit file written, slice written, daemon-reload(s) issued.
        let unit = dir
            .path()
            .join("units/ados-plugin-com-example-thermal.service");
        assert!(unit.exists());
        assert!(dir.path().join("units/ados-plugins.slice").exists());
        let calls = rec.calls.lock().unwrap();
        assert!(calls
            .iter()
            .any(|c| c == &vec!["daemon-reload".to_string()]));

        // State persisted with status installed.
        let reloaded = load_state(Some(&dir.path().join("state/plugin-state.json")));
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].status, PluginStatus::Installed);
    }

    #[test]
    fn enable_then_disable_subprocess_drives_systemctl() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingSystemctl::default());
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(rec.clone());
        let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
        sup.install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap();

        sup.enable("com.example.thermal").unwrap();
        assert_eq!(
            sup.find_install("com.example.thermal").unwrap().status,
            PluginStatus::Running
        );
        // Idempotent enable: a running plugin is left alone.
        sup.enable("com.example.thermal").unwrap();

        sup.disable("com.example.thermal").unwrap();
        assert_eq!(
            sup.find_install("com.example.thermal").unwrap().status,
            PluginStatus::Disabled
        );
        // Idempotent disable.
        sup.disable("com.example.thermal").unwrap();

        let calls = rec.calls.lock().unwrap();
        let flat: Vec<String> = calls.iter().map(|c| c.join(" ")).collect();
        assert!(flat.iter().any(|c| c.starts_with("enable ados-plugin-")));
        assert!(flat.iter().any(|c| c.starts_with("start ados-plugin-")));
        assert!(flat.iter().any(|c| c.starts_with("stop ados-plugin-")));
        assert!(flat.iter().any(|c| c.starts_with("disable ados-plugin-")));
    }

    #[test]
    fn remove_deletes_unit_dir_and_state() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingSystemctl::default());
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(rec.clone());
        let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
        sup.install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap();
        sup.enable("com.example.thermal").unwrap();

        sup.remove("com.example.thermal", false).unwrap();
        assert!(sup.find_install("com.example.thermal").is_none());
        assert!(!dir
            .path()
            .join("units/ados-plugin-com-example-thermal.service")
            .exists());
        assert!(!dir.path().join("plugins/com.example.thermal").exists());
        assert!(load_state(Some(&dir.path().join("state/plugin-state.json"))).is_empty());
    }

    #[test]
    fn grant_rejects_undeclared_permission() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()));
        let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
        sup.install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap();

        sup.grant_permission("com.example.thermal", "hardware.spi")
            .unwrap();
        assert!(state::is_permission_granted(
            sup.find_install("com.example.thermal").unwrap(),
            "hardware.spi"
        ));
        let err = sup
            .grant_permission("com.example.thermal", "mission.write")
            .unwrap_err();
        assert!(matches!(err, LifecycleError::Supervisor(_)));
    }

    #[test]
    fn grant_refuses_a_capability_the_host_cannot_back() {
        // A plugin that declares mission.read can have it recorded as a requested
        // permission, but the active Rust host does not implement mission.read,
        // so the supervisor must refuse the grant rather than let the operator
        // hand out a capability that can only error at call time.
        let dir = tempfile::tempdir().unwrap();
        let ungrantable: BTreeSet<String> = ["mission.read".to_string()].into_iter().collect();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()))
            .with_ungrantable_caps(ungrantable);
        let manifest = "id: com.example.mission\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - mission.read\n    - hardware.spi\n";
        let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        sup.install_contents(contents, Path::new("/tmp/mission.adosplug"))
            .unwrap();

        // The dead capability is refused with a clear message.
        let err = sup
            .grant_permission("com.example.mission", "mission.read")
            .unwrap_err();
        assert!(
            format!("{err}").contains("not supported by this agent runtime"),
            "{err}"
        );
        assert!(!state::is_permission_granted(
            sup.find_install("com.example.mission").unwrap(),
            "mission.read"
        ));

        // A still-backed capability the same plugin declares is granted normally.
        sup.grant_permission("com.example.mission", "hardware.spi")
            .unwrap();
        assert!(state::is_permission_granted(
            sup.find_install("com.example.mission").unwrap(),
            "hardware.spi"
        ));
    }

    #[test]
    fn network_access_follows_the_loopback_guard_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = dir.path().join("plugin-loopback-guard.json");
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()));
        let manifest = "id: com.example.net\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - network.outbound\n";
        let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        sup.install_contents(contents, Path::new("/tmp/net.adosplug"))
            .unwrap();
        let unit = || {
            std::fs::read_to_string(unit_path_for(
                "com.example.net",
                Some(&dir.path().join("units")),
            ))
            .unwrap()
        };

        // No guard loaded yet: the grant is refused and nothing is recorded.
        let err = sup
            .grant_permission("com.example.net", "network.outbound")
            .unwrap_err();
        assert!(
            format!("{err}").contains("loopback guard is unavailable"),
            "{err}"
        );
        assert!(!state::is_permission_granted(
            sup.find_install("com.example.net").unwrap(),
            "network.outbound"
        ));

        // Guard active: the grant opens the inet families.
        std::fs::write(&sidecar, r#"{"active":true,"reason":""}"#).unwrap();
        sup.grant_permission("com.example.net", "network.outbound")
            .unwrap();
        assert!(unit().contains("RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK"));
        assert!(!unit().contains("IPAddressDeny="));

        // A later boot where the guard cannot load re-renders the held grant
        // back to the no-grant socket policy.
        std::fs::write(&sidecar, r#"{"active":false,"reason":"nft missing"}"#).unwrap();
        sup.refresh_all_units();
        assert!(unit().contains("RestrictAddressFamilies=AF_UNIX\n"));
        assert!(unit().contains("IPAddressDeny=any"));
    }

    #[test]
    fn grant_allows_all_caps_when_none_are_marked_ungrantable() {
        // With no ungrantable set declared (the default), a cap the host happens
        // not to back is still grantable — the refusal is opt-in via
        // with_ungrantable_caps, so a fully-wired host or a test imposes nothing.
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()));
        let manifest = "id: com.example.mission\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - mission.read\n";
        let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        sup.install_contents(contents, Path::new("/tmp/mission.adosplug"))
            .unwrap();
        sup.grant_permission("com.example.mission", "mission.read")
            .unwrap();
        assert!(state::is_permission_granted(
            sup.find_install("com.example.mission").unwrap(),
            "mission.read"
        ));
    }

    #[test]
    fn realhost_ungrantable_caps_flow_through_the_grant_gate() {
        // End-to-end: feed the controller the exact set RealHost advertises and
        // confirm a RealHost-dead cap is refused while a RealHost-backed cap the
        // plugin declares is granted. This locks the wiring the daemons use.
        use crate::realhost::RealHost;
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()))
            .with_ungrantable_caps(RealHost::ungrantable_caps());
        // recording.write is dead on RealHost; sensor.camera.register is backed.
        let manifest = "id: com.example.rec\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - recording.write\n    - sensor.camera.register\n";
        let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        sup.install_contents(contents, Path::new("/tmp/rec.adosplug"))
            .unwrap();
        assert!(sup
            .grant_permission("com.example.rec", "recording.write")
            .is_err());
        sup.grant_permission("com.example.rec", "sensor.camera.register")
            .unwrap();
        assert!(state::is_permission_granted(
            sup.find_install("com.example.rec").unwrap(),
            "sensor.camera.register"
        ));
    }

    #[test]
    fn incompatible_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()));
        let manifest = "id: com.example.old\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.9.0,<0.10.0\"\nagent:\n  entrypoint: agent/py/x.py\n";
        let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        let err = sup
            .install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap_err();
        assert!(matches!(err, LifecycleError::Supervisor(_)), "{err}");
    }

    #[test]
    fn unsupported_board_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(
            paths_in(dir.path()),
            false,
            Some("rpi4b".to_string()),
            "0.48.11",
        )
        .with_systemctl(Arc::new(RecordingSystemctl::default()));
        let manifest = "id: com.example.board\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\n  supported_boards: [rock-5c-lite]\nagent:\n  entrypoint: agent/py/x.py\n";
        let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        let err = sup
            .install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap_err();
        assert!(format!("{err}").contains("does not support board"), "{err}");
    }

    #[test]
    fn inprocess_from_non_first_party_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()));
        let manifest = "id: com.evil.inproc\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: pkg:Class\n  isolation: inprocess\n";
        // Build a "signed" contents struct with a non-first-party signer; we
        // bypass require_signed and just exercise the isolation gate.
        let mut contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        contents.signer_id = Some("third-party".to_string());
        contents.signature_b64 = Some("QUJD".to_string());
        let err = sup
            .install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap_err();
        assert!(format!("{err}").contains("not first-party"), "{err}");
    }

    #[test]
    fn inprocess_from_first_party_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()));
        let manifest = "id: com.altnautica.inproc\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: pkg:Class\n  isolation: inprocess\n";
        let mut contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        contents.signer_id = Some("altnautica-2026-A".to_string());
        contents.signature_b64 = Some("QUJD".to_string());
        // inprocess -> no unit written, no systemctl daemon-reload needed.
        let res = sup
            .install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap();
        assert_eq!(res.signer_id.as_deref(), Some("altnautica-2026-A"));
        assert!(!dir
            .path()
            .join("units/ados-plugin-com-altnautica-inproc.service")
            .exists());
    }

    #[test]
    fn inline_gcs_from_third_party_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()));
        let manifest = "id: com.evil.panel\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/dist/index.js\n  isolation: inline\n";
        let mut contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        contents.signer_id = Some("third-party".to_string());
        contents.signature_b64 = Some("QUJD".to_string());
        let err = sup
            .install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap_err();
        assert!(format!("{err}").contains("inline GCS isolation"), "{err}");
    }

    #[test]
    fn manifest_hash_tamper_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()));
        let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
        sup.install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap();

        // Tamper with the on-disk manifest after install.
        let manifest_path = dir.path().join("plugins/com.example.thermal/manifest.yaml");
        let mut tampered = std::fs::read_to_string(&manifest_path).unwrap();
        tampered.push_str("\n# injected\n");
        std::fs::write(&manifest_path, tampered).unwrap();

        // Any lifecycle op that resolves the manifest now fails the hash check.
        let err = sup.enable("com.example.thermal").unwrap_err();
        assert!(format!("{err}").contains("manifest hash mismatch"), "{err}");
    }

    #[test]
    fn discover_filters_tampered_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state/plugin-state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        // Unpack a manifest declaring only hardware.spi.
        let install_dir = dir.path().join("plugins/com.example.thermal");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::write(install_dir.join("manifest.yaml"), SUBPROC_MANIFEST).unwrap();
        let manifest_hash = hex::encode(Sha256::digest(SUBPROC_MANIFEST.as_bytes()));

        // Write a state file that grants an extra permission the manifest does
        // not declare.
        let mut inst = PluginInstall {
            plugin_id: "com.example.thermal".into(),
            version: "1.0.0".into(),
            source: PluginSource::LocalFile,
            source_uri: None,
            signer_id: None,
            manifest_hash,
            status: PluginStatus::Installed,
            installed_at: now_ms(),
            enabled_at: None,
            failure_reason: None,
            permissions: BTreeMap::new(),
            auto_update: true,
            pinned_version: None,
            last_update_check_at: None,
            last_update_attempt: None,
            model_status: None,
            service_status: None,
        };
        grant_permission(&mut inst, "hardware.spi");
        grant_permission(&mut inst, "mission.write"); // not declared
        save_state(&[inst], Some(&state_path)).unwrap();

        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_systemctl(Arc::new(RecordingSystemctl::default()));
        sup.discover().unwrap();
        let install = sup.find_install("com.example.thermal").unwrap();
        assert!(install.permissions.contains_key("hardware.spi"));
        assert!(!install.permissions.contains_key("mission.write"));
    }
}
