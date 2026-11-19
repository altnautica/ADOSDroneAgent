//! Extensions: seed the plugin revocation list, replace the first-party
//! extensions this agent retired ([`retire`]), then install the first-party
//! Reconstruct extension (`com.altnautica.reconstruct`) on a node whose
//! operator asked for it. Optional, and after `health`, so it runs against a
//! node whose plugin host, control surface and loopback guard are already up.
//!
//! The revocation list (`/etc/ados/plugin-revocations.json`) is copied from
//! the shipped `scripts/plugin-keys/revoked.json` only when absent: an
//! existing list (merged from the registry, or edited by the operator) is
//! never overwritten.
//!
//! The toggle is [`crate::ctx::Ctx::install_reconstruct`]: the
//! `--reconstruct` / `--no-reconstruct` flag or the wizard's checkbox, else
//! the profile default ([`reconstruct_default`]). When it is on, the step:
//!
//! 1. finishes an extension that is already installed but was never switched
//!    on (an earlier run hit a guard or grant failure): it grants the missing
//!    required permissions and enables it. One the operator disabled is left
//!    alone, as are the operator's own grants on an enabled one (plugin
//!    auto-update keeps it current);
//! 2. reads the extension's entry from the bundled first-party catalog, the
//!    same document `/api/v1/plugins/catalog` serves;
//! 3. downloads the archive through the plugin host's allowlisted transport and
//!    checks it against the entry's `archive_sha256` pin, retrying a failed
//!    download;
//! 4. installs it through [`PluginSupervisor`] (signature, compatibility and
//!    profile gates, payloads, units), grants every permission the manifest
//!    marks `required` — the operator consented through the toggle — and
//!    enables it. A grant that fails while the loopback guard comes up is
//!    retried after another guard wait.
//!
//! A catalog with no entry, or an entry with no published release yet, is not
//! a failure: the step warns with the manual command and succeeds. Any real
//! failure that survives the retries degrades the install; a re-run of the
//! installer picks up where it stopped. It never aborts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_plugin_host::archive::ARCHIVE_MAX_BYTES;
use ados_plugin_host::download::{
    fetch_capped, validate_download_url, verify_sha256, DownloadSource, HttpDownloadSource,
};
use ados_plugin_host::realhost::RealHost;
use ados_plugin_host::{Paths, PluginStatus, PluginSupervisor};
use serde::Deserialize;

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// The Reconstruct extension's plugin id.
pub const RECONSTRUCT_ID: &str = "com.altnautica.reconstruct";

/// The command an operator runs to install the extension by hand.
pub const MANUAL_INSTALL: &str = "ados plugin install com.altnautica.reconstruct";

#[path = "extensions_retire.rs"]
pub mod retire;

#[path = "extensions_enable.rs"]
mod enable;
use enable::grant_required_and_enable;

/// The bundled first-party catalog, embedded so the installer reads the exact
/// document the agent it installs will serve.
const CATALOG_JSON: &str = include_str!("../../../../src/ados/data/plugin-catalog.json");

/// How long the Linux step waits for the plugin host's loopback guard before
/// granting `network.outbound`. The host installs the guard at daemon start,
/// which the `start` step kicked off without blocking.
const GUARD_WAIT: Duration = Duration::from_secs(30);

/// How many times a transient step (the download, a permission grant) is
/// attempted before the install gives up on it.
const ATTEMPTS: u32 = 3;

/// The pause between download attempts on a node.
pub const DOWNLOAD_RETRY_DELAY: Duration = Duration::from_secs(5);

/// The shipped revocation list, relative to the source tree.
pub const SHIPPED_REVOCATIONS: &str = "scripts/plugin-keys/revoked.json";

/// Copy the shipped revocation list to `target` when no list exists there.
/// Returns whether it copied. The shipped file must parse; an existing target
/// is never touched.
pub fn seed_revocation_list(source: Option<&Path>, target: &Path) -> Result<bool, String> {
    use std::io::Write;
    if target.exists() {
        return Ok(false);
    }
    let Some(shipped) = source.map(|s| s.join(SHIPPED_REVOCATIONS)) else {
        return Err("the source tree was not found, so no revocation list was seeded".into());
    };
    let bytes = std::fs::read(&shipped).map_err(|e| format!("{}: {e}", shipped.display()))?;
    ados_plugin_host::signing::parse_revocations(&bytes)
        .map_err(|e| format!("{}: {e}", shipped.display()))?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(format!("{}: {e}", target.display())),
    };
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("{}: {e}", target.display()))?;
    Ok(true)
}

/// Whether a profile installs Reconstruct when no flag or wizard answer
/// says otherwise: on for the workstation-class nodes that run its heavy half,
/// off for an aircraft, a ground station or a visor (an operator opts those in).
pub fn reconstruct_default(profile: &str) -> bool {
    matches!(profile, "workstation" | "compute")
}

/// The fields of a catalog row the install reads.
#[derive(Debug, Clone, Deserialize)]
struct CatalogEntry {
    id: String,
    #[serde(default)]
    download_url: String,
    #[serde(default)]
    archive_sha256: String,
}

#[derive(Debug, Deserialize)]
struct Catalog {
    #[serde(default)]
    plugins: Vec<CatalogEntry>,
}

/// What the install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionOutcome {
    /// Installed, granted and enabled by this run.
    Installed {
        version: String,
        granted: Vec<String>,
    },
    /// Already installed and switched on; left alone.
    AlreadyInstalled { version: String },
    /// Already installed but never switched on; this run granted the missing
    /// required permissions and enabled it.
    Finished {
        version: String,
        granted: Vec<String>,
    },
    /// The bundled catalog has nothing installable for it yet.
    Unavailable(String),
}

/// Where and as what the node installs the extension. Resolved by the caller:
/// the Linux step reads the FHS layout, the macOS path its per-user one.
pub struct NodeTarget {
    /// The plugin layout the controller writes to.
    pub paths: Paths,
    /// This node's profile, wire form (`ground-station`, not `ground_station`).
    pub profile: String,
    /// The installed agent semver, for the manifest's `ados_version` range.
    pub agent_version: String,
    /// The HAL board sidecar (`board.json`), for the board and tier gates.
    pub board_sidecar: PathBuf,
    /// How long to wait for the loopback guard before a `network.outbound`
    /// grant. Zero where the backend enforces no sandbox (the grant then does
    /// not consult the guard).
    pub guard_wait: Duration,
    /// The pause between download attempts ([`DOWNLOAD_RETRY_DELAY`] on a
    /// node).
    pub retry_delay: Duration,
}

/// What the extension pass did on a node.
#[derive(Debug)]
pub struct NodeExtensions {
    pub retired: Vec<retire::Retirement>,
    /// `None` when Reconstruct was not selected.
    pub reconstruct: Option<Result<ExtensionOutcome, String>>,
}

/// Replace retired extensions, then install Reconstruct when `reconstruct`
/// is set. Runs on its own OS thread: the installer drives its steps from
/// inside the async entry point, and the blocking HTTPS client must not be
/// built or dropped on a runtime thread.
pub fn install_node_extensions(
    target: NodeTarget,
    reconstruct: bool,
) -> Result<NodeExtensions, String> {
    std::thread::Builder::new()
        .name("extension-install".to_string())
        .spawn(move || {
            let (board_id, board_tier) = PluginSupervisor::board_identity(&target.board_sidecar);
            let mut supervisor =
                PluginSupervisor::production(target.paths, board_id, target.agent_version)
                    .with_profile(target.profile)
                    .with_board_tier(board_tier)
                    .with_ungrantable_caps(RealHost::ungrantable_caps());
            let source = HttpDownloadSource::new();
            retire::retire_then_install(
                &mut supervisor,
                CATALOG_JSON,
                &source,
                target.guard_wait,
                target.retry_delay,
                reconstruct,
            )
        })
        .map_err(|e| format!("could not start the install thread: {e}"))?
        .join()
        .map_err(|_| "the install thread panicked".to_string())
}

/// Install `plugin_id` from `catalog_json` through `supervisor` and switch it
/// on: [`install_extension`] with `enable` set.
pub fn install_from_catalog(
    supervisor: &mut PluginSupervisor,
    plugin_id: &str,
    catalog_json: &str,
    source: &dyn DownloadSource,
    guard_wait: Duration,
    retry_delay: Duration,
) -> Result<ExtensionOutcome, String> {
    install_extension(
        supervisor,
        plugin_id,
        catalog_json,
        source,
        guard_wait,
        retry_delay,
        true,
    )
}

/// Install `plugin_id` from `catalog_json` through `supervisor`: finish an
/// installed plugin that was never switched on, leave any other installed one
/// alone, report an unpublished one, else download (allowlist + sha256 pin,
/// retried), install, grant the manifest's required permissions and, when
/// `enable` is set, enable it. With `enable` unset the plugin stays installed
/// but off, its grants recorded.
pub fn install_extension(
    supervisor: &mut PluginSupervisor,
    plugin_id: &str,
    catalog_json: &str,
    source: &dyn DownloadSource,
    guard_wait: Duration,
    retry_delay: Duration,
    enable: bool,
) -> Result<ExtensionOutcome, String> {
    supervisor
        .discover()
        .map_err(|e| format!("reading the installed plugins failed: {e}"))?;
    if let Some(existing) = supervisor.find_install(plugin_id) {
        let version = existing.version.clone();
        // Only an install that never got switched on is unfinished. Disabled
        // is the operator's choice, and an enabled plugin's grants are theirs.
        if existing.status != PluginStatus::Installed {
            return Ok(ExtensionOutcome::AlreadyInstalled { version });
        }
        let granted = grant_required_and_enable(supervisor, plugin_id, guard_wait, enable)?;
        return Ok(ExtensionOutcome::Finished { version, granted });
    }

    let catalog: Catalog = serde_json::from_str(catalog_json)
        .map_err(|e| format!("the bundled plugin catalog is unreadable: {e}"))?;
    let Some(entry) = catalog.plugins.into_iter().find(|p| p.id == plugin_id) else {
        return Ok(ExtensionOutcome::Unavailable(format!(
            "{plugin_id} is not in the bundled plugin catalog yet"
        )));
    };
    if entry.download_url.trim().is_empty() {
        return Ok(ExtensionOutcome::Unavailable(format!(
            "{plugin_id} has no published release in the bundled plugin catalog yet"
        )));
    }
    if entry.archive_sha256.trim().is_empty() {
        return Err(format!(
            "the catalog entry for {plugin_id} carries no archive_sha256 pin; refusing an \
             unpinned download"
        ));
    }

    let url = entry.download_url.trim();
    validate_download_url(url).map_err(|e| format!("download refused: {e}"))?;
    let archive = download_with_retry(source, url, entry.archive_sha256.trim(), retry_delay)?;

    // A private directory (created exclusively, mode 0700) so nothing another
    // local user prepared in the temp root can stand in for the verified
    // archive between the pin check and the install. Removed on drop.
    let staging_dir = tempfile::Builder::new()
        .prefix("ados-extension-")
        .tempdir()
        .map_err(|e| format!("creating a private staging directory failed: {e}"))?;
    let staged = staging_dir.path().join("extension.adosplug");
    std::fs::write(&staged, &archive)
        .map_err(|e| format!("staging {} failed: {e}", staged.display()))?;
    let installed = supervisor
        .install_archive(&staged)
        .map_err(|e| format!("install refused: {e}"))?;
    drop(staging_dir);

    let granted = grant_required_and_enable(supervisor, plugin_id, guard_wait, enable)?;
    Ok(ExtensionOutcome::Installed {
        version: installed.version,
        granted,
    })
}

/// Download `url` and check it against `sha256`, retrying a failed fetch or a
/// mismatched body up to [`ATTEMPTS`] times, `retry_delay` apart.
fn download_with_retry(
    source: &dyn DownloadSource,
    url: &str,
    sha256: &str,
    retry_delay: Duration,
) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        let fetched = fetch_capped(source, url, ARCHIVE_MAX_BYTES)
            .map_err(|e| e.to_string())
            .and_then(|archive| match verify_sha256(&archive, Some(sha256)) {
                Ok(()) => Ok(archive),
                Err(e) => Err(e.to_string()),
            });
        match fetched {
            Ok(archive) => return Ok(archive),
            Err(e) => {
                tracing::warn!(attempt, error = %e, "extension download failed");
                last = e;
            }
        }
        if attempt < ATTEMPTS {
            std::thread::sleep(retry_delay);
        }
    }
    Err(format!("download failed: {last}"))
}

/// Map the install's result onto the step outcome, logging what happened. An
/// unpublished extension warns and succeeds; a real failure degrades.
fn step_outcome(result: Result<ExtensionOutcome, String>) -> StepOutcome {
    match result {
        Ok(ExtensionOutcome::Installed { version, granted }) => {
            tracing::info!(
                plugin = RECONSTRUCT_ID,
                %version,
                granted = ?granted,
                "Reconstruct extension installed and enabled"
            );
            StepOutcome::Ok
        }
        Ok(ExtensionOutcome::AlreadyInstalled { version }) => {
            tracing::info!(
                plugin = RECONSTRUCT_ID,
                %version,
                "Reconstruct extension already installed; left for plugin auto-update"
            );
            StepOutcome::Skipped
        }
        Ok(ExtensionOutcome::Finished { version, granted }) => {
            tracing::info!(
                plugin = RECONSTRUCT_ID,
                %version,
                granted = ?granted,
                "Reconstruct extension finished: required permissions granted and enabled"
            );
            StepOutcome::Ok
        }
        Ok(ExtensionOutcome::Unavailable(reason)) => {
            tracing::warn!(
                plugin = RECONSTRUCT_ID,
                "Reconstruct extension not installed: {reason}. Install it later with \
                 `{MANUAL_INSTALL}`"
            );
            StepOutcome::Skipped
        }
        Err(reason) => StepOutcome::Failed(format!(
            "Reconstruct extension not installed: {reason}. Install it later with \
             `{MANUAL_INSTALL}`"
        )),
    }
}

/// The extension step (see the module doc).
pub struct Extensions;

impl Step for Extensions {
    fn id(&self) -> &str {
        "extensions"
    }
    fn requires(&self) -> &[&str] {
        &["health"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: an installed extension is detected from plugin state,
        // which is the only record that cannot go stale.
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        let source = crate::env::resolve_source_dir(ctx.source_dir.as_deref());
        match seed_revocation_list(
            source.as_deref(),
            Path::new(ados_plugin_host::signing::PLUGIN_REVOCATIONS_PATH),
        ) {
            Ok(true) => tracing::info!("plugin revocation list seeded"),
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "plugin revocation list not seeded"),
        }
        let Some(agent_version) = crate::env::installed_agent_version() else {
            let why = "the installed agent version could not be read, so an extension's \
                       compatibility range cannot be checked";
            if !ctx.install_reconstruct {
                tracing::warn!("retired extensions not checked: {why}");
                return StepOutcome::Skipped;
            }
            return step_outcome(Err(why.to_string()));
        };
        if !ctx.install_reconstruct {
            tracing::info!(profile = %ctx.profile, "Reconstruct extension not selected");
        }
        ctx.progress
            .activity(self.id(), "installing extensions".to_string());
        let done = install_node_extensions(
            NodeTarget {
                paths: Paths::from_env(),
                profile: ados_config::normalize_profile(Some(&ctx.profile)),
                agent_version,
                board_sidecar: ados_hal_probe::board_sidecar::sidecar_path(),
                guard_wait: GUARD_WAIT,
                retry_delay: DOWNLOAD_RETRY_DELAY,
            },
            ctx.install_reconstruct,
        );
        let done = match done {
            Ok(d) => d,
            Err(e) => return step_outcome(Err(e)),
        };
        // Every retirement is logged. One that left a retired extension
        // installed (its successors not all in, or it would not uninstall)
        // keeps a generation this agent no longer reads: the optional step
        // fails, so the install finishes degraded and names it.
        let reports: Vec<bool> = done.retired.iter().map(retire::report).collect();
        if reports.contains(&true) {
            return StepOutcome::Failed(
                "a retired extension is still installed because not all of its successors \
                 installed or it could not be removed; re-run the installer"
                    .into(),
            );
        }
        match done.reconstruct {
            Some(r) => step_outcome(r),
            None if done.retired.is_empty() => StepOutcome::Skipped,
            None => StepOutcome::Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_revocation_list_is_seeded_only_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir_all(source.join("scripts/plugin-keys")).unwrap();
        std::fs::write(
            source.join(SHIPPED_REVOCATIONS),
            r#"{"signers":[],"archives":[]}"#,
        )
        .unwrap();
        let target = dir.path().join("etc/plugin-revocations.json");
        assert!(seed_revocation_list(Some(&source), &target).unwrap());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            r#"{"signers":[],"archives":[]}"#
        );
        // An existing list is never overwritten.
        std::fs::write(&target, r#"["alice"]"#).unwrap();
        assert!(!seed_revocation_list(Some(&source), &target).unwrap());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), r#"["alice"]"#);
        // A malformed shipped file, or no source tree, seeds nothing.
        let fresh = dir.path().join("etc/other.json");
        std::fs::write(source.join(SHIPPED_REVOCATIONS), "{").unwrap();
        assert!(seed_revocation_list(Some(&source), &fresh).is_err());
        assert!(seed_revocation_list(None, &fresh).is_err());
        assert!(!fresh.exists());
    }

    #[test]
    fn the_shipped_revocation_list_parses_and_is_empty() {
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(SHIPPED_REVOCATIONS);
        let revs =
            ados_plugin_host::signing::parse_revocations(&std::fs::read(shipped).unwrap()).unwrap();
        assert!(revs.signers.is_empty() && revs.archives.is_empty());
    }
    use std::io::Write;
    use std::sync::Arc;

    use ados_plugin_host::backend::RecordingBackend;
    use ados_plugin_host::download::StaticDownloadSource;
    use sha2::{Digest, Sha256};

    const URL: &str =
        "https://github.com/altnautica/ADOSExtensions/releases/download/x/we.adosplug";

    const MANIFEST: &str = "id: com.altnautica.reconstruct\n\
version: 1.0.0\n\
risk: high\n\
compatibility:\n  ados_version: \">=0.1.0,<99.0.0\"\n\
agent:\n  entrypoint: agent/py/x.py\n  target_profiles: [workstation]\n  permissions:\n    \
- hardware.spi\n    - id: mission.write\n      required: false\n";

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
            clock_feed: None,
        }
    }

    /// An unsigned archive; the test controller does not enforce signing.
    fn archive() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("manifest.yaml", opts).unwrap();
            w.write_all(MANIFEST.as_bytes()).unwrap();
            w.start_file("agent/py/x.py", opts).unwrap();
            w.write_all(b"print('hi')").unwrap();
            w.finish().unwrap();
        }
        buf
    }

    fn supervisor(dir: &Path) -> PluginSupervisor {
        PluginSupervisor::new(paths_in(dir), false, None, "0.100.0")
            .with_backend(Arc::new(RecordingBackend::default()))
            .with_profile("workstation")
    }

    fn catalog_with(url: &str, sha: &str) -> String {
        serde_json::json!({
            "schema_version": 1,
            "plugins": [{
                "id": RECONSTRUCT_ID,
                "version": "1.0.0",
                "download_url": url,
                "archive_sha256": sha,
            }],
        })
        .to_string()
    }

    #[test]
    fn profile_default_is_on_only_for_workstation_class_nodes() {
        assert!(reconstruct_default("workstation"));
        assert!(reconstruct_default("compute"));
        assert!(!reconstruct_default("drone"));
        assert!(!reconstruct_default("ground_station"));
        assert!(!reconstruct_default("visor"));
    }

    /// The bundled catalog gains its entry only when the extension is released,
    /// so today a workstation install must warn and carry on, not degrade.
    #[test]
    fn an_absent_catalog_entry_warns_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        let outcome = install_from_catalog(
            &mut sup,
            RECONSTRUCT_ID,
            r#"{"schema_version":1,"plugins":[]}"#,
            &StaticDownloadSource::default(),
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap();
        assert!(
            matches!(&outcome, ExtensionOutcome::Unavailable(r) if r.contains("not in the bundled")),
            "{outcome:?}"
        );
        assert_eq!(step_outcome(Ok(outcome)), StepOutcome::Skipped);
        assert!(sup.find_install(RECONSTRUCT_ID).is_none());
    }

    #[test]
    fn an_entry_with_no_published_release_warns_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        let outcome = install_from_catalog(
            &mut sup,
            RECONSTRUCT_ID,
            &catalog_with("", ""),
            &StaticDownloadSource::default(),
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap();
        assert!(
            matches!(&outcome, ExtensionOutcome::Unavailable(r) if r.contains("no published release")),
            "{outcome:?}"
        );
        assert_eq!(step_outcome(Ok(outcome)), StepOutcome::Skipped);
    }

    /// The embedded catalog must parse, or every toggled install degrades on a
    /// malformed document instead of on a real download problem.
    #[test]
    fn the_embedded_catalog_parses() {
        let catalog: Catalog = serde_json::from_str(CATALOG_JSON).unwrap();
        assert!(!catalog.plugins.is_empty());
    }

    #[test]
    fn a_published_entry_installs_grants_required_permissions_and_enables() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        let body = archive();
        let sha = hex::encode(Sha256::digest(&body));
        let source = StaticDownloadSource::default().with(URL, body);

        let outcome = install_from_catalog(
            &mut sup,
            RECONSTRUCT_ID,
            &catalog_with(URL, &sha),
            &source,
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            outcome,
            ExtensionOutcome::Installed {
                version: "1.0.0".to_string(),
                granted: vec!["hardware.spi".to_string()],
            }
        );
        let install = sup.find_install(RECONSTRUCT_ID).expect("installed");
        assert!(
            matches!(
                install.status,
                PluginStatus::Enabled | PluginStatus::Running
            ),
            "the extension must be switched on, got {:?}",
            install.status
        );
        // Only the required permission is granted; the optional one waits for
        // the operator.
        assert!(install.permissions["hardware.spi"].granted);
        assert!(install
            .permissions
            .get("mission.write")
            .is_none_or(|g| !g.granted));
        assert_eq!(step_outcome(Ok(outcome)), StepOutcome::Ok);

        // A re-run (an upgrade) leaves the installed extension alone, without
        // downloading anything.
        let again = install_from_catalog(
            &mut sup,
            RECONSTRUCT_ID,
            &catalog_with(URL, &sha),
            &StaticDownloadSource::default(),
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            again,
            ExtensionOutcome::AlreadyInstalled {
                version: "1.0.0".to_string()
            }
        );
    }

    /// An install that stopped before its grants and enable (a guard or grant
    /// failure on an earlier run) is finished by the next run, without a new
    /// download; one the operator switched off stays off.
    #[test]
    fn a_rerun_finishes_an_install_that_was_never_switched_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        let staged = dir.path().join("we.adosplug");
        std::fs::write(&staged, archive()).unwrap();
        sup.install_archive(&staged).unwrap();
        assert_eq!(
            sup.find_install(RECONSTRUCT_ID).unwrap().status,
            PluginStatus::Installed
        );

        let outcome = install_from_catalog(
            &mut sup,
            RECONSTRUCT_ID,
            &catalog_with(URL, &"00".repeat(32)),
            &StaticDownloadSource::default(),
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            outcome,
            ExtensionOutcome::Finished {
                version: "1.0.0".to_string(),
                granted: vec!["hardware.spi".to_string()],
            }
        );
        let install = sup.find_install(RECONSTRUCT_ID).unwrap();
        assert!(install.permissions["hardware.spi"].granted);
        assert!(matches!(
            install.status,
            PluginStatus::Enabled | PluginStatus::Running
        ));
        assert_eq!(step_outcome(Ok(outcome)), StepOutcome::Ok);

        sup.disable(RECONSTRUCT_ID).unwrap();
        let again = install_from_catalog(
            &mut sup,
            RECONSTRUCT_ID,
            &catalog_with(URL, &"00".repeat(32)),
            &StaticDownloadSource::default(),
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            again,
            ExtensionOutcome::AlreadyInstalled {
                version: "1.0.0".to_string()
            }
        );
        assert_eq!(
            sup.find_install(RECONSTRUCT_ID).unwrap().status,
            PluginStatus::Disabled
        );
    }

    #[test]
    fn a_pin_mismatch_degrades_with_the_manual_command_and_installs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        let source = StaticDownloadSource::default().with(URL, archive());
        let err = install_from_catalog(
            &mut sup,
            RECONSTRUCT_ID,
            &catalog_with(URL, &"00".repeat(32)),
            &source,
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(sup.find_install(RECONSTRUCT_ID).is_none());
        match step_outcome(Err(err)) {
            StepOutcome::Failed(msg) => assert!(msg.contains(MANUAL_INSTALL), "{msg}"),
            other => panic!("a failed install must degrade, got {other:?}"),
        }
    }

    #[test]
    fn an_unpinned_entry_is_refused_before_any_download() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        // The source would serve the archive; the refusal must come first.
        let source = StaticDownloadSource::default().with(URL, archive());
        let err = install_from_catalog(
            &mut sup,
            RECONSTRUCT_ID,
            &catalog_with(URL, ""),
            &source,
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(err.contains("archive_sha256"), "{err}");
        assert!(sup.find_install(RECONSTRUCT_ID).is_none());
    }

    #[test]
    fn the_step_skips_when_the_toggle_is_off() {
        let mut ctx = Ctx::for_test(crate::checkpoint::Checkpoint::new());
        ctx.install_reconstruct = false;
        assert_eq!(Extensions.run(&mut ctx), StepOutcome::Skipped);
    }
}
