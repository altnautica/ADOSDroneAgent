//! Service readiness and installed-tree integrity verification.

use super::*;

impl PluginSupervisor {
    /// Probe each declared service that runs here: `[{name, ready, reason}]`,
    /// or `None` when there are none (the heartbeat then omits the block).
    ///
    /// No `ready_check`: ready iff the unit is active. An HTTP check: ready on
    /// a 2xx GET. A command: ready on exit 0, run by the service backend with
    /// the plugin's sandbox. A probe error is `ready: false` with the reason.
    pub(super) fn readiness_of(
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

/// The failure reason recorded on an install whose files no longer match what
/// was installed.
pub const TAMPERED_REASON: &str = "tampered";

/// Check an installed plugin's files against its install record: the manifest
/// bytes against the recorded `manifest_hash`, and every file the install
/// attestation lists against its digest. A missing attested file is a
/// failure. An install without an attestation (recorded before attestations
/// existed) is checked on its manifest hash alone.
pub fn verify_install(install_dir: &Path, install: &PluginInstall) -> Result<(), String> {
    let dir = plugin_dir_under(install_dir, &install.plugin_id).map_err(|e| e.0)?;
    let manifest = std::fs::read(dir.join(MANIFEST_FILENAME))
        .map_err(|e| format!("manifest unreadable: {e}"))?;
    if !install.manifest_hash.is_empty()
        && hex::encode(Sha256::digest(&manifest)) != install.manifest_hash
    {
        return Err("manifest hash mismatch".to_string());
    }
    if !Attestation::path_in(&dir).exists() {
        return Ok(());
    }
    let attestation = Attestation::read_from(&dir).map_err(|e| e.0)?;
    for file in &attestation.files {
        let rel = Path::new(&file.path);
        if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            return Err(format!(
                "attested path {:?} is not under the install",
                file.path
            ));
        }
        let bytes =
            std::fs::read(dir.join(rel)).map_err(|e| format!("{} unreadable: {e}", file.path))?;
        if !hex::encode(Sha256::digest(&bytes)).eq_ignore_ascii_case(&file.sha256) {
            return Err(format!("{} digest mismatch", file.path));
        }
    }
    Ok(())
}

/// The stat stamp of every file [`verify_install`] reads.
type InstallStamp = Vec<Option<(std::time::SystemTime, u64)>>;

/// A cached verdict: the manifest hash and stamp it was computed against.
type CachedVerdict = (String, InstallStamp, Result<(), String>);

/// [`verify_install`] cached by the mtime and size of every file it reads, so
/// a poll that runs every two seconds hashes a plugin's files only after one
/// of them changed.
#[derive(Default)]
pub struct InstallVerifier {
    cache: std::sync::Mutex<BTreeMap<String, CachedVerdict>>,
}

impl InstallVerifier {
    pub fn verify(&self, install_dir: &Path, install: &PluginInstall) -> Result<(), String> {
        let stamp = install_stamp(install_dir, &install.plugin_id);
        if let Ok(cache) = self.cache.lock() {
            if let Some((hash, seen, result)) = cache.get(&install.plugin_id) {
                if *hash == install.manifest_hash && *seen == stamp {
                    return result.clone();
                }
            }
        }
        let result = verify_install(install_dir, install);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                install.plugin_id.clone(),
                (install.manifest_hash.clone(), stamp, result.clone()),
            );
        }
        result
    }

    /// Drop cached verdicts for plugins outside `keep`.
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.retain(|id, _| keep(id));
        }
    }
}

fn install_stamp(install_dir: &Path, plugin_id: &str) -> InstallStamp {
    let stat = |p: &Path| {
        std::fs::metadata(p)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())))
    };
    let Ok(dir) = plugin_dir_under(install_dir, plugin_id) else {
        return Vec::new();
    };
    let mut stamp = vec![
        stat(&dir.join(MANIFEST_FILENAME)),
        stat(&Attestation::path_in(&dir)),
    ];
    if let Ok(attestation) = Attestation::read_from(&dir) {
        stamp.extend(attestation.files.iter().map(|f| stat(&dir.join(&f.path))));
    }
    stamp
}
