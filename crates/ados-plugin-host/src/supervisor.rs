//! Plugin lifecycle controller: install / enable / disable / remove.
//!
//! This is the lifecycle controller, NOT the OS process supervisor. It reads
//! on-disk install state, reconciles it against unpacked archives at
//! `<install dir>/<id>/`, installs a `.adosplug` archive (verify signature,
//! run the compatibility and first-party gates, fetch pinned payloads, unpack,
//! write the service unit, persist state), and drives the
//! enable/disable/remove state machine. It does not run plugin code itself;
//! subprocess plugins are started by the host service manager from the
//! generated unit.
//!
//! Compatibility gates at install time:
//! * `compatibility.ados_version` must include the running agent version.
//! * `compatibility.supported_boards` (if non-empty) must include the current
//!   HAL board id.
//! * `agent.target_profiles` must include this node's profile.
//! * every `bin:<name>` the agent half runs must have this host's
//!   `<arch>-<os>` binary.
//! * `isolation: inprocess` (agent) is refused: nothing runs an in-process half.
//! * `isolation: inline` (GCS), `resources.class: heavy`, and any agent half on
//!   a service backend that enforces no sandbox require a first-party signer.
//!
//! The service-manager calls and filesystem ops are real, but the crate stays
//! lib-only: no test invokes a service manager (the [`ServiceBackend`] is
//! injectable and the tests use [`RecordingBackend`]).
//!
//! ## Stable consumer contract
//!
//! This controller is consumed in-process as a library by the agent's other
//! long-running services (the cloud relay, which drives plugin
//! install/enable/disable/remove from remote commands and a periodic
//! auto-update loop, and the control plane's lifecycle routes). Those callers
//! depend on the surface below; treat it as stable and change it deliberately:
//!
//! * [`install_archive`](PluginSupervisor::install_archive) `(&Path) -> Result<InstallResult, LifecycleError>`
//! * [`enable`](PluginSupervisor::enable), [`disable`](PluginSupervisor::disable),
//!   [`remove`](PluginSupervisor::remove) `(&str, keep_data: bool)`
//! * [`grant_permission`](PluginSupervisor::grant_permission),
//!   [`revoke_permission`](PluginSupervisor::revoke_permission)
//! * [`installs`](PluginSupervisor::installs),
//!   [`find_install`](PluginSupervisor::find_install),
//!   [`attestation`](PluginSupervisor::attestation),
//!   [`service_readiness`](PluginSupervisor::service_readiness)
//!
//! [`InstallResult`] (plugin_id, version, signer_id, risk, permissions_requested)
//! and [`PluginInstall`] (the on-disk install record) are part of the same
//! contract. There is no `configure` method: a batched permission change is the
//! caller's own grant/revoke sequence.
//!
//! Refusals carry stable message prefixes a route can map to an error code:
//! `incompatible: target_profiles excludes <profile>`,
//! `incompatible: no binary for <arch-os>`,
//! `refused: sandbox_unavailable_first_party_only`,
//! `refused: heavy_resources_first_party_only`.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::archive::{
    open_archive, parse_archive_bytes, unpack_to, ArchiveContents, MANIFEST_FILENAME,
};
use crate::attestation::Attestation;
use crate::backend::{default_backend, ServiceBackend};
use crate::download::{fetch_to_file, DownloadSource, HttpDownloadSource};
use crate::errors::{
    LifecycleError, ManifestError, SignatureError, SignatureErrorKind, SupervisorError,
};
use crate::manifest::{
    bin_reference, host_arch_os, AgentIsolation, DeclaredCapability, GcsIsolation, PluginManifest,
    ResourceClass, PAYLOAD_MAX_BYTES,
};
use crate::services::{
    build_service_spec, declared_services, probe_spec, service_argv, service_unit_name_for,
    ReadyCheck, ServiceSpec,
};
use crate::signing::{
    is_first_party_signer, load_revocation_list, load_trusted_keys, verify_archive_signature,
};
use crate::state::{
    self, filter_permissions_against_manifest, find_install, grant_permission, load_state, now_ms,
    remove_install, revoke_permission, save_state, upsert_install, PluginInstall, PluginSource,
    PluginStatus, StateLock,
};
use crate::systemd::{
    build_unit_spec, plugin_http_dir, unit_name_for, PLUGIN_LOG_DIR, PLUGIN_RUNNER_BINARY,
    PLUGIN_UNIT_DIR,
};

/// Default install directory for unpacked third-party archives.
pub const PLUGINS_INSTALL_DIR: &str = "/var/ados/plugins";

/// Default agent run directory (`ADOS_RUN_DIR` overrides it).
pub const DEFAULT_RUN_DIR: &str = "/run/ados";

/// Summary returned from a successful install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub plugin_id: String,
    pub version: String,
    pub signer_id: Option<String>,
    pub risk: String,
    pub permissions_requested: Vec<String>,
}

/// Filesystem + unit-dir layout the controller writes to. Tests point these at
/// a tempdir so the install path runs end-to-end without touching the host.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Unpacked plugin trees (`ADOS_PLUGIN_INSTALL_DIR`).
    pub install_dir: PathBuf,
    /// Where the service backend writes unit files or property lists
    /// (`ADOS_PLUGIN_UNIT_DIR`).
    pub unit_dir: PathBuf,
    /// The install-state file (`ADOS_PLUGIN_STATE`).
    pub state_path: PathBuf,
    /// Plugin append logs (`ADOS_PLUGIN_LOG_DIR`).
    pub log_dir: PathBuf,
    /// The plugin host's control dir, where its root-only control socket lives
    /// (`ADOS_PLUGIN_HOST_DIR`). The controller reaches the live daemon through
    /// it after a grant or revoke, so a permission change is effective
    /// immediately rather than at the next daemon restart.
    pub control_dir: PathBuf,
    /// The loopback-guard verdict sidecar the plugin-host daemon writes at
    /// startup. A `network.outbound` grant needs it to read active.
    pub loopback_guard_state: PathBuf,
    /// The per-plugin socket and token-file dir (`ADOS_PLUGIN_SOCKET_DIR`).
    pub socket_dir: PathBuf,
    /// The persisted capability-token issuer secret
    /// (`ADOS_PLUGIN_TOKEN_SECRET`).
    pub token_secret: PathBuf,
    /// The shared Python plugin runner (`ADOS_PLUGIN_RUNNER`).
    pub runner: PathBuf,
    /// The agent run dir (`ADOS_RUN_DIR`), holding each HTTP plugin's socket
    /// dir under `plugin-http/`.
    pub run_dir: PathBuf,
}

impl Paths {
    /// The layout this process runs with: each path from its environment
    /// variable, else the FHS default an SBC install uses. A per-user install
    /// (the macOS workstation) sets every variable to a path under its home.
    pub fn from_env() -> Self {
        fn env_path(key: &str, default: &str) -> PathBuf {
            std::env::var_os(key)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(default))
        }
        Paths {
            install_dir: env_path("ADOS_PLUGIN_INSTALL_DIR", PLUGINS_INSTALL_DIR),
            unit_dir: env_path("ADOS_PLUGIN_UNIT_DIR", PLUGIN_UNIT_DIR),
            state_path: env_path("ADOS_PLUGIN_STATE", state::PLUGIN_STATE_PATH),
            log_dir: env_path("ADOS_PLUGIN_LOG_DIR", PLUGIN_LOG_DIR),
            control_dir: env_path("ADOS_PLUGIN_HOST_DIR", crate::control::DEFAULT_CONTROL_DIR),
            loopback_guard_state: ados_protocol::plugin_loopback_guard::sidecar_path(),
            socket_dir: env_path("ADOS_PLUGIN_SOCKET_DIR", crate::server::DEFAULT_SOCKET_DIR),
            token_secret: env_path(
                "ADOS_PLUGIN_TOKEN_SECRET",
                crate::token_secret::PLUGIN_TOKEN_SECRET_PATH,
            ),
            runner: env_path("ADOS_PLUGIN_RUNNER", PLUGIN_RUNNER_BINARY),
            run_dir: env_path("ADOS_RUN_DIR", DEFAULT_RUN_DIR),
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
    /// This node's profile (`drone | ground-station | workstation | compute`):
    /// the `agent.target_profiles` gate, and which declared services and
    /// payloads apply here.
    current_profile: String,
    agent_version: String,
    backend: Arc<dyn ServiceBackend>,
    /// Payload transport; the live HTTPS client is built on first use.
    download: Option<Arc<dyn DownloadSource>>,
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
}

impl PluginSupervisor {
    /// Build a controller. `agent_version` is the running agent semver; the
    /// constraint check compares it against each plugin's
    /// `compatibility.ados_version`. The service backend is this host's
    /// ([`default_backend`]) and the node profile is `drone` until
    /// [`with_profile`](Self::with_profile) says otherwise.
    pub fn new(
        paths: Paths,
        require_signed: bool,
        current_board_id: Option<String>,
        agent_version: impl Into<String>,
    ) -> Self {
        let backend = default_backend(&paths.unit_dir);
        PluginSupervisor {
            paths,
            require_signed,
            current_board_id,
            current_board_tier: None,
            current_profile: "drone".to_string(),
            agent_version: agent_version.into(),
            backend,
            download: None,
            installs: Vec::new(),
            ungrantable_caps: BTreeSet::new(),
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

    /// The board id and compute tier the `compatibility.supported_boards` and
    /// `min_tier` gates key on, read from the HAL board sidecar at
    /// `board_sidecar`. Every daemon that builds a [`production`](Self::production)
    /// controller passes these, so a plugin refused on one install path is
    /// refused on all of them. `(None, None)` when the sidecar is absent or
    /// unreadable, and each half is `None` when it is `unknown` or out of range:
    /// a node that has not probed yet stays lenient rather than refusing every
    /// install.
    pub fn board_identity(board_sidecar: &Path) -> (Option<String>, Option<u8>) {
        let Some(board) = ados_hal_probe::board_sidecar::read_sidecar(board_sidecar) else {
            return (None, None);
        };
        let id = (!board.name.is_empty() && board.name != "unknown").then_some(board.name);
        let tier = (1..=4).contains(&board.tier).then_some(board.tier as u8);
        (id, tier)
    }

    /// Inject the service backend (tests use a [`RecordingBackend`]).
    pub fn with_backend(mut self, backend: Arc<dyn ServiceBackend>) -> Self {
        self.backend = backend;
        self
    }

    /// Set this node's profile (see `ados_config::node_profile`).
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.current_profile = profile.into();
        self
    }

    /// Inject the payload download transport (tests serve fixed bodies).
    pub fn with_download_source(mut self, source: Arc<dyn DownloadSource>) -> Self {
        self.download = Some(source);
        self
    }

    /// True when this controller enforces a valid first-party signature before
    /// installing an archive. Exposed so a caller / test can assert the live
    /// wiring path is signed.
    pub fn require_signed(&self) -> bool {
        self.require_signed
    }

    /// This node's profile.
    pub fn profile(&self) -> &str {
        &self.current_profile
    }

    /// The layout this controller writes to.
    pub fn paths(&self) -> &Paths {
        &self.paths
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

    /// Read on-disk state and filter each install's granted permissions down to
    /// what its manifest currently declares (defends against a tampered state
    /// file).
    pub fn discover(&mut self) -> Result<(), LifecycleError> {
        self.installs = load_state(Some(&self.paths.state_path));
        self.filter_installs_against_manifests();
        tracing::info!(
            installed_count = self.installs.len(),
            "plugin_supervisor_discovered"
        );
        Ok(())
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
    fn reload_installs(&mut self) -> Result<(), LifecycleError> {
        self.installs = state::load_state_checked(Some(&self.paths.state_path)).map_err(|e| {
            SupervisorError(format!(
                "plugin state is unreadable ({e}); refusing to overwrite it"
            ))
        })?;
        self.filter_installs_against_manifests();
        Ok(())
    }

    /// Filter every install's grants down to what its manifest declares.
    fn filter_installs_against_manifests(&mut self) {
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
        let dir = plugin_install_target(&self.paths.install_dir, plugin_id)?;
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
    fn update_installed(
        &mut self,
        plugin_id: &str,
        f: impl FnOnce(&mut PluginInstall),
    ) -> Result<(), LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
        f(self.require_install_mut(plugin_id)?);
        save_state(&self.installs, Some(&self.paths.state_path))
    }

    fn update_record(
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
        self.install_verified(contents, Path::new(&source), false)
    }

    /// Install from parsed contents, choosing whether a version downgrade is
    /// permitted.
    ///
    /// Order: signature, compatibility, the first-party gates and the service
    /// block, then the payload downloads (outside the state lock, so a large
    /// fetch never blocks another lifecycle call), then under the lock the
    /// downgrade and shared-topic checks, the staged unpack with payloads and
    /// the attestation placed into it, the swap, the unit and the record.
    pub fn install_contents_with(
        &mut self,
        contents: ArchiveContents,
        source_path: &Path,
        allow_downgrade: bool,
    ) -> Result<InstallResult, LifecycleError> {
        if self.require_signed {
            let manifest = &contents.manifest;
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
        self.install_verified(contents, source_path, allow_downgrade)
    }

    /// The install past the signature check: compatibility, the first-party
    /// gates and the service block, the payloads, then the locked unpack, unit
    /// and record.
    fn install_verified(
        &mut self,
        contents: ArchiveContents,
        source_path: &Path,
        allow_downgrade: bool,
    ) -> Result<InstallResult, LifecycleError> {
        let manifest = contents.manifest.clone();
        self.check_compatibility(&manifest)?;
        let signer = contents.signer_id.as_deref();
        self.reject_inline_for_third_party(&manifest, signer)?;
        self.reject_heavy_for_third_party(&manifest, signer)?;
        self.reject_unsandboxed_third_party(&manifest, signer)?;
        // A malformed service block (a foreign slice, an off-box ready_check,
        // an unrenderable command, an undeclared listener) is refused here, as
        // the Python manifest model refuses it at parse, rather than surfacing
        // at enable.
        for service in declared_services(&manifest)? {
            service_argv(&service.command)?;
        }

        let payload_dir = self.paths.install_dir.join(format!(
            ".{}.payloads-{}-{}",
            manifest.id,
            std::process::id(),
            now_ms()
        ));
        let result = self
            .fetch_payloads(&manifest, &payload_dir)
            .and_then(|fetched| {
                self.install_locked(
                    &contents,
                    &manifest,
                    source_path,
                    allow_downgrade,
                    &payload_dir,
                    &fetched,
                )
            });
        let _ = std::fs::remove_dir_all(&payload_dir);
        let result = result?;

        tracing::info!(
            plugin_id = %manifest.id,
            version = %manifest.version,
            signer_id = ?contents.signer_id,
            "plugin_installed"
        );
        Ok(result)
    }

    /// Download every payload that applies to this node (its profile and
    /// `<arch>-<os>`) into `dir`, each streamed, capped at
    /// [`PAYLOAD_MAX_BYTES`], and checked against the pinned size and sha256.
    /// Returns `(path, sha256)` per fetched payload.
    fn fetch_payloads(
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

    fn install_locked(
        &mut self,
        contents: &ArchiveContents,
        manifest: &PluginManifest,
        source_path: &Path,
        allow_downgrade: bool,
        payload_dir: &Path,
        fetched: &[(String, String)],
    ) -> Result<InstallResult, LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
        if !allow_downgrade {
            self.reject_downgrade(manifest)?;
        }
        self.reject_shared_topic_collision(manifest)?;

        // Unpack and check into a staging dir beside the target. Every check
        // runs against the staging copy, so a bad archive leaves an existing
        // install exactly as it was; only a validated tree replaces it.
        let target = plugin_install_target(&self.paths.install_dir, &manifest.id)?;
        let staging = self
            .paths
            .install_dir
            .join(format!(".{}.staging", manifest.id));
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        let staged = unpack_to(&contents.raw_archive_bytes, &staging)
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
        swap_into_place(&staging, &target)?;

        // Write the unit for subprocess agent halves. A fresh install has
        // granted nothing yet, so the unit renders with the most restrictive
        // sandbox; grant/revoke re-renders it.
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
            // The operator's update preferences outlive a reinstall.
            auto_update: previous.as_ref().is_none_or(|p| p.auto_update),
            pinned_version: previous.as_ref().and_then(|p| p.pinned_version.clone()),
            last_update_check_at: None,
            last_update_attempt: None,
            model_status: None,
            service_status: None,
        };
        self.installs = upsert_install(std::mem::take(&mut self.installs), install);
        save_state(&self.installs, Some(&self.paths.state_path))?;

        Ok(InstallResult {
            plugin_id: manifest.id.clone(),
            version: manifest.version.clone(),
            signer_id: contents.signer_id.clone(),
            risk: manifest.risk.clone(),
            permissions_requested: manifest.declared_permissions().into_iter().collect(),
        })
    }

    /// Grant a declared permission. Rejects a permission the manifest does not
    /// declare, and a permission whose capability the active host runtime cannot
    /// back (see [`with_ungrantable_caps`](Self::with_ungrantable_caps)) so an
    /// operator never grants a capability that can only error at call time.
    pub fn grant_permission(
        &mut self,
        plugin_id: &str,
        permission_id: &str,
    ) -> Result<(), LifecycleError> {
        let lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
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
        // the plugin off the agent's own listeners. A backend that enforces no
        // sandbox runs first-party plugins only, with no socket policy for the
        // guard to complement.
        if permission_id == crate::sandbox::NETWORK_OUTBOUND_CAP && self.backend.enforces_sandbox()
        {
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
        // Release the state lock before re-entering the state path.
        drop(lock);
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
    pub fn revoke_permission(
        &mut self,
        plugin_id: &str,
        permission_id: &str,
    ) -> Result<(), LifecycleError> {
        let lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
        let manifest = self.manifest_for(plugin_id)?;
        let install = self.require_install_mut(plugin_id)?;
        revoke_permission(install, permission_id);
        save_state(&self.installs, Some(&self.paths.state_path))?;
        // Release the state lock before re-entering the state path.
        drop(lock);
        self.apply_permission_change(plugin_id, &manifest)?;
        Ok(())
    }

    /// Re-render every installed plugin's unit against the current grant set
    /// and loopback-guard verdict, restarting a running plugin whose sandbox
    /// changed. The plugin-host daemon calls it at startup, after loading the
    /// guard, so a unit rendered while the guard was active cannot keep network
    /// access into a boot where it is not. It also re-creates each HTTP
    /// plugin's socket dir, which lives on the run-dir tmpfs.
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

    /// Rewrite one plugin's units when their rendered sandbox differs from
    /// what is installed, then restart the running ones (the service manager
    /// applies the sandbox at exec).
    fn refresh_unit(
        &self,
        plugin_id: &str,
        manifest: &PluginManifest,
    ) -> Result<(), LifecycleError> {
        if !manifest.is_subprocess_agent() {
            return Ok(());
        }
        self.ensure_http_dir(manifest)?;
        let granted = self
            .find_install(plugin_id)
            .map(state::granted_caps)
            .unwrap_or_default();
        let guard =
            ados_protocol::plugin_loopback_guard::read_state_at(&self.paths.loopback_guard_state);
        let running = self
            .find_install(plugin_id)
            .is_some_and(|i| matches!(i.status, PluginStatus::Running));
        if let Some(spec) = build_unit_spec(
            manifest,
            &self.paths,
            &self.current_profile,
            &granted,
            guard.active,
        )? {
            let name = unit_name_for(plugin_id);
            if self.backend.install(&name, &spec)? && running {
                self.backend.restart(&name)?;
            }
        }
        // Declared services run under the same capability sandbox as the main
        // unit, so they follow every grant and revoke too. A plugin that is not
        // running has its services rendered afresh when it next starts.
        if !running {
            return Ok(());
        }
        for service in self.active_services_of(plugin_id, manifest) {
            let spec = match build_service_spec(
                manifest,
                &service,
                &self.paths,
                &self.current_profile,
                &granted,
                guard.active,
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(plugin_id, service = %service.name, error = %e, "plugin_service_render_failed");
                    continue;
                }
            };
            let name = service_unit_name_for(plugin_id, &service.name);
            if self.backend.install(&name, &spec)? {
                self.backend.restart(&name)?;
            }
        }
        Ok(())
    }

    /// Make a permission change real, right now.
    ///
    /// Two mechanisms enforce a plugin's capabilities and both are snapshots
    /// taken when something was last written, so both have to be refreshed:
    ///
    /// 1. **The sandbox.** `hardware.*`, `network.*` and `filesystem.host` live
    ///    in the generated unit (see [`crate::sandbox`]), so the unit is
    ///    re-rendered and, when the grant set actually changed the text, the
    ///    plugin is restarted to pick up the new namespace. A restart is the
    ///    only way: the service manager applies `DeviceAllow`/
    ///    `RestrictAddressFamilies` at exec time.
    /// 2. **The capability token.** Every wire-gated capability rides in the
    ///    plugin's HMAC token. The live plugin host re-mints it through the
    ///    control socket, which also pushes the fresh token to the open
    ///    connection so the next request re-gates against the new set.
    ///
    /// A restart is skipped when the unit text is unchanged (a purely
    /// wire-gated capability), because bouncing a running geofence plugin to
    /// apply a token change it can receive live is a needless gap in coverage.
    fn apply_permission_change(
        &self,
        plugin_id: &str,
        manifest: &PluginManifest,
    ) -> Result<(), LifecycleError> {
        self.refresh_unit(plugin_id, manifest)?;

        // Re-mint the live token. A plugin host that is not up has nothing to
        // re-mint against and will read the new grant set off state when it
        // starts, so an unreachable control socket is logged, not an error. The
        // host's state poll also compares each live session's grant set with
        // state, so a missed poke still lands within one poll period.
        match crate::rotate_token_via_control(&self.paths.control_dir, plugin_id) {
            Ok(true) => tracing::info!(plugin_id, "plugin_token_rotated_after_permission_change"),
            Ok(false) => tracing::info!(
                plugin_id,
                "plugin token re-minted; no live session took it, the plugin reads it on connect"
            ),
            Err(e) => tracing::warn!(
                plugin_id,
                detail = %e,
                "plugin host control socket unreachable; the host's state poll applies \
                 the new grant set to the live session"
            ),
        }
        Ok(())
    }

    /// Enable a plugin. Idempotent: a running plugin is left running. A plugin
    /// with no agent unit (a GCS-only plugin) only flips state; subprocess
    /// plugins are enabled and started on the service backend, with their
    /// declared services.
    pub fn enable(&mut self, plugin_id: &str) -> Result<(), LifecycleError> {
        let _lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
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
        self.ensure_http_dir(&manifest)?;
        self.backend.enable_start(&unit_name_for(plugin_id))?;
        // Each declared service under its own unit, additional to the main
        // runner. A failure on one is surfaced as not-ready with its reason,
        // never as a failed enable: the plugin's main half still runs.
        self.start_declared_services(plugin_id, &manifest);
        let service_status = self.readiness_of(plugin_id, &manifest);
        let install = self.require_install_mut(plugin_id)?;
        install.status = PluginStatus::Running;
        install.enabled_at = Some(now_ms());
        install.service_status = service_status;
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
            let install = self.require_install_ref(plugin_id)?;
            if install.status == PluginStatus::Disabled {
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
        save_state(&self.installs, Some(&self.paths.state_path))?;
        tracing::info!(plugin_id, "plugin_disabled");
        Ok(())
    }

    /// Remove a plugin: disable it (if running/enabled), remove the units,
    /// delete the unpacked dir and (unless `keep_data`) the log, and drop the
    /// state.
    pub fn remove(&mut self, plugin_id: &str, keep_data: bool) -> Result<(), LifecycleError> {
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
        let manifest = self.manifest_for(plugin_id)?;
        if manifest.is_subprocess_agent() {
            // disable() already stopped the declared services; delete their
            // units alongside the main unit.
            self.delete_declared_service_units(plugin_id, &manifest);
            self.backend.uninstall(&unit_name_for(plugin_id))?;
        }
        let http_dir = plugin_http_dir(&self.paths.run_dir, plugin_id);
        if http_dir.exists() {
            let _ = std::fs::remove_dir_all(&http_dir);
        }
        let target = plugin_install_target(&self.paths.install_dir, plugin_id)?;
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        if !keep_data {
            let log_file = crate::systemd::log_path_for(&self.paths.log_dir, plugin_id);
            if log_file.exists() {
                std::fs::remove_file(&log_file)?;
            }
        }
        self.installs = remove_install(std::mem::take(&mut self.installs), plugin_id);
        save_state(&self.installs, Some(&self.paths.state_path))?;
        tracing::info!(plugin_id, keep_data, "plugin_removed");
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

    /// The on-disk manifest bytes of a plugin, re-checked against the recorded
    /// `manifest_hash` (tamper detection).
    fn manifest_bytes_checked(&self, plugin_id: &str) -> Result<Vec<u8>, SupervisorError> {
        let manifest_path =
            plugin_install_target(&self.paths.install_dir, plugin_id)?.join(MANIFEST_FILENAME);
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
    fn manifest_for(&self, plugin_id: &str) -> Result<PluginManifest, SupervisorError> {
        let manifest_bytes = self.manifest_bytes_checked(plugin_id)?;
        let text = std::str::from_utf8(&manifest_bytes)
            .map_err(|e| SupervisorError(format!("manifest is not valid UTF-8: {e}")))?;
        PluginManifest::from_yaml_text(text).map_err(|e: ManifestError| SupervisorError(e.0))
    }

    /// Run the version + board + tier + profile + binary + inprocess gates.
    fn check_compatibility(&self, manifest: &PluginManifest) -> Result<(), LifecycleError> {
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

    /// Reject `resources.class: heavy` from a non-first-party signer: the
    /// lifted bounds let one plugin take most of a node.
    fn reject_heavy_for_third_party(
        &self,
        manifest: &PluginManifest,
        signer_id: Option<&str>,
    ) -> Result<(), LifecycleError> {
        let heavy = manifest
            .agent
            .as_ref()
            .is_some_and(|a| a.resources.class == ResourceClass::Heavy);
        if heavy && signer_id.map(is_first_party_signer) != Some(true) {
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
    fn reject_unsandboxed_third_party(
        &self,
        manifest: &PluginManifest,
        signer_id: Option<&str>,
    ) -> Result<(), LifecycleError> {
        if manifest.is_subprocess_agent()
            && !self.backend.enforces_sandbox()
            && signer_id.map(is_first_party_signer) != Some(true)
        {
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
    fn reject_shared_topic_collision(
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

    /// Create an `agent.http` plugin's socket dir, owned by the account its
    /// units run as. The dir lives on the run-dir tmpfs, so it is re-created
    /// at every enable and at daemon start.
    fn ensure_http_dir(&self, manifest: &PluginManifest) -> Result<(), LifecycleError> {
        if !manifest.agent.as_ref().is_some_and(|a| a.http) {
            return Ok(());
        }
        let dir = plugin_http_dir(&self.paths.run_dir, &manifest.id);
        std::fs::create_dir_all(&dir)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770))?;
        }
        // A sandboxed unit runs as `ados`; the dir must be its to create the
        // socket in. Only root can hand it over, and only a sandboxing backend
        // runs units as another account.
        if self.backend.enforces_sandbox() && nix::unistd::geteuid().is_root() {
            let user = nix::unistd::User::from_name("ados").ok().flatten();
            let group = nix::unistd::Group::from_name("ados").ok().flatten();
            match (user, group) {
                (Some(u), Some(g)) => nix::unistd::chown(&dir, Some(u.uid), Some(g.gid))
                    .map_err(|e| SupervisorError(format!("chown {}: {e}", dir.display())))?,
                _ => tracing::warn!(
                    dir = %dir.display(),
                    "the ados account is absent; the plugin HTTP dir stays root-owned"
                ),
            }
        }
        Ok(())
    }

    /// Stop a plugin whose files are about to be replaced by a reinstall.
    ///
    /// Best-effort: an old manifest that no longer reads, or a unit that is
    /// already gone, must not block the install. The reinstalled record lands as
    /// `Installed`, so the operator's next enable starts the NEW version.
    fn stop_for_replacement(&self, previous: &PluginInstall) {
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
    fn services_of(&self, plugin_id: &str, manifest: &PluginManifest) -> Vec<ServiceSpec> {
        declared_services(manifest).unwrap_or_else(|e| {
            tracing::warn!(plugin_id, error = %e, "plugin_services_unreadable");
            Vec::new()
        })
    }

    /// The declared services that run on this node's profile.
    fn active_services_of(&self, plugin_id: &str, manifest: &PluginManifest) -> Vec<ServiceSpec> {
        self.services_of(plugin_id, manifest)
            .into_iter()
            .filter(|s| s.applies_to(&self.current_profile))
            .collect()
    }

    /// Render, enable and start one unit per declared service that runs on
    /// this node. Best-effort per service: a failure is logged and surfaces as
    /// not-ready on the readiness probe; it never aborts the enable of the
    /// plugin's main half.
    fn start_declared_services(&self, plugin_id: &str, manifest: &PluginManifest) {
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
    fn stop_declared_services(&self, plugin_id: &str, manifest: &PluginManifest) {
        for service in self.services_of(plugin_id, manifest) {
            let name = service_unit_name_for(plugin_id, &service.name);
            if let Err(e) = self.backend.stop_disable(&name) {
                tracing::warn!(plugin_id, service = %service.name, error = %e, "plugin_service_stop_failed");
            }
        }
    }

    /// Delete each declared service's unit. Best-effort.
    fn delete_declared_service_units(&self, plugin_id: &str, manifest: &PluginManifest) {
        for service in self.services_of(plugin_id, manifest) {
            let name = service_unit_name_for(plugin_id, &service.name);
            if let Err(e) = self.backend.uninstall(&name) {
                tracing::warn!(plugin_id, service = %service.name, error = %e, "plugin_service_delete_failed");
            }
        }
    }

    /// Probe each declared service that runs here: `[{name, ready, reason}]`,
    /// or `None` when there are none (the heartbeat then omits the block).
    ///
    /// No `ready_check`: ready iff the unit is active. An HTTP check: ready on
    /// a 2xx GET. A command: ready on exit 0, run by the service backend with
    /// the plugin's sandbox. A probe error is `ready: false` with the reason.
    fn readiness_of(
        &self,
        plugin_id: &str,
        manifest: &PluginManifest,
    ) -> Option<serde_json::Value> {
        let services = self.active_services_of(plugin_id, manifest);
        if services.is_empty() {
            return None;
        }
        let granted = self
            .find_install(plugin_id)
            .map(state::granted_caps)
            .unwrap_or_default();
        let guard =
            ados_protocol::plugin_loopback_guard::read_state_at(&self.paths.loopback_guard_state);
        let entries = services
            .iter()
            .map(|service| {
                let (ready, reason) = match &service.ready_check {
                    None => {
                        let unit = service_unit_name_for(plugin_id, &service.name);
                        match self.backend.is_active(&unit) {
                            Ok(true) => (true, None),
                            Ok(false) | Err(_) => (false, Some("unit not active".to_string())),
                        }
                    }
                    Some(ReadyCheck::Http { url, port, path }) => {
                        crate::services::probe_http(*port, path, url.starts_with("https://"))
                    }
                    Some(ReadyCheck::Command(argv)) => {
                        match probe_spec(manifest, argv, &self.paths, &granted, guard.active)
                            .and_then(|probe| self.backend.probe(&probe))
                        {
                            Ok(()) => (true, None),
                            Err(e) => (false, Some(e.0.chars().take(200).collect())),
                        }
                    }
                };
                serde_json::json!({"name": service.name, "ready": ready, "reason": reason})
            })
            .collect();
        Some(serde_json::Value::Array(entries))
    }
}

/// Move each fetched payload from `payload_dir` into the staged tree at its
/// manifest path. A payload may not overwrite a file the archive ships. A
/// payload that is a `binaries` entry is made executable.
fn place_payloads(
    manifest: &PluginManifest,
    payload_dir: &Path,
    staging: &Path,
    fetched: &[(String, String)],
) -> Result<(), LifecycleError> {
    let executables: BTreeSet<&str> = manifest
        .agent
        .as_ref()
        .map(|a| {
            a.binaries
                .values()
                .flat_map(|by_arch| by_arch.values().map(String::as_str))
                .collect()
        })
        .unwrap_or_default();
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
        let mode = if executables.contains(path.as_str()) {
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
fn builtin_contents(manifest_yaml: &str) -> Result<ArchiveContents, LifecycleError> {
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

/// The plugin's unpacked dir: a direct child of `install_dir` named by the id.
///
/// The install path swaps a freshly unpacked tree into this dir and deletes
/// the one it replaces, as root, so an id that is absolute, carries a
/// separator, or walks up with `..` would aim both at an arbitrary host
/// directory. The manifest parser already refuses such an id; this is the check
/// at the point of use, so no id that reaches here some other way can widen the
/// blast radius past one plugin dir.
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

/// Replace `target` with the validated `staging` tree by renames, so the
/// install dir only ever holds a complete old tree or a complete new one.
fn swap_into_place(staging: &Path, target: &Path) -> Result<(), LifecycleError> {
    if !target.exists() {
        std::fs::rename(staging, target)?;
        return Ok(());
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
    let _ = std::fs::remove_dir_all(&retired);
    Ok(())
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

/// Layouts for the crate's own tests.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// The FHS defaults, independent of the test process environment.
    pub(crate) fn fhs_paths() -> Paths {
        Paths {
            install_dir: PathBuf::from(PLUGINS_INSTALL_DIR),
            unit_dir: PathBuf::from(PLUGIN_UNIT_DIR),
            state_path: PathBuf::from(state::PLUGIN_STATE_PATH),
            log_dir: PathBuf::from(PLUGIN_LOG_DIR),
            control_dir: PathBuf::from(crate::control::DEFAULT_CONTROL_DIR),
            loopback_guard_state: PathBuf::from("/run/ados/plugin-loopback-guard.json"),
            socket_dir: PathBuf::from(crate::server::DEFAULT_SOCKET_DIR),
            token_secret: PathBuf::from(crate::token_secret::PLUGIN_TOKEN_SECRET_PATH),
            runner: PathBuf::from(PLUGIN_RUNNER_BINARY),
            run_dir: PathBuf::from(DEFAULT_RUN_DIR),
        }
    }

    /// Every path under `dir`.
    pub(crate) fn paths_in(dir: &Path) -> Paths {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::parse_archive_bytes;
    use std::collections::BTreeMap;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    use super::tests_support::paths_in;
    use crate::backend::RecordingBackend;

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
            .with_backend(Arc::new(RecordingBackend::default()));
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
        let rec = Arc::new(RecordingBackend::default());
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(rec.clone());

        let archive = build_unsigned_archive(SUBPROC_MANIFEST);
        let contents = parse_archive_bytes(archive).unwrap();
        let res = sup
            .install_contents(contents, Path::new("/tmp/thermal.adosplug"))
            .unwrap();
        assert_eq!(res.plugin_id, "com.example.thermal");
        assert_eq!(res.risk, "high");
        assert_eq!(res.permissions_requested, vec!["hardware.spi".to_string()]);

        // The unit was handed to the backend with the most restrictive sandbox,
        // and the attestation landed beside the unpacked tree.
        let unit = rec
            .unit("ados-plugin-com-example-thermal.service")
            .expect("unit installed");
        assert!(unit.contains("PrivateDevices=yes"));
        let attestation = sup.attestation("com.example.thermal").unwrap();
        assert_eq!(attestation.signature, None);
        let paths: Vec<&str> = attestation.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["agent/py/x.py", "manifest.yaml"]);
        assert_eq!(
            attestation.files[1].sha256,
            hex::encode(Sha256::digest(SUBPROC_MANIFEST.as_bytes()))
        );
        assert_eq!(
            sup.installed_manifest_bytes("com.example.thermal").unwrap(),
            SUBPROC_MANIFEST.as_bytes()
        );

        // State persisted with status installed.
        let reloaded = load_state(Some(&dir.path().join("state/plugin-state.json")));
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].status, PluginStatus::Installed);
    }

    const OTHER_MANIFEST: &str = "id: com.example.other\nversion: 1.0.0\nrisk: low\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n";

    /// A long-lived controller must modify what is on disk now, not the list
    /// it read at boot. The cloud relay holds one for its whole life while the
    /// LAN install path writes the same state file; saving the stale copy
    /// erased the other writer's install.
    #[test]
    fn a_mutation_keeps_an_install_another_writer_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingBackend::default());
        let mut relay = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(rec.clone());
        relay
            .install_contents(
                parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap(),
                Path::new("/tmp/thermal.adosplug"),
            )
            .unwrap();

        // A second writer (the LAN path) records its own install.
        let mut lan = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(rec.clone());
        lan.discover().unwrap();
        lan.install_contents(
            parse_archive_bytes(build_unsigned_archive(OTHER_MANIFEST)).unwrap(),
            Path::new("/tmp/other.adosplug"),
        )
        .unwrap();

        // The relay's controller, still holding its earlier view, mutates.
        relay.enable("com.example.thermal").unwrap();

        let on_disk = load_state(Some(&dir.path().join("state/plugin-state.json")));
        let ids: BTreeSet<&str> = on_disk.iter().map(|i| i.plugin_id.as_str()).collect();
        assert!(
            ids.contains("com.example.other"),
            "the other writer's install was erased: {ids:?}"
        );
        assert!(ids.contains("com.example.thermal"));
    }

    /// A reinstall checks the new archive before touching the old install, and
    /// stops the running plugin before its files are swapped.
    #[test]
    fn a_bad_reinstall_keeps_the_working_install_and_a_good_one_stops_the_old_process() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingBackend::default());
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(rec.clone());
        sup.install_contents(
            parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap(),
            Path::new("/tmp/thermal.adosplug"),
        )
        .unwrap();
        sup.enable("com.example.thermal").unwrap();
        let installed_file = dir.path().join("plugins/com.example.thermal/agent/py/x.py");
        assert!(installed_file.exists());

        // v1.1.0 declares a GCS bundle it does not ship: refused.
        let broken = SUBPROC_MANIFEST.replace("version: 1.0.0", "version: 1.1.0")
            + "gcs:\n  entrypoint: gcs/dist/index.js\n";
        let err = sup
            .install_contents(
                parse_archive_bytes(build_unsigned_archive(&broken)).unwrap(),
                Path::new("/tmp/thermal-broken.adosplug"),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("not present"), "{err}");
        assert!(
            installed_file.exists(),
            "a refused archive must leave the working install on disk"
        );
        assert_eq!(
            sup.find_install("com.example.thermal").unwrap().status,
            PluginStatus::Running
        );
        assert!(!dir
            .path()
            .join("plugins/.com.example.thermal.staging")
            .exists());

        // A good v1.1.0 stops the running unit before replacing its files.
        rec.clear_calls();
        let good = SUBPROC_MANIFEST.replace("version: 1.0.0", "version: 1.1.0");
        sup.install_contents(
            parse_archive_bytes(build_unsigned_archive(&good)).unwrap(),
            Path::new("/tmp/thermal-good.adosplug"),
        )
        .unwrap();
        let unit = "ados-plugin-com-example-thermal.service";
        let calls = rec.calls();
        let stopped = calls
            .iter()
            .position(|c| c == &("stop_disable".to_string(), unit.to_string()));
        let reinstalled = calls
            .iter()
            .position(|c| c == &("install".to_string(), unit.to_string()));
        assert!(
            matches!((stopped, reinstalled), (Some(s), Some(i)) if s < i),
            "the old process must be stopped before the swap: {calls:?}"
        );
        let record = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(record.version, "1.1.0");
        assert_eq!(record.status, PluginStatus::Installed);
    }

    #[test]
    fn enable_then_disable_subprocess_drives_the_backend() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingBackend::default());
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(rec.clone());
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

        let unit = "ados-plugin-com-example-thermal.service";
        assert!(rec.called("enable_start", unit));
        assert!(rec.called("stop_disable", unit));
        // The idempotent repeats issued nothing more.
        let count = |verb: &str| rec.calls().iter().filter(|(v, _)| v == verb).count();
        assert_eq!(count("enable_start"), 1);
        assert_eq!(count("stop_disable"), 1);
    }

    #[test]
    fn remove_deletes_unit_dir_and_state() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingBackend::default());
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(rec.clone());
        let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
        sup.install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap();
        sup.enable("com.example.thermal").unwrap();

        sup.remove("com.example.thermal", false).unwrap();
        assert!(sup.find_install("com.example.thermal").is_none());
        assert!(rec
            .unit("ados-plugin-com-example-thermal.service")
            .is_none());
        assert!(!dir.path().join("plugins/com.example.thermal").exists());
        assert!(load_state(Some(&dir.path().join("state/plugin-state.json"))).is_empty());
    }

    #[test]
    fn grant_rejects_undeclared_permission() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::default()));
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
            .with_backend(Arc::new(RecordingBackend::default()))
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
        let rec = Arc::new(RecordingBackend::default());
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(rec.clone());
        let manifest = "id: com.example.net\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - network.outbound\n";
        let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        sup.install_contents(contents, Path::new("/tmp/net.adosplug"))
            .unwrap();
        let unit = || rec.unit("ados-plugin-com-example-net.service").unwrap();

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
            .with_backend(Arc::new(RecordingBackend::default()));
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
            .with_backend(Arc::new(RecordingBackend::default()))
            .with_ungrantable_caps(RealHost::ungrantable_caps());
        // recording.write and sensor.camera.register are dead on RealHost;
        // mavlink.read is backed.
        let manifest = "id: com.example.rec\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - recording.write\n    - sensor.camera.register\n    - mavlink.read\n";
        let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        sup.install_contents(contents, Path::new("/tmp/rec.adosplug"))
            .unwrap();
        assert!(sup
            .grant_permission("com.example.rec", "recording.write")
            .is_err());
        assert!(sup
            .grant_permission("com.example.rec", "sensor.camera.register")
            .is_err());
        sup.grant_permission("com.example.rec", "mavlink.read")
            .unwrap();
        assert!(state::is_permission_granted(
            sup.find_install("com.example.rec").unwrap(),
            "mavlink.read"
        ));
    }

    #[test]
    fn incompatible_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::default()));
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
        .with_backend(Arc::new(RecordingBackend::default()));
        let manifest = "id: com.example.board\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\n  supported_boards: [rock-5c-lite]\nagent:\n  entrypoint: agent/py/x.py\n";
        let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
        let err = sup
            .install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap_err();
        assert!(format!("{err}").contains("does not support board"), "{err}");
    }

    #[test]
    fn board_identity_reads_the_sidecar_and_stays_lenient_without_one() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            PluginSupervisor::board_identity(&dir.path().join("absent.json")),
            (None, None)
        );
        let sidecar = dir.path().join("board.json");
        std::fs::write(
            &sidecar,
            serde_json::json!({
                "version": 1, "name": "rock-5c-lite", "model": "x", "tier": 3,
                "ram_mb": 4096, "cpu_cores": 6, "vendor": "radxa", "soc": "rk3582",
                "arch": "aarch64", "hw_video_codecs": [], "npu_tops": 0.0,
                "has_accelerator": false, "local_inference": "none",
                "has_local_inference": false,
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            PluginSupervisor::board_identity(&sidecar),
            (Some("rock-5c-lite".to_string()), Some(3))
        );
    }

    /// Nothing runs an in-process agent half, so it is refused whoever signed
    /// it; a first-party signature used to let one install and read enabled
    /// while never running.
    #[test]
    fn inprocess_is_refused_even_from_a_first_party_signer() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::default()));
        let manifest = "id: com.altnautica.inproc\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: pkg:Class\n  isolation: inprocess\n";
        for signer in ["altnautica-2026-A", "third-party"] {
            let mut contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
            contents.signer_id = Some(signer.to_string());
            contents.signature_b64 = Some("QUJD".to_string());
            let err = sup
                .install_contents(contents, Path::new("/tmp/x.adosplug"))
                .unwrap_err();
            assert!(format!("{err}").contains("does not run"), "{signer}: {err}");
        }
        assert!(sup.installs().is_empty());
    }

    #[test]
    fn inline_gcs_from_third_party_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::default()));
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
            .with_backend(Arc::new(RecordingBackend::default()));
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
            .with_backend(Arc::new(RecordingBackend::default()));
        sup.discover().unwrap();
        let install = sup.find_install("com.example.thermal").unwrap();
        assert!(install.permissions.contains_key("hardware.spi"));
        assert!(!install.permissions.contains_key("mission.write"));
    }

    /// A hung service-manager call must not hold the controller: the call is
    /// abandoned at its deadline and reported as a timeout.
    #[test]
    fn a_hung_service_call_is_abandoned_at_its_deadline() {
        use crate::backend::run_bounded;
        let started = std::time::Instant::now();
        let err = run_bounded(
            "sleep",
            &["30"],
            None,
            std::time::Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(err.0.contains("timed out"), "{}", err.0);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(run_bounded("false", &[], None, std::time::Duration::from_secs(5)).is_err());
        assert_eq!(
            run_bounded("echo", &["hi"], None, std::time::Duration::from_secs(5)).unwrap(),
            "hi\n"
        );
    }

    /// Declared services follow the plugin's lifecycle on this path exactly as
    /// on the LAN path: rendered and started on enable, readiness recorded,
    /// stopped on disable, units deleted on remove.
    #[test]
    fn declared_services_follow_enable_disable_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingBackend::default());
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(rec.clone());
        let manifest = SUBPROC_MANIFEST.to_string()
            + "  contributes:\n    services:\n      - name: bridge\n        command: bin/bridge --fast\n";
        sup.install_contents(
            parse_archive_bytes(build_unsigned_archive(&manifest)).unwrap(),
            Path::new("/tmp/thermal.adosplug"),
        )
        .unwrap();
        let svc_unit = "ados-plugin-com-example-thermal-bridge.service";

        sup.enable("com.example.thermal").unwrap();
        let unit = rec.unit(svc_unit).expect("service unit installed");
        assert!(unit.contains("ExecStart=bin/bridge --fast"));
        assert!(rec.called("enable_start", svc_unit));
        assert_eq!(
            sup.find_install("com.example.thermal")
                .unwrap()
                .service_status,
            Some(serde_json::json!([{"name": "bridge", "ready": true, "reason": null}]))
        );
        assert_eq!(
            sup.service_readiness("com.example.thermal").unwrap(),
            Some(serde_json::json!([{"name": "bridge", "ready": true, "reason": null}]))
        );

        sup.disable("com.example.thermal").unwrap();
        assert!(rec.called("stop_disable", svc_unit));
        assert!(sup
            .find_install("com.example.thermal")
            .unwrap()
            .service_status
            .is_none());

        sup.remove("com.example.thermal", false).unwrap();
        assert!(
            rec.unit(svc_unit).is_none(),
            "the service unit must be deleted"
        );
        assert!(sup.service_readiness("com.example.thermal").is_err());
    }

    fn archive_with(manifest_yaml: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            w.start_file("manifest.yaml", opts).unwrap();
            w.write_all(manifest_yaml.as_bytes()).unwrap();
            for (path, bytes) in files {
                w.start_file(*path, opts).unwrap();
                w.write_all(bytes).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    fn signed_as(archive: Vec<u8>, signer: &str) -> ArchiveContents {
        let mut contents = parse_archive_bytes(archive).unwrap();
        contents.signer_id = Some(signer.to_string());
        contents.signature_b64 = Some("QUJD".to_string());
        contents
    }

    const WORKSTATION_ONLY: &str = "id: com.example.ws\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  target_profiles: [workstation, compute]\n";

    #[test]
    fn an_agent_half_that_does_not_target_this_profile_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::default()));
        let err = sup
            .install_contents(
                parse_archive_bytes(build_unsigned_archive(WORKSTATION_ONLY)).unwrap(),
                Path::new("/tmp/ws.adosplug"),
            )
            .unwrap_err();
        assert_eq!(
            format!("{err}"),
            "incompatible: target_profiles excludes drone"
        );
        assert!(sup.installs().is_empty());

        // The same archive installs on a node of a profile it targets.
        let mut compute = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::default()))
            .with_profile("compute");
        compute
            .install_contents(
                parse_archive_bytes(build_unsigned_archive(WORKSTATION_ONLY)).unwrap(),
                Path::new("/tmp/ws.adosplug"),
            )
            .unwrap();

        // A GCS-only plugin has no agent half to place and is not gated.
        let gcs = "id: com.example.panel\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/index.js\n";
        sup.install_contents(
            parse_archive_bytes(archive_with(gcs, &[("gcs/index.js", b"x")])).unwrap(),
            Path::new("/tmp/panel.adosplug"),
        )
        .unwrap();
    }

    const HEAVY: &str = "id: com.altnautica.heavy\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  resources:\n    class: heavy\n    max_ram_mb: 16384\n    max_cpu_percent: 800\n    max_pids: 1024\n";

    #[test]
    fn heavy_resources_are_first_party_only() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingBackend::default());
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(rec.clone());
        for contents in [
            parse_archive_bytes(build_unsigned_archive(HEAVY)).unwrap(),
            signed_as(build_unsigned_archive(HEAVY), "third-party"),
        ] {
            let err = sup
                .install_contents(contents, Path::new("/tmp/heavy.adosplug"))
                .unwrap_err();
            assert_eq!(
                format!("{err}"),
                "refused: heavy_resources_first_party_only"
            );
        }
        sup.install_contents(
            signed_as(build_unsigned_archive(HEAVY), "altnautica-2026-A"),
            Path::new("/tmp/heavy.adosplug"),
        )
        .unwrap();
        let unit = rec
            .unit("ados-plugin-com-altnautica-heavy.service")
            .unwrap();
        assert!(unit.contains("MemoryMax=16384M") && unit.contains("CPUQuota=800%"));
    }

    #[test]
    fn a_backend_without_a_sandbox_installs_first_party_agents_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::without_sandbox()));
        for contents in [
            parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap(),
            signed_as(build_unsigned_archive(SUBPROC_MANIFEST), "third-party"),
        ] {
            let err = sup
                .install_contents(contents, Path::new("/tmp/x.adosplug"))
                .unwrap_err();
            assert_eq!(
                format!("{err}"),
                "refused: sandbox_unavailable_first_party_only"
            );
        }
        assert!(sup.installs().is_empty());
        // A GCS-only plugin runs nothing on the host, so it needs no sandbox.
        let gcs = "id: com.example.panel\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/index.js\n";
        sup.install_contents(
            parse_archive_bytes(archive_with(gcs, &[("gcs/index.js", b"x")])).unwrap(),
            Path::new("/tmp/panel.adosplug"),
        )
        .unwrap();
        // A first-party agent installs, and its network grant does not wait on
        // a loopback guard the backend has no sandbox to pair with.
        let net = SUBPROC_MANIFEST.replace("hardware.spi", "network.outbound");
        sup.install_contents(
            signed_as(build_unsigned_archive(&net), "altnautica-2026-B"),
            Path::new("/tmp/x.adosplug"),
        )
        .unwrap();
        sup.grant_permission("com.example.thermal", "network.outbound")
            .unwrap();
    }

    fn bin_manifest(arch_os: &str) -> String {
        format!(
            "id: com.example.multi\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: \"bin:multi-link\"\n  runtime: rust\n  binaries:\n    multi-link:\n      {arch_os}: bin/{arch_os}/multi-link\n"
        )
    }

    #[test]
    fn a_bin_entrypoint_without_this_hosts_binary_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingBackend::default());
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(rec.clone());
        let host = host_arch_os();
        let other = if host == "riscv64-linux" {
            "aarch64-linux"
        } else {
            "riscv64-linux"
        };
        let foreign = bin_manifest(other);
        let err = sup
            .install_contents(
                parse_archive_bytes(archive_with(
                    &foreign,
                    &[(&format!("bin/{other}/multi-link"), b"x")],
                ))
                .unwrap(),
                Path::new("/tmp/multi.adosplug"),
            )
            .unwrap_err();
        assert_eq!(
            format!("{err}"),
            format!("incompatible: no binary for {host}")
        );
        // The host's binary named but not shipped is refused at unpack.
        let native = bin_manifest(&host);
        let err = sup
            .install_contents(
                parse_archive_bytes(archive_with(&native, &[])).unwrap(),
                Path::new("/tmp/multi.adosplug"),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("not present"), "{err}");
        // Shipped: installs and the unit execs the resolved path.
        sup.install_contents(
            parse_archive_bytes(archive_with(
                &native,
                &[(&format!("bin/{host}/multi-link"), b"x")],
            ))
            .unwrap(),
            Path::new("/tmp/multi.adosplug"),
        )
        .unwrap();
        let unit = rec.unit("ados-plugin-com-example-multi.service").unwrap();
        assert!(unit.contains(&format!(
            "/plugins/com.example.multi/bin/{host}/multi-link com.example.multi --socket"
        )));
    }

    fn payload_manifest(sha256: &str, size: usize) -> String {
        format!(
            "id: com.altnautica.payload\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: \"bin:tool\"\n  runtime: rust\n  binaries:\n    tool:\n      {arch}: bin/tool\n  payloads:\n    - path: bin/tool\n      source: https://github.com/example/releases/download/v1/tool\n      sha256: {sha256}\n      size_bytes: {size}\n    - path: models/other-arch.bin\n      source: https://github.com/example/releases/download/v1/other\n      sha256: {zeros}\n      size_bytes: 4\n      arch_os: riscv64-none\n",
            arch = host_arch_os(),
            zeros = "0".repeat(64),
        )
    }

    #[test]
    fn payloads_land_only_when_their_pinned_digest_matches() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"#!/bin/sh\necho tool\n".to_vec();
        let sha = hex::encode(Sha256::digest(&body));
        let source = Arc::new(crate::download::StaticDownloadSource::default().with(
            "https://github.com/example/releases/download/v1/tool",
            body.clone(),
        ));
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::default()))
            .with_download_source(source);

        // A pinned digest the body does not match: refused, nothing installed,
        // nothing left behind.
        let wrong = payload_manifest(&"a".repeat(64), body.len());
        let err = sup
            .install_contents(
                parse_archive_bytes(archive_with(&wrong, &[])).unwrap(),
                Path::new("/tmp/payload.adosplug"),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("sha256 mismatch"), "{err}");
        assert!(sup.installs().is_empty());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("plugins"))
            .map(|d| d.filter_map(Result::ok).map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // The right digest: the payload lands executable in the tree, the
        // payload for another architecture is not fetched, and the
        // attestation lists the payload after the archive files.
        let right = payload_manifest(&sha, body.len());
        sup.install_contents(
            parse_archive_bytes(archive_with(&right, &[])).unwrap(),
            Path::new("/tmp/payload.adosplug"),
        )
        .unwrap();
        let tool = dir.path().join("plugins/com.altnautica.payload/bin/tool");
        assert_eq!(std::fs::read(&tool).unwrap(), body);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&tool).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!dir
            .path()
            .join("plugins/com.altnautica.payload/models/other-arch.bin")
            .exists());
        let attestation = sup.attestation("com.altnautica.payload").unwrap();
        let last = attestation.files.last().unwrap();
        assert_eq!(
            (last.path.as_str(), last.sha256.as_str(), last.payload),
            ("bin/tool", sha.as_str(), true)
        );
        assert!(attestation.files[..attestation.files.len() - 1]
            .iter()
            .all(|f| !f.payload));
    }

    #[test]
    fn services_run_only_on_the_profiles_they_name() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(RecordingBackend::default());
        let manifest = SUBPROC_MANIFEST.replace("  permissions:", "  target_profiles: [drone, workstation]\n  permissions:")
            + "  contributes:\n    services:\n      - name: capture\n        command: bin/capture\n        profiles: [drone]\n      - name: node\n        command: bin/node\n        profiles: [workstation]\n";
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(rec.clone())
            .with_profile("workstation");
        sup.install_contents(
            parse_archive_bytes(build_unsigned_archive(&manifest)).unwrap(),
            Path::new("/tmp/x.adosplug"),
        )
        .unwrap();
        sup.enable("com.example.thermal").unwrap();
        assert!(rec
            .unit("ados-plugin-com-example-thermal-node.service")
            .is_some());
        assert!(rec
            .unit("ados-plugin-com-example-thermal-capture.service")
            .is_none());
        assert_eq!(
            sup.service_readiness("com.example.thermal").unwrap(),
            Some(serde_json::json!([{"name": "node", "ready": true, "reason": null}]))
        );
    }

    #[test]
    fn a_shared_topic_another_plugin_owns_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::default()));
        let owner = |id: &str| {
            format!(
                "id: {id}\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  contributes:\n    shared_topics:\n      - topic: plugin.mapper.pose\n        subscribe_capability: telemetry.read\n"
            )
        };
        sup.install_contents(
            parse_archive_bytes(build_unsigned_archive(&owner("com.first.mapper"))).unwrap(),
            Path::new("/tmp/a.adosplug"),
        )
        .unwrap();
        let err = sup
            .install_contents(
                parse_archive_bytes(build_unsigned_archive(&owner("com.second.mapper"))).unwrap(),
                Path::new("/tmp/b.adosplug"),
            )
            .unwrap_err();
        assert!(
            format!("{err}").contains("already owned by com.first.mapper"),
            "{err}"
        );
        // Reinstalling the owner itself is not a collision.
        sup.install_contents(
            parse_archive_bytes(build_unsigned_archive(&owner("com.first.mapper"))).unwrap(),
            Path::new("/tmp/a.adosplug"),
        )
        .unwrap();
    }

    #[test]
    fn an_http_plugin_gets_its_socket_dir_on_enable_and_loses_it_on_remove() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::default()));
        let manifest = SUBPROC_MANIFEST.replace("  permissions:", "  http: true\n  permissions:");
        sup.install_contents(
            parse_archive_bytes(build_unsigned_archive(&manifest)).unwrap(),
            Path::new("/tmp/x.adosplug"),
        )
        .unwrap();
        let http_dir = dir.path().join("run/plugin-http/com.example.thermal");
        sup.enable("com.example.thermal").unwrap();
        assert!(http_dir.is_dir());
        sup.remove("com.example.thermal", false).unwrap();
        assert!(!http_dir.exists());
    }

    #[test]
    fn a_declared_capability_is_found_with_its_owner_and_risk() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::default()));
        let manifest = "id: com.example.mapper\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  declared_capabilities:\n    - id: plugin.mapper.world.read\n      risk: low\n      description: Read the world model\n";
        sup.install_contents(
            parse_archive_bytes(build_unsigned_archive(manifest)).unwrap(),
            Path::new("/tmp/m.adosplug"),
        )
        .unwrap();
        let (owner, cap) = sup.declared_capability("plugin.mapper.world.read").unwrap();
        assert_eq!(owner, "com.example.mapper");
        assert_eq!(cap.risk, "low");
        assert!(sup.declared_capability("plugin.mapper.other").is_none());
    }
}
