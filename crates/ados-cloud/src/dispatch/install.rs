//! Cloud-relay plugin install path (`plugin.install`).
//!
//! The local-first multipart upload is the primary transport; this rides the
//! cloud command queue when the GCS cannot reach the agent directly. Both
//! converge on `PluginSupervisor::install_contents` — signature verify, unpack,
//! and unit render are not re-implemented here. Ports the install half of
//! `RemoteInstallReceiver` from `src/ados/plugins/remote_install.py`: the
//! idempotency short-circuit, the allowlisted size-capped download, the archive
//! install, the requested-permission grant loop, and the ACK shape. When the
//! command names a `pluginId`, an archive declaring any other id is refused
//! before anything is installed, so the operator's approved permissions only
//! ever reach the plugin they were approved for.
//!
//! The download is behind the plugin host's [`DownloadSource`] seam so the
//! install logic is unit-tested with no network; its `HttpDownloadSource` is
//! the live blocking client (allowlist + size cap enforced).

use std::path::Path;

use ados_plugin_host::archive::{parse_archive_bytes, ARCHIVE_MAX_BYTES};
use ados_plugin_host::download::{
    fetch_capped, validate_download_url, verify_sha256, DownloadSource,
};
use ados_plugin_host::PluginSupervisor;

use super::seen_jobs;
use super::{CommandResult, CommandStatus};

/// A parsed `plugin.install` command. The args the install path reads.
#[derive(Debug, Clone)]
pub struct InstallCommand {
    pub job_id: String,
    pub plugin_id: Option<String>,
    pub signed_url: String,
    pub requested_permissions: Vec<String>,
    /// Optional defense-in-depth manifest hash (verified against the download
    /// before the supervisor's own Ed25519 check). Empty when the row omits it.
    pub expected_sha256: String,
}

impl InstallCommand {
    /// Parse a `plugin.install` command-queue row.
    pub fn from_row(row: &serde_json::Value) -> Self {
        let args = row.get("args").cloned().unwrap_or(serde_json::Value::Null);
        let job_id = args
            .get("jobId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| row.get("_id").and_then(|v| v.as_str()).map(str::to_string))
            .unwrap_or_default();
        let plugin_id = args
            .get("pluginId")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let signed_url = args
            .get("signedUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let requested_permissions = args
            .get("requestedPermissions")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let expected_sha256 = args
            .get("manifestHash")
            .or_else(|| args.get("archiveSha256"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        InstallCommand {
            job_id,
            plugin_id,
            signed_url,
            requested_permissions,
            expected_sha256,
        }
    }
}

/// Run the cloud-relay install. Mirrors `RemoteInstallReceiver.handle_install`:
/// validate jobId, idempotency short-circuit, download (allowlist + size cap +
/// optional sha), parse the archive and check it is the commanded plugin,
/// install, grant the requested permissions, mark seen, return the ACK.
pub fn handle_install(
    supervisor: &mut PluginSupervisor,
    cmd: &InstallCommand,
    source: &dyn DownloadSource,
    seen_jobs_path: &Path,
) -> CommandResult {
    if cmd.job_id.is_empty() {
        return CommandResult::failed("jobId required");
    }
    if seen_jobs::already_seen(&cmd.job_id, seen_jobs_path) {
        return CommandResult::completed("already_processed").with_data(serde_json::json!({
            "jobId": cmd.job_id,
            "replay": true,
        }));
    }

    // Validate + download.
    if let Err(e) = validate_download_url(&cmd.signed_url) {
        return CommandResult::failed(format!("download failed: {e}"))
            .with_data(serde_json::json!({"code": "download_failed", "jobId": cmd.job_id}));
    }
    let archive_bytes = match fetch_capped(source, &cmd.signed_url, ARCHIVE_MAX_BYTES) {
        Ok(b) => b,
        Err(e) => {
            return CommandResult::failed(format!("download failed: {e}"))
                .with_data(serde_json::json!({"code": "download_failed", "jobId": cmd.job_id}));
        }
    };
    if let Err(e) = verify_sha256(&archive_bytes, &cmd.expected_sha256) {
        return CommandResult::failed(format!("download failed: {e}"))
            .with_data(serde_json::json!({"code": "download_failed", "jobId": cmd.job_id}));
    }

    let contents = match parse_archive_bytes(archive_bytes) {
        Ok(c) => c,
        Err(e) => {
            return CommandResult::failed(format!("install: {e}"))
                .with_data(serde_json::json!({"code": "supervisor_error", "jobId": cmd.job_id}));
        }
    };
    if let Some(expected) = cmd.plugin_id.as_deref() {
        if contents.manifest.id != expected {
            tracing::warn!(
                job_id = %cmd.job_id,
                commanded = %expected,
                archive = %contents.manifest.id,
                "remote install refused: archive is a different plugin"
            );
            return CommandResult::failed(format!(
                "install: archive is plugin {} but the command is for {expected}",
                contents.manifest.id
            ))
            .with_data(serde_json::json!({"code": "plugin_mismatch", "jobId": cmd.job_id}));
        }
    }

    let source_uri = format!("cloud:{}", cmd.job_id);
    let result = match supervisor.install_contents(contents, Path::new(&source_uri)) {
        Ok(r) => r,
        Err(e) => {
            return CommandResult::failed(format!("install: {e}"))
                .with_data(serde_json::json!({"code": "supervisor_error", "jobId": cmd.job_id}));
        }
    };

    // Apply requested permission grants. The supervisor filters against the
    // manifest; a grant error is logged-and-skipped (the install succeeded).
    let mut granted: Vec<String> = Vec::new();
    for perm in &cmd.requested_permissions {
        match supervisor.grant_permission(&result.plugin_id, perm) {
            Ok(()) => granted.push(perm.clone()),
            Err(e) => tracing::warn!(
                job_id = %cmd.job_id,
                permission = %perm,
                error = %e,
                "remote install grant skipped"
            ),
        }
    }

    let _ = seen_jobs::mark_seen(&cmd.job_id, seen_jobs_path);
    let manifest_hash = supervisor
        .find_install(&result.plugin_id)
        .map(|i| i.manifest_hash.clone())
        .unwrap_or_default();

    CommandResult {
        status: CommandStatus::Completed,
        result: serde_json::json!({"success": true, "message": "installed"}),
        data: Some(serde_json::json!({
            "installId": cmd.job_id,
            "pluginId": result.plugin_id,
            "version": result.version,
            "signerId": result.signer_id,
            "manifestHash": manifest_hash,
            "granted": granted,
        })),
    }
}

#[cfg(test)]
mod tests;
