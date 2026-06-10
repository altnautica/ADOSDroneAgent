//! Cloud command dispatch.
//!
//! Inbound commands arrive on the command-poll loop and are dispatched by name.
//! Each handler returns `(status, result, data)`: `status` is `completed` or
//! `failed`, `result` is the small ACK doc, `data` is the optional larger
//! payload. Plugin lifecycle commands route to the frozen `PluginSupervisor`
//! through [`plugin_commands`] (idempotent). Ports `execute_command` from
//! `src/ados/services/cloud/command_dispatcher.py`.

pub mod download;
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
/// optional larger `data` payload. Mirrors the Python `(status, result, data)`
/// tuple, with `result` carrying `{success, message}`.
#[derive(Debug, Clone)]
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
mod tests {
    use super::*;

    #[test]
    fn status_serializes_to_wire_strings() {
        assert_eq!(CommandStatus::Completed.as_str(), "completed");
        assert_eq!(CommandStatus::Failed.as_str(), "failed");
        assert_eq!(
            serde_json::to_value(CommandStatus::Completed).unwrap(),
            serde_json::json!("completed")
        );
    }

    #[test]
    fn result_builders_shape_the_ack() {
        let ok = CommandResult::completed("installed");
        assert_eq!(ok.result["success"], true);
        assert_eq!(ok.result["message"], "installed");
        let bad = CommandResult::failed("boom").with_data(serde_json::json!({"code": "x"}));
        assert_eq!(bad.result["success"], false);
        assert_eq!(bad.data.unwrap()["code"], "x");
    }
}
