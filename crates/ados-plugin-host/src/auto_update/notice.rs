//! Operator notices raised by the update cycle.

use super::*;

pub(super) fn notice(install: &PluginInstall, latest: &str, reason: &str) -> Value {
    json!({
        "plugin_id": install.plugin_id,
        "current_version": install.version,
        "latest_version": latest,
        "reason": reason,
        "timestamp_ms": now_ms(),
    })
}

/// The notice for an update whose signer is not the installed version's.
pub(super) fn signer_change_notice(
    install: &PluginInstall,
    latest: &str,
    offered: Option<&str>,
) -> Value {
    let mut n = notice(install, latest, SIGNER_CHANGE_REASON);
    n["current_signer"] = json!(install.signer_id);
    n["offered_signer"] = json!(offered);
    n
}

impl CodeSurface {
    pub(super) fn of(manifest: &PluginManifest) -> Self {
        let agent = manifest.agent.as_ref();
        CodeSurface {
            risk: manifest.risk.clone(),
            services: agent
                .and_then(|a| a.extra.get("contributes"))
                .and_then(|c| c.get("services"))
                .cloned(),
            spawn: agent
                .map(|a| a.subprocess_spawn.clone())
                .unwrap_or_default(),
            binaries: agent.map(|a| a.binaries.clone()).unwrap_or_default(),
            payloads: agent
                .map(|a| a.payloads.iter().map(|p| p.path.clone()).collect())
                .unwrap_or_default(),
        }
    }
}
