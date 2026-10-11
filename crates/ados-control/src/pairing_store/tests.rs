use super::*;
use serde_json::Value;

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn generate_code_is_six_safe_chars() {
    for _ in 0..200 {
        let code = generate_code().expect("getrandom must succeed on the test host");
        assert_eq!(code.len(), CODE_LENGTH, "code {code} wrong length");
        for c in code.bytes() {
            assert!(
                SAFE_CHARSET.contains(&c),
                "code {code} has out-of-charset byte {c}"
            );
        }
    }
}

#[test]
fn generate_api_key_has_the_python_shape() {
    let key = generate_api_key().expect("getrandom must succeed on the test host");
    assert!(key.starts_with("ados_"), "key {key} missing prefix");
    // ados_ (5) + 32 bytes url-safe-no-pad base64 (ceil(32*4/3)=43) = 48.
    assert_eq!(key.len(), 48, "key {key} wrong length");
    let body = &key["ados_".len()..];
    // url-safe alphabet, no padding.
    assert!(
        body.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "key body {body} has a non-url-safe char"
    );
    assert!(!body.contains('='), "url-safe-no-pad must have no padding");
}

#[test]
fn generated_api_keys_are_random_and_well_formed() {
    // Two fresh keys must differ — a fixed/all-zero fallback would make them
    // equal. The key returns a Result (fail-closed on a getrandom error); the
    // success path always yields a prefixed key of the right length.
    let a = generate_api_key().expect("getrandom must succeed on the test host");
    let b = generate_api_key().expect("getrandom must succeed on the test host");
    assert_ne!(a, b, "two generated keys must differ (no fixed fallback)");
    for key in [&a, &b] {
        assert!(key.starts_with("ados_"), "key {key} missing prefix");
        assert_eq!(key.len(), 48, "key {key} wrong length");
    }
    // The all-zero-buffer key that the removed fallback would emit, proving no
    // code path can produce it: ados_ + base64(32 zero bytes).
    let all_zero = {
        use base64::Engine as _;
        let body =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0u8; API_KEY_RANDOM_BYTES]);
        format!("ados_{body}")
    };
    assert_ne!(a, all_zero, "a generated key must not be the all-zero key");
    assert_ne!(b, all_zero, "a generated key must not be the all-zero key");
}

#[test]
fn claim_prefers_the_pending_key_and_writes_the_four_python_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    std::fs::write(
        &path,
        r#"{"pairing_code":"ABC234","code_created_at":123.0,"pending_api_key":"ados_PENDING"}"#,
    )
    .unwrap();

    let out = claim(&path, "user-99", 1700000000.5).unwrap();
    // The pending key is preferred verbatim.
    assert_eq!(out.api_key, "ados_PENDING");

    let on_disk = read_json(&path);
    // Exactly the four Python keys, the code + pending key dropped.
    let keys: std::collections::BTreeSet<_> =
        on_disk.as_object().unwrap().keys().cloned().collect();
    let want: std::collections::BTreeSet<_> = ["paired", "api_key", "owner_id", "paired_at"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(
        keys, want,
        "claimed pairing.json keys differ from PairingManager"
    );
    assert_eq!(on_disk["paired"], Value::Bool(true));
    assert_eq!(on_disk["api_key"], Value::String("ados_PENDING".into()));
    assert_eq!(on_disk["owner_id"], Value::String("user-99".into()));
    assert_eq!(on_disk["paired_at"], serde_json::json!(1700000000.5));
    assert!(on_disk.get("pairing_code").is_none());
    assert!(on_disk.get("pending_api_key").is_none());
}

#[test]
fn reclaim_of_a_paired_agent_returns_the_existing_key_without_rotating() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    // Already paired with a live key, and a DIFFERENT pending key present
    // (e.g. the pair code rotated since). A re-claim must return the
    // existing key, never the pending one, and never mint a new one — so a
    // client already holding the key is not silently locked out.
    std::fs::write(
            &path,
            r#"{"paired":true,"api_key":"ados_LIVE","owner_id":"op","paired_at":1.0,"pending_api_key":"ados_OTHER"}"#,
        )
        .unwrap();
    let out = claim(&path, "someone-else", 2.0).unwrap();
    assert_eq!(
        out.api_key, "ados_LIVE",
        "re-claim must reveal the live key"
    );
    let on_disk = read_json(&path);
    assert_eq!(on_disk["api_key"], Value::String("ados_LIVE".into()));
    assert_eq!(
        on_disk["owner_id"],
        Value::String("op".into()),
        "re-claim must not reassign ownership"
    );
}

#[test]
fn claim_mints_a_fresh_key_when_no_pending_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    std::fs::write(&path, r#"{"pairing_code":"ABC234","code_created_at":1.0}"#).unwrap();
    let out = claim(&path, "user-1", 42.0).unwrap();
    assert!(out.api_key.starts_with("ados_"));
    assert_eq!(out.api_key.len(), 48);
    let on_disk = read_json(&path);
    assert_eq!(on_disk["api_key"], Value::String(out.api_key.clone()));
}

#[test]
fn claim_writes_the_four_keys_in_the_python_insertion_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    claim(&path, "u", 1.0).unwrap();
    // The serialized struct order is paired, api_key, owner_id, paired_at —
    // the Python dict insertion order. Assert the raw text key order.
    let text = std::fs::read_to_string(&path).unwrap();
    let p = text.find("\"paired\"").unwrap();
    let k = text.find("\"api_key\"").unwrap();
    let o = text.find("\"owner_id\"").unwrap();
    let a = text.find("\"paired_at\"").unwrap();
    assert!(
        p < k && k < o && o < a,
        "key order drifted from PairingManager: {text}"
    );
}

#[cfg(unix)]
#[test]
fn claim_persists_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    claim(&path, "u", 1.0).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "pairing.json must be 0600");
}

#[test]
fn unpair_writes_an_empty_object() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    std::fs::write(
        &path,
        r#"{"paired":true,"api_key":"k","owner_id":"o","paired_at":1.0}"#,
    )
    .unwrap();
    unpair(&path, &dir.path().join("relay-peer-secret")).unwrap();
    let on_disk = read_json(&path);
    assert_eq!(on_disk, serde_json::json!({}), "unpair must write {{}}");
}

#[cfg(unix)]
#[test]
fn unpair_persists_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    unpair(&path, &dir.path().join("relay-peer-secret")).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn write_new_code_persists_a_fresh_six_char_code_and_keeps_the_pending_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    std::fs::write(&path, r#"{"pending_api_key":"ados_beacon"}"#).unwrap();
    let code = write_new_code(&path, 99.0).unwrap();
    assert_eq!(code.len(), CODE_LENGTH);
    let on_disk = read_json(&path);
    assert_eq!(on_disk["pairing_code"], Value::String(code));
    assert_eq!(on_disk["code_created_at"], serde_json::json!(99.0));
    assert_eq!(on_disk["pending_api_key"], "ados_beacon");
}

#[test]
fn a_new_code_without_a_pending_key_mints_one_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    write_new_code(&path, 99.0).unwrap();
    let key = read_json(&path)["pending_api_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(key.starts_with("ados_"));
    // Keeping a live code and its key writes nothing new.
    let code = read_json(&path)["pairing_code"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(ensure_code(&path, 100.0).unwrap(), code);
    assert_eq!(read_json(&path)["pending_api_key"], Value::String(key));
}

#[test]
fn a_new_code_never_overwrites_a_paired_document() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    let paired = r#"{"paired":true,"api_key":"k"}"#;
    std::fs::write(&path, paired).unwrap();
    assert!(write_new_code(&path, 99.0).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), paired);
}

#[test]
fn load_reads_the_cloud_pair_fields_when_paired() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    std::fs::write(
            &path,
            r#"{"paired":true,"api_key":"k","owner_id":"user-42","paired_at":1700000000.0,"pairing_code":"ZZ"}"#,
        )
        .unwrap();
    let doc = PairingDoc::read(&path).unwrap();
    assert!(doc.is_paired());
    assert_eq!(doc.info_owner_id(), Some("user-42".to_string()));
    assert_eq!(doc.info_paired_at(), Some(1700000000.0));
    // Paired → no code, even if a stale code lingers in the file.
    assert_eq!(doc.live_code(2.0), None);
}

#[test]
fn a_code_is_live_only_inside_its_lifetime_and_only_when_unpaired() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    std::fs::write(&path, r#"{"pairing_code":"ABC234","code_created_at":1.0}"#).unwrap();
    let doc = PairingDoc::read(&path).unwrap();
    assert!(!doc.is_paired());
    assert_eq!(doc.live_code(2.0), Some("ABC234".to_string()));
    // The same lifetime the Python writer applies, so both surfaces agree.
    assert_eq!(doc.live_code(1.0 + CODE_TTL_SECONDS), None);
    // Unpaired → owner/paired_at omitted.
    assert_eq!(doc.info_owner_id(), None);
    assert_eq!(doc.info_paired_at(), None);
}

#[test]
fn load_of_an_absent_file_is_the_unpaired_default() {
    let dir = tempfile::tempdir().unwrap();
    let doc = PairingDoc::read(&dir.path().join("absent.json")).unwrap();
    assert!(!doc.is_paired());
    assert_eq!(doc.info_owner_id(), None);
    assert_eq!(doc.live_code(0.0), None);
}

/// A present file that cannot be parsed is not "unpaired": reading it is an
/// error, and a claim neither hands out a key nor overwrites it.
#[test]
fn a_malformed_file_fails_closed_and_the_claim_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    for body in [
        r#"{"paired": true, "api_key": "k""#,
        r#"{"paired": true}"#,
        "[]",
    ] {
        std::fs::write(&path, body).unwrap();
        assert!(PairingDoc::read(&path).is_err(), "{body} must not read");
        assert!(matches!(
            claim(&path, "intruder", 1.0),
            Err(ClaimError::Unreadable(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
    }
}

/// Unpair drops the relay secret, so the next ground station's offer is
/// accepted instead of refused as "already held".
#[test]
fn unpair_then_a_new_relay_offer_is_accepted() {
    use ados_protocol::relay_ticket::{apply_offered_secret, AcceptDecision};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    let secret = dir.path().join("secrets/relay-peer-secret");
    let first = "a".repeat(64);
    let second = "b".repeat(64);
    assert_eq!(
        apply_offered_secret(&secret, &first).unwrap(),
        AcceptDecision::Accept
    );
    assert_ne!(
        apply_offered_secret(&secret, &second).unwrap(),
        AcceptDecision::Accept,
        "held: a second offer is refused"
    );
    unpair(&path, &secret).unwrap();
    assert_eq!(
        apply_offered_secret(&secret, &second).unwrap(),
        AcceptDecision::Accept
    );
}
