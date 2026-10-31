use super::*;

fn reply(line: &[u8]) -> Value {
    match parse_command(line) {
        Parsed::Reply(v) => v,
        Parsed::Cmd(c) => panic!("expected an early reply, got command {c:?}"),
    }
}

fn cmd(line: &[u8]) -> Command {
    match parse_command(line) {
        Parsed::Cmd(c) => c,
        Parsed::Reply(v) => panic!("expected a command, got reply {v}"),
    }
}

#[test]
fn bad_json_is_rejected_before_any_manager_access() {
    let v = reply(b"not json");
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
}

#[test]
fn unknown_op_is_rejected() {
    let v = reply(br#"{"op":"frob"}"#);
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().starts_with("E_UNKNOWN_OP"));
}

#[test]
fn join_requires_a_nonempty_ssid() {
    assert_eq!(reply(br#"{"op":"wifi_join"}"#)["error"], "E_MISSING_SSID");
    assert_eq!(
        reply(br#"{"op":"wifi_join","ssid":""}"#)["error"],
        "E_MISSING_SSID"
    );
    // An open network (no passphrase) parses; an empty passphrase normalises
    // to None so the manager treats it as open, not as a blank key.
    assert_eq!(
        cmd(br#"{"op":"wifi_join","ssid":"Open"}"#),
        Command::Join {
            ssid: "Open".to_string(),
            passphrase: None,
            force: false,
        }
    );
    assert_eq!(
        cmd(br#"{"op":"wifi_join","ssid":"Net","passphrase":"","force":true}"#),
        Command::Join {
            ssid: "Net".to_string(),
            passphrase: None,
            force: true,
        }
    );
    assert_eq!(
        cmd(br#"{"op":"wifi_join","ssid":"Net","passphrase":"pw"}"#),
        Command::Join {
            ssid: "Net".to_string(),
            passphrase: Some("pw".to_string()),
            force: false,
        }
    );
}

#[test]
fn forget_requires_a_nonempty_name() {
    assert_eq!(reply(br#"{"op":"wifi_forget"}"#)["error"], "E_MISSING_NAME");
    assert_eq!(
        reply(br#"{"op":"wifi_forget","name":""}"#)["error"],
        "E_MISSING_NAME"
    );
    assert_eq!(
        cmd(br#"{"op":"wifi_forget","name":"Net"}"#),
        Command::Forget {
            name: "Net".to_string()
        }
    );
}

#[test]
fn leave_and_status_parse_with_no_fields() {
    assert_eq!(cmd(br#"{"op":"wifi_leave"}"#), Command::Leave);
    assert_eq!(cmd(br#"{"op":"wifi_status"}"#), Command::Status);
}

// ── wifi_autoconnect parse ───────────────────────────────────────────────

#[test]
fn autoconnect_requires_name_and_enabled() {
    // Missing `enabled` is malformed.
    assert_eq!(
        reply(br#"{"op":"wifi_autoconnect","name":"Net"}"#)["error"],
        "E_MISSING_ENABLED"
    );
    // Missing `name` is malformed.
    assert_eq!(
        reply(br#"{"op":"wifi_autoconnect","enabled":true}"#)["error"],
        "E_MISSING_NAME"
    );
    // An empty name is forwarded (the manager replies name_required).
    assert_eq!(
        cmd(br#"{"op":"wifi_autoconnect","name":"","enabled":false}"#),
        Command::Autoconnect {
            name: "".to_string(),
            enabled: false,
        }
    );
    assert_eq!(
        cmd(br#"{"op":"wifi_autoconnect","name":"HomeNet","enabled":true}"#),
        Command::Autoconnect {
            name: "HomeNet".to_string(),
            enabled: true,
        }
    );
}

// ── ap_config parse ──────────────────────────────────────────────────────

#[test]
fn ap_config_parses_optional_fields_normalising_blanks() {
    // An all-fields request.
    assert_eq!(
        cmd(br#"{"op":"ap_config","ssid":"ADOS-GS-1234","passphrase":"pw","channel":11,"enabled":true}"#),
        Command::ApConfig {
            ssid: Some("ADOS-GS-1234".to_string()),
            passphrase: Some("pw".to_string()),
            channel: Some(11),
            enabled: Some(true),
        }
    );
    // Blank ssid + passphrase normalise to None (the manager leaves them
    // untouched), and the absent fields are None.
    assert_eq!(
        cmd(br#"{"op":"ap_config","ssid":"","passphrase":""}"#),
        Command::ApConfig {
            ssid: None,
            passphrase: None,
            channel: None,
            enabled: None,
        }
    );
}

// ── eth_config parse ─────────────────────────────────────────────────────

#[test]
fn eth_config_static_carries_ip_gateway_dns() {
    assert_eq!(
        cmd(br#"{"op":"eth_config","mode":"static","ip":"10.0.0.5/24","gateway":"10.0.0.1","dns":["8.8.8.8","1.1.1.1"]}"#),
        Command::EthStatic {
            ip: "10.0.0.5/24".to_string(),
            gateway: "10.0.0.1".to_string(),
            dns: vec!["8.8.8.8".to_string(), "1.1.1.1".to_string()],
        }
    );
    // Missing ip/gateway/dns default to empty (the REST layer rejects the
    // missing-fields case before forwarding, but the parse stays tolerant).
    assert_eq!(
        cmd(br#"{"op":"eth_config","mode":"static"}"#),
        Command::EthStatic {
            ip: "".to_string(),
            gateway: "".to_string(),
            dns: Vec::new(),
        }
    );
}

#[test]
fn eth_config_dhcp_and_any_other_mode_is_dhcp() {
    assert_eq!(
        cmd(br#"{"op":"eth_config","mode":"dhcp"}"#),
        Command::EthDhcp
    );
    // No mode → dhcp (matches the Python `else`).
    assert_eq!(cmd(br#"{"op":"eth_config"}"#), Command::EthDhcp);
    // An unrecognised mode → dhcp too.
    assert_eq!(
        cmd(br#"{"op":"eth_config","mode":"weird"}"#),
        Command::EthDhcp
    );
}

// ── modem_config parse ───────────────────────────────────────────────────

#[test]
fn modem_config_parses_optional_fields() {
    assert_eq!(
        cmd(br#"{"op":"modem_config","apn":"internet","cap_gb":5.0,"enabled":true}"#),
        Command::ModemConfig {
            apn: Some("internet".to_string()),
            cap_gb: Some(5.0),
            enabled: Some(true),
        }
    );
    assert_eq!(
        cmd(br#"{"op":"modem_config"}"#),
        Command::ModemConfig {
            apn: None,
            cap_gb: None,
            enabled: None,
        }
    );
}

#[test]
fn share_uplink_requires_the_enabled_flag() {
    assert_eq!(
        cmd(br#"{"op":"share_uplink","enabled":true}"#),
        Command::ShareUplink { enabled: true }
    );
    assert_eq!(
        reply(br#"{"op":"share_uplink"}"#)["error"],
        "E_MISSING_ENABLED"
    );
}

// ── view shapers ─────────────────────────────────────────────────────────

#[test]
fn ap_view_reshapes_status_into_the_view_body() {
    let status = json!({
        "running": true,
        "ssid": "ADOS-GS-1234",
        "channel": 6,
        "interface": "wlan0",
        "gateway": "192.168.4.1",
        "connected_clients": ["aa:bb:cc:dd:ee:ff"],
    });
    assert_eq!(
        ap_view(&status),
        json!({
            "ok": true,
            "enabled": true,
            "running": true,
            "ssid": "ADOS-GS-1234",
            "channel": 6,
            "interface": "wlan0",
            "gateway": "192.168.4.1",
            "connected_clients": ["aa:bb:cc:dd:ee:ff"],
        })
    );
    // A not-running status reports enabled=false and a null gateway, the empty
    // client list defaulting when absent.
    let down = json!({
        "running": false,
        "ssid": "ADOS-GS-1234",
        "channel": 6,
        "interface": "wlan0",
        "gateway": Value::Null,
        "connected_clients": [],
    });
    let v = ap_view(&down);
    assert_eq!(v["enabled"], json!(false));
    assert_eq!(v["running"], json!(false));
    assert_eq!(v["gateway"], Value::Null);
    assert_eq!(v["connected_clients"], json!([]));
}

#[test]
fn with_ok_marks_a_processed_result_ok_and_preserves_fields() {
    // A processed command is ok:true regardless of the join outcome — the
    // `joined` field carries success, which the REST layer inspects.
    let joined = with_ok(json!({"joined": true, "ip": "1.2.3.4"}));
    assert_eq!(joined["ok"], true);
    assert_eq!(joined["ip"], "1.2.3.4");
    let failed = with_ok(json!({"joined": false, "error": "station_busy_ap_active"}));
    assert_eq!(failed["ok"], true);
    assert_eq!(failed["error"], "station_busy_ap_active");
    // A non-object result is a hard transport error.
    assert_eq!(with_ok(json!("oops"))["ok"], false);
}

#[test]
fn an_empty_line_is_a_bad_request_not_a_panic() {
    let v = reply(b"");
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
}

fn lone_wifi_manager(dir: &std::path::Path) -> Mutex<WifiClientManager> {
    let runner: Arc<dyn crate::cmd::CmdRunner> =
        Arc::new(crate::cmd::testing::ScriptedRunner::new());
    Mutex::new(WifiClientManager::with_paths(
        "wlan0",
        runner,
        dir.join("ados-wlan0.lock"),
        dir.join("ap-was-up"),
        dir.join("wifi-client.json"),
    ))
}

#[tokio::test]
async fn a_lone_wifi_manager_serves_the_wifi_ops() {
    let dir = tempfile::tempdir().unwrap();
    let wifi = lone_wifi_manager(dir.path());
    let st = dispatch_wifi_client(br#"{"op":"wifi_status"}"#, &wifi).await;
    assert_eq!(st["ok"], true);
    assert!(st.get("connected").is_some(), "{st}");
    // A malformed request is still refused before any manager work.
    let bad = dispatch_wifi_client(br#"{"op":"wifi_join"}"#, &wifi).await;
    assert_eq!(bad["error"], "E_MISSING_SSID");
}

#[tokio::test]
async fn a_lone_wifi_manager_refuses_the_uplink_daemon_ops() {
    let dir = tempfile::tempdir().unwrap();
    let wifi = lone_wifi_manager(dir.path());
    for line in [
        &br#"{"op":"ap_config","enabled":true}"#[..],
        &br#"{"op":"eth_config","mode":"dhcp"}"#[..],
        &br#"{"op":"modem_config","enabled":false}"#[..],
    ] {
        let v = dispatch_wifi_client(line, &wifi).await;
        assert_eq!(v["ok"], false);
        assert!(
            v["error"].as_str().unwrap().starts_with("E_UNSUPPORTED_OP"),
            "{v}"
        );
    }
}
