use super::*;
use serde_json::json;

fn source() -> StateSource {
    // A source with no real disk/api dependency: an unreachable base and
    // tmp sidecar paths. `compose` is driven with hand-built JSON so the
    // mapping is exercised without a live agent.
    StateSource::with_paths(
        "http://127.0.0.1:1",
        Path::new("/nonexistent/pairing.json"),
        PathBuf::from("/nonexistent/hop.json"),
    )
}

#[test]
fn clock_is_hh_mm_ss() {
    let c = current_clock();
    assert_eq!(c.len(), 8);
    assert_eq!(&c[2..3], ":");
    assert_eq!(&c[5..6], ":");
    // Hours/minutes/seconds are all parseable two-digit fields.
    let parts: Vec<&str> = c.split(':').collect();
    assert_eq!(parts.len(), 3);
    for p in parts {
        assert_eq!(p.len(), 2);
        assert!(p.parse::<u32>().is_ok());
    }
}

#[test]
fn missing_sources_yield_a_safe_default_context() {
    let mut src = source();
    let ctx = src.compose(None, None, None);
    // The chrome fields still resolve from the OS / clock.
    assert!(!ctx.hostname.is_empty());
    assert_eq!(ctx.clock.len(), 8);
    // Every sub-context is its default (no panic, no blank-field crash).
    assert!(ctx.link.rssi_dbm.is_none());
    assert_eq!(ctx.mesh.peer_count, None);
    assert!(ctx.video.camera_label.is_none());
    // No snapshot, no supply path: the brownout warning has nothing to key on.
    assert!(ctx.link.topology.is_none());
    assert!(!ctx.hopping.present);
    // History buffers got their first (None) sample.
    assert_eq!(ctx.link.rssi_history.len(), 1);
    assert_eq!(ctx.system.cpu_history.len(), 1);
}

/// The power path comes from the link block the radio writes. A status with
/// no topology leaves it unknown; it is never assumed to be host VBUS.
#[test]
fn topology_is_read_from_the_link_block_and_never_assumed() {
    let mut src = source();
    let status = json!({"link": {"rssi_dbm": -60.0}});
    let ctx = src.compose(Some(&status), None, None);
    assert_eq!(ctx.link.topology, None);
    let status = json!({"link": {"state": "stale", "topology": "powered_hub"}});
    let ctx = src.compose(Some(&status), None, None);
    assert_eq!(ctx.link.topology.as_deref(), Some("powered_hub"));
}

#[test]
fn status_block_maps_link_role_mesh_and_system() {
    let mut src = source();
    let status = json!({
        "link": {
            "state": "connected",
            "rssi_dbm": -67.0,
            "snr_db": 22.0,
            "bitrate_kbps": 20000,
            "bitrate_mbps": 20.0,
            "fec_recovered": 1247,
            "fec_failed": 3,
            "channel": 149,
            "tx_power_dbm": 5,
            "packets_received": 5000,
            "packets_lost": 12,
            "loss_percent": 0.2
        },
        "role": {"current": "receiver", "configured": "receiver", "mesh_capable": true},
        "mesh": {"up": true, "peer_count": 2, "selected_gateway": "gw-2", "partition": false},
        "network": {"ap_ssid": "ados-ap", "uplink_type": "eth", "uplink_reachable": true},
        "system": {
            "cpu_pct": 22.0, "ram_used_mb": 1234, "ram_total_mb": 16384,
            "temp_c": 47.0, "uptime_seconds": 3600, "agent_version": "0.49.41"
        },
        "video": {"recording": true, "camera_count": 1, "mediamtx_ready": true}
    });
    let ctx = src.compose(Some(&status), None, None);

    assert_eq!(ctx.link.state.as_deref(), Some("connected"));
    assert_eq!(ctx.link.rssi_dbm, Some(-67.0));
    assert_eq!(ctx.link.channel, Some(149));
    // fec_lost falls back to the producer's fec_failed key.
    assert_eq!(ctx.link.fec_lost, Some(3));
    assert_eq!(ctx.link.tx_power_dbm, Some(5));

    assert_eq!(ctx.role.current.as_deref(), Some("receiver"));
    assert!(ctx.role.mesh_capable);

    assert_eq!(ctx.mesh.up, Some(true));
    assert_eq!(ctx.mesh.peer_count, Some(2));
    assert_eq!(ctx.mesh.selected_gateway.as_deref(), Some("gw-2"));
    // No roster in the block: unknown, not an empty list.
    assert!(ctx.mesh.peers.is_none());

    assert_eq!(ctx.network.uplink_type.as_deref(), Some("eth"));
    assert_eq!(ctx.network.uplink_reachable, Some(true));

    assert_eq!(ctx.system.cpu_pct, Some(22.0));
    assert_eq!(ctx.system.agent_version.as_deref(), Some("0.49.41"));

    assert!(ctx.video.recording);
    assert_eq!(ctx.video.camera_count, 1);
    assert_eq!(ctx.video.mediamtx_ready, Some(true));
    // Not reported, so not "inactive".
    assert_eq!(ctx.video.active, None);
}

/// A relay whose mesh poll has no current snapshot gets the agent's nulled,
/// stale-marked block. That is "unknown", never "down" with zero peers.
#[test]
fn a_stale_mesh_block_is_unknown_not_down() {
    let mut src = source();
    let status = json!({
        "mesh": {"up": null, "peer_count": null, "selected_gateway": null,
                 "partition": null, "mesh_id": null, "stale": true}
    });
    let ctx = src.compose(Some(&status), None, None);
    assert_eq!(ctx.mesh.up, None);
    assert_eq!(ctx.mesh.peer_count, None);
    assert!(ctx.mesh.stale);
    assert_eq!(ctx.mesh.state(), crate::pages::MeshState::Unknown);

    // A stale block that still says up is unknown too.
    let status = json!({"mesh": {"up": true, "peer_count": 3, "stale": true}});
    let ctx = src.compose(Some(&status), None, None);
    assert_eq!(ctx.mesh.state(), crate::pages::MeshState::Unknown);

    let status = json!({"mesh": {"up": false, "peer_count": 0, "stale": false}});
    let ctx = src.compose(Some(&status), None, None);
    assert_eq!(ctx.mesh.state(), crate::pages::MeshState::Down);
}

/// A stale radio snapshot is no data: the dead producer's last RSSI,
/// bitrate, FEC, channel and TX power never reach a page, and the RSSI
/// trend breaks instead of carrying the frozen value forward.
#[test]
fn a_stale_link_block_carries_no_readings() {
    let mut src = source();
    let status = json!({
        "link": {
            "state": "stale",
            "rssi_dbm": -58.0,
            "bitrate_kbps": 20000,
            "bitrate_mbps": 20.0,
            "fec_recovered": 12,
            "fec_failed": 1,
            "channel": 149,
            "tx_power_dbm": 10
        }
    });
    let ctx = src.compose(Some(&status), None, None);
    assert!(ctx.link.is_stale());
    assert_eq!(ctx.link.rssi_dbm, None);
    assert_eq!(ctx.link.bitrate_kbps, None);
    assert_eq!(ctx.link.fec_recovered, None);
    assert_eq!(ctx.link.fec_lost, None);
    assert_eq!(ctx.link.channel, None);
    assert_eq!(ctx.link.tx_power_dbm, None);
    assert_eq!(ctx.link.rssi_history.last(), Some(&None));
}

/// No `armed` in the FC block is an unknown arm state, not DISARMED.
#[test]
fn an_absent_arm_report_stays_unknown() {
    let mut src = source();
    let ctx = src.compose(Some(&json!({"fc": {"mode": "LOITER"}})), None, None);
    assert_eq!(ctx.fc.armed, None);
    assert_eq!(ctx.drone.armed, None);
    let ctx = src.compose(Some(&json!({"fc": {"armed": false}})), None, None);
    assert_eq!(ctx.fc.armed, Some(false));
}

#[test]
fn paired_drone_and_link_topology_map() {
    let mut src = source();
    let status = json!({
        "paired_drone": {"device_id": "drone-aabbcc", "key_fingerprint": "deadbeef"},
        "link": {"topology": "external_5v"}
    });
    let ctx = src.compose(Some(&status), None, None);
    assert_eq!(ctx.paired_drone.device_id.as_deref(), Some("drone-aabbcc"));
    assert_eq!(
        ctx.paired_drone.key_fingerprint.as_deref(),
        Some("deadbeef")
    );
    // The drone tile falls back to the paired_drone block when no live
    // `drone` block is present.
    assert_eq!(ctx.drone.device_id.as_deref(), Some("drone-aabbcc"));
    assert_eq!(ctx.link.topology.as_deref(), Some("external_5v"));
}

#[test]
fn hop_history_filters_and_maps_entries() {
    let mut src = source();
    let hop = json!({
        "band": "u-nii-3",
        "history": [
            {"at": 100.0, "from": 149, "to": 161, "ok": true, "trigger": "reactive"},
            {"at": 200.0, "from": 161, "to": 149, "ok": false},
            {"from": 1, "to": 2, "ok": true},
            "garbage"
        ]
    });
    let link = json!({"channel": 161});
    let status = json!({"link": link});
    let ctx = src.compose(Some(&status), None, Some(&hop));

    assert!(ctx.hopping.present);
    assert_eq!(ctx.hopping.band.as_deref(), Some("u-nii-3"));
    // The third row (missing `at`) and the bare string are dropped.
    assert_eq!(ctx.hopping.history.len(), 2);
    assert_eq!(ctx.hopping.history[0].from_channel, 149);
    assert_eq!(ctx.hopping.history[0].to_channel, 161);
    assert!(ctx.hopping.history[0].ok);
    assert_eq!(ctx.hopping.history[0].trigger.as_deref(), Some("reactive"));
    // The reference line is the live link channel.
    assert_eq!(ctx.hopping.radio_channel, Some(161));
}

/// A hop-supervisor sidecar older than its refresh window describes no
/// running supervisor, so the page is told there is no data rather than
/// being handed the last history as current.
#[test]
fn a_stale_hop_sidecar_is_not_present() {
    let dir = tempfile::tempdir().unwrap();
    let hop_path = dir.path().join("hop-supervisor.json");
    std::fs::write(&hop_path, r#"{"band":"u-nii-3","history":[]}"#).unwrap();
    let mut src = StateSource::with_paths(
        "http://127.0.0.1:1",
        Path::new("/nonexistent/pairing.json"),
        hop_path.clone(),
    );
    assert!(src.build_context(true).hopping.present);

    let old = std::time::SystemTime::now() - HOP_SIDECAR_FRESH - Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&hop_path)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let ctx = src.build_context(true);
    assert!(!ctx.hopping.present);
    assert!(ctx.hopping.band.is_none());
}

/// The one system-metrics source: the status `system` block, disk included.
#[test]
fn the_system_block_carries_every_host_metric() {
    let mut src = source();
    let status = json!({"system": {
        "cpu_pct": 31.5, "ram_used_mb": 1024.0, "ram_total_mb": 4096.0,
        "temp_c": 52.3, "disk_pct": 12.0
    }});
    let ctx = src.compose(Some(&status), None, None);
    assert_eq!(ctx.system.cpu_pct, Some(31.5));
    assert_eq!(ctx.system.ram_pct(), Some(25.0));
    assert_eq!(ctx.system.disk_pct, Some(12.0));
    assert_eq!(ctx.system.temp_c, Some(52.3));
}

#[test]
fn setup_snapshot_drives_completion_and_pair_code() {
    let mut src = source();
    let setup = json!({
        "finalized": false,
        "completion_percent": 70.0,
        "next_action": "pair with Mission Control",
        "pairing_code": "7YTFC7",
        "device_name": "gs-example",
        "version": "0.49.41"
    });
    let ctx = src.compose(None, Some(&setup), None);
    assert!(!ctx.setup_finalized);
    assert_eq!(ctx.completion_percent, Some(70.0));
    assert_eq!(
        ctx.next_action.as_deref(),
        Some("pair with Mission Control")
    );
    assert_eq!(ctx.pairing.code.as_deref(), Some("7YTFC7"));
    assert_eq!(ctx.cloud.pair_code.as_deref(), Some("7YTFC7"));
    assert_eq!(ctx.device.device_name.as_deref(), Some("gs-example"));
    assert_eq!(ctx.device.version.as_deref(), Some("0.49.41"));
}

/// The setup status carries the LAN host, the advertised Mission Control
/// URL and the hardware checklist (`hardware_check.items`, with the
/// producer's ids and `required` flag). All three feed the early-life tiles.
#[test]
fn setup_snapshot_maps_reach_names_and_the_hardware_checklist() {
    let mut src = source();
    let setup = json!({
        "lan_host": "ados-9f2c1a.local",
        "access_urls": [
            {"kind": "setup", "url": "http://192.168.1.50:8080/setup"},
            {"kind": "mission_control", "url": "https://mc.example.com"}
        ],
        "hardware_check": {
            "profile": "ground_station",
            "items": [
                {"id": "board", "label": "Companion compute", "required": true, "state": "ok"},
                {"id": "radio_wfb", "label": "WFB radio adapter", "required": true,
                 "state": "missing", "fix_hint": "Plug in an RTL8812EU/AU USB adapter."}
            ]
        }
    });
    let ctx = src.compose(None, Some(&setup), None);
    assert_eq!(ctx.lan_host.as_deref(), Some("ados-9f2c1a.local"));
    assert_eq!(
        ctx.mission_control_url.as_deref(),
        Some("https://mc.example.com")
    );
    let radio = ctx
        .hardware_check
        .iter()
        .find(|it| it.id.as_deref() == Some("radio_wfb"))
        .expect("the radio row maps");
    assert!(radio.required);
    assert_eq!(radio.state.as_deref(), Some("missing"));
    assert_eq!(ctx.hardware_check.len(), 2);

    // The ground-station status has no hardware_check block; a stray one
    // there is not read.
    let status = json!({"hardware_check": {"items": [{"id": "radio_wfb", "state": "missing"}]}});
    assert!(src
        .compose(Some(&status), None, None)
        .hardware_check
        .is_empty());
}

#[test]
fn history_buffers_accumulate_and_cap_at_sixty() {
    let mut src = source();
    let status = json!({
        "link": {"rssi_dbm": -55.0},
        "system": {"cpu_pct": 10.0, "temp_c": 40.0},
        "fc": {"battery_remaining": 88.0}
    });
    // Drive 65 ticks; the buffers should cap at HISTORY_LEN.
    let mut ctx = src.compose(Some(&status), None, None);
    for _ in 0..64 {
        ctx = src.compose(Some(&status), None, None);
    }
    assert_eq!(ctx.link.rssi_history.len(), HISTORY_LEN);
    assert_eq!(ctx.system.cpu_history.len(), HISTORY_LEN);
    assert_eq!(ctx.system.temp_history.len(), HISTORY_LEN);
    assert_eq!(ctx.fc.battery_history.len(), HISTORY_LEN);
    // Newest sample is at the tail.
    assert_eq!(ctx.link.rssi_history.last().copied().flatten(), Some(-55.0));
    assert_eq!(ctx.fc.battery_history.last().copied().flatten(), Some(88.0));
}

#[test]
fn auto_pair_flag_reads_the_wfb_pair_body() {
    assert_eq!(
        auto_pair_enabled(Some(&json!({"auto_pair_enabled": false}))),
        Some(false)
    );
    assert_eq!(
        auto_pair_enabled(Some(&json!({"auto_pair_enabled": true}))),
        Some(true)
    );
    // No answer, or no flag, is unknown rather than a guessed state.
    assert_eq!(auto_pair_enabled(None), None);
    assert_eq!(auto_pair_enabled(Some(&json!({"paired": false}))), None);
}

#[test]
fn api_key_loads_from_pairing_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    std::fs::write(&path, r#"{"api_key":"abc123","paired":true}"#).unwrap();
    assert_eq!(load_api_key(&path).as_deref(), Some("abc123"));
    // Missing file / empty key both yield None (the unpaired case).
    assert!(load_api_key(Path::new("/nonexistent/pairing.json")).is_none());
    std::fs::write(&path, r#"{"api_key":""}"#).unwrap();
    assert!(load_api_key(&path).is_none());
}

#[test]
fn run_json_reads_present_and_tolerates_garbage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.json");
    std::fs::write(&path, r#"{"a":1}"#).unwrap();
    assert!(read_run_json(&path).is_some());
    std::fs::write(&path, "not json").unwrap();
    assert!(read_run_json(&path).is_none());
    assert!(read_run_json(Path::new("/nonexistent/x.json")).is_none());
}
