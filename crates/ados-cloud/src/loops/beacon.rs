//! Pairing-beacon loop.
//!
//! When UNPAIRED and only when `pairing.beacon_enabled` is true, POST the
//! pairing code to `{convex}/pairing/register` every `beacon_interval` seconds.
//! The default deployment ships with the beacon OFF — the agent stays LAN-only
//! and waits for a local `POST /api/pairing/claim`. Ports
//! `src/ados/services/cloud/beacon_loop.py`.

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

/// Build the `/pairing/register` POST body. Mirrors the Python `beacon_body`:
/// camelCase keys, `mdnsHost` empty, `pairingCodeExpiresAt` included only when
/// the code has an expiry.
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
mod tests {
    use super::*;

    fn inputs() -> BeaconInputs {
        BeaconInputs {
            device_id: "dev1".to_string(),
            pairing_code: "123456".to_string(),
            api_key: "k-abc".to_string(),
            name: "ADOS Agent".to_string(),
            version: "0.1.0".to_string(),
            board_name: "rock-5c-lite".to_string(),
            board_tier: 3,
            local_ip: "192.168.1.50".to_string(),
            code_expires_at: None,
        }
    }

    #[test]
    fn default_off() {
        assert!(!beacon_enabled(false));
        assert!(beacon_enabled(true));
    }

    #[test]
    fn beacon_body_has_camelcase_keys() {
        let b = build_beacon_body(&inputs());
        assert_eq!(b["deviceId"], "dev1");
        assert_eq!(b["pairingCode"], "123456");
        assert_eq!(b["apiKey"], "k-abc");
        assert_eq!(b["board"], "rock-5c-lite");
        assert_eq!(b["tier"], 3);
        assert_eq!(b["mdnsHost"], "");
        assert_eq!(b["localIp"], "192.168.1.50");
        // No expiry key when the code has none.
        assert!(b.as_object().unwrap().get("pairingCodeExpiresAt").is_none());
    }

    #[test]
    fn beacon_body_includes_expiry_when_present() {
        let mut i = inputs();
        i.code_expires_at = Some(1716940800000);
        let b = build_beacon_body(&i);
        assert_eq!(b["pairingCodeExpiresAt"], 1716940800000_i64);
    }

    #[test]
    fn response_claimed_detects_both_flags() {
        assert!(response_claimed(
            &serde_json::json!({"alreadyClaimed": true})
        ));
        assert!(response_claimed(&serde_json::json!({"autoMatched": true})));
        assert!(!response_claimed(
            &serde_json::json!({"alreadyClaimed": false})
        ));
        assert!(!response_claimed(&serde_json::json!({})));
    }
}
