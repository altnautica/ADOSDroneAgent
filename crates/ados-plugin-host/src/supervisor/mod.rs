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
//!   a service backend that enforces no sandbox require a first-party signer
//!   whose Ed25519 signature verified against an enrolled key. The id an
//!   archive declares is never trusted on its own, even with signing relaxed.
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::archive::{
    open_archive, parse_archive_bytes, unpack_to, ArchiveContents, Executables, MANIFEST_FILENAME,
};
use crate::attestation::Attestation;
use crate::backend::{default_backend, ServiceBackend};
use crate::download::{fetch_to_file, DownloadSource, HttpDownloadSource};
use crate::errors::{
    LifecycleError, ManifestError, SignatureError, SignatureErrorKind, SupervisorError,
};
use crate::manifest::{
    bin_reference, host_arch_os, AgentIsolation, BoardId, DeclaredCapability, GcsIsolation,
    PluginManifest, ResourceClass, PAYLOAD_MAX_BYTES,
};
use crate::plugin_account::{host_accounts, Accounts};
use crate::server::{plugin_socket_dir, plugin_socket_path};
use crate::services::{
    build_service_spec, declared_services, probe_spec, service_argv, service_unit_name_for,
    ReadyCheck, ServiceSpec,
};
use crate::signing::{
    is_first_party_signer, load_revocations, load_trusted_keys, verify_archive_signature,
    Revocations, PLUGIN_KEYS_DIR, PLUGIN_REVOCATIONS_PATH,
};
use crate::state::{
    self, filter_permissions_against_manifest, find_install, grant_permission, load_state, now_ms,
    remove_install, revoke_permission, save_state, upsert_install, PluginInstall, PluginSource,
    PluginStatus, StateLock,
};
use crate::systemd::{
    build_unit_spec, plugin_http_dir, plugin_unit_basename, unit_name_for, PLUGIN_LOG_DIR,
    PLUGIN_LOG_SUFFIX, PLUGIN_RUNNER_BINARY, PLUGIN_UNIT_DIR, PLUGIN_UNIT_PREFIX,
};
use crate::token_secret::{
    plugin_data_dir, shared_issuer, token_credential_path, write_token_credential, PLUGIN_DATA_DIR,
};
use ados_protocol::identity::device_id_at;

/// Default install directory for unpacked third-party archives.
pub const PLUGINS_INSTALL_DIR: &str = "/var/ados/plugins";

/// Default agent run directory (`ADOS_RUN_DIR` overrides it).
pub const DEFAULT_RUN_DIR: &str = "/run/ados";

/// Summary returned from a successful install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub plugin_id: String,
    pub version: String,
    /// The signer whose signature verified at install; `None` for an unsigned
    /// archive or one whose signature did not verify.
    pub signer_id: Option<String>,
    pub risk: String,
    pub permissions_requested: Vec<String>,
}

/// Summary returned from a successful remove. `warning` names what could not
/// be read when the install was removed without its manifest (a missing,
/// tampered or no-longer-valid one); `None` on a normal remove.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveOutcome {
    pub warning: Option<String>,
}

/// What one revocation pass did: the plugins it disabled, and the plugins it
/// left running because the vehicle counts as armed (their records carry
/// `revocation_pending`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RevocationSweep {
    pub disabled: Vec<String>,
    pub deferred: Vec<String>,
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
    /// Each plugin's persistent data dir lives at `<data_root>/<id>`
    /// (`ADOS_PLUGIN_DATA_DIR_ROOT`).
    pub data_root: PathBuf,
    /// This node's device id, which places a plugin's per-drone data dir
    /// (`ADOS_DEVICE_ID_PATH`).
    pub device_id_file: PathBuf,
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
            data_root: env_path("ADOS_PLUGIN_DATA_DIR_ROOT", PLUGIN_DATA_DIR),
            device_id_file: ados_protocol::identity::device_id_path(),
        }
    }
}

/// The refusal reason recorded when an archive's signer differs from the
/// signer of the install it would replace.
pub const SIGNER_CHANGE_REASON: &str = "signer_change";

/// The failure reason recorded on an install whose signer has been revoked.
pub const SIGNER_REVOKED_REASON: &str = "signer_revoked";

/// The refusal reason when the revocation list exists but cannot be read: a
/// signed plugin is neither installed nor enabled until it can be.
pub const REVOCATION_LIST_UNREADABLE_REASON: &str = "revocation_list_unreadable";

/// The refusal and failure reason for an archive whose digest is revoked.
pub const ARCHIVE_REVOKED_REASON: &str = "archive_revoked";

/// The refusal and failure reason for a plugin release the registry revoked
/// by `<plugin_id>@<version>`.
pub const VERSION_REVOKED_REASON: &str = "version_revoked";

/// The refusal reason for a plugin whose `supported_boards` excludes this board.
pub const BOARD_MISMATCH_REASON: &str = "board_mismatch";

/// Plugin lifecycle controller. Constructed once per agent.
pub struct PluginSupervisor {
    paths: Paths,
    require_signed: bool,
    current_board_id: Option<BoardId>,
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
    /// The enrolled signer keys an archive signature verifies against.
    trusted_keys_dir: PathBuf,
    /// The signer revocation list (`None` reads the default path).
    revocations_path: Option<PathBuf>,
    /// Creates and removes each plugin's system user and hands its writable
    /// dirs to it, on a sandboxing backend.
    accounts: Accounts,
    /// Extra first-party keys a unit test pins (see `with_first_party_key`).
    #[cfg(test)]
    test_first_party: Vec<(String, [u8; 32])>,
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
        current_board_id: Option<BoardId>,
        agent_version: impl Into<String>,
    ) -> Self {
        let backend = default_backend(&paths.unit_dir);
        let accounts = host_accounts(&paths.control_dir);
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
            trusted_keys_dir: PathBuf::from(PLUGIN_KEYS_DIR),
            revocations_path: None,
            accounts,
            #[cfg(test)]
            test_first_party: Vec::new(),
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
        current_board_id: Option<BoardId>,
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

    /// The board identity and compute tier the `compatibility.supported_boards`
    /// and `min_tier` gates key on, read from the HAL board sidecar at
    /// `board_sidecar`. Every daemon that builds a [`production`](Self::production)
    /// controller passes these, so a plugin refused on one install path is
    /// refused on all of them. The identity is the profile stem plus the SoC
    /// (empty when `unknown`); it is `None` when the sidecar is absent,
    /// unreadable or names no profile, and the tier is `None` when out of
    /// range: a node that has not probed yet stays lenient rather than refusing
    /// every install.
    pub fn board_identity(board_sidecar: &Path) -> (Option<BoardId>, Option<u8>) {
        let Some(board) = ados_hal_probe::board_sidecar::read_sidecar(board_sidecar) else {
            return (None, None);
        };
        let tier = (1..=4).contains(&board.tier).then_some(board.tier as u8);
        let id = (!board.profile.is_empty()).then(|| BoardId {
            profile: board.profile,
            soc: if board.soc.eq_ignore_ascii_case("unknown") {
                String::new()
            } else {
                board.soc
            },
        });
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

    /// Read the enrolled signer keys from `dir` instead of
    /// [`PLUGIN_KEYS_DIR`] (tests enrol a throwaway key).
    pub fn with_trusted_keys_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.trusted_keys_dir = dir.into();
        self
    }

    /// Inject how plugin accounts are created, removed and handed their dirs
    /// (tests record the calls instead of touching the account database).
    pub fn with_accounts(mut self, accounts: Accounts) -> Self {
        self.accounts = accounts;
        self
    }

    /// Read the revocation list from `path` instead of the default
    /// (tests revoke a throwaway key).
    pub fn with_revocations_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.revocations_path = Some(path.into());
        self
    }

    /// The revocation list this controller enforces.
    pub fn revocations_path(&self) -> PathBuf {
        self.revocations_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(PLUGIN_REVOCATIONS_PATH))
    }

    /// The current revocation list. `Err` when the file exists but is
    /// unreadable or malformed: the callers refuse rather than trust a list
    /// they could not read.
    pub fn revocations(&self) -> Result<Revocations, String> {
        load_revocations(&self.revocations_path())
    }

    /// True when `signer_id` verified against an enrolled key that is a
    /// pinned first-party key. A first-party NAME on another key is a third
    /// party.
    fn is_first_party(&self, signer_id: Option<&str>) -> bool {
        let Some(signer_id) = signer_id else {
            return false;
        };
        let Some(key) = load_trusted_keys(Some(&self.trusted_keys_dir)).remove(signer_id) else {
            return false;
        };
        #[cfg(test)]
        if self
            .test_first_party
            .iter()
            .any(|(id, bytes)| id == signer_id && key.verifying_key.as_bytes() == bytes)
        {
            return true;
        }
        is_first_party_signer(signer_id, &key.verifying_key)
    }

    /// Treat `key` as a pinned first-party key for `signer_id` (unit tests sign
    /// with throwaway keys; the real pinned keys have no private half here).
    #[cfg(test)]
    pub(crate) fn with_first_party_key(
        mut self,
        signer_id: &str,
        key: ed25519_dalek::VerifyingKey,
    ) -> Self {
        self.test_first_party
            .push((signer_id.to_string(), key.to_bytes()));
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
}

mod compat;
mod enable_disable;
mod grants;
mod install;
mod readiness;
mod refresh;
mod units;

pub use readiness::{verify_install, InstallVerifier, TAMPERED_REASON};
use units::*;
pub use units::{require_signed_default, semver_in_range};

/// Layouts for the crate's own tests.
#[cfg(test)]
pub(crate) mod tests_support;

#[cfg(test)]
mod tests;
