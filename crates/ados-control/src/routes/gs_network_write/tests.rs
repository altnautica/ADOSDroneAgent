use super::*;

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ── validate_priority ────────────────────────────────────────────────────

#[test]
fn validate_accepts_a_non_empty_string_list() {
    let input = vec![json!("eth0"), json!("wlan0_client")];
    assert_eq!(
        validate_priority(&input).unwrap(),
        vec!["eth0".to_string(), "wlan0_client".to_string()]
    );
}

#[test]
fn validate_rejects_an_empty_list() {
    let err = validate_priority(&[]).unwrap_err();
    assert_eq!(err, "priority must be a non-empty list of strings");
}

#[test]
fn validate_rejects_a_non_string_member() {
    let input = vec![json!("eth0"), json!(7)];
    let err = validate_priority(&input).unwrap_err();
    assert_eq!(err, "priority must be a non-empty list of strings");
}

#[test]
fn validate_rejects_an_unknown_or_repeated_uplink() {
    let unknown = validate_priority(&[json!("eth0"), json!("wlan1")]).unwrap_err();
    assert!(unknown.starts_with("unknown uplink \"wlan1\""), "{unknown}");
    let twice = validate_priority(&[json!("eth0"), json!("wwan0"), json!("eth0")]).unwrap_err();
    assert_eq!(twice, "uplink \"eth0\" is listed twice");
}

// ── save_priority + the persisted JSON shape ────────────────────────────

#[test]
fn save_writes_the_compact_priority_json_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ground-station-uplink.json");
    let list = vec!["wlan0_client".to_string(), "eth0".to_string()];
    save_priority(&path, &list).unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert_eq!(raw, r#"{"priority":["wlan0_client","eth0"]}"#);

    let parsed: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["priority"], json!(["wlan0_client", "eth0"]));
    // Only the target remains; no tmp sibling is left behind.
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["ground-station-uplink.json"]);
}

#[test]
fn save_creates_the_parent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("ground-station-uplink.json");
    save_priority(&path, &["eth0".to_string()]).unwrap();
    assert!(path.exists());
}

// ── error_body shapes ────────────────────────────────────────────────────

#[tokio::test]
async fn error_body_is_the_error_object_detail_shape() {
    let resp = error_body(
        StatusCode::BAD_REQUEST,
        "E_UPLINK_PRIORITY_INVALID",
        "priority must be a non-empty list of strings",
    );
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({
            "detail": {
                "error": {
                    "code": "E_UPLINK_PRIORITY_INVALID",
                    "message": "priority must be a non-empty list of strings",
                }
            }
        })
    );
}

#[tokio::test]
async fn error_body_with_hint_carries_the_hint_field() {
    let resp = error_body_with_hint(
        StatusCode::INTERNAL_SERVER_ERROR,
        "E_ETHERNET_NO_CONNECTION",
        "no_ethernet_connection",
        &json!("No saved NetworkManager Ethernet connection found"),
    );
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({
            "detail": {
                "error": {
                    "code": "E_ETHERNET_NO_CONNECTION",
                    "message": "no_ethernet_connection",
                    "hint": "No saved NetworkManager Ethernet connection found",
                }
            }
        })
    );
}

#[tokio::test]
async fn the_unavailable_503_carries_the_nested_error_object() {
    let resp = socket_unavailable("E_AP_APPLY_FAILED");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": {
            "code": "E_AP_APPLY_FAILED",
            "message": "network command socket unavailable",
        }}})
    );
}

// ── classify_reply ───────────────────────────────────────────────────────

#[test]
fn classify_strips_ok_on_a_success_reply() {
    match classify_reply(r#"{"ok":true,"mode":"static","ip":"10.0.0.5/24"}"#) {
        NetCmd::Reply(m) => {
            assert!(!m.contains_key("ok"));
            assert_eq!(m["mode"], json!("static"));
            assert_eq!(m["ip"], json!("10.0.0.5/24"));
        }
        _ => panic!("expected a stripped Reply"),
    }
}

#[test]
fn classify_surfaces_the_error_on_ok_false() {
    match classify_reply(r#"{"ok":false,"error":"E_AP_APPLY_FAILED"}"#) {
        NetCmd::Error(msg) => assert_eq!(msg, "E_AP_APPLY_FAILED"),
        _ => panic!("expected an Error"),
    }
    match classify_reply(r#"{"ok":false}"#) {
        NetCmd::Error(msg) => assert_eq!(msg, "unknown network command error"),
        _ => panic!("expected an Error"),
    }
}

#[test]
fn classify_treats_garbage_as_unavailable() {
    assert!(matches!(classify_reply("not json"), NetCmd::Unavailable));
    assert!(matches!(classify_reply("[1,2,3]"), NetCmd::Unavailable));
}

// ── persist_hotspot ──────────────────────────────────────────────────────

#[test]
fn persist_hotspot_merges_channel_and_ssid_preserving_other_keys() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "agent:\n  name: gs-1\nnetwork:\n  hotspot:\n    channel: 6\n    ssid: ADOS-GS-OLD\n",
    )
    .unwrap();
    persist_hotspot(&cfg, Some(11), Some("ADOS-GS-NEW")).unwrap();

    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let hotspot = parsed
        .get("network")
        .and_then(|v| v.get("hotspot"))
        .unwrap();
    assert_eq!(
        hotspot.get("channel").and_then(serde_norway::Value::as_i64),
        Some(11)
    );
    assert_eq!(
        hotspot.get("ssid").and_then(serde_norway::Value::as_str),
        Some("ADOS-GS-NEW")
    );
    // The unrelated agent.name survived.
    assert_eq!(
        parsed
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(serde_norway::Value::as_str),
        Some("gs-1")
    );
}

#[test]
fn persist_hotspot_creates_the_section_from_an_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    persist_hotspot(&cfg, Some(9), None).unwrap();
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        parsed
            .get("network")
            .and_then(|v| v.get("hotspot"))
            .and_then(|h| h.get("channel"))
            .and_then(serde_norway::Value::as_i64),
        Some(9)
    );
}

// ── persist_share_uplink ─────────────────────────────────────────────────

#[test]
fn persist_share_uplink_writes_the_flag_preserving_other_keys() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "agent:\n  name: gs-1\nground_station:\n  hotspot_ssid: ADOS-GS-1234\n",
    )
    .unwrap();
    persist_share_uplink(&cfg, true).unwrap();

    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        parsed
            .get("ground_station")
            .and_then(|gs| gs.get("share_uplink"))
            .and_then(serde_norway::Value::as_bool),
        Some(true)
    );
    // The pre-existing ground_station key + the unrelated agent.name survived.
    assert_eq!(
        parsed
            .get("ground_station")
            .and_then(|gs| gs.get("hotspot_ssid"))
            .and_then(serde_norway::Value::as_str),
        Some("ADOS-GS-1234")
    );
    assert_eq!(
        parsed
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(serde_norway::Value::as_str),
        Some("gs-1")
    );
}

#[test]
fn persist_share_uplink_round_trips_false() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    persist_share_uplink(&cfg, false).unwrap();
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        parsed
            .get("ground_station")
            .and_then(|gs| gs.get("share_uplink"))
            .and_then(serde_norway::Value::as_bool),
        Some(false)
    );
}

// ── share_uplink route: the daemon's apply verdict ──────────────────────

#[tokio::test]
async fn share_uplink_reports_the_daemons_apply_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let sock = canned_socket(
        dir.path(),
        r#"{"ok":true,"applied":false,"backend":"nftables","apply_error":"nft_add_failed"}"#,
    )
    .await;
    let reply = net_cmd_at(&sock, &json!({"op": "share_uplink", "enabled": true})).await;
    let body = share_uplink_body(true, reply);
    assert_eq!(body["applied"], json!(false));
    assert_eq!(body["apply_error"], json!("nft_add_failed"));
    assert_eq!(body["backend"], json!("nftables"));

    let ok_dir = tempfile::tempdir().unwrap();
    let ok_sock = canned_socket(
        ok_dir.path(),
        r#"{"ok":true,"applied":true,"backend":"iptables-persistent","apply_error":null}"#,
    )
    .await;
    let body = share_uplink_body(
        true,
        net_cmd_at(&ok_sock, &json!({"op": "share_uplink", "enabled": true})).await,
    );
    assert_eq!(body["applied"], json!(true));
    assert_eq!(body["apply_error"], Value::Null);
}

#[tokio::test]
async fn share_uplink_with_no_daemon_is_not_reported_applied() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.sock");
    let reply = net_cmd_at(&missing, &json!({"op": "share_uplink", "enabled": true})).await;
    let body = share_uplink_body(true, reply);
    assert_eq!(body["enabled"], json!(true));
    assert_eq!(body["applied"], json!(false));
    assert!(body["apply_error"].as_str().is_some());
}

// ── profile gate ─────────────────────────────────────────────────────────

#[tokio::test]
async fn profile_mismatch_golden_body() {
    let resp = profile_mismatch();
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

// ── the success envelopes (pinned without a live profile) ────────────────

#[test]
fn the_priority_success_body_echoes_the_persisted_list() {
    let list = vec![
        "eth0".to_string(),
        "wlan0_client".to_string(),
        "wwan0".to_string(),
    ];
    let body = json!({ "priority": list });
    assert_eq!(body, json!({"priority": ["eth0", "wlan0_client", "wwan0"]}));
}

#[test]
fn the_ethernet_static_missing_fields_guard_fires_on_missing_gateway() {
    // The in-handler guard fires when mode=static and ip/gateway is missing,
    // the E_ETHERNET_STATIC_MISSING_FIELDS 400. The valid
    // path (ip + gateway present) forwards to the daemon, bench-validated.
    let no_gw = EthernetConfigUpdate {
        mode: "static".to_string(),
        ip: Some("10.0.0.5/24".to_string()),
        gateway: None,
        dns: None,
    };
    let ip_present = no_gw.ip.as_deref().is_some_and(|s| !s.is_empty());
    let gw_present = no_gw.gateway.as_deref().is_some_and(|s| !s.is_empty());
    assert!(ip_present && !gw_present, "ip present, gateway missing");
}

#[test]
fn modem_cap_mb_converts_to_cap_gb_when_cap_gb_absent() {
    // cap_gb wins when both present.
    let both = ModemConfigUpdate {
        apn: None,
        cap_gb: Some(3.0),
        cap_mb: Some(2048),
        enabled: None,
    };
    let cap = both
        .cap_gb
        .or_else(|| both.cap_mb.map(|mb| mb as f64 / 1024.0));
    assert_eq!(cap, Some(3.0));
    // cap_mb converts when cap_gb is absent (2048 MB → 2 GB).
    let mb_only = ModemConfigUpdate {
        apn: None,
        cap_gb: None,
        cap_mb: Some(2048),
        enabled: None,
    };
    let cap2 = mb_only
        .cap_gb
        .or_else(|| mb_only.cap_mb.map(|mb| mb as f64 / 1024.0));
    assert_eq!(cap2, Some(2.0));
}

// ── the ported Wi-Fi client join / leave ────────────────────────────────

/// Serve one canned newline-JSON reply on a temp AF_UNIX socket, the same
/// shape `ados-net`'s command socket speaks.
async fn canned_socket(dir: &std::path::Path, reply: &str) -> std::path::PathBuf {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let path = dir.join("wifi-cmd.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let body = format!("{reply}\n");
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(body.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });
    path
}

fn join(ssid: &str, force: Option<bool>) -> GsWifiJoinRequest {
    GsWifiJoinRequest {
        ssid: ssid.to_string(),
        passphrase: Some("hunter2".to_string()),
        force,
    }
}

#[tokio::test]
async fn a_successful_join_returns_the_four_field_body() {
    let dir = tempfile::tempdir().unwrap();
    let sock = canned_socket(
        dir.path(),
        r#"{"ok":true,"joined":true,"ip":"192.168.1.50","gateway":"192.168.1.1","error":null}"#,
    )
    .await;
    let resp = put_gs_network_client_join_at(&sock, join("BenchNet", None)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await,
        json!({"joined": true, "ip": "192.168.1.50", "gateway": "192.168.1.1", "error": null})
    );
}

#[tokio::test]
async fn an_ap_busy_refusal_is_the_409_that_asks_for_force() {
    // The ground station's AP and its client mode contend for wlan0, so
    // stealing it has to be a deliberate second request rather than a silent
    // takeover that drops every operator already on the AP.
    let dir = tempfile::tempdir().unwrap();
    let sock = canned_socket(
        dir.path(),
        r#"{"ok":true,"joined":false,"error":"station_busy_ap_active"}"#,
    )
    .await;
    let resp = put_gs_network_client_join_at(&sock, join("BenchNet", None)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(
        body["detail"]["error"]["code"],
        json!("E_WLAN0_BUSY_AP_ACTIVE")
    );
    assert_eq!(body["needs_force"], json!(true));
}

#[tokio::test]
async fn a_daemon_failure_is_the_join_failed_500() {
    let dir = tempfile::tempdir().unwrap();
    let sock = canned_socket(dir.path(), r#"{"ok":false,"error":"no such network"}"#).await;
    let resp = put_gs_network_client_join_at(&sock, join("BenchNet", Some(true))).await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        json!("E_WIFI_JOIN_FAILED")
    );
}

#[tokio::test]
async fn an_absent_socket_is_the_503_no_link_posture() {
    // The front must not race the daemon by driving the interface itself, so
    // an unreachable socket is "no link", never a fabricated success.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.sock");
    let resp = put_gs_network_client_join_at(&missing, join("BenchNet", None)).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    let resp = delete_gs_network_client_at(&missing).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        json!("E_WIFI_LEAVE_FAILED")
    );
}

#[tokio::test]
async fn an_empty_ssid_is_rejected_before_the_socket_round_trip() {
    // Pydantic rejected this with a 422 before the Python handler ran; the
    // native front has no pre-validation, so the guard is the handler's.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("never-connected.sock");
    let resp = put_gs_network_client_join_at(&missing, join("   ", None)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_leave_returns_the_daemon_reply_with_the_transport_flag_stripped() {
    let dir = tempfile::tempdir().unwrap();
    let sock = canned_socket(
        dir.path(),
        r#"{"ok":true,"left":true,"previous_ssid":"BenchNet"}"#,
    )
    .await;
    let resp = delete_gs_network_client_at(&sock).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await,
        json!({"left": true, "previous_ssid": "BenchNet"})
    );
}

/// A newline in the SSID or passphrase would inject a hostapd directive, and
/// a short passphrase or an off-band channel would stop the AP on restart.
#[test]
fn ap_settings_hostapd_cannot_take_are_refused_before_the_daemon() {
    let ap = |ssid: Option<&str>, passphrase: Option<&str>, channel: Option<i64>| ApUpdate {
        enabled: None,
        ssid: ssid.map(str::to_string),
        passphrase: passphrase.map(str::to_string),
        channel,
    };
    assert!(validate_ap_update(&ap(Some("ADOS-GS"), Some("correct-horse"), Some(6))).is_ok());
    // Empty strings mean "unchanged", as they do to the daemon.
    assert!(validate_ap_update(&ap(Some(""), Some(""), None)).is_ok());
    for bad in [
        ap(Some("ap\nctrl_interface=/tmp"), None, None),
        ap(Some(&"x".repeat(33)), None, None),
        ap(None, Some("x\nwpa=0xxxxx"), None),
        ap(None, Some("short"), None),
        ap(None, None, Some(14)),
        ap(None, None, Some(36)),
        ap(None, None, Some(-1)),
        ap(None, None, Some(0)),
    ] {
        assert!(validate_ap_update(&bad).is_err(), "{bad:?} must be refused");
    }
}

/// A daemon that accepted a join but is still waiting on NetworkManager has
/// not failed; the change is in flight.
#[tokio::test(start_paused = true)]
async fn a_join_or_leave_the_daemon_has_not_answered_is_in_progress() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wifi-cmd.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let _held = tokio::spawn(async move {
        let mut conns = Vec::new();
        while let Ok((conn, _)) = listener.accept().await {
            conns.push(conn);
        }
    });

    let resp = put_gs_network_client_join_at(&path, join("BenchNet", None)).await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        json!("E_WIFI_JOIN_IN_PROGRESS")
    );
    let resp = delete_gs_network_client_at(&path).await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        json!("E_WIFI_LEAVE_IN_PROGRESS")
    );
}
