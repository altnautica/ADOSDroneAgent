//! Cloud command dispatch.
//!
//! Inbound commands arrive on the command-poll loop and are dispatched by name.
//! Each handler returns `(status, result, data)`: `status` is `completed` or
//! `failed`, `result` is the small ACK doc, `data` is the optional larger
//! payload. Plugin lifecycle commands route to the frozen `PluginSupervisor`
//! through [`plugin_commands`] (idempotent).

pub mod executed_commands;
pub mod install;
pub mod loopback;
pub mod plugin_commands;
pub mod seen_jobs;

use serde::Serialize;

/// A command outcome status. Serializes to the wire strings `completed` /
/// `failed` the ACK expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandStatus {
    Completed,
    Failed,
}

impl CommandStatus {
    /// The wire string the ACK payload carries.
    pub fn as_str(self) -> &'static str {
        match self {
            CommandStatus::Completed => "completed",
            CommandStatus::Failed => "failed",
        }
    }
}

/// The dispatch outcome: the status plus the small `result` ACK doc and the
/// optional larger `data` payload, with `result` carrying `{success,
/// message}`.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandResult {
    pub status: CommandStatus,
    pub result: serde_json::Value,
    pub data: Option<serde_json::Value>,
}

impl CommandResult {
    /// A `completed` result with `{success: true, message}`.
    pub fn completed(message: impl Into<String>) -> Self {
        CommandResult {
            status: CommandStatus::Completed,
            result: serde_json::json!({"success": true, "message": message.into()}),
            data: None,
        }
    }

    /// A `failed` result with `{success: false, message}`.
    pub fn failed(message: impl Into<String>) -> Self {
        CommandResult {
            status: CommandStatus::Failed,
            result: serde_json::json!({"success": false, "message": message.into()}),
            data: None,
        }
    }

    /// Attach the optional larger `data` payload.
    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
}

#[cfg(test)]
mod tests;
