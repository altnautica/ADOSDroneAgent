use super::*;

#[test]
fn root_keys_are_camel_case_and_radio_is_snake_case() {
    let mut p = minimal_payload();
    p.radio = RadioBlock {
        state: Some("connected".to_string()),
        channel: Some(149),
        freq_mhz: Some(5745),
        ..RadioBlock::absent()
    };
    // A real scan verdict, so the hoisted root key is present to assert
    // its casing (with no verdict the key is stripped entirely).
    p.wfb_adapter_injection_ok = Some(true);
    let v = p.to_value();
    let obj = v.as_object().unwrap();
    // Root keys: camelCase.
    assert!(obj.contains_key("deviceId"));
    assert!(obj.contains_key("uptimeSeconds"));
    assert!(obj.contains_key("boardName"));
    assert!(obj.contains_key("wfbAdapterInjectionOk"));
    // No snake_case leak at the root.
    assert!(!obj.contains_key("device_id"));
    assert!(!obj.contains_key("uptime_seconds"));
    // The radio sub-block keys are snake_case.
    let radio = obj.get("radio").unwrap().as_object().unwrap();
    assert!(radio.contains_key("freq_mhz"));
    assert!(radio.contains_key("rssi_dbm"));
    assert!(radio.contains_key("adapter_injection_ok"));
    assert!(!radio.contains_key("freqMhz"));
}

#[test]
fn adapter_injection_verdict_defaults_to_unknown_never_false() {
    // With no radio view there is no injection verdict. BOTH the nested
    // block key and the hoisted root key read JSON null — never a fabricated
    // false (which a three-state consumer would render as a measured scan
    // outcome for a rig that never scanned) and never omitted (which the
    // receiver cannot tell from an agent that predates the surface, and
    // which leaves a previous tick's verdict standing).
    let p = minimal_payload();
    let v = p.to_value();
    let radio = v.get("radio").unwrap().as_object().unwrap();
    assert!(radio.contains_key("adapter_injection_ok"));
    assert!(radio["adapter_injection_ok"].is_null());
    let root = v.as_object().unwrap();
    assert!(root.contains_key("wfbAdapterInjectionOk"));
    assert!(root["wfbAdapterInjectionOk"].is_null());
    assert!(root.contains_key("wfbAdapterChipset"));
    assert!(root["wfbAdapterChipset"].is_null());

    // A measured verdict rides the wire in both directions.
    let mut p = minimal_payload();
    p.radio = RadioBlock {
        adapter_injection_ok: Some(false),
        ..RadioBlock::absent()
    };
    p.wfb_adapter_injection_ok = Some(false);
    let v = p.to_value();
    assert_eq!(v["radio"]["adapter_injection_ok"], serde_json::json!(false));
    assert_eq!(v["wfbAdapterInjectionOk"], serde_json::json!(false));
}

#[test]
fn radio_rf_unverified_rides_the_block_and_defaults_to_unknown() {
    // With no radio view the verdict is JSON null (unknown), never false:
    // a false would claim an unproven transmit path had been proven.
    let p = minimal_payload();
    let v = p.to_value();
    let radio = v.get("radio").unwrap().as_object().unwrap();
    assert!(radio.contains_key("rf_unverified"));
    assert!(radio["rf_unverified"].is_null());

    // A real verdict rides through unchanged, under the snake_case key the
    // radio itself writes.
    let mut p = minimal_payload();
    p.radio = RadioBlock {
        state: Some("rf_unverified".to_string()),
        rf_unverified: Some(true),
        channel_locked: Some(false),
        ..RadioBlock::absent()
    };
    let v = p.to_value();
    assert_eq!(v["radio"]["rf_unverified"], serde_json::json!(true));
}

#[test]
fn crsf_block_is_omitted_when_absent_and_snake_case_when_present() {
    // No fresh lane sidecar ⇒ the key is absent entirely — a node without
    // the lane never carries a fabricated all-null block.
    let p = minimal_payload();
    assert!(!p.to_value().as_object().unwrap().contains_key("crsf"));

    // A present block keeps snake_case keys and its nested nulls (the
    // radio-block convention: unmeasured reads null inside the block).
    let mut p = minimal_payload();
    p.crsf = Some(CrsfBlock {
        v: Some(1),
        state: Some("link_ok".to_string()),
        rssi_dbm: Some(-51),
        rf_unverified: Some(false),
        packet_rate_hz: Some(150),
        fc_command_down_gated: Some(true),
        ..CrsfBlock::absent()
    });
    let v = p.to_value();
    let crsf = v.get("crsf").unwrap().as_object().unwrap();
    assert_eq!(crsf["state"], "link_ok");
    assert_eq!(crsf["rssi_dbm"], -51);
    assert_eq!(crsf["packet_rate_hz"], 150);
    assert_eq!(crsf["rf_unverified"], false);
    // The command-down gate rides the block as a real bool so a consumer of
    // only the crsf lane can read the ELRS MAVLink command path's gate.
    assert_eq!(crsf["fc_command_down_gated"], true);
    // Unmeasured fields ride as nested nulls, under snake_case keys.
    assert!(crsf["band"].is_null());
    assert!(crsf["tx_frames_per_s"].is_null());
    assert!(!crsf.contains_key("rssiDbm"));
    assert!(!crsf.contains_key("packetRateHz"));
}

#[test]
fn crsf_rf_unverified_defaults_to_unknown_never_false() {
    // A block with no liveness verdict carries null — never a confident
    // false that would claim an unproven transmit path had been proven.
    let mut p = minimal_payload();
    p.crsf = Some(CrsfBlock {
        state: Some("ready".to_string()),
        ..CrsfBlock::absent()
    });
    let v = p.to_value();
    assert!(v["crsf"]["rf_unverified"].is_null());
}

#[test]
fn none_root_fields_are_omitted_not_null_outside_the_declared_null_set() {
    // temperature None must be absent, not JSON null (Convex v.optional).
    let p = minimal_payload();
    let v = p.to_value();
    let obj = v.as_object().unwrap();
    assert!(!obj.contains_key("temperature"));
    assert!(!obj.contains_key("missionControlUrl"));
    assert!(!obj.contains_key("peripherals"));
    // The node's reach is omitted rather than sent as `""`: an empty string
    // is a reach nothing proved, and it overwrites a good one.
    assert!(!obj.contains_key("lastIp"));
    assert!(!obj.contains_key("mdnsHost"));
    assert!(!obj.contains_key("setupUrl"));
    assert!(!obj.contains_key("apiUrl"));
    // No top-level key serializes as explicit null EXCEPT the declared
    // null-tolerant set, for which null is the "reported, not measured"
    // reading that clears the receiver's column.
    for (k, val) in obj.iter() {
        assert!(
            !val.is_null() || crate::loops::heartbeat::UNMEASURED_NULL_KEYS.contains(&k.as_str()),
            "root key {k} must not be JSON null"
        );
    }
}

#[test]
fn present_temperature_serializes() {
    let mut p = minimal_payload();
    p.temperature = Some(48.5);
    let v = p.to_value();
    assert_eq!(v["temperature"], serde_json::json!(48.5));
}

fn minimal_payload() -> HeartbeatPayload {
    HeartbeatPayload {
        device_id: "ados-test".to_string(),
        version: "0.1.0".to_string(),
        profile: Some("drone".to_string()),
        role: None,
        uptime_seconds: 10,
        board_name: "unknown".to_string(),
        board_tier: 0,
        board_soc: String::new(),
        board_arch: String::new(),
        npu_tops: 0.0,
        has_accelerator: false,
        perception_tier: "none".to_string(),
        perception_offload_target: None,
        cpu_percent: Some(0.0),
        memory_percent: Some(0.0),
        disk_percent: Some(0.0),
        temperature: None,
        memory_used_mb: None,
        memory_total_mb: None,
        disk_used_gb: None,
        disk_total_gb: None,
        cpu_cores: None,
        board_ram_mb: None,
        cpu_history: None,
        memory_history: None,
        fc_connected: Some(false),
        fc_port: None,
        fc_baud: None,
        transport_open: None,
        mavlink_alive: None,
        heartbeat_age_s: None,
        fc_source: None,
        fc_link_hint: None,
        fc_variant: None,
        services: Some(vec![]),
        // Unmeasured reach: the minimal payload proves no LAN name, so it
        // advertises none.
        last_ip: None,
        mdns_host: None,
        setup_url: None,
        api_url: None,
        agent_version: "0.1.0".to_string(),
        video_state: Some("stopped".to_string()),
        video_whep_port: None,
        mavlink_ws_port: None,
        mavlink_ws_url: None,
        video_whep_url: None,
        mission_control_url: None,
        remote_access: RemoteAccess {
            provider: "none".to_string(),
            public_urls: vec![],
        },
        last_plugin_update_check_at: None,
        peripherals: None,
        radio: RadioBlock::absent(),
        crsf: None,
        wfb_adapter_chipset: None,
        mac_stability: None,
        wfb_adapter_injection_ok: None,
        lcd_active_page: None,
        ui_theme: None,
        lcd_touch_calibrated: None,
        lcd_rotation: None,
        lcd_snapshot_url: None,
        lcd_last_touch_at: None,
        lcd_last_gesture: None,
        video_local_decoder_active: None,
        video_local_decoder_type: None,
        video_local_decoder_fps: None,
        video_recording: None,
        video_streams: None,
        linked_peers: None,
        display_type: None,
        can_buses: None,
        plugin_state: None,
        config_errors: None,
    }
}
