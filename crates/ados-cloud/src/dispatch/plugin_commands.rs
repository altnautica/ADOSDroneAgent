//! Plugin lifecycle command dispatch.
//!
//! Routes the `plugin.{uninstall,enable,disable,configure}` cloud commands to
//! the frozen `PluginSupervisor` consumer contract (enable / disable / remove /
//! grant_permission / revoke_permission), with `_seen_jobs` idempotency.
//! `plugin.configure` is the caller-side grant+revoke batch — there is NO
//! supervisor `configure` method. Ports `RemoteInstallReceiver.dispatch` from
//! `src/ados/plugins/remote_install.py`.
//!
//! `plugin.install` (the signed-URL download path) is handled in
//! [`super::install`].

use std::path::Path;

use ados_plugin_host::{LifecycleError, PluginSupervisor};

use super::seen_jobs;
use super::CommandResult;

/// A parsed plugin lifecycle command. The wire command name plus the args the
/// receiver reads. Built from the cloud command-queue row.
#[derive(Debug, Clone)]
pub struct PluginCommand {
    pub command: String,
    pub plugin_id: String,
    pub job_id: String,
    pub keep_data: bool,
    pub grant_permissions: Vec<String>,
    pub revoke_permissions: Vec<String>,
}

impl PluginCommand {
    /// Parse a command-queue row into a [`PluginCommand`]. The row shape is the
    /// Convex `cmd_droneCommands` document: `{command, args: {pluginId, jobId,
    /// keepData, grantPermissions, revokePermissions}, _id}`. `job_id` falls
    /// back to `_id` then `pluginId`, matching the Python `dispatch`.
    pub fn from_row(row: &serde_json::Value) -> Option<Self> {
        let command = row.get("command")?.as_str()?.to_string();
        let args = row.get("args").cloned().unwrap_or(serde_json::Value::Null);
        let plugin_id = args
            .get("pluginId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let job_id = args
            .get("jobId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| row.get("_id").and_then(|v| v.as_str()).map(str::to_string))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| plugin_id.clone());
        let keep_data = args
            .get("keepData")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let grant_permissions = str_array(&args, "grantPermissions");
        let revoke_permissions = str_array(&args, "revokePermissions");
        Some(PluginCommand {
            command,
            plugin_id,
            job_id,
            keep_data,
            grant_permissions,
            revoke_permissions,
        })
    }
}

fn str_array(args: &serde_json::Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a command name is a plugin lifecycle command this receiver handles.
/// Mirrors `is_plugin_command`.
pub fn is_plugin_command(command: &str) -> bool {
    matches!(
        command,
        "plugin.install"
            | "plugin.uninstall"
            | "plugin.enable"
            | "plugin.disable"
            | "plugin.configure"
    )
}

/// Handle a non-install plugin lifecycle command against the supervisor.
/// Mirrors `RemoteInstallReceiver.dispatch`: validate pluginId, idempotency
/// short-circuit, run the supervisor op, mark seen, return the ACK tuple.
///
/// `plugin.configure` runs the grant+revoke batch (there is no supervisor
/// `configure` method). A `SupervisorError` from any op becomes a `failed` ACK
/// with `code: supervisor_error`.
pub fn dispatch(
    supervisor: &mut PluginSupervisor,
    cmd: &PluginCommand,
    seen_jobs_path: &Path,
) -> CommandResult {
    if cmd.plugin_id.is_empty() {
        return CommandResult::failed("pluginId required");
    }
    if seen_jobs::already_seen(&cmd.job_id, seen_jobs_path) {
        return CommandResult::completed("already_processed").with_data(serde_json::json!({
            "jobId": cmd.job_id,
            "replay": true,
        }));
    }

    let action = match cmd.command.as_str() {
        "plugin.uninstall" => supervisor
            .remove(&cmd.plugin_id, cmd.keep_data)
            .map(|_| "uninstalled"),
        "plugin.enable" => supervisor.enable(&cmd.plugin_id).map(|_| "enabled"),
        "plugin.disable" => supervisor.disable(&cmd.plugin_id).map(|_| "disabled"),
        "plugin.configure" => configure(supervisor, cmd).map(|_| "configured"),
        other => {
            return CommandResult::failed(format!("unknown plugin command {other}"));
        }
    };

    match action {
        Ok(action) => {
            let _ = seen_jobs::mark_seen(&cmd.job_id, seen_jobs_path);
            CommandResult::completed(action).with_data(serde_json::json!({
                "jobId": cmd.job_id,
                "pluginId": cmd.plugin_id,
                "action": action,
            }))
        }
        Err(e) => CommandResult::failed(e.to_string()).with_data(serde_json::json!({
            "code": "supervisor_error",
            "jobId": cmd.job_id,
        })),
    }
}

/// `plugin.configure`: apply the grant batch, then the revoke batch. Mirrors the
/// configure branch of the Python `dispatch` (a grant/revoke loop, NOT a
/// supervisor method). The first error short-circuits.
fn configure(supervisor: &mut PluginSupervisor, cmd: &PluginCommand) -> Result<(), LifecycleError> {
    for perm in &cmd.grant_permissions {
        supervisor.grant_permission(&cmd.plugin_id, perm)?;
    }
    for perm in &cmd.revoke_permissions {
        supervisor.revoke_permission(&cmd.plugin_id, perm)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::CommandStatus;
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

    fn installed_supervisor(dir: &Path) -> PluginSupervisor {
        let mut sup = PluginSupervisor::new(paths_in(dir), false, None, "1.0.0")
            .with_systemctl(Arc::new(RecordingSystemctl::default()));
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
}
