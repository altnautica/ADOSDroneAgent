//! Cloud command-poll loop.
//!
//! Every 5 s, when paired, GET `{convex}/agent/commands?deviceId=...` with
//! `X-ADOS-Key`, dispatch each command, and ACK the result to
//! `{convex}/agent/commands/ack`. Originally ported from the Python poll loop,
//! which has since been deleted. The auth is always the `X-ADOS-Key` header,
//! never a URL/query param.

use std::time::Duration;

use crate::dispatch::CommandResult;

/// Poll cadence: a 5 s sleep, as the receiver expects.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Build the ACK payload for a dispatched command. The shape is `{commandId, deviceId,
/// status, result?, data?}`. `result` is included when non-null; `data` when present.
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
mod tests;
