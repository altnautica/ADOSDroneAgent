//! Golden-JSON byte-parity test for the cloud status heartbeat wire model.
//!
//! The fixtures under `tests/fixtures/` were captured from the Python emit
//! (`src/ados/services/cloud/heartbeat_loop.py` assembly + the real pure
//! `build_radio_block`, with the loop's top-level `None`-strip applied). This
//! builds the same logical input in Rust and asserts the Rust serialization
//! equals the fixture as a `serde_json::Value` — comparing as values so key
//! ORDER does not matter (the receiver validates by key, not order). The parity
//! that matters is FIELD SET + CASING + NULL-STRIPPING, not the psutil-derived
//! numbers.

use ados_cloud::heartbeat::{CanBus, HeartbeatPayload, Peripheral, RadioBlock, RemoteAccess};

const VERSION: &str = "0.49.3";
const DEVICE_ID: &str = "ados-test";

/// A payload with the controlled fixed values the capture script used. The two
/// modes differ only in the fields the helpers set, so this is the shared base.
fn base_payload() -> HeartbeatPayload {
    HeartbeatPayload {
        device_id: DEVICE_ID.to_string(),
        version: VERSION.to_string(),
        profile: Some("drone".to_string()),
        role: None,
        uptime_seconds: 10,
        board_name: "unknown".to_string(),
        board_tier: 0,
        board_soc: String::new(),
        board_arch: String::new(),
        cpu_percent: Some(1.5),
        memory_percent: Some(12.0),
        disk_percent: Some(3.0),
        temperature: None,
        memory_used_mb: 256,
        memory_total_mb: 4096,
        disk_used_gb: 2.0,
        disk_total_gb: 64.0,
        cpu_cores: 4,
        board_ram_mb: 4096,
        cpu_history: vec![1.5],
        memory_history: vec![12.0],
        fc_connected: Some(false),
        fc_port: String::new(),
        fc_baud: 0,
        transport_open: None,
        mavlink_alive: None,
        heartbeat_age_s: None,
        fc_source: None,
        fc_link_hint: None,
        services: Some(vec![]),
        last_ip: "127.0.0.1".to_string(),
        mdns_host: String::new(),
        setup_url: "http://127.0.0.1:8080".to_string(),
        api_url: "http://127.0.0.1:8080/api".to_string(),
        agent_version: VERSION.to_string(),
        video_state: Some("stopped".to_string()),
        video_whep_port: 0,
        mavlink_ws_port: 0,
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
        wfb_adapter_chipset: None,
        wfb_adapter_injection_ok: false,
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
        video_pipeline_flavor: None,
        video_encoder_name: None,
        video_encoder_hw_accel: None,
        video_camera_source: None,
        video_pipeline_state: None,
        display_type: None,
        can_buses: None,
        compute_role: None,
        compute_cluster_master_id: None,
        compute_queue_depth: None,
        compute_active_jobs: None,
        compute_workers_idle: None,
        compute_cluster_aggregate_workers_idle: None,
        compute_cluster_slaves: None,
        plugin_state: None,
    }
}

fn load_fixture(name: &str) -> serde_json::Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read fixture {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse fixture {name}: {e}"))
}

#[test]
fn local_stripped_matches_python_emit() {
    // Drone profile, role None (stripped), absent radio, no optionals.
    let payload = base_payload();
    let got = payload.to_value();
    let want = load_fixture("heartbeat_local_stripped.json");
    assert_eq!(
        got, want,
        "local-stripped heartbeat must match the Python emit"
    );
}

#[test]
fn paired_full_matches_python_emit() {
    // Ground-station, populated radio + the representative enrichment set.
    let mut payload = base_payload();
    payload.profile = Some("ground-station".to_string());
    payload.role = Some("receiver".to_string());
    payload.radio = RadioBlock {
        state: Some("connected".to_string()),
        iface: Some("wlan1".to_string()),
        driver: None,
        channel: Some(149),
        freq_mhz: Some(5745),
        bandwidth_mhz: Some(20),
        tx_power_dbm: Some(20),
        topology: Some("one-to-one".to_string()),
        rssi_dbm: Some(-48.0),
        snr_db: Some(30.0),
        bitrate_kbps: Some(4096),
        paired: true,
        paired_with_device_id: Some("ados-peer".to_string()),
        adapter_chipset: Some("RTL8812EU".to_string()),
        adapter_injection_ok: true,
        ..RadioBlock::absent()
    };
    payload.wfb_adapter_chipset = Some("RTL8812EU".to_string());
    payload.wfb_adapter_injection_ok = true;
    payload.mission_control_url = Some("https://mc.example".to_string());
    payload.video_whep_url = Some("https://tunnel.example/main/".to_string());
    payload.peripherals = Some(vec![Peripheral {
        id: "local-display".to_string(),
        name: "Waveshare 3.5\" SPI LCD".to_string(),
        category: "display".to_string(),
        kind: "spi-lcd".to_string(),
        bus: "spi".to_string(),
        address: "/dev/fb1".to_string(),
        rate_hz: 0,
        status: "ok".to_string(),
        last_reading: "2026-05-29T00:00:00+00:00".to_string(),
        extra: serde_json::json!({"controller": "ili9486", "has_touch": true}),
    }]);
    payload.ui_theme = Some("dark".to_string());
    payload.lcd_touch_calibrated = Some(true);
    payload.lcd_rotation = Some(90);
    payload.video_recording = Some(false);
    payload.display_type = Some("lcd".to_string());
    payload.last_plugin_update_check_at = Some(1716940800000);
    payload.can_buses = Some(vec![CanBus {
        port: 1,
        driver: 1,
        bitrate: 1000000,
        protocol: 1,
    }]);

    let got = payload.to_value();
    let want = load_fixture("heartbeat_paired_full.json");
    assert_eq!(
        got, want,
        "paired-full heartbeat must match the Python emit"
    );
}
