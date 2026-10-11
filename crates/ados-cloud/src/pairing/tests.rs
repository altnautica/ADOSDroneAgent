use super::*;
use std::io::Write;

fn temp_json(name: &str, body: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ados-pairing-{}-{}.json", std::process::id(), name));
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    p
}

#[test]
fn paired_state_exposes_api_key() {
    let path = temp_json(
        "paired",
        r#"{"paired": true, "api_key": "k-123", "owner_id": "u1"}"#,
    );
    let s = PairingState::load_from(&path);
    assert!(s.is_paired());
    assert_eq!(s.api_key(), Some("k-123"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn unpaired_state_hides_api_key() {
    let path = temp_json("unpaired", r#"{"paired": false, "api_key": "k-stale"}"#);
    let s = PairingState::load_from(&path);
    assert!(!s.is_paired());
    // The key is gated on paired, matching the Python property.
    assert_eq!(s.api_key(), None);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn missing_file_is_unpaired() {
    let s = PairingState::load_from(Path::new("/nonexistent/ados/pairing.json"));
    assert!(!s.is_paired());
    assert_eq!(s.api_key(), None);
}

#[test]
fn unpaired_state_exposes_the_code_pending_key_and_expiry() {
    // The Python manager's unpaired state: a code + a pending key + a
    // creation time, no `paired` flag.
    let path = temp_json(
        "beacon",
        r#"{"pairing_code":"ABC234","pending_api_key":"ados_pending","code_created_at":1000.0}"#,
    );
    let s = PairingState::load_from(&path);
    assert!(!s.is_paired());
    assert_eq!(s.pairing_code(), Some("ABC234"));
    assert_eq!(s.pending_api_key(), Some("ados_pending"));
    // Expiry = (created + 24h) * 1000 ms = (1000 + 86400) * 1000.
    let expected_ms = ((1000.0 + super::CODE_TTL_SECS) * 1000.0) as i64;
    assert_eq!(s.code_expires_at_ms(), Some(expected_ms));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn paired_state_hides_the_pairing_code() {
    // Once paired the manager pops the code; the accessor must not surface a
    // residual one (a stale code must never ride a claimed agent's beacon).
    let path = temp_json(
        "paired-code",
        r#"{"paired":true,"api_key":"k","pairing_code":"STALE1"}"#,
    );
    let s = PairingState::load_from(&path);
    assert!(s.is_paired());
    assert_eq!(s.pairing_code(), None);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn code_expiry_is_none_without_a_creation_time() {
    let path = temp_json("nocode", r#"{"pairing_code":"ABC234"}"#);
    let s = PairingState::load_from(&path);
    // A code with no creation time has no derivable expiry.
    assert_eq!(s.code_expires_at_ms(), None);
    let _ = std::fs::remove_file(&path);
}

/// The beacon advertises only a live code that travels with a pending key;
/// an expired code or a code without a key sends the beacon to the local
/// API to mint one.
#[test]
fn beacon_credentials_need_a_live_code_and_a_pending_key() {
    let live = PairingState {
        pairing_code: Some("ABC234".into()),
        pending_api_key: Some("ados_pending".into()),
        code_created_at: Some(1000.0),
        ..PairingState::default()
    };
    assert_eq!(
        live.beacon_credentials(1000.0 + CODE_TTL_SECS - 1.0),
        Some(("ABC234", "ados_pending"))
    );
    assert_eq!(live.beacon_credentials(1000.0 + CODE_TTL_SECS), None);

    let keyless = PairingState {
        pending_api_key: None,
        ..live.clone()
    };
    assert_eq!(keyless.beacon_credentials(1001.0), None);

    let paired = PairingState {
        paired: true,
        ..live
    };
    assert_eq!(paired.beacon_credentials(1001.0), None);
}
