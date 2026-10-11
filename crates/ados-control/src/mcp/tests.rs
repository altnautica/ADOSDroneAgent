use super::*;

fn store(dir: &Path) -> McpTokenStore {
    McpTokenStore::with_path(dir.join("mcp-token.json"))
}

fn paired() -> Pairing {
    Pairing::Paired("ados_secret".to_string())
}

/// A default mint request (`api_key = ados_secret`, node `node-1`), overriding
/// only the scopes / TTL / mint-time a given test cares about.
fn req(scopes: &[String], ttl_ms: i64, now_ms: i64) -> MintRequest<'_> {
    MintRequest {
        api_key: "ados_secret",
        label: "claude",
        operator_id: "cloud:usr_1",
        node_id: "node-1",
        scopes,
        allowed_nodes: &[],
        ttl_ms,
        now_secs: now_ms as f64 / 1000.0,
        now_ms,
    }
}

#[test]
fn absent_file_mints_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    assert!(!s.any_minted());
    // Verify against an empty store is None (no salt to key with).
    assert!(s.verify(&paired(), "anything.here", 1, "node-1").is_none());
    let (tokens, revoked) = s.status();
    assert!(tokens.is_empty() && revoked.is_empty());
}

#[test]
fn mint_then_verify_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let scopes = ["read".to_string(), "admin".to_string()];
    let tok = s.mint(&req(&scopes, 3_600_000, 1_000_000_000_000)).unwrap();
    assert!(s.any_minted());
    let claims = s
        .verify(&paired(), &tok, 1_000_000_100_000, "node-1")
        .expect("a fresh token verifies");
    assert_eq!(claims.scopes, vec!["read", "admin"]);
    assert_eq!(claims.iss, "agent:node-1");
    // The registry records the mint (sans secret).
    let (tokens, _) = s.status();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].label, "claude");
}

#[test]
fn wrong_node_and_wrong_key_do_not_verify() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let scopes = ["read".to_string()];
    let tok = s.mint(&req(&scopes, 3_600_000, 0)).unwrap();
    // A different node subject is rejected.
    assert!(s.verify(&paired(), &tok, 1_000, "node-2").is_none());
    // A different pairing key is rejected.
    assert!(s
        .verify(&Pairing::Paired("other".into()), &tok, 1_000, "node-1")
        .is_none());
    // Unpaired never verifies (data plane is open anyway).
    assert!(s
        .verify(&Pairing::Unpaired, &tok, 1_000, "node-1")
        .is_none());
}

#[test]
fn expired_token_does_not_verify() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    // TTL 1000 ms from mint-time 0 -> expires at 1000.
    let scopes = ["read".to_string()];
    let tok = s.mint(&req(&scopes, 1_000, 0)).unwrap();
    assert!(s.verify(&paired(), &tok, 999, "node-1").is_some());
    assert!(s.verify(&paired(), &tok, 1_000, "node-1").is_none());
}

#[test]
fn revoke_denylists_one_token() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let scopes = ["read".to_string()];
    let tok = s.mint(&req(&scopes, 3_600_000, 0)).unwrap();
    let (tokens, _) = s.status();
    let id = tokens[0].token_id.clone();
    assert!(s.verify(&paired(), &tok, 1_000, "node-1").is_some());
    s.revoke(&id).unwrap();
    assert!(
        s.verify(&paired(), &tok, 1_000, "node-1").is_none(),
        "a revoked token id no longer verifies"
    );
    // Revoking an unknown id is a no-op success.
    s.revoke("mct_unknown").unwrap();
}

#[test]
fn revoke_all_rotates_and_kills_prior_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let scopes = ["read".to_string()];
    let tok = s.mint(&req(&scopes, 3_600_000, 0)).unwrap();
    assert!(s.verify(&paired(), &tok, 1_000, "node-1").is_some());
    s.revoke_all().unwrap();
    assert!(!s.any_minted());
    assert!(
        s.verify(&paired(), &tok, 1_000, "node-1").is_none(),
        "revoke_all rotates the salt so the prior token dies"
    );
    // A fresh mint installs a new salt + verifies again.
    let tok2 = s.mint(&req(&scopes, 3_600_000, 0)).unwrap();
    assert!(s.verify(&paired(), &tok2, 1_000, "node-1").is_some());
    assert_ne!(tok, tok2, "a new salt yields a different token");
}

#[test]
fn concurrent_mints_do_not_lose_writes() {
    // Serialized load-modify-write + a unique temp file per write means every
    // concurrent mint is recorded (an unlocked RMW sharing one temp name would
    // drop writes). 8 threads mint against one store; all 8 must land.
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(store(dir.path()));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let s = s.clone();
            std::thread::spawn(move || {
                let scopes = ["read".to_string()];
                s.mint(&req(&scopes, 3_600_000, 1_000_000 + i)).unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let (tokens, _) = s.status();
    assert_eq!(tokens.len(), 8, "every concurrent mint is recorded");
}

#[test]
fn mint_rejects_unknown_scope() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let scopes = ["read".to_string(), "root".to_string()];
    let err = s.mint(&req(&scopes, 1, 0)).unwrap_err();
    assert!(matches!(err, MintError::UnknownScope(s) if s == "root"));
}

#[cfg(unix)]
#[test]
fn record_is_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let scopes = ["read".to_string()];
    s.mint(&req(&scopes, 1, 0)).unwrap();
    let mode = std::fs::metadata(s.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "mcp-token.json must be 0600");
}

// ---- route_scope ----

#[test]
fn reads_require_read() {
    for p in [
        "/api/status",
        "/api/telemetry",
        "/api/params",
        "/api/params/RC1_MIN",
        "/api/services",
        "/api/wfb",
        "/api/fleet/peers",
        "/api/mcp/status",
    ] {
        assert_eq!(route_scope(&Method::GET, p), Some(ScopeClass::Read), "{p}");
    }
}

#[test]
fn command_route_requires_flight() {
    // The raw command passthrough can arm/fly the vehicle -> flight scope.
    assert_eq!(
        route_scope(&Method::POST, "/api/command"),
        Some(ScopeClass::Flight)
    );
    // CAN passthrough injects frames to the FC/ESCs/servos -> also flight.
    assert_eq!(
        route_scope(&Method::POST, "/api/can/passthrough"),
        Some(ScopeClass::Flight)
    );
}

#[test]
fn secret_read_class_gates_on_the_secret_read_scope() {
    // A read-only token cannot reach a secret-read route; a secret_read token can.
    assert_eq!(ScopeClass::SecretRead.group_name(), "secret_read");
    assert!(!ados_protocol::mcp_token::scope_allows_class(
        ScopeClass::SecretRead,
        &["read".to_string()]
    ));
    assert!(ados_protocol::mcp_token::scope_allows_class(
        ScopeClass::SecretRead,
        &["read".to_string(), "secret_read".to_string()]
    ));
}

#[test]
fn no_native_get_is_secret_bearing_today() {
    // GET->Read is a blanket, so any GET that returns a secret MUST be listed
    // as secret_read or a plain `read` token reaches it. This guards the
    // invariant: adding a secret-bearing GET without classifying it fails here.
    assert!(
        SECRET_GET_ROUTES.is_empty(),
        "classify any secret-bearing native GET as secret_read"
    );
    // A representative native GET stays plain read.
    assert_eq!(
        route_scope(&Method::GET, "/api/status"),
        Some(ScopeClass::Read)
    );
    // A plugin's stored config may hold a credential.
    assert_eq!(
        route_scope(&Method::GET, "/api/plugins/com.example.hello/config"),
        Some(ScopeClass::SecretRead)
    );
}

#[test]
fn writes_map_to_their_class() {
    assert_eq!(
        route_scope(&Method::POST, "/api/params/ARMING_CHECK"),
        Some(ScopeClass::Admin)
    );
    assert_eq!(
        route_scope(&Method::POST, "/api/services/ados-mavlink/restart"),
        Some(ScopeClass::Admin)
    );
    assert_eq!(
        route_scope(&Method::POST, "/api/pairing/unpair"),
        Some(ScopeClass::Destructive)
    );
    assert_eq!(
        route_scope(&Method::POST, "/api/vision/designate"),
        Some(ScopeClass::SafeWrite)
    );
    assert_eq!(
        route_scope(&Method::PUT, "/api/plugins/battery-panel/config"),
        Some(ScopeClass::Admin)
    );
    assert_eq!(
        route_scope(&Method::PUT, "/api/v1/ground-station/wfb"),
        Some(ScopeClass::Admin)
    );
    assert_eq!(
        route_scope(&Method::POST, "/api/_ws/ticket"),
        Some(ScopeClass::Admin)
    );
    // A plugin tool invoke is classified by the route from the tool's
    // declared safety class, not by the edge.
    assert_eq!(
        route_scope(&Method::POST, "/api/plugins/com.x.p/tools/greet/invoke"),
        None
    );
    assert!(route_decides_scope(
        &Method::POST,
        "/api/plugins/com.x.p/tools/greet/invoke"
    ));
    assert!(!route_decides_scope(
        &Method::POST,
        "/api/plugins/com.x.p/install"
    ));
    // A plugin path that is NOT the tool-invoke shape stays fail-closed.
    assert_eq!(
        route_scope(&Method::POST, "/api/plugins/com.x.p/install"),
        None
    );
}

/// An admin-only token cannot command the aircraft by wrapping a flight
/// request in the relay-proxy, injecting RC, or taking command.
#[test]
fn flight_actions_need_the_flight_scope_on_every_path_to_them() {
    let relay = "/api/v1/ground-station/relay-proxy/0a1b2c3d4e5f";
    assert_eq!(
        route_scope(&Method::POST, &format!("{relay}/api/command")),
        Some(ScopeClass::Flight)
    );
    assert_eq!(
        route_scope(&Method::POST, &format!("{relay}/api/params/ARMING_CHECK")),
        Some(ScopeClass::Admin)
    );
    assert_eq!(
        route_scope(&Method::GET, &format!("{relay}/api/plugins/com.x.p/config")),
        Some(ScopeClass::SecretRead),
        "a relayed plugin config read is still a secret read"
    );
    assert_eq!(
        route_scope(&Method::GET, &format!("{relay}/api/status")),
        Some(ScopeClass::Read)
    );
    assert_eq!(
        route_scope(&Method::POST, &format!("{relay}/api/does-not-exist")),
        None
    );
    for path in [
        "/api/v1/ground-station/crsf/channels",
        "/api/v1/ground-station/pic/claim",
    ] {
        assert_eq!(
            route_scope(&Method::POST, path),
            Some(ScopeClass::Flight),
            "{path}"
        );
    }
}

#[test]
fn a_tool_needs_the_scope_its_safety_class_names() {
    assert_eq!(tool_scope_class("flight"), Some(ScopeClass::Flight));
    assert_eq!(tool_scope_class("safe_write"), Some(ScopeClass::SafeWrite));
    assert_eq!(tool_scope_class("read"), Some(ScopeClass::Read));
    assert_eq!(
        tool_scope_class(""),
        None,
        "undeclared is not token-reachable"
    );
    let manifest = ados_plugin_host::manifest::PluginManifest::from_yaml_text(
            "id: com.example.pod\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  contributes:\n    tools:\n      - name: fire\n        safety_class: flight\n      - name: status\n        safety_class: read\n",
        )
        .unwrap();
    assert_eq!(
        tool_safety_class(&manifest, "fire").as_deref(),
        Some("flight")
    );
    assert_eq!(
        tool_safety_class(&manifest, "status").as_deref(),
        Some("read")
    );
    assert_eq!(tool_safety_class(&manifest, "absent"), None);
}

#[test]
fn unclassified_writes_fail_closed() {
    // An unknown write path is DENIED to a token (must use the full key).
    assert_eq!(route_scope(&Method::POST, "/api/does-not-exist"), None);
    assert_eq!(route_scope(&Method::POST, "/api/mcp/tokens"), None); // mint is on-box/key only
    assert_eq!(route_scope(&Method::PUT, "/api/does-not-exist"), None);
    assert_eq!(route_scope(&Method::DELETE, "/api/does-not-exist"), None);
    // A trailing-slash / multi-segment name is not a param write.
    assert_eq!(route_scope(&Method::POST, "/api/params/"), None);
    assert_eq!(route_scope(&Method::POST, "/api/params/a/b"), None);
}

/// The native plugin lifecycle is unreachable with any MCP token, reads
/// included, while a plugin's config, published state and tool invocation
/// keep their classes.
#[test]
fn plugin_lifecycle_routes_stay_fail_closed() {
    for (method, path) in [
        (Method::GET, "/api/plugins"),
        (Method::GET, "/api/plugins/com.x.p"),
        (Method::GET, "/api/plugins/com.x.p/attestation"),
        (Method::GET, "/api/plugins/com.x.p/gcs/plugin.bundle.js"),
        (Method::GET, "/api/plugins/com.x.p/x/status"),
        (Method::GET, "/api/plugins/com.x.p/x/tools/a/invoke"),
        (Method::GET, "/api/plugins/jobs/job-1"),
        (Method::GET, "/api/v1/plugins/catalog"),
        (Method::POST, "/api/plugins/install"),
        (Method::POST, "/api/plugins/com.x.p/enable"),
        (Method::POST, "/api/plugins/com.x.p/x/config"),
        (Method::DELETE, "/api/plugins/com.x.p"),
    ] {
        assert_eq!(route_scope(&method, path), None, "{method} {path}");
    }
    assert_eq!(
        route_scope(&Method::GET, "/api/plugins/com.x.p/state"),
        Some(ScopeClass::Read)
    );
    assert_eq!(
        route_scope(&Method::GET, "/api/plugins/com.x.p/config"),
        Some(ScopeClass::SecretRead)
    );
}
