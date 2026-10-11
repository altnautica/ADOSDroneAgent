use super::*;
use serde_json::Value;

// ── MAC validation + normalisation ────────────────────────────────────────

#[test]
fn mac_validation_matches_the_python_regex() {
    // Valid: six two-hex octets, colon- or dash-separated (and mixed, as the
    // Python per-separator regex allows).
    assert!(mac_is_valid("02:c6:75:83:1a:3e"));
    assert!(mac_is_valid("02-C6-75-83-1A-3E"));
    assert!(mac_is_valid("AA:bb:CC:dd:EE:ff"));
    assert!(mac_is_valid("02-c6:75-83:1a-3e"));
    // Invalid: too few/many octets, a non-hex digit, a bad separator, wrong
    // octet width.
    assert!(!mac_is_valid("02:c6:75:83:1a"));
    assert!(!mac_is_valid("02:c6:75:83:1a:3e:99"));
    assert!(!mac_is_valid("0g:c6:75:83:1a:3e"));
    assert!(!mac_is_valid("02_c6_75_83_1a_3e"));
    assert!(!mac_is_valid("2:c6:75:83:1a:3e"));
    assert!(!mac_is_valid(""));
    assert!(!mac_is_valid("not-a-mac"));
}

#[test]
fn normalise_lowercases_and_swaps_dashes_for_colons() {
    assert_eq!(normalise_mac("02-C6-75-83-1A-3E"), "02:c6:75:83:1a:3e");
    assert_eq!(normalise_mac("AA:BB:CC:DD:EE:FF"), "aa:bb:cc:dd:ee:ff");
}

// ── state-file candidate resolution ───────────────────────────────────────

#[test]
fn proposed_mac_reads_the_state_file_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("mac-pins.state");
    std::fs::write(
        &state,
        serde_json::to_string(&json!({
            "version": 1,
            "adapters": [
                {"name": "wlan0", "pinned_mac": "02:c6:75:83:1a:3e"},
                {"name": "wlan1"},
            ],
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        proposed_mac_for(&state, "wlan0"),
        Some("02:c6:75:83:1a:3e".to_string())
    );
    // An adapter with no pinned_mac, an unknown name, and an absent file all
    // resolve to None.
    assert_eq!(proposed_mac_for(&state, "wlan1"), None);
    assert_eq!(proposed_mac_for(&state, "eth0"), None);
    assert_eq!(
        proposed_mac_for(&dir.path().join("absent.state"), "wlan0"),
        None
    );
}

// ── config merge: set + remove preserve the rest of the file ──────────────

#[test]
fn set_override_creates_the_section_and_preserves_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();
    config_set_override(&cfg, "wlan0", "02:c6:75:83:1a:3e").unwrap();

    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let ov = parsed
        .get("network")
        .and_then(|n| n.get("mac_pin"))
        .and_then(|m| m.get("overrides"))
        .and_then(|o| o.get("wlan0"))
        .and_then(serde_norway::Value::as_str);
    assert_eq!(ov, Some("02:c6:75:83:1a:3e"));
    // The unrelated agent.name survived.
    assert_eq!(
        parsed
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str()),
        Some("my-drone")
    );
}

#[test]
fn set_override_on_an_absent_file_seeds_an_empty_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    config_set_override(&cfg, "wlan0", "aa:bb:cc:dd:ee:ff").unwrap();
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        parsed
            .get("network")
            .and_then(|n| n.get("mac_pin"))
            .and_then(|m| m.get("overrides"))
            .and_then(|o| o.get("wlan0"))
            .and_then(serde_norway::Value::as_str),
        Some("aa:bb:cc:dd:ee:ff")
    );
}

#[test]
fn remove_override_pops_a_present_key_and_reports_true() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
            &cfg,
            "agent:\n  name: my-drone\nnetwork:\n  mac_pin:\n    overrides:\n      wlan0: 02:c6:75:83:1a:3e\n",
        )
        .unwrap();
    let removed = config_remove_override(&cfg, "wlan0").unwrap();
    assert!(removed);
    // The key is gone; agent.name survived.
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let ov_present = parsed
        .get("network")
        .and_then(|n| n.get("mac_pin"))
        .and_then(|m| m.get("overrides"))
        .and_then(|o| o.get("wlan0"))
        .is_some();
    assert!(!ov_present);
    assert_eq!(
        parsed
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str()),
        Some("my-drone")
    );
}

#[test]
fn remove_override_of_an_absent_key_reports_false_and_does_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();
    let before = std::fs::read_to_string(&cfg).unwrap();
    let removed = config_remove_override(&cfg, "wlan0").unwrap();
    assert!(!removed);
    // The file is unchanged (the Python only re-persists when it popped a key).
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
}

// ── apply_live_allowed flag ───────────────────────────────────────────────

#[test]
fn apply_live_allowed_defaults_false_and_reads_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    // Absent file / absent section default to false.
    assert!(!apply_live_allowed(&cfg));
    std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();
    assert!(!apply_live_allowed(&cfg));
    // The explicit true flag reads true.
    std::fs::write(&cfg, "network:\n  mac_pin:\n    apply_live_allowed: true\n").unwrap();
    assert!(apply_live_allowed(&cfg));
}

// ── POST handler: the guard order + the success body ──────────────────────

/// The adapter key the seeded state file gives `wlan0`.
const WLAN0_KEY: &str = "a69c:8d81@5-1.0";

/// Seed a state file whose adapters are `(name, pinned_mac)`, the i-th one
/// on USB port `5-1.<i>`.
fn seed_state(path: &Path, adapters: &[(&str, Option<&str>)]) {
    let list: Vec<Value> = adapters
        .iter()
        .enumerate()
        .map(|(i, (name, pinned))| {
            json!({
                "name": name,
                "vidpid": "a69c:8d81",
                "usb_path": format!("5-1.{i}"),
                "pinned_mac": pinned,
            })
        })
        .collect();
    std::fs::write(
        path,
        serde_json::to_string(&json!({"version": 1, "adapters": list})).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn an_interface_the_agent_never_enumerated_is_a_400_e_unknown_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let state = dir.path().join("mac-pins.state");
    seed_state(&state, &[("wlan0", None)]);
    let resp = post_mac_pin_at(
        &cfg,
        &state,
        MacPinRequest {
            iface: "wlan7".to_string(),
            mac: Some("02:c6:75:83:1a:3e".to_string()),
            apply_now: false,
        },
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        json!("E_UNKNOWN_ADAPTER")
    );
    assert!(!cfg.exists(), "nothing is persisted under a name key");
}

#[tokio::test]
async fn no_mac_and_no_candidate_is_a_400_e_no_mac() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let state = dir.path().join("mac-pins.state");
    seed_state(&state, &[("wlan0", None)]);
    let resp = post_mac_pin_at(
        &cfg,
        &state,
        MacPinRequest {
            iface: "wlan0".to_string(),
            mac: None,
            apply_now: false,
        },
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": {"code": "E_NO_MAC", "message": "provide a MAC, or pin a candidate that already has a proposed value"}}})
    );
}

#[tokio::test]
async fn a_malformed_mac_is_a_400_e_bad_mac() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let state = dir.path().join("mac-pins.state");
    seed_state(&state, &[("wlan0", None)]);
    let resp = post_mac_pin_at(
        &cfg,
        &state,
        MacPinRequest {
            iface: "wlan0".to_string(),
            mac: Some("not-a-mac".to_string()),
            apply_now: false,
        },
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": {"code": "E_BAD_MAC", "message": "malformed MAC: not-a-mac"}}})
    );
}

#[tokio::test]
async fn an_explicit_mac_pins_and_normalises_with_the_next_boot_note() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let state = dir.path().join("mac-pins.state");
    seed_state(&state, &[("wlan0", None)]);
    let resp = post_mac_pin_at(
        &cfg,
        &state,
        MacPinRequest {
            iface: "wlan0".to_string(),
            mac: Some("02-C6-75-83-1A-3E".to_string()),
            apply_now: false,
        },
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({
            "status": "ok",
            "iface": "wlan0",
            "adapterKey": WLAN0_KEY,
            "mac": "02:c6:75:83:1a:3e",
            "persisted": true,
            "appliedLive": false,
            "note": "pinned for next boot; the agent writes the .link on its next reconcile",
        })
    );
    // The normalised MAC landed in the config override, keyed by the adapter.
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        parsed
            .get("network")
            .and_then(|n| n.get("mac_pin"))
            .and_then(|m| m.get("overrides"))
            .and_then(|o| o.get(WLAN0_KEY))
            .and_then(serde_norway::Value::as_str),
        Some("02:c6:75:83:1a:3e")
    );
}

#[tokio::test]
async fn an_empty_body_mac_falls_back_to_the_state_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let state = dir.path().join("mac-pins.state");
    seed_state(&state, &[("wlan0", Some("AA:BB:CC:DD:EE:FF"))]);
    let resp = post_mac_pin_at(
        &cfg,
        &state,
        MacPinRequest {
            iface: "wlan0".to_string(),
            mac: Some("   ".to_string()),
            apply_now: false,
        },
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    // The candidate is normalised on resolve.
    assert_eq!(body["mac"], json!("aa:bb:cc:dd:ee:ff"));
    assert_eq!(body["appliedLive"], json!(false));
}

#[tokio::test]
async fn apply_now_without_the_config_flag_refuses_with_the_not_permitted_note() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let state = dir.path().join("mac-pins.state");
    seed_state(&state, &[("wlan0", None), ("wlan9", None)]);
    let resp = post_mac_pin_at(
        &cfg,
        &state,
        MacPinRequest {
            iface: "wlan9".to_string(),
            mac: Some("02:c6:75:83:1a:3e".to_string()),
            apply_now: true,
        },
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["appliedLive"], json!(false));
    assert_eq!(
            body["note"],
            json!("live re-tag not permitted (set network.mac_pin.apply_live_allowed=true); pinned for next boot")
        );
}

// ── apply_now_outcome gate decisions (no ip IO on these paths) ─────────────

#[tokio::test]
async fn apply_now_outcome_refuses_when_the_flag_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    // No flag → refused, not permitted. A determinable management interface
    // is passed in so the assertion is that the flag gate wins first.
    let out = apply_now_outcome(&cfg, "wlan0", "02:c6:75:83:1a:3e", Some("eth0")).await;
    assert!(!out.applied_live);
    assert!(out.note.contains("not permitted"));
}

/// The management-interface gate, both branches. This used to be guarded on
/// `default_route_iface().is_none()`, so on any host that HAS a default route
/// it executed no assertions at all. The interface is now an injected
/// parameter, so both refusals are asserted unconditionally.
///
/// The third branch (a determinable mgmt iface that is NOT the named one)
/// is deliberately not exercised here: it proceeds to `apply_live`, which on
/// Linux would re-tag a real interface.
#[tokio::test]
async fn apply_now_outcome_refuses_on_both_mgmt_iface_branches() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "network:\n  mac_pin:\n    apply_live_allowed: true\n").unwrap();

    // Undeterminable route → "uncertain", refuse for safety.
    let undeterminable = apply_now_outcome(&cfg, "wlan0", "02:c6:75:83:1a:3e", None).await;
    assert!(!undeterminable.applied_live);
    assert!(undeterminable
        .note
        .contains("could not determine the management interface"));

    // The named interface IS the management interface → refuse rather than
    // drop the operator's own connection.
    let is_mgmt = apply_now_outcome(&cfg, "wlan0", "02:c6:75:83:1a:3e", Some("wlan0")).await;
    assert!(!is_mgmt.applied_live);
    assert!(is_mgmt
        .note
        .contains("refusing to re-tag wlan0 live: it carries the management route"));
}

#[test]
fn parse_route_get_dev_reads_the_token_after_dev() {
    // The real single-line answer.
    assert_eq!(
        parse_route_get_dev("1.1.1.1 via 192.168.1.1 dev eth0 src 192.168.1.50 uid 0\n").as_deref(),
        Some("eth0")
    );
    // No `dev` token, and a trailing `dev` with nothing after it, are both
    // "undeterminable" — which the gate turns into a refusal.
    assert_eq!(parse_route_get_dev("RTNETLINK answers: unreachable"), None);
    assert_eq!(parse_route_get_dev(""), None);
    assert_eq!(parse_route_get_dev("1.1.1.1 via 192.168.1.1 dev"), None);
}

// ── DELETE handler ────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_removes_a_present_override_and_link_and_reports_both_true() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let state = dir.path().join("mac-pins.state");
    seed_state(&state, &[("wlan0", None)]);
    let netd = dir.path().join("networkd");
    std::fs::create_dir_all(&netd).unwrap();
    std::fs::write(
        &cfg,
        format!("network:\n  mac_pin:\n    overrides:\n      '{WLAN0_KEY}': 02:c6:75:83:1a:3e\n"),
    )
    .unwrap();
    // Seed a .link file at the engine's canonical name so the remove reports true.
    let link = netd.join(ados_macpin::engine::link_file_name(WLAN0_KEY));
    std::fs::write(
        &link,
        "[Match]\nPath=platform-xhci-usb-0:1.0:1.0\n[Link]\nMACAddress=02:c6:75:83:1a:3e\n",
    )
    .unwrap();

    let resp = delete_mac_pin_at(&cfg, &state, &netd, "wlan0").await;
    let body = body_json(resp).await;
    assert_eq!(body["status"], json!("ok"));
    assert_eq!(body["iface"], json!("wlan0"));
    assert_eq!(body["adapterKey"], json!(WLAN0_KEY));
    assert_eq!(body["removedOverride"], json!(true));
    assert_eq!(body["removedLinkFile"], json!(true));
    assert_eq!(
            body["note"],
            json!("a known no-efuse adapter is re-pinned automatically unless network.mac_pin.enabled is false")
        );
    // The .link file is gone.
    assert!(!link.exists());
}

#[tokio::test]
async fn delete_of_an_absent_override_and_link_reports_both_false() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let state = dir.path().join("mac-pins.state");
    seed_state(&state, &[("wlan0", None)]);
    let netd = dir.path().join("networkd");
    std::fs::create_dir_all(&netd).unwrap();
    std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();

    let resp = delete_mac_pin_at(&cfg, &state, &netd, "wlan0").await;
    let body = body_json(resp).await;
    assert_eq!(body["removedOverride"], json!(false));
    assert_eq!(body["removedLinkFile"], json!(false));
}

#[tokio::test]
async fn delete_reports_a_link_that_exists_but_cannot_be_removed() {
    // A `.link` path that cannot be unlinked (here a directory standing at the
    // canonical name, which refuses remove_file even for root, like the
    // read-only networkd dir a sandbox imposes) must surface its error, not
    // collapse into `removedLinkFile: false` as if no file existed.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let state = dir.path().join("mac-pins.state");
    seed_state(&state, &[("wlan0", None)]);
    let netd = dir.path().join("networkd");
    let link = netd.join(ados_macpin::engine::link_file_name(WLAN0_KEY));
    std::fs::create_dir_all(&link).unwrap();
    std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();

    let resp = delete_mac_pin_at(&cfg, &state, &netd, "wlan0").await;
    let body = body_json(resp).await;
    assert_eq!(body["removedLinkFile"], json!(false));
    assert!(
        body["link_error"].as_str().is_some_and(|e| !e.is_empty()),
        "a surviving .link must be reported: {body}"
    );
    assert!(link.exists());
}

// ── shared body helpers ───────────────────────────────────────────────────

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
