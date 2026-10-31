//! Pairing-beacon loop.
//!
//! When UNPAIRED and only when `pairing.beacon_enabled` is true, POST the
//! pairing code to `{convex}/pairing/register` every `beacon_interval` seconds.
//! The default deployment ships with the beacon OFF — the agent stays LAN-only
//! and waits for a local `POST /api/pairing/claim`.

use std::time::Duration;

/// Default beacon cadence. Mirrors `beacon_interval` default of 30 s.
pub const DEFAULT_BEACON_INTERVAL: Duration = Duration::from_secs(30);

/// Whether the beacon should run at all. Default OFF: the agent stays LAN-only
/// unless the operator flips `config.pairing.beacon_enabled`. Mirrors the
/// `beacon_enabled` gate (default `False`).
pub fn beacon_enabled(config_beacon_enabled: bool) -> bool {
    config_beacon_enabled
}

/// The inputs the beacon body is assembled from.
#[derive(Debug, Clone)]
pub struct BeaconInputs {
    pub device_id: String,
    pub pairing_code: String,
    pub api_key: String,
    pub name: String,
    pub version: String,
    pub board_name: String,
    pub board_tier: i64,
    pub local_ip: String,
    /// Optional code-expiry epoch ms; included only when present.
    pub code_expires_at: Option<i64>,
}

/// Build the `/pairing/register` POST body.
pub fn build_beacon_body(inputs: &BeaconInputs) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    body.insert("deviceId".to_string(), serde_json::json!(inputs.device_id));
    body.insert(
        "pairingCode".to_string(),
        serde_json::json!(inputs.pairing_code),
    );
    body.insert("apiKey".to_string(), serde_json::json!(inputs.api_key));
    body.insert("name".to_string(), serde_json::json!(inputs.name));
    body.insert("version".to_string(), serde_json::json!(inputs.version));
    body.insert("board".to_string(), serde_json::json!(inputs.board_name));
    body.insert("tier".to_string(), serde_json::json!(inputs.board_tier));
    body.insert("mdnsHost".to_string(), serde_json::json!(""));
    body.insert("localIp".to_string(), serde_json::json!(inputs.local_ip));
    if let Some(exp) = inputs.code_expires_at {
        body.insert("pairingCodeExpiresAt".to_string(), serde_json::json!(exp));
    }
    serde_json::Value::Object(body)
}

/// Whether a `/pairing/register` response says the agent was claimed (so the
/// loop should transition to paired). Mirrors `result.get("alreadyClaimed") or
/// result.get("autoMatched")`.
pub fn response_claimed(body: &serde_json::Value) -> bool {
    body.get("alreadyClaimed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || body
            .get("autoMatched")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests;
