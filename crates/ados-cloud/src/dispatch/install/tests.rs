use super::*;
use ados_plugin_host::backend::RecordingBackend;
use ados_plugin_host::download::StaticDownloadSource;
use ados_plugin_host::supervisor::Paths;
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

fn build_archive() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("manifest.yaml", opts).unwrap();
        w.write_all(MANIFEST.as_bytes()).unwrap();
        w.start_file("agent/py/x.py", opts).unwrap();
        w.write_all(b"print('hi')").unwrap();
        w.finish().unwrap();
    }
    buf
}

/// Serves the test archive at every URL the tests use.
fn fake_source(body: Vec<u8>) -> StaticDownloadSource {
    StaticDownloadSource::default()
        .with("https://abc.convex.cloud/x", body.clone())
        .with("https://evil.example.com/x", body)
}

fn supervisor(dir: &Path) -> PluginSupervisor {
    PluginSupervisor::new(paths_in(dir), false, None, "1.0.0")
        .with_backend(Arc::new(RecordingBackend::default()))
}

/// A signature-enforcing supervisor, matching the live cloud-relay posture
/// (`PluginSupervisor::production` bakes require_signed=true).
fn signed_supervisor(dir: &Path) -> PluginSupervisor {
    let sup = PluginSupervisor::new(paths_in(dir), true, None, "1.0.0")
        .with_backend(Arc::new(RecordingBackend::default()));
    assert!(
        sup.require_signed(),
        "the cloud-install posture must enforce signing"
    );
    sup
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
    let src = fake_source(build_archive());
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
fn cloud_install_rejects_an_unsigned_archive_when_signing_is_required() {
    // The live cloud-relay supervisor enforces signing. An unsigned
    // `.adosplug` arriving over the cloud command queue must be rejected, not
    // installed. This is the regression guard for the moment the cloud
    // install path was wired live: it must carry require_signed=true.
    let dir = tempfile::tempdir().unwrap();
    let mut sup = signed_supervisor(dir.path());
    let seen = dir.path().join("seen.json");
    let src = fake_source(build_archive()); // build_archive() is unsigned
    let r = handle_install(
        &mut sup,
        &install_cmd("j-unsigned", "https://abc.convex.cloud/x"),
        &src,
        &seen,
    );
    assert_eq!(r.status, CommandStatus::Failed);
    let data = r.data.unwrap();
    assert_eq!(data["code"], "supervisor_error");
    let message = r.result["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("unsigned"),
        "the failure must name the missing signature: {message}"
    );
    // The plugin must not be installed.
    assert!(sup.find_install("com.example.thermal").is_none());
    // And the job is NOT marked seen, so a re-send after the plugin is signed
    // can still succeed rather than short-circuiting as already-processed.
    let again = handle_install(
        &mut sup,
        &install_cmd("j-unsigned", "https://abc.convex.cloud/x"),
        &src,
        &seen,
    );
    assert_eq!(again.status, CommandStatus::Failed);
    assert_ne!(
        again.data.unwrap().get("replay"),
        Some(&serde_json::Value::Bool(true)),
        "a rejected unsigned install must not be recorded as a replayable success"
    );
}

#[test]
fn install_rejects_off_allowlist_url_before_download() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = supervisor(dir.path());
    let seen = dir.path().join("seen.json");
    let src = fake_source(build_archive());
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
    let src = fake_source(build_archive());
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
    let src = fake_source(build_archive());
    let r = handle_install(
        &mut sup,
        &install_cmd("", "https://abc.convex.cloud/x"),
        &src,
        &seen,
    );
    assert_eq!(r.status, CommandStatus::Failed);
}

#[test]
fn an_archive_for_another_plugin_is_refused_before_install() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = supervisor(dir.path());
    let seen = dir.path().join("seen.json");
    let src = fake_source(build_archive());
    let mut cmd = install_cmd("j-other", "https://abc.convex.cloud/x");
    cmd.plugin_id = Some("com.example.other".to_string());
    let r = handle_install(&mut sup, &cmd, &src, &seen);
    assert_eq!(r.status, CommandStatus::Failed);
    assert_eq!(r.data.unwrap()["code"], "plugin_mismatch");
    assert!(sup.find_install("com.example.thermal").is_none());
    assert!(sup.find_install("com.example.other").is_none());
}
