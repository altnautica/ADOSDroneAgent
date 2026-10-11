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
