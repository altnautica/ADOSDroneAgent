//! Daily auto-update check for installed plugins.
//!
//! Once a day every enabled install is compared with the newest version the
//! plugin registry publishes, and one of four things happens:
//!
//! * **silent install**: a patch or minor bump with an unchanged permission set
//!   on a still-supported board, for a plugin that is not pinned and has
//!   auto-update on. It goes through the supervisor's signed-archive install,
//!   so the signature, compatibility and downgrade gates all apply; the prior
//!   grants are restored and the plugin is re-enabled. A refused archive rolls
//!   back to the still-installed old version.
//! * **notify**: a major bump, a permission change or a board the new version
//!   drops. The operator gets an `update_available` notice and decides.
//! * **skipped**: pinned, auto-update off, nothing newer, or no registry row.
//! * **failed**: registry, download, install or enable error, recorded on the
//!   install so the GCS shows it. The next daily check retries.
//!
//! The engine is transport-free: the registry query, the archive download and
//! the notice go through [`UpdateSource`], which the service holding the
//! pairing credentials implements.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::watch;

use crate::manifest::PluginManifest;
use crate::state::{self, now_ms, PluginInstall, PluginStatus};
use crate::supervisor::PluginSupervisor;

/// The fixed interval between checks. A failed check waits for the next one;
/// there is no attempt cap.
pub const DAILY_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Delay before the first check, so it does not race the rest of boot.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);

/// The newest published version of a plugin, as the registry reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRow {
    pub version: String,
    pub manifest_yaml: String,
    pub supported_boards: Vec<String>,
    pub download_url: String,
    pub archive_sha256: String,
}

/// The transport the engine runs over. Blocking: the engine runs on the
/// blocking pool alongside the supervisor's own filesystem and `systemctl`
/// work.
pub trait UpdateSource: Send + Sync {
    /// Whether the registry can be queried at all (paired, a registry URL
    /// known). `false` skips the whole cycle.
    fn ready(&self) -> bool;
    /// The newest version row for `plugin_id`, `Ok(None)` when the registry
    /// has no row. Implementations fetch the registry payload and hand it to
    /// [`latest_version_row`].
    fn latest(&self, plugin_id: &str) -> Result<Option<VersionRow>, String>;
    /// Download `url` (allowlist + size cap) and verify it against `sha256`.
    fn download(&self, url: &str, sha256: &str) -> Result<Vec<u8>, String>;
    /// Deliver an `update_available` notice to the operator.
    fn notify(&self, notice: &Value);
}

/// What one plugin's check did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    SilentInstall,
    Notify,
    Skipped,
    Failed,
}

/// The newest version row out of a registry `{plugin, versions}` payload
/// (versions are ordered newest first). An `error` body is an error.
pub fn latest_version_row(payload: &Value) -> Result<Option<VersionRow>, String> {
    if payload.get("plugin").is_none() {
        if let Some(err) = payload.get("error") {
            return Err(format!("registry error: {err}"));
        }
    }
    let Some(row) = payload
        .get("versions")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
    else {
        return Ok(None);
    };
    let text = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Some(VersionRow {
        version: text("version"),
        manifest_yaml: text("manifest_yaml"),
        supported_boards: row
            .get("supported_boards")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|b| b.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        download_url: text("download_url"),
        archive_sha256: text("archive_sha256"),
    }))
}

fn semver(v: &str) -> Option<(u64, u64, u64)> {
    let base = v.split(['-', '+']).next().unwrap_or(v);
    let mut parts = base.split('.').map(|p| p.parse::<u64>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

fn notice(install: &PluginInstall, latest: &str, reason: &str) -> Value {
    json!({
        "plugin_id": install.plugin_id,
        "current_version": install.version,
        "latest_version": latest,
        "reason": reason,
        "timestamp_ms": now_ms(),
    })
}

/// Check one install against the registry and act. Never fails: every error
/// becomes a recorded `Failed` outcome.
pub fn check_one(
    supervisor: &mut PluginSupervisor,
    install: &PluginInstall,
    source: &dyn UpdateSource,
    board: Option<&str>,
) -> Outcome {
    let id = install.plugin_id.as_str();
    if install.pinned_version.is_some() || !install.auto_update {
        return Outcome::Skipped;
    }
    let record = |sup: &mut PluginSupervisor, version: &str, outcome: &str, error: Option<&str>| {
        if let Err(e) = sup.note_update_attempt(id, version, outcome, error) {
            tracing::warn!(plugin_id = id, error = %e, "auto_update_record_failed");
        }
    };
    let row = match source.latest(id) {
        Ok(Some(row)) => row,
        Ok(None) => return Outcome::Skipped,
        Err(e) => {
            tracing::warn!(plugin_id = id, error = %e, "auto_update_registry_query_failed");
            record(supervisor, "unknown", "failed", Some(&e));
            return Outcome::Failed;
        }
    };
    let (Some(latest), Some(current)) = (semver(&row.version), semver(&install.version)) else {
        return Outcome::Skipped;
    };
    if latest <= current {
        return Outcome::Skipped;
    }
    let Ok(remote) = PluginManifest::from_yaml_text(&row.manifest_yaml) else {
        tracing::warn!(plugin_id = id, "auto_update_remote_manifest_unparseable");
        return Outcome::Skipped;
    };

    let notify = |sup: &mut PluginSupervisor, n: Value| {
        source.notify(&n);
        record(sup, &row.version, "notify", None);
        tracing::info!(plugin_id = id, notice = %n, "auto_update_notify");
        Outcome::Notify
    };
    if latest.0 > current.0 {
        return notify(supervisor, notice(install, &row.version, "major_bump"));
    }
    if let Some(board) = board {
        if !row.supported_boards.is_empty() && !row.supported_boards.iter().any(|b| b == board) {
            return notify(supervisor, notice(install, &row.version, "board_mismatch"));
        }
    }
    // Any change to the declared permission set, added or removed, is the
    // operator's to approve.
    let current_declared = supervisor
        .installed_manifest(id)
        .map(|m| m.declared_permissions())
        .unwrap_or_default();
    let remote_declared = remote.declared_permissions();
    if remote_declared != current_declared {
        let mut n = notice(install, &row.version, "permission_delta");
        n["new_permissions"] = json!(remote_declared
            .difference(&current_declared)
            .collect::<Vec<_>>());
        n["removed_permissions"] = json!(current_declared
            .difference(&remote_declared)
            .collect::<Vec<_>>());
        return notify(supervisor, n);
    }

    match silent_install(supervisor, install, source, &row) {
        Ok(()) => {
            record(supervisor, &row.version, "success", None);
            tracing::info!(plugin_id = id, from = %install.version, to = %row.version, "auto_update_installed");
            Outcome::SilentInstall
        }
        Err(e) => {
            tracing::warn!(plugin_id = id, error = %e, "auto_update_install_failed");
            record(supervisor, &row.version, "failed", Some(&e));
            Outcome::Failed
        }
    }
}

/// Download, stop, install over, restore grants, re-enable. A refused archive
/// leaves the old version installed, so the rollback is a re-enable.
fn silent_install(
    supervisor: &mut PluginSupervisor,
    install: &PluginInstall,
    source: &dyn UpdateSource,
    row: &VersionRow,
) -> Result<(), String> {
    let id = install.plugin_id.as_str();
    if row.download_url.is_empty() {
        return Err("no download_url on registry row".to_string());
    }
    let bytes = source
        .download(&row.download_url, &row.archive_sha256)
        .map_err(|e| format!("download: {e}"))?;
    let granted: BTreeSet<String> = state::granted_caps(install);

    // Unique per attempt, so two checks can never write or delete each other's
    // staged archive.
    static STAGE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = STAGE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staged = std::env::temp_dir().join(format!(
        "ados-update-{}-{}-{seq}.adosplug",
        sanitize(id),
        std::process::id()
    ));
    std::fs::write(&staged, &bytes).map_err(|e| format!("stage: {e}"))?;
    let result = (|| {
        supervisor
            .disable(id)
            .map_err(|e| format!("disable: {e}"))?;
        if let Err(e) = supervisor.install_archive(&staged) {
            if let Err(re) = supervisor.enable(id) {
                tracing::warn!(plugin_id = id, error = %re, "auto_update_rollback_enable_failed");
            }
            return Err(format!("install: {e}"));
        }
        for perm in &granted {
            if let Err(e) = supervisor.grant_permission(id, perm) {
                tracing::warn!(plugin_id = id, permission = %perm, error = %e, "auto_update_regrant_failed");
            }
        }
        supervisor.enable(id).map_err(|e| format!("enable: {e}"))
    })();
    let _ = std::fs::remove_file(&staged);
    result
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// One pass over every enabled install. Every checked install gets its
/// check time stamped, whatever the outcome.
pub fn run_cycle(
    supervisor: &mut PluginSupervisor,
    source: &dyn UpdateSource,
    board: Option<&str>,
) -> Vec<(String, Outcome)> {
    if let Err(e) = supervisor.refresh() {
        tracing::warn!(error = %e, "auto_update_state_unreadable");
        return Vec::new();
    }
    let installs: Vec<PluginInstall> = supervisor
        .installs()
        .iter()
        .filter(|i| matches!(i.status, PluginStatus::Enabled | PluginStatus::Running))
        .cloned()
        .collect();
    installs
        .iter()
        .map(|install| {
            let outcome = check_one(supervisor, install, source, board);
            if let Err(e) = supervisor.note_update_check(&install.plugin_id, now_ms()) {
                tracing::warn!(plugin_id = %install.plugin_id, error = %e, "auto_update_check_stamp_failed");
            }
            (install.plugin_id.clone(), outcome)
        })
        .collect()
}

/// Run the check once after [`FIRST_CHECK_DELAY`] and then every
/// [`DAILY_INTERVAL`] until `shutdown` turns true. A cycle whose source is not
/// ready (unpaired) is skipped, not retried early.
pub async fn run_daily_loop(
    supervisor: Arc<Mutex<PluginSupervisor>>,
    source: Arc<dyn UpdateSource>,
    board: Option<String>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut wait = FIRST_CHECK_DELAY;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
                continue;
            }
        }
        wait = DAILY_INTERVAL;
        if !source.ready() {
            tracing::debug!("auto_update_skip_unpaired");
            continue;
        }
        let (sup, src, board) = (Arc::clone(&supervisor), Arc::clone(&source), board.clone());
        let joined = tokio::task::spawn_blocking(move || {
            let mut guard = sup.lock().unwrap_or_else(|p| p.into_inner());
            run_cycle(&mut guard, src.as_ref(), board.as_deref())
        })
        .await;
        match joined {
            Ok(results) => tracing::info!(checked = results.len(), "auto_update_cycle_done"),
            Err(e) => tracing::warn!(error = %e, "auto_update_cycle_crashed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::RecordingBackend;
    use crate::supervisor::Paths;
    use std::io::Write;
    use std::path::Path;

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

    fn manifest(version: &str, perms: &[&str]) -> String {
        let mut m = format!(
            "id: com.example.thermal\nversion: {version}\nrisk: high\ncompatibility:\n  \
             ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n"
        );
        for p in perms {
            m.push_str(&format!("    - {p}\n"));
        }
        m
    }

    fn archive(manifest_yaml: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("manifest.yaml", opts).unwrap();
            w.write_all(manifest_yaml.as_bytes()).unwrap();
            w.start_file("agent/py/x.py", opts).unwrap();
            w.write_all(b"print('hi')").unwrap();
            w.finish().unwrap();
        }
        buf
    }

    struct FakeSource {
        row: VersionRow,
        archive: Vec<u8>,
        notices: Mutex<Vec<Value>>,
        queried: Mutex<u32>,
    }

    impl UpdateSource for FakeSource {
        fn ready(&self) -> bool {
            true
        }
        fn latest(&self, _: &str) -> Result<Option<VersionRow>, String> {
            *self.queried.lock().unwrap() += 1;
            Ok(Some(self.row.clone()))
        }
        fn download(&self, _: &str, _: &str) -> Result<Vec<u8>, String> {
            Ok(self.archive.clone())
        }
        fn notify(&self, notice: &Value) {
            self.notices.lock().unwrap().push(notice.clone());
        }
    }

    fn source(version: &str, perms: &[&str]) -> FakeSource {
        let yaml = manifest(version, perms);
        FakeSource {
            row: VersionRow {
                version: version.to_string(),
                manifest_yaml: yaml.clone(),
                supported_boards: Vec::new(),
                download_url: "https://example.com/p.adosplug".to_string(),
                archive_sha256: String::new(),
            },
            archive: archive(&yaml),
            notices: Mutex::new(Vec::new()),
            queried: Mutex::new(0),
        }
    }

    fn running_v1(dir: &Path) -> PluginSupervisor {
        let mut sup = PluginSupervisor::new(paths_in(dir), false, None, "1.0.0")
            .with_backend(Arc::new(RecordingBackend::default()));
        let contents =
            crate::archive::parse_archive_bytes(archive(&manifest("1.0.0", &["hardware.spi"])))
                .unwrap();
        sup.install_contents(contents, Path::new("/tmp/v1.adosplug"))
            .unwrap();
        sup.grant_permission("com.example.thermal", "hardware.spi")
            .unwrap();
        sup.enable("com.example.thermal").unwrap();
        sup
    }

    #[test]
    fn a_minor_bump_with_the_same_permissions_installs_and_keeps_the_grants() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = running_v1(dir.path());
        let src = source("1.1.0", &["hardware.spi"]);
        let results = run_cycle(&mut sup, &src, None);
        assert_eq!(
            results,
            vec![("com.example.thermal".to_string(), Outcome::SilentInstall)]
        );
        let rec = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.version, "1.1.0");
        assert_eq!(rec.status, PluginStatus::Running);
        assert!(state::granted_caps(rec).contains("hardware.spi"));
        assert_eq!(
            rec.last_update_attempt.as_ref().unwrap()["outcome"],
            "success"
        );
        assert!(rec.last_update_check_at.is_some());
    }

    #[test]
    fn a_major_bump_or_a_permission_change_only_notifies() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = running_v1(dir.path());
        let major = source("2.0.0", &["hardware.spi"]);
        assert_eq!(run_cycle(&mut sup, &major, None)[0].1, Outcome::Notify);
        assert_eq!(major.notices.lock().unwrap()[0]["reason"], "major_bump");

        let widened = source("1.2.0", &["hardware.spi", "mavlink.write"]);
        assert_eq!(run_cycle(&mut sup, &widened, None)[0].1, Outcome::Notify);
        let n = &widened.notices.lock().unwrap()[0];
        assert_eq!(n["reason"], "permission_delta");
        assert_eq!(n["new_permissions"], json!(["mavlink.write"]));
        assert_eq!(
            sup.find_install("com.example.thermal").unwrap().version,
            "1.0.0"
        );
    }

    #[test]
    fn a_pinned_plugin_is_never_queried() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = running_v1(dir.path());
        let mut install = sup.find_install("com.example.thermal").unwrap().clone();
        install.pinned_version = Some("1.0.0".to_string());
        let src = source("1.1.0", &["hardware.spi"]);
        assert_eq!(check_one(&mut sup, &install, &src, None), Outcome::Skipped);
        assert_eq!(*src.queried.lock().unwrap(), 0);
    }

    #[test]
    fn a_refused_archive_rolls_back_to_the_running_old_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = running_v1(dir.path());
        let mut src = source("1.1.0", &["hardware.spi"]);
        src.archive = b"not a zip".to_vec();
        assert_eq!(run_cycle(&mut sup, &src, None)[0].1, Outcome::Failed);
        let rec = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.version, "1.0.0");
        assert_eq!(rec.status, PluginStatus::Running);
        assert_eq!(
            rec.last_update_attempt.as_ref().unwrap()["outcome"],
            "failed"
        );
    }

    #[test]
    fn the_registry_payload_yields_its_newest_row() {
        let payload = json!({"plugin": {}, "versions": [
            {"version": "1.2.0", "manifest_yaml": "x", "supported_boards": ["rpi4"],
             "download_url": "https://example.com/a", "archive_sha256": "ab"},
            {"version": "1.1.0"}]});
        let row = latest_version_row(&payload).unwrap().unwrap();
        assert_eq!(row.version, "1.2.0");
        assert_eq!(row.supported_boards, vec!["rpi4".to_string()]);
        assert!(latest_version_row(&json!({"error": "nope"})).is_err());
        assert_eq!(
            latest_version_row(&json!({"plugin": {}, "versions": []})).unwrap(),
            None
        );
    }
}
