use super::*;

/// The exact aggregate-view shape the FastAPI `/network` route returns on a
/// fresh ground station with no config, no command socket, and no store: the
/// manager-absent defaults for every leg. The AP leg's running flag is the
/// live probe, so the fixture composes the static legs over an explicit
/// not-running AP rather than calling the `systemctl`/`iw` probe.
#[test]
fn network_aggregate_default_shape_is_the_golden_fixture() {
    let cfg = json!({});
    // The AP leg as composed when the live probe reports the unit down: the
    // resolved SSID + default channel + default interface, with the gateway +
    // clients gated off.
    let ap = ap_view_compose(
        "ADOS-GS-0000",
        AP_DEFAULT_CHANNEL,
        AP_IFACE,
        Some(false),
        Vec::new(),
    );
    let priority = priority_list_from(None);
    let share = share_uplink_flag(&cfg);

    // Compose the body the way the handler does, for the legs that do not need
    // async seams (wifi_client / modem / active_uplink degrade independently;
    // tested separately).
    let body = json!({
        "ap": ap,
        "priority": priority,
        "share_uplink": share,
    });
    let want = json!({
        "ap": {
            "enabled": false,
            "running": false,
            // An empty config has no device id, so the SSID resolves to the
            // zero-padded short id, matching the live hostapd manager over a
            // fresh box (NOT the raw `ADOS-{device_id}` template).
            "ssid": "ADOS-GS-0000",
            // The channel is the manager's default (6), never null.
            "channel": 6,
            // The interface is the manager's default AP interface, never null.
            "interface": "wlan0",
            // The gateway + clients are gated off while the AP is down.
            "gateway": null,
            "connected_clients": [],
        },
        "priority": ["eth0", "wlan0_client", "wwan0", "usb0"],
        "share_uplink": false,
    });
    assert_eq!(body, want);
}

#[test]
fn ap_view_compose_running_shape_carries_gateway_and_clients() {
    // The live shape the bench observed: a running AP reports enabled +
    // running true, the resolved SSID, channel, interface, the 192.168.4.1
    // gateway, and the associated station MAC.
    let view = ap_view_compose(
        "ADOS-GS-D9DB",
        6,
        "wlan0",
        Some(true),
        vec!["dc:ea:e7:30:74:a6".to_string()],
    );
    let want = json!({
        "enabled": true,
        "running": true,
        "ssid": "ADOS-GS-D9DB",
        "channel": 6,
        "interface": "wlan0",
        "gateway": "192.168.4.1",
        "connected_clients": ["dc:ea:e7:30:74:a6"],
    });
    assert_eq!(view, want);
}

#[test]
fn ap_view_compose_not_running_gates_the_gateway_and_clients() {
    // A down AP reports enabled + running false, keeps the resolved SSID +
    // channel + interface, and gates the gateway to null + the clients to the
    // empty list (the manager's status reports the gateway only when up).
    let view = ap_view_compose("ADOS-GS-ABCD", 11, "wlan0", Some(false), Vec::new());
    assert_eq!(view["enabled"], json!(false));
    assert_eq!(view["running"], json!(false));
    assert_eq!(view["ssid"], json!("ADOS-GS-ABCD"));
    assert_eq!(view["channel"], json!(11));
    assert_eq!(view["interface"], json!("wlan0"));
    assert_eq!(view["gateway"], Value::Null);
    assert_eq!(view["connected_clients"], json!([]));
}

#[test]
fn an_unanswered_hostapd_probe_reads_unknown_not_down() {
    let view = ap_view_compose("ADOS-GS-ABCD", 11, "wlan0", None, Vec::new());
    assert_eq!(view["running"], Value::Null);
    assert_eq!(view["enabled"], Value::Null);
    assert_eq!(view["gateway"], Value::Null);
}

#[test]
fn ap_view_resolves_the_ssid_channel_and_interface_from_config() {
    // The SSID / channel / interface legs are sourced from config. A configured
    // ADOS-GS- SSID is honoured verbatim; the channel + interface come from the
    // hotspot section. The running flag itself is the live probe (asserted
    // separately via ap_view_compose), so this test pins only the config-sourced
    // legs.
    let cfg = json!({"network": {"hotspot": {"ssid": "ADOS-GS-ABCD", "channel": 6}}});
    let hotspot = cfg.get("network").and_then(|n| n.get("hotspot")).unwrap();
    let ssid = resolve_ap_ssid(
        hotspot.get("ssid").and_then(Value::as_str).unwrap_or(""),
        "",
    );
    let channel = hotspot
        .get("channel")
        .and_then(json_to_i64)
        .unwrap_or(AP_DEFAULT_CHANNEL);
    assert_eq!(ssid, "ADOS-GS-ABCD");
    assert_eq!(channel, 6);
    assert_eq!(ap_interface(&cfg), "wlan0");
}

#[test]
fn ap_view_resolves_the_device_id_template_to_a_built_ssid() {
    // The default hotspot SSID carries the `{device_id}` placeholder, which
    // the live hostapd manager never echoes: it builds `ADOS-GS-<short id>`
    // from the device id. The front reproduces that, so the route never
    // leaks the raw `ADOS-{device_id}` template the prior shape emitted.
    let cfg = json!({
        "agent": {"device_id": "deadbeef1234"},
        "network": {"hotspot": {"ssid": "ADOS-{device_id}", "channel": 11}},
    });
    let hotspot = cfg.get("network").and_then(|n| n.get("hotspot")).unwrap();
    let device_id = cfg
        .get("agent")
        .and_then(|a| a.get("device_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let ssid = resolve_ap_ssid(
        hotspot.get("ssid").and_then(Value::as_str).unwrap_or(""),
        device_id,
    );
    let channel = hotspot
        .get("channel")
        .and_then(json_to_i64)
        .unwrap_or(AP_DEFAULT_CHANNEL);
    assert_eq!(ssid, "ADOS-GS-DEAD");
    assert_eq!(channel, 11);
}

#[test]
fn ap_view_ignores_a_non_ados_gs_configured_ssid() {
    // A configured SSID that does not start with ADOS-GS- is NOT honored
    // (mirrors the hostapd manager's ssid_override gate); the built SSID
    // wins instead.
    let ssid = resolve_ap_ssid("MyOwnNetwork", "00ff");
    assert_eq!(ssid, "ADOS-GS-00FF");
}

#[test]
fn ap_interface_reads_the_configured_value_and_falls_back_when_it_is_blank() {
    // These assertions pin the CONFIG parsing only. On a host with no
    // /sys/class/net to enumerate -- every developer machine, and this test
    // runner -- resolution finds no candidate and the fallback answers, so
    // a passing run here says nothing about driver classification. The
    // resolver's own behaviour is pinned where it lives, in
    // ados_protocol::netif, against a fixture sysfs tree.
    assert_eq!(ap_interface(&json!({})), "wlan0");
    assert_eq!(
        ap_interface(&json!({"network": {"hotspot": {"interface": "  "}}})),
        "wlan0"
    );
    // A configured interface this box does not have cannot be classified,
    // so it is reported as asked for rather than silently replaced.
    assert_eq!(
        ap_interface(&json!({"network": {"hotspot": {"interface": "ap0"}}})),
        "ap0"
    );
}

#[test]
fn ap_view_default_channel_is_six_when_unset() {
    // No channel field → the manager's default 6, never null.
    let cfg = json!({"network": {"hotspot": {}}});
    let channel = cfg
        .get("network")
        .and_then(|n| n.get("hotspot"))
        .and_then(|h| h.get("channel"))
        .and_then(json_to_i64)
        .unwrap_or(AP_DEFAULT_CHANNEL);
    assert_eq!(channel, 6);
}

#[test]
fn parse_station_dump_extracts_lowercased_macs() {
    // The `iw … station dump` output: each station starts with a
    // `Station <mac> (on <iface>)` line; the MAC is the second token,
    // lowercased. Indented detail lines (tx bytes, signal, etc.) are ignored.
    let dump = "Station DC:EA:E7:30:74:A6 (on wlan0)\n\
                    \tinactive time:\t40 ms\n\
                    \trx bytes:\t12345\n\
                    \tsignal:  \t-42 dBm\n\
                    Station aa:bb:cc:dd:ee:ff (on wlan0)\n\
                    \ttx bytes:\t67890\n";
    assert_eq!(
        parse_station_dump(dump),
        vec![
            "dc:ea:e7:30:74:a6".to_string(),
            "aa:bb:cc:dd:ee:ff".to_string(),
        ]
    );
    // No stations → the empty list.
    assert_eq!(parse_station_dump(""), Vec::<String>::new());
    assert_eq!(
        parse_station_dump("Some unrelated output\n"),
        Vec::<String>::new()
    );
}

#[test]
fn short_id_matches_the_python_short_id() {
    // First 4 hex chars, uppercased.
    assert_eq!(short_id("deadbeef"), "DEAD");
    // Non-hex characters are stripped before taking the first 4.
    assert_eq!(short_id("xy-12-34-56"), "1234");
    // Fewer than 4 hex chars zero-pad on the right.
    assert_eq!(short_id("a1"), "A100");
    // No hex at all falls back to the all-zero placeholder.
    assert_eq!(short_id(""), "0000");
    assert_eq!(short_id("zzzz"), "0000");
}

#[test]
fn resolve_ap_ssid_gate_matches_the_hostapd_override_rule() {
    // Honored: non-empty, no template, ADOS-GS- prefix.
    assert_eq!(resolve_ap_ssid("ADOS-GS-1234", "ffff"), "ADOS-GS-1234");
    // Rejected: carries the template placeholder.
    assert_eq!(
        resolve_ap_ssid("ADOS-GS-{device_id}", "abcd"),
        "ADOS-GS-ABCD"
    );
    // Rejected: empty.
    assert_eq!(resolve_ap_ssid("", "abcd"), "ADOS-GS-ABCD");
    // Rejected: wrong prefix.
    assert_eq!(resolve_ap_ssid("Other-1234", "abcd"), "ADOS-GS-ABCD");
}

/// The configured legs are real; every measured leg this front cannot probe
/// is `null`.
///
/// The old expectations were `connected:false`, `iface:"wwan0"`,
/// `signal_quality:-1`, `technology:"unknown"`, `operator:""`,
/// `state:"disconnected"`, `data_used_mb:0`, `percent:0.0`, commented as "the
/// live manager's no-modem defaults, NOT nulls". They were exactly the wrong
/// thing: the live manager returns those after looking at a modem-less box,
/// and this front never looks — so a working modem read as disconnected on a
/// named interface with a -1 signal bar and zero data consumed.
#[test]
fn modem_body_reports_config_legs_and_nulls_every_unprobed_leg() {
    let cfg: Map<String, Value> =
        serde_json::from_value(json!({"enabled": true, "apn": "internet", "cap_gb": 2.0})).unwrap();
    let v = modem_body(&cfg, None);
    // Configured: real.
    assert_eq!(v["enabled"], json!(true));
    assert_eq!(v["apn"], json!("internet"));
    assert_eq!(v["cap_mb"], json!(2048)); // 2 GB → 2048 MB
                                          // Measured but unprobed on this front: null, with the key present.
    for leg in [
        "connected",
        "iface",
        "ip",
        "signal_quality",
        "technology",
        "operator",
        "state",
        "data_used_mb",
        "percent",
    ] {
        assert_eq!(v[leg], Value::Null, "{leg} must be null, not a default");
        assert!(
            v.as_object().unwrap().contains_key(leg),
            "{leg} must be present as an explicit null"
        );
    }
    // An empty modem config: nothing is configured, so nothing is claimed.
    let v2 = modem_body(&Map::new(), None);
    assert_eq!(v2["apn"], Value::Null);
    assert_eq!(v2["enabled"], json!(false));
    assert_eq!(
        v2["cap_mb"],
        Value::Null,
        "no configured cap is not a 0 MB cap"
    );
}

#[test]
fn modem_body_overlays_the_store_usage_block() {
    let cfg: Map<String, Value> =
        serde_json::from_value(json!({"enabled": true, "cap_gb": 1.0})).unwrap();
    let usage: Map<String, Value> =
        serde_json::from_value(json!({"data_used_mb": 512, "cap_mb": 1000, "percent": 51.234}))
            .unwrap();
    let v = modem_body(&cfg, Some(&usage));
    assert_eq!(v["data_used_mb"], json!(512));
    assert_eq!(v["cap_mb"], json!(1000)); // store cap wins over the config cap
    assert_eq!(v["percent"], json!(51.23)); // rounded to 2 decimals
                                            // The connectivity legs are still unprobed even when usage is known.
    assert_eq!(v["connected"], Value::Null);
}

#[test]
fn wifi_client_view_is_unknown_when_the_daemon_does_not_answer() {
    // The daemon is down: the station state is unknown, while the
    // enabled_on_boot config fact is still reported.
    let v = wifi_client_view_compose(None, true);
    let want = json!({
        "enabled_on_boot": true,
        "connected": null,
        "ssid": null,
        "signal": null,
        "ip": null,
    });
    assert_eq!(v, want);
}

#[test]
fn wifi_client_view_reshapes_a_status_reply() {
    // A wifi_status reply carries the full station status; the view picks the
    // four live legs + the enabled_on_boot flag from the client config.
    let status: Map<String, Value> = serde_json::from_value(json!({
        "ok": true,
        "connected": true,
        "ssid": "HomeNet",
        "bssid": "aa:bb:cc:dd:ee:ff",
        "signal": 72,
        "ip": "192.168.1.50",
        "gateway": "192.168.1.1",
        "security": "WPA2",
    }))
    .unwrap();
    let v = wifi_client_view_compose(Some(&status), true);
    let want = json!({
        "enabled_on_boot": true,
        "connected": true,
        "ssid": "HomeNet",
        "signal": 72,
        "ip": "192.168.1.50",
    });
    assert_eq!(v, want);
}

#[test]
fn priority_list_falls_back_to_the_default_chain() {
    // Absent file → the default chain.
    assert_eq!(
        priority_list_from(None),
        json!(["eth0", "wlan0_client", "wwan0", "usb0"])
    );
    // An empty list → the default chain.
    let empty: Map<String, Value> = serde_json::from_value(json!({"priority": []})).unwrap();
    assert_eq!(
        priority_list_from(Some(&empty)),
        json!(["eth0", "wlan0_client", "wwan0", "usb0"])
    );
    // A non-string member → the default chain.
    let bad: Map<String, Value> = serde_json::from_value(json!({"priority": ["eth0", 7]})).unwrap();
    assert_eq!(
        priority_list_from(Some(&bad)),
        json!(["eth0", "wlan0_client", "wwan0", "usb0"])
    );
}

#[test]
fn priority_list_honours_a_valid_custom_list() {
    let custom: Map<String, Value> =
        serde_json::from_value(json!({"priority": ["wlan0_client", "eth0"]})).unwrap();
    assert_eq!(
        priority_list_from(Some(&custom)),
        json!(["wlan0_client", "eth0"])
    );
}

#[test]
fn share_uplink_flag_reads_the_config_and_defaults_false() {
    assert!(!share_uplink_flag(&json!({})));
    assert!(!share_uplink_flag(&json!({"ground_station": {}})));
    assert!(share_uplink_flag(
        &json!({"ground_station": {"share_uplink": true}})
    ));
    assert!(!share_uplink_flag(
        &json!({"ground_station": {"share_uplink": false}})
    ));
}

/// With no wired port nothing is claimed: the cable state stays unknown
/// rather than reading as unplugged. A live port's legs are the readings.
#[test]
fn ethernet_body_reports_the_live_port_and_claims_nothing_without_one() {
    let none = ethernet_body(Value::Null, None);
    assert_eq!(none["connection_name"], Value::Null);
    assert_eq!(none["link"], Value::Null);
    assert_eq!(none["current_ip"], Value::Null);

    let port = WiredLink {
        iface: "end1".into(),
        link: Some(true),
        speed_mbps: Some(1000),
        ip: Some("192.168.1.50".into()),
        gateway: Some("192.168.1.1".into()),
    };
    let body = ethernet_body(json!("netplan-end1"), Some(&port));
    assert_eq!(body["link"], json!(true));
    assert_eq!(body["speed_mbps"], json!(1000));
    assert_eq!(body["current_ip"], json!("192.168.1.50"));
    assert_eq!(body["current_gateway"], json!("192.168.1.1"));
    assert_eq!(body["mode"], Value::Null, "the saved profile is not read");
}

#[test]
fn ip_output_parses_to_address_and_gateway() {
    assert_eq!(
        parse_inet_addr(
            "2: end1    inet 192.168.1.50/24 brd 192.168.1.255 scope global dynamic end1\n"
        )
        .as_deref(),
        Some("192.168.1.50")
    );
    assert_eq!(parse_inet_addr(""), None);
    assert_eq!(
        parse_default_via("default via 192.168.1.1 proto dhcp src 192.168.1.50 metric 100\n")
            .as_deref(),
        Some("192.168.1.1")
    );
    assert_eq!(parse_default_via(""), None);
}

#[test]
fn pick_primary_connection_prefers_the_active_ethernet_on_the_interface() {
    // The bench shape: an active netplan-eth0 802-3-ethernet on eth0 is the
    // primary, so connection_name reads "netplan-eth0" (NOT null).
    let saved = vec![
        vec![
            "netplan-eth0".into(),
            "802-3-ethernet".into(),
            "eth0".into(),
        ],
        vec![
            "preconfigured".into(),
            "802-11-wireless".into(),
            "wlan0".into(),
        ],
    ];
    let active = vec![vec![
        "netplan-eth0".into(),
        "802-3-ethernet".into(),
        "eth0".into(),
    ]];
    assert_eq!(
        pick_primary_connection_name(&saved, &active, "eth0"),
        Some("netplan-eth0".to_string())
    );
}

#[test]
fn pick_primary_connection_falls_back_to_a_saved_profile_on_the_interface() {
    // No active ethernet, but a saved profile pinned to eth0 → that name.
    let saved = vec![vec![
        "Wired connection 1".into(),
        "802-3-ethernet".into(),
        "eth0".into(),
    ]];
    let active: Vec<Vec<String>> = vec![];
    assert_eq!(
        pick_primary_connection_name(&saved, &active, "eth0"),
        Some("Wired connection 1".to_string())
    );
}

#[test]
fn pick_primary_connection_falls_back_to_the_first_ethernet_of_any_device() {
    // No match on the interface; the first saved ethernet of any device wins.
    let saved = vec![vec![
        "Office".into(),
        "802-3-ethernet".into(),
        "enp3s0".into(),
    ]];
    let active: Vec<Vec<String>> = vec![];
    assert_eq!(
        pick_primary_connection_name(&saved, &active, "eth0"),
        Some("Office".to_string())
    );
}

#[test]
fn pick_primary_connection_returns_none_without_an_ethernet_profile() {
    // Only wireless connections → null, the non-NM-ethernet default.
    let saved = vec![vec![
        "HomeWiFi".into(),
        "802-11-wireless".into(),
        "wlan0".into(),
    ]];
    let active: Vec<Vec<String>> = vec![];
    assert_eq!(pick_primary_connection_name(&saved, &active, "eth0"), None);
    // An empty NM list (no nmcli / no connections) is also null.
    assert_eq!(pick_primary_connection_name(&[], &[], "eth0"), None);
}

#[test]
fn parse_nmcli_terse_handles_escapes_and_short_rows() {
    // Plain colon split, truncated to the requested column count.
    assert_eq!(
        parse_nmcli_terse("netplan-eth0:802-3-ethernet:eth0\n", 3),
        vec![vec![
            "netplan-eth0".to_string(),
            "802-3-ethernet".to_string(),
            "eth0".to_string()
        ]]
    );
    // A name with an escaped colon stays one field.
    assert_eq!(
        parse_nmcli_terse_line(r"My\:Conn:802-3-ethernet:eth0"),
        vec!["My:Conn", "802-3-ethernet", "eth0"]
    );
    // Blank lines are skipped and rows shorter than the field count dropped.
    assert_eq!(
        parse_nmcli_terse("a:b:c\n\nx:y\n", 3),
        vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]]
    );
    // A missing trailing device column is preserved as an empty field.
    assert_eq!(
        parse_nmcli_terse_line("name:802-3-ethernet:"),
        vec!["name", "802-3-ethernet", ""]
    );
}

#[test]
fn modem_status_never_claims_no_modem_without_looking() {
    // With ModemManager installed the front has not probed for a modem, so
    // a working modem must not read as "No modem detected".
    let probed_nothing = modem_status_body(true);
    assert_eq!(probed_nothing["present"], Value::Null);
    assert_eq!(probed_nothing["reason"], json!("not_probed"));
    // Without ModemManager there is nothing to probe with, and that is a fact.
    let no_manager = modem_status_body(false);
    assert_eq!(no_manager["present"], json!(false));
    assert_eq!(no_manager["reason"], json!("modemmanager_not_installed"));
}

#[test]
fn profile_mismatch_body_is_the_object_detail() {
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[test]
fn json_truthy_matches_python_bool() {
    assert!(!json_truthy(&Value::Null));
    assert!(!json_truthy(&json!(false)));
    assert!(json_truthy(&json!(true)));
    assert!(!json_truthy(&json!(0)));
    assert!(json_truthy(&json!(1)));
    assert!(!json_truthy(&json!("")));
    assert!(json_truthy(&json!("x")));
}

// --- Pure test seams mirroring the async handlers' composition, so the
// shapes can be asserted without the socket / store wiring. ---

/// The priority list as composed from an optional priority-file object.
fn priority_list_from(obj: Option<&Map<String, Value>>) -> Value {
    let default = || Value::Array(DEFAULT_PRIORITY.iter().map(|s| json!(s)).collect());
    let Some(obj) = obj else { return default() };
    let Some(arr) = obj.get("priority").and_then(Value::as_array) else {
        return default();
    };
    if arr.is_empty() || !arr.iter().all(Value::is_string) {
        return default();
    }
    Value::Array(arr.clone())
}
