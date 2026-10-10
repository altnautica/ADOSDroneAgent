use super::*;
use std::io::Write;

fn temp_yaml(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.yaml");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    (dir, p)
}

#[test]
fn load_config_object_is_always_an_object() {
    // The routes index straight into the result, so every failure mode has
    // to land on `{}` rather than null/scalar. These four cases are the
    // contract the per-route readers had before they shared one loader; the
    // only intended change is that the malformed case now logs the parser
    // error instead of being silently identical to the absent one.

    // Absent — the normal fresh-node case.
    assert_eq!(
        load_config_object(Path::new("/nonexistent/ados/config.yaml")),
        serde_json::json!({})
    );

    // Present but malformed. Asserted to be a genuine parse failure first:
    // if this text merely parsed to a non-mapping it would still land on
    // `{}` and the case would pass without ever exercising the loud path.
    let malformed = "agent: {device_id: [unclosed\n  bad";
    assert!(
        serde_norway::from_str::<serde_json::Value>(malformed).is_err(),
        "fixture must be a parse error, otherwise this case proves nothing"
    );
    let (_bad_dir, bad) = temp_yaml(malformed);
    assert_eq!(load_config_object(&bad), serde_json::json!({}));

    // Parses cleanly, but the root is not a mapping — a distinct path from
    // the one above, and the reason the object coercion exists at all.
    let scalar_text = "just-a-string\n";
    let parsed = serde_norway::from_str::<serde_json::Value>(scalar_text)
        .expect("fixture must parse, to separate this case from the malformed one");
    assert!(!parsed.is_object());
    let (_scalar_dir, scalar) = temp_yaml(scalar_text);
    assert_eq!(load_config_object(&scalar), serde_json::json!({}));

    // The happy path still projects the mapping through untouched.
    let (_good_dir, good) = temp_yaml("agent:\n  name: test-drone\nvideo:\n  mode: disabled\n");
    let v = load_config_object(&good);
    assert_eq!(v["agent"]["name"], "test-drone");
    assert_eq!(v["video"]["mode"], "disabled");
}

#[test]
fn missing_file_yields_the_python_defaults() {
    let cfg = PairingConfig::load_from(Path::new("/nonexistent/ados/config.yaml"));
    assert_eq!(cfg.agent.device_id, "");
    assert_eq!(cfg.agent.name, "my-drone");
    assert_eq!(cfg.agent.profile, "auto");
    assert_eq!(cfg.radio_peer_device_id(), None);
}

#[test]
fn reads_agent_identity_ignoring_the_rest() {
    let yaml = "\
mavlink:
  port: /dev/ttyACM0
agent:
  device_id: abcdef1234567890
  name: test-drone
  profile: drone
video:
  mode: disabled
";
    let (_dir, path) = temp_yaml(yaml);
    let cfg = PairingConfig::load_from(&path);
    assert_eq!(cfg.agent.device_id, "abcdef1234567890");
    assert_eq!(cfg.agent.name, "test-drone");
    assert_eq!(cfg.agent.profile, "drone");
    // The cached reader projects the same values from its single parse.
    let cached = PairingConfig::cached(&path);
    assert_eq!(cached.agent.device_id, "abcdef1234567890");
    assert_eq!(cached.agent.profile, "drone");
}

#[test]
fn reads_the_radio_peer_device_id() {
    let yaml = "\
video:
  wfb:
    channel: 149
    paired_with_device_id: peer-1234567890
";
    let (_dir, path) = temp_yaml(yaml);
    let cfg = PairingConfig::load_from(&path);
    assert_eq!(
        cfg.radio_peer_device_id(),
        Some("peer-1234567890".to_string())
    );
}

#[test]
fn an_empty_peer_is_treated_as_no_peer() {
    // Matches the Python `isinstance(peer_raw, str) and peer_raw` guard: a
    // present-but-empty value reads as no peer.
    let yaml = "\
video:
  wfb:
    paired_with_device_id: \"\"
";
    let (_dir, path) = temp_yaml(yaml);
    let cfg = PairingConfig::load_from(&path);
    assert_eq!(cfg.radio_peer_device_id(), None);
}

#[test]
fn security_defaults_are_the_python_defaults() {
    // A missing file → all-defaults → the same defaults the Python agent uses,
    // and an absent file is not an unreadable one.
    let path = Path::new("/nonexistent/ados/config.yaml");
    let snap = config_snapshot(path, Duration::ZERO);
    let cfg = snap.security.expect("an absent config is the defaults");
    let s = cfg.security();
    assert_eq!(s.api.api_key, "");
    assert!(!s.hmac_enabled);
    assert_eq!(s.hmac_secret, "");
    assert!(!s.setup_token_required);
}

#[test]
fn a_python_written_config_reads_identically() {
    // A config the Python agent wrote reads the same field values here.
    let yaml = "\
security:
  api:
    api_key: configured-key
  hmac_enabled: true
  hmac_secret: a-very-long-secret-key
  setup_token_required: true
";
    let (_dir, path) = temp_yaml(yaml);
    let cfg = ControlSecurityConfig::cached(&path);
    let s = cfg.security();
    assert_eq!(s.api.api_key, "configured-key");
    assert!(s.hmac_enabled);
    assert_eq!(s.hmac_secret, "a-very-long-secret-key");
    assert!(s.setup_token_required);
}

#[test]
fn a_malformed_config_reports_the_security_slice_unreadable() {
    let (_dir, path) = temp_yaml("security: {hmac_enabled: [unclosed\n");
    let snap = config_snapshot(&path, Duration::ZERO);
    assert!(snap.security.is_err());
    // The pairing slice still answers with defaults (pairing-info stays 200).
    assert_eq!(snap.pairing.agent.profile, "auto");
    // The lenient accessor falls back to the defaults.
    assert!(!ControlSecurityConfig::cached(&path).security.hmac_enabled);
}

#[test]
fn a_changed_config_is_reparsed_once_the_ttl_lapses() {
    let (_dir, path) = temp_yaml("agent:\n  profile: drone\n");
    assert_eq!(
        config_snapshot(&path, Duration::ZERO).pairing.agent.profile,
        "drone"
    );
    // A different length, so the stamp changes even on a coarse mtime clock.
    std::fs::write(&path, "agent:\n  profile: ground_station\n").unwrap();
    // Within a long TTL the cached snapshot stands.
    assert_eq!(
        config_snapshot(&path, Duration::from_secs(3600))
            .pairing
            .agent
            .profile,
        "drone"
    );
    // Past it the stamp moved, so the file is parsed again.
    assert_eq!(
        config_snapshot(&path, Duration::ZERO).pairing.agent.profile,
        "ground_station"
    );
}

/// A writer the test subscriber logs into, shared with the assertion.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_malformed_config_logs_its_parse_error_once() {
    let (_dir, path) = temp_yaml("agent: {profile: [unclosed\n");
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..100 {
            let _ = config_snapshot(&path, Duration::ZERO);
        }
    });
    let text = String::from_utf8(captured.0.lock().clone()).unwrap();
    assert_eq!(text.matches("config parse failed").count(), 1, "{text}");
}
