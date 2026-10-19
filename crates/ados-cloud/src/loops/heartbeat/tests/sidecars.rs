//! Sidecar readers folded into the heartbeat: plugin state, linked peers,
//! CRSF status and config-status errors.

use super::*;

fn write_named(dir: &std::path::Path, name: &str, body: &str) {
    use std::io::Write;
    std::fs::create_dir_all(dir).unwrap();
    std::fs::File::create(dir.join(name))
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
}

#[test]
fn fresh_plugin_sidecars_fold_keyed_by_id_with_the_slice_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    write_named(
        &dir,
        "com.example.world-state.json",
        r#"{"state":"active","gaussianCount":42}"#,
    );
    write_named(&dir, "follow-me-state.json", r#"{"lock":"locked"}"#);
    write_named(&dir, "bad-state.json", "{ not json");
    write_named(&dir, "notes.txt", "ignored: not a *-state.json file");

    let out = read_plugin_state_sidecars_from(&dir, std::time::SystemTime::now());
    // Keyed by id (filename minus -state.json); the slice is opaque/verbatim.
    assert_eq!(out["com.example.world"]["state"], "active");
    assert_eq!(out["com.example.world"]["gaussianCount"], 42);
    assert_eq!(out["follow-me"]["lock"], "locked");
    // Malformed JSON + non-state files are skipped, never the whole read.
    assert!(!out.contains_key("bad"));
    assert!(!out.contains_key("notes"));
}

#[test]
fn a_stale_plugin_sidecar_is_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    write_named(
        &dir,
        "com.example.world-state.json",
        r#"{"state":"active"}"#,
    );
    // A reference `now` an hour after the just-written file -> past the gate.
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    assert!(read_plugin_state_sidecars_from(&dir, later).is_empty());
}

#[test]
fn an_absent_plugin_dir_is_an_empty_map() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("does-not-exist");
    assert!(read_plugin_state_sidecars_from(&dir, std::time::SystemTime::now()).is_empty());
}

#[test]
fn linked_peers_fold_fresh_remap_camelcase_and_drop_stale_and_idless() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let now_ms = (now_s * 1000.0) as i64;
    // A fresh peer, a stale one (2 min old, past the 60 s window), and an
    // id-less entry — only the fresh, id-bearing peer survives.
    let body = format!(
        r#"{{"version":1,"wall_time_unix":{now_s},"peers":[
                {{"device_id":"drone-a","role":"drone","channel":149,"rssi_dbm":-51,"last_seen_unix":{now_s}}},
                {{"device_id":"drone-old","role":"drone","channel":157,"rssi_dbm":-70,"last_seen_unix":{}}},
                {{"device_id":"","role":"drone","channel":153,"rssi_dbm":-60,"last_seen_unix":{now_s}}}
            ]}}"#,
        now_s - 120.0
    );
    write_named(&dir, "linked-peers.json", &body);
    let out = read_linked_peers_sidecar_from(&dir.join("linked-peers.json"), now_ms).unwrap();
    assert_eq!(out.len(), 1);
    // The wire keys are the camelCase set the GCS `LinkedPeer` consumes and
    // the strict cloud validator declares (nothing snake_case survives).
    let v = serde_json::to_value(&out[0]).unwrap();
    assert_eq!(v["deviceId"], "drone-a");
    assert_eq!(v["role"], "drone");
    assert_eq!(v["channel"], 149);
    assert_eq!(v["rssiDbm"], -51);
    assert!((v["seenAtUnix"].as_f64().unwrap() - now_s).abs() < 1.0);
    for k in v.as_object().unwrap().keys() {
        assert!(!k.contains('_'), "{k} must be camelCase on the wire");
    }
}

#[test]
fn linked_peers_absent_when_missing_or_every_entry_stale() {
    // A missing file reads None so a drone / a peerless ground node stays
    // byte-identical (never an empty `linkedPeers`).
    let missing = std::path::Path::new("/run/ados/does-not-exist-linked-peers.json");
    assert!(read_linked_peers_sidecar_from(missing, 1_000_000).is_none());
    // A file whose only peer is stale reads None — the dead-writer case
    // never republishes a ghost peer as a confident list.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    write_named(
        &dir,
        "linked-peers.json",
        r#"{"peers":[{"device_id":"d","role":"drone","channel":1,"rssi_dbm":-1,"last_seen_unix":1.0}]}"#,
    );
    // A reference `now` far in the future puts the sole entry past the gate.
    assert!(
        read_linked_peers_sidecar_from(&dir.join("linked-peers.json"), 10_000_000_000).is_none()
    );
}

#[test]
fn a_fresh_crsf_sidecar_folds_and_a_stale_or_bad_one_reads_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    // The exact sidecar body the lane writes, including its extra derived
    // `flyable` field — outside the pinned block, ignored by the parse.
    write_named(
        &dir,
        "crsf-stats.json",
        r#"{"v":1,"state":"link_ok","rssi_dbm":-51,"lq_uplink":99,"lq_downlink":97,
                "snr_db":8,"band":null,"packet_rate_hz":150,"tx_power_mw":100,
                "tx_frames_per_s":149.8,"rx_frames_per_s":12.0,"rf_unverified":false,
                "flyable":true,"mode":"crsf_rc","channel_source":"hid","relay_role":null,
                "fc_command_down_gated":null}"#,
    );
    let path = dir.join("crsf-stats.json");
    let fresh = read_crsf_sidecar_from(&path, std::time::SystemTime::now()).unwrap();
    assert_eq!(fresh.state.as_deref(), Some("link_ok"));
    assert_eq!(fresh.rssi_dbm, Some(-51));
    assert_eq!(fresh.packet_rate_hz, Some(150));
    // The measured TX power carries under the sidecar's real key. A
    // `tx_power_dbm` field here silently dropped it to null (serde ignored
    // the unmatched key), so a cloud-reached node showed no power.
    assert_eq!(fresh.tx_power_mw, Some(100));
    assert_eq!(fresh.rf_unverified, Some(false));
    // A crsf_rc lane has no MAVLink-over-ELRS command path to gate.
    assert!(fresh.fc_command_down_gated.is_none());
    assert_eq!(fresh.band, None);
    assert_eq!(fresh.relay_role, None);
    // Stale mtime (a reference `now` an hour later) → absent, so a dead
    // lane service's lingering file stops asserting a live lane.
    let later = std::time::SystemTime::now() + Duration::from_secs(3600);
    assert!(read_crsf_sidecar_from(&path, later).is_none());
    // Missing + malformed both read absent, never a default block.
    assert!(read_crsf_sidecar_from(&dir.join("nope.json"), std::time::SystemTime::now()).is_none());
    write_named(&dir, "crsf-bad.json", "{ not json");
    assert!(
        read_crsf_sidecar_from(&dir.join("crsf-bad.json"), std::time::SystemTime::now()).is_none()
    );
}

#[test]
fn a_mavlink_standby_crsf_sidecar_carries_the_command_down_gate() {
    // The exact body the lane writes while standing by in mavlink mode: the
    // MAVLink router owns the carrier telemetry-only, so the command-down
    // gate reads true. A consumer of only the crsf block must see it.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    write_named(
        &dir,
        "crsf-stats.json",
        r#"{"v":1,"state":"ready","rssi_dbm":null,"lq_uplink":null,"lq_downlink":null,
                "snr_db":null,"band":null,"packet_rate_hz":null,"tx_power_mw":null,
                "tx_frames_per_s":null,"rx_frames_per_s":null,"rf_unverified":null,
                "flyable":false,"mode":"mavlink","channel_source":null,"pic":null,
                "relay_role":null,"fc_command_down_gated":true}"#,
    );
    let path = dir.join("crsf-stats.json");
    let fresh = read_crsf_sidecar_from(&path, std::time::SystemTime::now()).unwrap();
    assert_eq!(fresh.state.as_deref(), Some("ready"));
    assert_eq!(fresh.mode.as_deref(), Some("mavlink"));
    assert_eq!(fresh.fc_command_down_gated, Some(true));
}

#[test]
fn config_status_sidecars_surface_only_live_errors_sorted_by_service() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    // Two faulty services (unsorted on disk), one healthy (null error → skip).
    write_named(
        &dir,
        "config-status-mavlink.json",
        r#"{"service":"mavlink","error":"invalid type: string, expected u32","generated_at_ms":1}"#,
    );
    write_named(
        &dir,
        "config-status-cloud.json",
        r#"{"service":"cloud","error":"unknown field `bogus`","generated_at_ms":2}"#,
    );
    write_named(
        &dir,
        "config-status-supervisor.json",
        r#"{"service":"supervisor","error":null,"generated_at_ms":3}"#,
    );
    // Non-matching + staging + malformed files are ignored, never the read.
    write_named(&dir, "config-status-bad.json", "{ not json");
    write_named(
        &dir,
        "config-status-ground_station.json.tmp.999",
        r#"{"service":"ground_station","error":"x"}"#,
    );
    write_named(&dir, "notes.txt", "unrelated");

    let out = read_config_error_sidecars_from(&dir);
    // Only the two live errors, sorted by service (cloud before mavlink).
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].service, "cloud");
    assert_eq!(out[0].error, "unknown field `bogus`");
    assert_eq!(out[1].service, "mavlink");
    // The healthy (null-error) service and the staging/malformed/other files
    // are absent.
    assert!(!out.iter().any(|e| e.service == "supervisor"));
    assert!(!out.iter().any(|e| e.service == "ground_station"));
    assert!(!out.iter().any(|e| e.service == "bad"));
}

#[test]
fn an_absent_config_status_dir_is_an_empty_list() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("does-not-exist");
    assert!(read_config_error_sidecars_from(&dir).is_empty());
}

#[test]
fn config_status_version_matches_registry() {
    // The const lives in the writer crate (ados-config); this reader crate
    // sees both it and the shared registry, so the drift gate lives here.
    assert_eq!(
        ados_config::CONFIG_STATUS_SIDECAR_VERSION,
        ados_protocol::contracts::sidecar_version("config-status").unwrap()
    );
}
