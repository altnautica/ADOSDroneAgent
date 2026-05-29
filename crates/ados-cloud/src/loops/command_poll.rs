//! Cloud command-poll loop.
//!
//! Every 5 s, when paired, GET `{convex}/agent/commands?deviceId=...` with
//! `X-ADOS-Key`, dispatch each command, and ACK the result to
//! `{convex}/agent/commands/ack`. Ports
//! `src/ados/services/cloud/command_poll_loop.py`. The auth is always the
//! `X-ADOS-Key` header, never a URL/query param.

use std::time::Duration;

use crate::dispatch::CommandResult;

/// Poll cadence. Mirrors the Python loop's 5 s sleep.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Build the ACK payload for a dispatched command. Mirrors the Python
/// `ack_payload` shape: `{commandId, deviceId, status, result?, data?}`. `result`
/// is included when non-null; `data` when present.
pub fn build_ack(command_id: &str, device_id: &str, result: &CommandResult) -> serde_json::Value {
    let mut ack = serde_json::Map::new();
    ack.insert("commandId".to_string(), serde_json::json!(command_id));
    ack.insert("deviceId".to_string(), serde_json::json!(device_id));
    ack.insert(
        "status".to_string(),
        serde_json::json!(result.status.as_str()),
    );
    // The small result doc is always present here (the dispatcher always sets
    // it); include it under `result` matching the Python `if result:` branch.
    ack.insert("result".to_string(), result.result.clone());
    if let Some(data) = &result.data {
        ack.insert("data".to_string(), data.clone());
    }
    serde_json::Value::Object(ack)
}

/// Extract the command id from a command-queue row (`_id`). Mirrors
/// `cmd.get("_id")`.
pub fn command_id(row: &serde_json::Value) -> &str {
    row.get("_id").and_then(|v| v.as_str()).unwrap_or("")
}

/// Extract the command name from a row (`command`). Mirrors
/// `cmd.get("command", "unknown")`.
pub fn command_name(row: &serde_json::Value) -> &str {
    row.get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
}

/// Parse the `commands[]` array out of a `GET /agent/commands` response body.
pub fn parse_commands(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body.get("commands")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::CommandResult;

    #[test]
    fn ack_carries_command_id_device_status_result_and_data() {
        let result =
            CommandResult::completed("installed").with_data(serde_json::json!({"pluginId": "p"}));
        let ack = build_ack("cmd-1", "dev1", &result);
        assert_eq!(ack["commandId"], "cmd-1");
        assert_eq!(ack["deviceId"], "dev1");
        assert_eq!(ack["status"], "completed");
        assert_eq!(ack["result"]["success"], true);
        assert_eq!(ack["data"]["pluginId"], "p");
    }

    #[test]
    fn ack_omits_data_when_absent() {
        let result = CommandResult::failed("boom");
        let ack = build_ack("cmd-2", "dev1", &result);
        assert_eq!(ack["status"], "failed");
        assert!(ack.as_object().unwrap().get("data").is_none());
    }

    #[test]
    fn parse_commands_reads_the_array() {
        let body = serde_json::json!({
            "commands": [
                {"_id": "c1", "command": "get_services"},
                {"_id": "c2", "command": "plugin.enable", "args": {"pluginId": "p"}}
            ]
        });
        let cmds = parse_commands(&body);
        assert_eq!(cmds.len(), 2);
        assert_eq!(command_id(&cmds[0]), "c1");
        assert_eq!(command_name(&cmds[1]), "plugin.enable");
    }

    #[test]
    fn parse_commands_empty_when_missing() {
        assert!(parse_commands(&serde_json::json!({})).is_empty());
    }
}
