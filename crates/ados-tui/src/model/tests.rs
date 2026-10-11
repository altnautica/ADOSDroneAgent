use super::*;
use serde_json::json;

#[test]
fn state_label_matches_states() {
    assert_eq!(state_label("complete"), "ready");
    assert_eq!(state_label("needs_action"), "needs action");
    assert_eq!(state_label("in_progress"), "in progress");
    assert_eq!(state_label("pending"), "pending");
}

#[test]
fn viewer_url_strips_whep_suffix() {
    assert_eq!(
        viewer_url_from_whep(Some("http://host:8889/main/whep")).as_deref(),
        Some("http://host:8889/main/")
    );
    assert_eq!(
        viewer_url_from_whep(Some("http://host:8889/main/")).as_deref(),
        Some("http://host:8889/main/")
    );
    assert_eq!(viewer_url_from_whep(None), None);
    assert_eq!(viewer_url_from_whep(Some("")), None);
    assert_eq!(viewer_url_from_whep(Some("/whep")), None);
}

#[test]
fn ident_is_device_and_profile() {
    let dash = Dashboard {
        device_name: "ados-e996786c".into(),
        profile: "ground_station".into(),
        ..Dashboard::default()
    };
    assert_eq!(dash.ident(), "ados-e996786c · ground_station");
}

#[test]
fn reach_rank_classifies_hosts() {
    assert_eq!(reach_rank("http://ados-abc.local:8080/setup"), 0);
    assert_eq!(reach_rank("http://192.168.1.5:8080/setup"), 1);
    assert_eq!(reach_rank("https://tunnel.example.com/setup"), 2);
    assert_eq!(reach_rank("http://localhost:8080/setup"), 3);
    assert_eq!(reach_rank("http://127.0.0.1:8080/setup"), 3);
    assert_eq!(reach_rank("http://[::1]:8080/setup"), 3);
}

#[test]
fn is_loopback_url_detects_loopback() {
    assert!(is_loopback_url("http://localhost:8080/"));
    assert!(is_loopback_url("http://127.0.0.1:8080/"));
    assert!(!is_loopback_url("http://ados-abc.local:8080/"));
    assert!(!is_loopback_url("http://192.168.1.5:8080/"));
}

#[test]
fn reach_links_groups_all_kinds_full_url_mdns_and_ip() {
    let data = json!({
        "access_urls": [
            {"kind": "setup", "label": "mDNS setup", "url": "http://ados-x.local:8080/setup", "primary": true},
            {"kind": "setup", "label": "LAN setup", "url": "http://192.168.1.5:8080/setup"},
            {"kind": "setup", "label": "local", "url": "http://localhost:8080/setup"},
            {"kind": "video", "label": "Local video viewer", "url": "http://ados-x.local:8889/main/"},
            {"kind": "mavlink", "label": "MAVLink WebSocket", "url": "ws://ados-x.local:8765/"},
            {"kind": "mission_control", "label": "Mission Control", "url": "https://command.example.com"},
            {"kind": "cloud", "label": "Remote access", "url": "https://tunnel.example.com/setup"}
        ],
        "mavlink": {"connected": true, "tcp_url": "tcp://ados-x.local:5760", "websocket_url": "ws://ados-x.local:8765/"}
    });
    let groups = Dashboard::from_status(&data).reach_links();
    let titles: Vec<&str> = groups.iter().map(|g| g.title).collect();
    assert_eq!(
        titles,
        vec![
            "Console",
            "Connect to GCS",
            "Ground control",
            "Video",
            "Mission Control",
            "Remote"
        ]
    );

    // Console shows both the mDNS and the LAN-IP full URLs, loopback dropped.
    let console: Vec<&str> = groups[0].rows.iter().map(|r| r.url.as_str()).collect();
    assert_eq!(
        console,
        vec![
            "http://ados-x.local:8080/setup",
            "http://192.168.1.5:8080/setup"
        ]
    );

    // Connect to GCS strips /setup to the bare base address, both hosts.
    let gcs: Vec<&str> = groups[1].rows.iter().map(|r| r.url.as_str()).collect();
    assert_eq!(
        gcs,
        vec!["http://ados-x.local:8080", "http://192.168.1.5:8080"]
    );

    // Ground control synthesises tcp + ws on both LAN hosts.
    let gc: Vec<&str> = groups[2].rows.iter().map(|r| r.url.as_str()).collect();
    assert!(gc.contains(&"tcp://ados-x.local:5760"));
    assert!(gc.contains(&"ws://ados-x.local:8765/"));
    assert!(gc.contains(&"tcp://192.168.1.5:5760"));
    assert!(gc.contains(&"ws://192.168.1.5:8765/"));

    // Video is projected onto both LAN hosts too.
    let vid: Vec<&str> = groups[3].rows.iter().map(|r| r.url.as_str()).collect();
    assert!(vid.contains(&"http://ados-x.local:8889/main/"));
    assert!(vid.contains(&"http://192.168.1.5:8889/main/"));
}

#[test]
fn reach_links_drops_empty_groups() {
    let data = json!({
        "access_urls": [{"kind": "setup", "url": "http://ados-x.local:8080/setup", "primary": true}]
    });
    let groups = Dashboard::from_status(&data).reach_links();
    // Console and its derived Connect-to-GCS base survive; the rest are empty.
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].title, "Console");
    assert_eq!(groups[1].title, "Connect to GCS");
    assert_eq!(groups[1].rows[0].url, "http://ados-x.local:8080");
}

#[test]
fn fc_link_refines_with_api_status() {
    // The gated boolean wins when connected.
    let connected = Dashboard {
        mavlink_connected: true,
        ..Dashboard::default()
    };
    assert_eq!(connected.fc_link(), FcLink::Connected);

    // Transport open but no heartbeat (from /api/status) reads port-open.
    let mut port_open = Dashboard::default();
    port_open.merge_fc_status(&json!({
        "transportOpen": true, "mavlinkAlive": false,
        "fcLinkHint": "msp_detected", "fc_port": "ttyS0", "fc_baud": 921600
    }));
    assert_eq!(port_open.fc_link(), FcLink::PortOpen);
    assert_eq!(
        port_open.fc_hint().as_deref(),
        Some("Flight controller speaking MSP (Betaflight/iNav) — a valid native link.")
    );
    assert_eq!(port_open.fc_endpoint().as_deref(), Some("ttyS0 · 921600"));

    // An MSP FC identified by the agent (fcVariant present) reads as MSP,
    // not "port open" and not "not connected".
    let mut msp = Dashboard::default();
    msp.merge_fc_status(&json!({
        "transportOpen": true, "mavlinkAlive": false,
        "fcLinkHint": "msp_detected", "fcVariant": "betaflight",
        "fc_port": "ttyACM0", "fc_baud": 57600
    }));
    assert_eq!(msp.fc_link(), FcLink::Msp);
    assert_eq!(msp.fc_variant_label().as_deref(), Some("Betaflight"));

    // No transport detail at all → down.
    assert_eq!(Dashboard::default().fc_link(), FcLink::Down);
}

#[test]
fn merge_names_the_mavlink_firmware_family() {
    // A live ArduPilot FC is named ArduPilot, not a generic "FC connected".
    let mut ap = Dashboard {
        mavlink_connected: true,
        ..Dashboard::default()
    };
    ap.merge_fc_status(&json!({
        "transportOpen": true, "mavlinkAlive": true, "fcFirmware": "ardupilot"
    }));
    assert_eq!(ap.fc_link(), FcLink::Connected);
    assert_eq!(ap.fc_family_label().as_deref(), Some("ArduPilot"));

    // PX4 the same.
    let mut px4 = Dashboard {
        mavlink_connected: true,
        ..Dashboard::default()
    };
    px4.merge_fc_status(&json!({
        "transportOpen": true, "mavlinkAlive": true, "fcFirmware": "px4"
    }));
    assert_eq!(px4.fc_family_label().as_deref(), Some("PX4"));

    // The honest `unknown` sentinel yields no name (falls back to "FC").
    let mut unknown = Dashboard {
        mavlink_connected: true,
        ..Dashboard::default()
    };
    unknown.merge_fc_status(&json!({
        "transportOpen": true, "mavlinkAlive": true, "fcFirmware": "unknown"
    }));
    assert_eq!(unknown.fc_firmware, None);
    assert_eq!(unknown.fc_family_label(), None);
}

#[test]
fn port_of_and_swap_host() {
    assert_eq!(port_of("ws://host:8765/"), Some(8765));
    assert_eq!(port_of("tcp://192.168.1.5:5760"), Some(5760));
    assert_eq!(port_of("http://host/no-port"), None);
    assert_eq!(
        swap_host("http://ados-x.local:8889/main/", "192.168.1.5"),
        "http://192.168.1.5:8889/main/"
    );
    assert_eq!(
        swap_host("ws://ados-x.local:8765/", "10.0.0.9"),
        "ws://10.0.0.9:8765/"
    );
}

#[test]
fn base_url_strips_path() {
    assert_eq!(
        base_url("http://ados-x.local:8080/setup"),
        "http://ados-x.local:8080"
    );
    assert_eq!(
        base_url("http://192.168.1.5:8080/setup"),
        "http://192.168.1.5:8080"
    );
    assert_eq!(base_url("http://host:8080"), "http://host:8080");
    assert_eq!(base_url("https://host/a/b?c#d"), "https://host");
}

#[test]
fn fc_hint_expands_no_heartbeat_token() {
    let mut d = Dashboard::default();
    d.merge_fc_status(
        &json!({"transportOpen": true, "mavlinkAlive": false, "fcLinkHint": "no_heartbeat"}),
    );
    assert_eq!(
            d.fc_hint().as_deref(),
            Some("Serial device found but no MAVLink heartbeat — check the FC baud rate, power, and wiring.")
        );
}

#[test]
fn steps_carry_detail() {
    let data = json!({
        "steps": [
            {"label": "Flight controller", "state": "needs_action", "detail": "No flight controller detected on USB/serial."}
        ]
    });
    let dash = Dashboard::from_status(&data);
    assert_eq!(
        dash.steps[0].detail,
        "No flight controller detected on USB/serial."
    );
}

#[test]
fn health_healthy_when_configured_connected_and_all_running() {
    let data = json!({
        "profile": "drone",
        "paired": true,
        "mavlink": {"connected": true},
        "services": [{"state": "running"}, {"state": "running"}],
        "steps": [{"label": "Profile", "state": "complete"}]
    });
    assert_eq!(Dashboard::from_status(&data).health(), Health::Healthy);
}

#[test]
fn health_degraded_when_a_service_is_down() {
    let data = json!({
        "profile": "drone",
        "paired": true,
        "mavlink": {"connected": true},
        "services": [{"state": "running"}, {"state": "stopped"}],
        "steps": [{"label": "Profile", "state": "complete"}]
    });
    assert_eq!(Dashboard::from_status(&data).health(), Health::Degraded);
}

#[test]
fn health_degraded_when_configured_drone_loses_fc() {
    let data = json!({
        "profile": "drone",
        "paired": true,
        "mavlink": {"connected": false},
        "services": [{"state": "running"}]
    });
    assert_eq!(Dashboard::from_status(&data).health(), Health::Degraded);
}

#[test]
fn health_setup_when_unpaired_and_steps_incomplete() {
    let data = json!({
        "profile": "drone",
        "paired": false,
        "mavlink": {"connected": false},
        "services": [{"state": "running"}],
        "steps": [{"label": "Profile", "state": "pending"}]
    });
    assert_eq!(Dashboard::from_status(&data).health(), Health::Setup);
}

#[test]
fn health_healthy_for_ground_station_without_fc() {
    let data = json!({
        "profile": "ground_station",
        "paired": true,
        "mavlink": {"connected": false},
        "services": [{"state": "running"}]
    });
    assert_eq!(Dashboard::from_status(&data).health(), Health::Healthy);
}

#[test]
fn health_healthy_for_a_paired_msp_drone() {
    let data = json!({
        "profile": "drone",
        "paired": true,
        "mavlink": {"connected": false},
        "services": [{"state": "running"}]
    });
    let mut dash = Dashboard::from_status(&data);
    dash.merge_fc_status(&json!({"fcVariant": "betaflight", "transportOpen": true}));
    assert_eq!(dash.fc_link(), FcLink::Msp);
    assert_eq!(dash.health(), Health::Healthy);
}

#[test]
fn status_summary_combines_pairing_and_link() {
    let paired_local = Dashboard {
        paired: true,
        cloud_mode: "local".into(),
        ..Dashboard::default()
    };
    assert_eq!(paired_local.status_summary(), "paired · local mode");

    let code_relay = Dashboard {
        paired: false,
        pairing_code: Some("AB12".into()),
        cloud_configured: true,
        ..Dashboard::default()
    };
    assert_eq!(code_relay.status_summary(), "code AB12 · cloud relay");
}

#[test]
fn cloud_relay_local_mode() {
    let data = json!({"cloud_choice": {"mode": "local"}});
    let dash = Dashboard::from_status(&data);
    assert_eq!(dash.cloud_relay, "disabled (local mode)");
}

#[test]
fn full_status_extracts_typed_fields() {
    let data = json!({
        "version": "0.46.12",
        "device_name": "ados-e996786c",
        "profile": "drone",
        "paired": false,
        "pairing_code": "WM325P",
        "access_urls": [
            {"label": "Setup", "url": "http://ados-x.local:8080/setup", "primary": true}
        ],
        "steps": [{"label": "Profile", "state": "complete"}],
        "mavlink": {"connected": true, "tcp_url": "tcp://x:5760"},
        "video": {"state": "running", "whep_url": "http://x:8889/main/whep"},
        "network": {"hotspot_enabled": true, "hotspot_ssid": "ADOS-AP"},
        "remote_access": {"status": "disabled"},
        "cloud_choice": {"mode": "local"},
        "telemetry": {
            "mode": "STABILIZE", "armed": false,
            "battery": {"remaining": 82},
            "gps": {"fix_type": 3, "satellites": 14},
            "position": {"alt_rel": 12.5}
        },
        "services": [{"state": "running"}, {"state": "running"}, {"state": "stopped"}],
        "next_action": "Open Mission Control"
    });
    let dash = Dashboard::from_status(&data);

    assert_eq!(dash.version, "0.46.12");
    assert_eq!(dash.device_name, "ados-e996786c");
    assert_eq!(dash.pairing_code.as_deref(), Some("WM325P"));
    assert!(dash.access_urls[0].primary);
    assert!(dash.steps_all_complete);
    assert!(dash.mavlink_connected);
    assert_eq!(dash.video_state, "running");
    assert_eq!(dash.video_viewer.as_deref(), Some("http://x:8889/main/"));
    assert_eq!(dash.hotspot, "ADOS-AP");
    assert_eq!(dash.mode.as_deref(), Some("STABILIZE"));
    assert_eq!(dash.armed, Some(false));
    assert_eq!(dash.battery, Some(82.0));
    assert_eq!(dash.gps_fix.as_deref(), Some("3D"));
    assert_eq!(dash.satellites, Some(14));
    assert_eq!(dash.alt, Some(12.5));
    assert_eq!((dash.services_running, dash.services_total), (2, 3));
    assert_eq!(dash.next_action, "Open Mission Control");
}

#[test]
fn empty_status_has_no_telemetry() {
    let dash = Dashboard::from_status(&json!({}));
    assert!(dash.mode.is_none());
    assert!(dash.battery.is_none());
    assert!(dash.alt.is_none());
    assert_eq!(dash.version, "?");
}

#[test]
fn fix_label_maps_gps_fix_types() {
    assert_eq!(fix_label(0), "no GPS");
    assert_eq!(fix_label(1), "no fix");
    assert_eq!(fix_label(3), "3D");
    assert_eq!(fix_label(6), "RTK fixed");
    assert_eq!(fix_label(99), "unknown");
}

#[test]
fn flight_telemetry_hidden_when_fc_disconnected() {
    // The empty snapshot carries all-zero nested telemetry even with the FC
    // down; none of it may render, or a disconnected drone reads live.
    let data = json!({
        "profile": "drone",
        "mavlink": {"connected": false},
        "telemetry": {
            "mode": "", "armed": false,
            "battery": {"remaining": -1},
            "gps": {"fix_type": 0, "satellites": 0},
            "position": {"alt_rel": 0.0}
        }
    });
    let dash = Dashboard::from_status(&data);
    assert!(dash.mode.is_none());
    assert!(dash.battery.is_none());
    assert!(dash.gps_fix.is_none());
    assert!(dash.satellites.is_none());
    assert!(dash.alt.is_none());
}

#[test]
fn battery_unknown_when_negative() {
    let data = json!({
        "mavlink": {"connected": true},
        "telemetry": {"battery": {"remaining": -1}}
    });
    assert!(Dashboard::from_status(&data).battery.is_none());
}

#[test]
fn hotspot_hidden_when_disabled() {
    let off = json!({"network": {"hotspot_enabled": false, "hotspot_ssid": "ADOS-AP"}});
    assert_eq!(Dashboard::from_status(&off).hotspot, "");
    let on = json!({"network": {"hotspot_enabled": true, "hotspot_ssid": "ADOS-AP"}});
    assert_eq!(Dashboard::from_status(&on).hotspot, "ADOS-AP");
}

#[test]
fn history_records_present_values_and_caps() {
    let mut history = History::default();
    for i in 0..(HISTORY_CAP + 5) {
        let dash = Dashboard {
            battery: Some(i as f64),
            alt: None,
            ..Dashboard::default()
        };
        history.record(&dash);
    }
    // Battery capped at HISTORY_CAP, oldest dropped; alt never recorded.
    assert_eq!(history.battery.len(), HISTORY_CAP);
    assert_eq!(*history.battery.first().unwrap(), 5.0);
    assert!(history.alt.is_empty());
}
