use super::*;
use crate::dispatch::CommandStatus;
use ados_plugin_host::backend::RecordingBackend;
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

fn installed_supervisor(dir: &Path) -> PluginSupervisor {
    let mut sup = PluginSupervisor::new(paths_in(dir), false, None, "1.0.0")
        .with_backend(Arc::new(RecordingBackend::default()));
    let contents = ados_plugin_host::archive::parse_archive_bytes(build_archive()).unwrap();
    sup.install_contents(contents, Path::new("/tmp/x.adosplug"))
        .unwrap();
    sup
}

fn cmd(command: &str, plugin_id: &str, job_id: &str) -> PluginCommand {
    PluginCommand {
        command: command.to_string(),
        plugin_id: plugin_id.to_string(),
        job_id: job_id.to_string(),
        keep_data: false,
        grant_permissions: vec![],
        revoke_permissions: vec![],
    }
}

#[test]
fn from_row_parses_and_falls_back_job_id() {
    let row = serde_json::json!({
        "command": "plugin.enable",
        "_id": "row-1",
        "args": {"pluginId": "com.example.thermal"}
    });
    let c = PluginCommand::from_row(&row).unwrap();
    assert_eq!(c.command, "plugin.enable");
    assert_eq!(c.plugin_id, "com.example.thermal");
    // job_id falls back to _id when args.jobId is absent.
    assert_eq!(c.job_id, "row-1");
}

#[test]
fn enable_then_disable_drives_supervisor_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = installed_supervisor(dir.path());
    let seen = dir.path().join("seen.json");

    let r = dispatch(
        &mut sup,
        &cmd("plugin.enable", "com.example.thermal", "j1"),
        &seen,
    );
    assert_eq!(r.status, CommandStatus::Completed);
    // Replaying the same job is a no-op marked replay.
    let r2 = dispatch(
        &mut sup,
        &cmd("plugin.enable", "com.example.thermal", "j1"),
        &seen,
    );
    assert_eq!(r2.status, CommandStatus::Completed);
    assert_eq!(r2.data.as_ref().unwrap()["replay"], true);

    let r3 = dispatch(
        &mut sup,
        &cmd("plugin.disable", "com.example.thermal", "j2"),
        &seen,
    );
    assert_eq!(r3.status, CommandStatus::Completed);
}

#[test]
fn configure_runs_grant_then_revoke() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = installed_supervisor(dir.path());
    let seen = dir.path().join("seen.json");
    let mut c = cmd("plugin.configure", "com.example.thermal", "j-cfg");
    c.grant_permissions = vec!["hardware.spi".to_string()];
    let r = dispatch(&mut sup, &c, &seen);
    assert_eq!(r.status, CommandStatus::Completed);
    assert_eq!(r.data.as_ref().unwrap()["action"], "configured");
}

#[test]
fn missing_plugin_id_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = installed_supervisor(dir.path());
    let seen = dir.path().join("seen.json");
    let r = dispatch(&mut sup, &cmd("plugin.enable", "", "j"), &seen);
    assert_eq!(r.status, CommandStatus::Failed);
}

#[test]
fn is_plugin_command_matches_the_five() {
    for c in [
        "plugin.install",
        "plugin.uninstall",
        "plugin.enable",
        "plugin.disable",
        "plugin.configure",
    ] {
        assert!(is_plugin_command(c));
    }
    assert!(!is_plugin_command("get_services"));
}
