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
mod tests;
