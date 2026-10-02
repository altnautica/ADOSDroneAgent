//! Extensions: install the first-party World Engine extension
//! (`com.altnautica.world-engine`) on a node whose operator asked for it.
//! Optional, and after `health`, so it runs against a node whose plugin host,
//! control surface and loopback guard are already up.
//!
//! The toggle is [`crate::ctx::Ctx::install_world_engine`]: the
//! `--world-engine` / `--no-world-engine` flag or the wizard's checkbox, else
//! the profile default ([`world_engine_default`]). When it is on, the step:
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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ados_plugin_host::archive::ARCHIVE_MAX_BYTES;
use ados_plugin_host::download::{
    fetch_capped, validate_download_url, verify_sha256, DownloadSource, HttpDownloadSource,
};
use ados_plugin_host::realhost::RealHost;
use ados_plugin_host::sandbox::NETWORK_OUTBOUND_CAP;
use ados_plugin_host::{Paths, PluginStatus, PluginSupervisor};
use serde::Deserialize;

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// The World Engine extension's plugin id.
pub const WORLD_ENGINE_ID: &str = "com.altnautica.world-engine";

/// The command an operator runs to install the extension by hand.
pub const MANUAL_INSTALL: &str = "ados plugin install com.altnautica.world-engine";

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

/// Whether a profile installs the World Engine when no flag or wizard answer
/// says otherwise: on for the workstation-class nodes that run its heavy half,
/// off for an aircraft or a ground station (an operator opts those in).
pub fn world_engine_default(profile: &str) -> bool {
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

/// Install the World Engine on this node. Runs on its own OS thread: the
/// installer drives its steps from inside the async entry point, and the
/// blocking HTTPS client must not be built or dropped on a runtime thread.
pub fn install_world_engine(target: NodeTarget) -> Result<ExtensionOutcome, String> {
    std::thread::Builder::new()
        .name("world-engine-install".to_string())
        .spawn(move || {
            let (board_id, board_tier) = PluginSupervisor::board_identity(&target.board_sidecar);
            let mut supervisor =
                PluginSupervisor::production(target.paths, board_id, target.agent_version)
                    .with_profile(target.profile)
                    .with_board_tier(board_tier)
                    .with_ungrantable_caps(RealHost::ungrantable_caps());
            install_from_catalog(
                &mut supervisor,
                WORLD_ENGINE_ID,
                CATALOG_JSON,
                &HttpDownloadSource::new(),
                target.guard_wait,
                target.retry_delay,
            )
        })
        .map_err(|e| format!("could not start the install thread: {e}"))?
        .join()
        .map_err(|_| "the install thread panicked".to_string())?
}

/// Install `plugin_id` from `catalog_json` through `supervisor`: finish an
/// installed plugin that was never switched on, leave any other installed one
/// alone, report an unpublished one, else download (allowlist + sha256 pin,
/// retried), install, grant the manifest's required permissions, enable.
pub fn install_from_catalog(
    supervisor: &mut PluginSupervisor,
    plugin_id: &str,
    catalog_json: &str,
    source: &dyn DownloadSource,
    guard_wait: Duration,
    retry_delay: Duration,
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
        let granted = grant_required_and_enable(supervisor, plugin_id, guard_wait)?;
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

    let granted = grant_required_and_enable(supervisor, plugin_id, guard_wait)?;
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
            .and_then(|archive| match verify_sha256(&archive, sha256) {
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

/// Grant every required permission `plugin_id` does not hold yet, then enable
/// it. A grant is retried up to [`ATTEMPTS`] times; a `network.outbound` grant
/// waits for the loopback guard before each try. Returns the permissions this
/// call granted.
fn grant_required_and_enable(
    supervisor: &mut PluginSupervisor,
    plugin_id: &str,
    guard_wait: Duration,
) -> Result<Vec<String>, String> {
    let required = required_permissions(supervisor, plugin_id)?;
    let held: BTreeSet<String> = supervisor
        .find_install(plugin_id)
        .map(|i| {
            i.permissions
                .iter()
                .filter(|(_, g)| g.granted)
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut granted = Vec::new();
    for permission in required.difference(&held) {
        let mut last = String::new();
        let mut ok = false;
        for attempt in 1..=ATTEMPTS {
            if permission == NETWORK_OUTBOUND_CAP {
                wait_for_loopback_guard(&supervisor.paths().loopback_guard_state, guard_wait);
            }
            match supervisor.grant_permission(plugin_id, permission) {
                Ok(_) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!(attempt, %permission, error = %e, "extension grant failed");
                    last = e.to_string();
                }
            }
        }
        if !ok {
            return Err(not_enabled(
                plugin_id,
                &format!("granting {permission} failed: {last}"),
            ));
        }
        granted.push(permission.clone());
    }
    supervisor
        .enable(plugin_id)
        .map_err(|e| not_enabled(plugin_id, &format!("enable failed: {e}")))?;
    Ok(granted)
}

/// Every permission either half of the installed manifest marks `required`.
fn required_permissions(
    supervisor: &PluginSupervisor,
    plugin_id: &str,
) -> Result<BTreeSet<String>, String> {
    let manifest = supervisor
        .installed_manifest(plugin_id)
        .map_err(|e| format!("reading the installed manifest failed: {e}"))?;
    let agent = manifest.agent.iter().flat_map(|a| a.permissions.iter());
    let gcs = manifest.gcs.iter().flat_map(|g| g.permissions.iter());
    Ok(agent
        .chain(gcs)
        .filter(|p| p.required)
        .map(|p| p.id.clone())
        .collect())
}

/// The message for a plugin that installed but could not be switched on. A
/// re-run of the installer finishes it; the manual path is named too.
fn not_enabled(plugin_id: &str, why: &str) -> String {
    format!(
        "{plugin_id} is installed but not enabled ({why}); re-run the installer to finish it, \
         or enable it from Mission Control's plugin page or with `ados plugin enable {plugin_id}`"
    )
}

/// Wait up to `timeout` for the plugin host to report its loopback guard
/// active. Returns either way: a guard that never comes up is reported by the
/// grant it blocks, with the guard's own reason.
fn wait_for_loopback_guard(state: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !ados_protocol::plugin_loopback_guard::read_state_at(state).active
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Map the install's result onto the step outcome, logging what happened. An
/// unpublished extension warns and succeeds; a real failure degrades.
fn step_outcome(result: Result<ExtensionOutcome, String>) -> StepOutcome {
    match result {
        Ok(ExtensionOutcome::Installed { version, granted }) => {
            tracing::info!(
                plugin = WORLD_ENGINE_ID,
                %version,
                granted = ?granted,
                "World Engine extension installed and enabled"
            );
            StepOutcome::Ok
        }
        Ok(ExtensionOutcome::AlreadyInstalled { version }) => {
            tracing::info!(
                plugin = WORLD_ENGINE_ID,
                %version,
                "World Engine extension already installed; left for plugin auto-update"
            );
            StepOutcome::Skipped
        }
        Ok(ExtensionOutcome::Finished { version, granted }) => {
            tracing::info!(
                plugin = WORLD_ENGINE_ID,
                %version,
                granted = ?granted,
                "World Engine extension finished: required permissions granted and enabled"
            );
            StepOutcome::Ok
        }
        Ok(ExtensionOutcome::Unavailable(reason)) => {
            tracing::warn!(
                plugin = WORLD_ENGINE_ID,
                "World Engine extension not installed: {reason}. Install it later with \
                 `{MANUAL_INSTALL}`"
            );
            StepOutcome::Skipped
        }
        Err(reason) => StepOutcome::Failed(format!(
            "World Engine extension not installed: {reason}. Install it later with \
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
        if !ctx.install_world_engine {
            tracing::info!(profile = %ctx.profile, "World Engine extension not selected");
            return StepOutcome::Skipped;
        }
        let Some(agent_version) = crate::env::installed_agent_version() else {
            return step_outcome(Err(
                "the installed agent version could not be read, so the extension's \
                 compatibility range cannot be checked"
                    .to_string(),
            ));
        };
        ctx.progress.activity(
            self.id(),
            "installing the World Engine extension".to_string(),
        );
        step_outcome(install_world_engine(NodeTarget {
            paths: Paths::from_env(),
            profile: ados_config::normalize_profile(Some(&ctx.profile)),
            agent_version,
            board_sidecar: ados_hal_probe::board_sidecar::sidecar_path(),
            guard_wait: GUARD_WAIT,
            retry_delay: DOWNLOAD_RETRY_DELAY,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;

    use ados_plugin_host::backend::RecordingBackend;
    use ados_plugin_host::download::StaticDownloadSource;
    use sha2::{Digest, Sha256};

    const URL: &str =
        "https://github.com/altnautica/ADOSExtensions/releases/download/x/we.adosplug";

    const MANIFEST: &str = "id: com.altnautica.world-engine\n\
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
                "id": WORLD_ENGINE_ID,
                "version": "1.0.0",
                "download_url": url,
                "archive_sha256": sha,
            }],
        })
        .to_string()
    }

    #[test]
    fn profile_default_is_on_only_for_workstation_class_nodes() {
        assert!(world_engine_default("workstation"));
        assert!(world_engine_default("compute"));
        assert!(!world_engine_default("drone"));
        assert!(!world_engine_default("ground_station"));
    }

    /// The bundled catalog gains its entry only when the extension is released,
    /// so today a workstation install must warn and carry on, not degrade.
    #[test]
    fn an_absent_catalog_entry_warns_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        let outcome = install_from_catalog(
            &mut sup,
            WORLD_ENGINE_ID,
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
        assert!(sup.find_install(WORLD_ENGINE_ID).is_none());
    }

    #[test]
    fn an_entry_with_no_published_release_warns_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        let outcome = install_from_catalog(
            &mut sup,
            WORLD_ENGINE_ID,
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
            WORLD_ENGINE_ID,
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
        let install = sup.find_install(WORLD_ENGINE_ID).expect("installed");
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
            WORLD_ENGINE_ID,
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
            sup.find_install(WORLD_ENGINE_ID).unwrap().status,
            PluginStatus::Installed
        );

        let outcome = install_from_catalog(
            &mut sup,
            WORLD_ENGINE_ID,
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
        let install = sup.find_install(WORLD_ENGINE_ID).unwrap();
        assert!(install.permissions["hardware.spi"].granted);
        assert!(matches!(
            install.status,
            PluginStatus::Enabled | PluginStatus::Running
        ));
        assert_eq!(step_outcome(Ok(outcome)), StepOutcome::Ok);

        sup.disable(WORLD_ENGINE_ID).unwrap();
        let again = install_from_catalog(
            &mut sup,
            WORLD_ENGINE_ID,
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
            sup.find_install(WORLD_ENGINE_ID).unwrap().status,
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
            WORLD_ENGINE_ID,
            &catalog_with(URL, &"00".repeat(32)),
            &source,
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(sup.find_install(WORLD_ENGINE_ID).is_none());
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
            WORLD_ENGINE_ID,
            &catalog_with(URL, ""),
            &source,
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(err.contains("archive_sha256"), "{err}");
        assert!(sup.find_install(WORLD_ENGINE_ID).is_none());
    }

    #[test]
    fn the_step_skips_when_the_toggle_is_off() {
        let mut ctx = Ctx::for_test(crate::checkpoint::Checkpoint::new());
        ctx.install_world_engine = false;
        assert_eq!(Extensions.run(&mut ctx), StepOutcome::Skipped);
    }
}
