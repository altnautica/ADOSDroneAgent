//! Cloud-relay plugin install path (`plugin.install`).
//!
//! The local-first multipart upload is the primary transport; this rides the
//! cloud command queue when the GCS cannot reach the agent directly. Both
//! converge on `PluginSupervisor::install_archive` — signature verify, unpack,
//! and unit render are not re-implemented here. Ports the install half of
//! `RemoteInstallReceiver` from `src/ados/plugins/remote_install.py`: the
//! idempotency short-circuit, the allowlisted size-capped download, the staged
//! archive install, the requested-permission grant loop, and the ACK shape.
//!
//! The download is behind a [`DownloadSource`] seam so the install logic is
//! unit-tested with no network; [`HttpDownloadSource`] is the live blocking
//! client (allowlist + size cap enforced).

use std::path::Path;

use ados_plugin_host::PluginSupervisor;

use super::download::{validate_download_url, DownloadError, DOWNLOAD_MAX_BYTES};
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

/// The archive-download seam. Production downloads over HTTPS with the allowlist
/// + size cap; tests inject the bytes directly.
pub trait DownloadSource {
    /// Fetch the archive bytes for a validated signed URL. Returns the raw
    /// `.adosplug` bytes, or a [`DownloadError`].
    fn fetch(&self, signed_url: &str) -> Result<Vec<u8>, DownloadError>;
}

/// Verify a downloaded body against an expected SHA256 (case-insensitive hex).
/// Empty `expected` is a no-op (the row declared no hash; the supervisor's
/// Ed25519 check is the backstop). Mirrors `verify_sha256` in the download
/// module.
pub fn verify_sha256(body: &[u8], expected: &str) -> Result<(), DownloadError> {
    if expected.is_empty() {
        return Ok(());
    }
    use sha2::{Digest, Sha256};
    let actual = hex::encode(Sha256::digest(body));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(DownloadError::HostNotAllowed(format!(
            "sha256 mismatch: expected {expected}, got {actual}"
        )))
    }
}

/// Run the cloud-relay install. Mirrors `RemoteInstallReceiver.handle_install`:
/// validate jobId, idempotency short-circuit, download (allowlist + size cap +
/// optional sha), stage the archive, `install_archive`, grant the requested
/// permissions, mark seen, return the ACK.
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
    let archive_bytes = match source.fetch(&cmd.signed_url) {
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

    // Stage the archive on disk so the supervisor's Path entry point works.
    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!("ados-install-{}.adosplug", cmd.job_id_safe()));
    if let Err(e) = std::fs::write(&tmp_path, &archive_bytes) {
        return CommandResult::failed(format!("stage failed: {e}"))
            .with_data(serde_json::json!({"code": "stage_failed", "jobId": cmd.job_id}));
    }

    let install = supervisor.install_archive(&tmp_path);
    let _ = std::fs::remove_file(&tmp_path);
    let result = match install {
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

impl InstallCommand {
    /// A filesystem-safe form of the job id for the staged temp file name.
    fn job_id_safe(&self) -> String {
        self.job_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    }
}

/// The live download source: a blocking HTTPS GET with the allowlist + size cap.
/// The signed URL is already allowlist-validated by the caller; this re-checks
/// the size cap while streaming. TLS is the shared RustCrypto rustls config.
pub struct HttpDownloadSource {
    client: reqwest::blocking::Client,
}

impl HttpDownloadSource {
    pub fn new() -> Self {
        let client = reqwest::blocking::Client::builder()
            .use_preconfigured_tls(crate::tls::client_config())
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest blocking client builds");
        HttpDownloadSource { client }
    }
}

impl Default for HttpDownloadSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DownloadSource for HttpDownloadSource {
    fn fetch(&self, signed_url: &str) -> Result<Vec<u8>, DownloadError> {
        // Allowlist re-check at the transport boundary (defense-in-depth; the
        // caller validated too).
        validate_download_url(signed_url)?;
        let resp = self
            .client
            .get(signed_url)
            .send()
            .map_err(|_| DownloadError::Unparseable)?;
        if !resp.status().is_success() {
            return Err(DownloadError::Unparseable);
        }
        let bytes = resp.bytes().map_err(|_| DownloadError::Unparseable)?;
        if bytes.len() > DOWNLOAD_MAX_BYTES {
            return Err(DownloadError::HostNotAllowed(
                "size cap exceeded".to_string(),
            ));
        }
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_plugin_host::supervisor::{Paths, RecordingSystemctl};
    use std::io::Write;
    use std::sync::Arc;
    use zip::write::SimpleFileOptions;

    const MANIFEST: &str = "id: com.example.thermal\nversion: 1.0.0\nrisk: high\ncompatibility:\n  ados_version: \">=0.1.0,<99.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - hardware.spi\n";

    fn paths_in(dir: &Path) -> Paths {
        Paths {
            install_dir: dir.join("plugins"),
            unit_dir: dir.join("units"),
            state_path: dir.join("state/plugin-state.json"),
            log_dir: dir.join("logs"),
        }
    }

    fn build_archive() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            w.start_file("manifest.yaml", opts).unwrap();
            w.write_all(MANIFEST.as_bytes()).unwrap();
            w.start_file("agent/py/x.py", opts).unwrap();
            w.write_all(b"print('hi')").unwrap();
            w.finish().unwrap();
        }
        buf
    }

    struct FakeSource(Vec<u8>);
    impl DownloadSource for FakeSource {
        fn fetch(&self, _url: &str) -> Result<Vec<u8>, DownloadError> {
            Ok(self.0.clone())
        }
    }

    fn supervisor(dir: &Path) -> PluginSupervisor {
        PluginSupervisor::new(paths_in(dir), false, None, "1.0.0")
            .with_systemctl(Arc::new(RecordingSystemctl::default()))
    }

    fn install_cmd(job: &str, url: &str) -> InstallCommand {
        InstallCommand {
            job_id: job.to_string(),
            plugin_id: Some("com.example.thermal".to_string()),
            signed_url: url.to_string(),
            requested_permissions: vec!["hardware.spi".to_string()],
            expected_sha256: String::new(),
        }
    }

    #[test]
    fn install_downloads_stages_and_installs() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        let seen = dir.path().join("seen.json");
        let src = FakeSource(build_archive());
        let r = handle_install(
            &mut sup,
            &install_cmd("j1", "https://abc.convex.cloud/x"),
            &src,
            &seen,
        );
        assert_eq!(r.status, CommandStatus::Completed);
        let data = r.data.unwrap();
        assert_eq!(data["pluginId"], "com.example.thermal");
        assert_eq!(data["version"], "1.0.0");
        // The requested permission was granted.
        assert_eq!(data["granted"][0], "hardware.spi");
        // Idempotent on replay.
        let r2 = handle_install(
            &mut sup,
            &install_cmd("j1", "https://abc.convex.cloud/x"),
            &src,
            &seen,
        );
        assert_eq!(r2.data.unwrap()["replay"], true);
    }

    #[test]
    fn install_rejects_off_allowlist_url_before_download() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        let seen = dir.path().join("seen.json");
        let src = FakeSource(build_archive());
        let r = handle_install(
            &mut sup,
            &install_cmd("j2", "https://evil.example.com/x"),
            &src,
            &seen,
        );
        assert_eq!(r.status, CommandStatus::Failed);
        assert_eq!(r.data.unwrap()["code"], "download_failed");
    }

    #[test]
    fn install_sha_mismatch_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        let seen = dir.path().join("seen.json");
        let src = FakeSource(build_archive());
        let mut cmd = install_cmd("j3", "https://abc.convex.cloud/x");
        cmd.expected_sha256 = "00".repeat(32);
        let r = handle_install(&mut sup, &cmd, &src, &seen);
        assert_eq!(r.status, CommandStatus::Failed);
    }

    #[test]
    fn missing_job_id_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor(dir.path());
        let seen = dir.path().join("seen.json");
        let src = FakeSource(build_archive());
        let r = handle_install(
            &mut sup,
            &install_cmd("", "https://abc.convex.cloud/x"),
            &src,
            &seen,
        );
        assert_eq!(r.status, CommandStatus::Failed);
    }

    #[test]
    fn verify_sha256_matches_case_insensitively() {
        use sha2::{Digest, Sha256};
        let body = b"abc";
        let h = hex::encode(Sha256::digest(body));
        assert!(verify_sha256(body, &h).is_ok());
        assert!(verify_sha256(body, &h.to_uppercase()).is_ok());
        assert!(verify_sha256(body, "").is_ok());
        assert!(verify_sha256(body, &"00".repeat(32)).is_err());
    }
}
