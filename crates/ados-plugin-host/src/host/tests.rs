use super::*;

#[test]
fn noop_host_returns_not_implemented_with_the_method_name() {
    let host = NoopHost;
    let result = host
        .mission_read("p", &Value::Map(vec![]))
        .expect("NoopHost methods never error");
    let map = match result {
        Value::Map(m) => m,
        other => panic!("expected map, got {other:?}"),
    };
    // {"error": "not_implemented", "method": "mission.read"}
    let error = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("error"))
        .and_then(|(_, v)| v.as_str());
    let method = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("method"))
        .and_then(|(_, v)| v.as_str());
    assert_eq!(error, Some("not_implemented"));
    assert_eq!(method, Some("mission.read"));
}

#[test]
fn host_error_renders_the_exact_python_wire_bodies() {
    // These strings are the contract SDK clients string-match on
    // (e.g. error.startswith("allowlist_violation:")). Lock them.
    assert_eq!(HostError::Rpc("boom".to_string()).body(), "boom");
    assert_eq!(
        HostError::CapabilityDenied("mavlink.write".to_string()).body(),
        "capability_denied: mavlink.write"
    );
    assert_eq!(
        HostError::AllowlistViolation("ffmpeg".to_string()).body(),
        "allowlist_violation: ffmpeg"
    );
}
