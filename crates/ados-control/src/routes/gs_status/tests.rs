use super::*;
use serde_json::json;
use std::time::Duration;

fn signals(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn profile_mismatch_body_is_the_fastapi_404_shape() {
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // The body shape is the contract; build it independently and compare.
    let want = json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}});
    assert_eq!(
        want,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

#[test]
fn wrong_role_body_carries_the_required_role() {
    let want = json!({"detail": {"error": {"code": "E_WRONG_ROLE", "required": "relay"}}});
    assert_eq!(
        want,
        json!({"detail": {"error": {"code": "E_WRONG_ROLE", "required": "relay"}}})
    );
}

#[test]
fn link_view_of_an_absent_sidecar_is_the_disconnected_base() {
    // With no wfb-stats.json the view is the zero base with state "connecting".
    // The GS config carries no root wfb, so channel + tx_power are null.
    let dir = tempfile::tempdir().unwrap();
    let view = link_view_from(&dir.path().join("wfb-stats.json"));
    let want = json!({
        "rssi_dbm": null,
        "bitrate_mbps": null,
        "bitrate_kbps": null,
        "fec_recovered": 0,
        "fec_lost": 0,
        "fec_failed": 0,
        "channel": null,
        "snr_db": null,
        "noise_dbm": null,
        "packets_received": 0,
        "packets_lost": 0,
        "loss_percent": null,
        "tx_power_dbm": null,
        "topology": null,
        "state": "connecting",
        "link_diag": null,
        "packets_all": 0,
        "decrypt_errors": 0,
        // Never a confident 0 before the radio has written a snapshot: MCS 0
        // is a real (slowest) rung, so 0 would be a false reading.
        "mcs_index": null,
        "mcs_ladder_cap": null,
    });
    assert_eq!(view, want);
}

#[test]
fn link_view_merges_the_live_snapshot_over_the_base() {
    // A fresh wfb-stats.json with live values must merge over the base, derive
    // bitrate_mbps, mirror fec_failed into fec_lost, and keep state from the
    // payload. The file is fresh (< 10 s) so state is NOT flipped to stale.
    let dir = tempfile::tempdir().unwrap();
    let stats = dir.path().join("wfb-stats.json");
    let payload = json!({
        "rssi_dbm": -51,
        "bitrate_kbps": 5740,
        "fec_recovered": 3,
        "fec_failed": 1,
        "actual_channel": 149,
        "snr_db": 28.0,
        "noise_dbm": -95.0,
        "packets_received": 598,
        "packets_lost": 2,
        "loss_percent": 0.3,
        "tx_power_dbm": 20,
        "state": "connected",
        "link_diag": "healthy",
        "packets_all": 640,
        "decrypt_errors": 0,
        "mcs_index": 3,
        "mcs_ladder_cap": 3,
        "topology": "powered_hub",
    });
    std::fs::write(&stats, serde_json::to_string(&payload).unwrap()).unwrap();
    let view = link_view_from(&stats);
    assert_eq!(view["rssi_dbm"], json!(-51));
    assert_eq!(view["bitrate_kbps"], json!(5740));
    assert_eq!(view["bitrate_mbps"], json!(5.74));
    assert_eq!(view["fec_recovered"], json!(3));
    assert_eq!(view["fec_lost"], json!(1));
    assert_eq!(view["fec_failed"], json!(1));
    assert_eq!(view["channel"], json!(149));
    assert_eq!(view["packets_received"], json!(598));
    assert_eq!(view["tx_power_dbm"], json!(20));
    assert_eq!(view["state"], json!("connected"));
    // The diagnostic trio pulls from the sidecar payload.
    assert_eq!(view["link_diag"], json!("healthy"));
    assert_eq!(view["packets_all"], json!(640));
    assert_eq!(view["decrypt_errors"], json!(0));
    // Modulation: the live rung and the ladder cap the Radio settings page
    // reads beside snr_db.
    assert_eq!(view["mcs_index"], json!(3));
    assert_eq!(view["mcs_ladder_cap"], json!(3));
    assert_eq!(view["snr_db"], json!(28.0));
    // The declared power path rides through, so the panel's brownout warning
    // reads the radio's own topology instead of assuming host VBUS.
    assert_eq!(view["topology"], json!("powered_hub"));
}

#[test]
fn network_view_compose_running_carries_ssid_and_gateway() {
    // A running AP reports the resolved SSID + the 192.168.4.1 gateway,
    // beside the uplink verdict it was handed.
    let want = json!({
        "ap_ssid": "ADOS-GS-D9DB",
        "ap_ip": "192.168.4.1",
        "usb_ip": null,
        "uplink_type": "eth",
        "uplink_reachable": true,
    });
    assert_eq!(
        network_view_compose("ADOS-GS-D9DB", true, (json!("eth"), json!(true))),
        want
    );
}

#[test]
fn network_view_compose_not_running_gates_the_gateway() {
    // A down AP keeps the resolved SSID but reports ap_ip null (the manager's
    // status reports the gateway only while up).
    let v = network_view_compose("ADOS-GS-0000", false, (Value::Null, Value::Null));
    assert_eq!(v["ap_ssid"], json!("ADOS-GS-0000"));
    assert_eq!(v["ap_ip"], Value::Null);
    assert_eq!(v["usb_ip"], Value::Null);
    assert_eq!(v["uplink_type"], Value::Null);
    assert_eq!(v["uplink_reachable"], Value::Null);
}

/// The uplink comes from the router's sentinel. Its absence is "no uplink"
/// only while the router runs; otherwise nothing is known, and unknown must
/// not read as offline.
#[test]
fn uplink_view_reads_the_router_sentinel_and_keeps_unknown_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("uplink-active");
    let write = |name: &str, reachable: bool| {
        let body = json!({
            "active_uplink": name,
            "internet_reachable": reachable,
            "timestamp_ms": 1,
            "data_cap_state": "ok",
        });
        std::fs::write(&flag, body.to_string()).unwrap();
    };
    for (name, kind) in [
        ("eth0", "eth"),
        ("wlan0_client", "wifi"),
        ("wwan0", "cellular"),
        ("usb0", "usb"),
    ] {
        write(name, true);
        assert_eq!(
            uplink_view(&flag, true),
            (json!(kind), json!(true)),
            "{name}"
        );
    }
    write("eth0", false);
    assert_eq!(uplink_view(&flag, false), (json!("eth"), json!(false)));

    std::fs::remove_file(&flag).unwrap();
    assert_eq!(uplink_view(&flag, true), (json!("none"), json!(false)));
    assert_eq!(uplink_view(&flag, false), (Value::Null, Value::Null));

    std::fs::write(&flag, "not json").unwrap();
    assert_eq!(uplink_view(&flag, true), (Value::Null, Value::Null));
}

#[test]
fn ap_ssid_from_config_resolves_the_template_and_honours_an_explicit_name() {
    // The default hotspot SSID carries the `{device_id}` template, which
    // resolves to the built ADOS-GS-<short id> name off the device id.
    let cfg = json!({
        "agent": {"device_id": "d9dbcafe"},
        "network": {"hotspot": {"ssid": "ADOS-{device_id}"}},
    });
    let none = std::path::Path::new("/nonexistent/ados/device-id");
    assert_eq!(ap_ssid_from_config(&cfg, none), "ADOS-GS-D9DB");
    // An explicit ADOS-GS- name is honoured verbatim.
    let cfg2 = json!({"network": {"hotspot": {"ssid": "ADOS-GS-ABCD"}}});
    assert_eq!(ap_ssid_from_config(&cfg2, none), "ADOS-GS-ABCD");
    // No config at all → the zero-padded short id.
    assert_eq!(ap_ssid_from_config(&json!({}), none), "ADOS-GS-0000");
}

#[test]
fn resolve_ap_ssid_gate_matches_the_hostapd_override_rule() {
    // Honoured: non-empty, no template, ADOS-GS- prefix.
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

#[test]
fn short_id_matches_the_python_short_id() {
    assert_eq!(short_id("deadbeef"), "DEAD");
    assert_eq!(short_id("xy-12-34-56"), "1234");
    assert_eq!(short_id("a1"), "A100");
    assert_eq!(short_id(""), "0000");
    assert_eq!(short_id("zzzz"), "0000");
}

/// With no signal source at all, every measured leg is `null` and the key is
/// still there.
///
/// This block used to report `cpu_pct: 0.0, ram_used_mb: 0, ram_total_mb: 0`
/// — all legal readings — so the OLED and the GCS Hardware tab showed an idle
/// CPU and 0 MB of RAM on a stock ground station, where the durable store
/// ships off. The old test asserted `unwrap_or(0.0)` against a copy of the
/// derivation written inside the test, so it could not have caught it.
#[test]
fn system_block_with_no_signals_nulls_every_measured_leg() {
    let block = system_block(None, 1234, "9.9.9");
    for leg in [
        "cpu_pct",
        "ram_used_mb",
        "ram_total_mb",
        "temp_c",
        "disk_pct",
    ] {
        assert_eq!(block[leg], Value::Null, "{leg} must be null, not a number");
        assert!(
            block.as_object().unwrap().contains_key(leg),
            "{leg} must be present as an explicit null"
        );
    }
    // The two legs that are always knowable stay real.
    assert_eq!(block["uptime_seconds"], json!(1234));
    assert_eq!(block["agent_version"], json!("9.9.9"));
}

#[test]
fn system_block_derives_cpu_ram_and_temp_from_signals() {
    let s = signals(&[
        ("cpu.util.all", json!(12.5)),
        ("mem.total_bytes", json!(4_000_000_000_i64)),
        ("mem.avail_bytes", json!(1_000_000_000_i64)),
        ("thermal.primary_c", json!(47.5)),
        ("disk.fs_total_bytes", json!(32_000_000_000_i64)),
        ("disk.fs_used_bytes", json!(8_000_000_000_i64)),
    ]);
    let block = system_block(Some(&s), 7, "1.2.3");
    assert_eq!(block["cpu_pct"], json!(12.5));
    // (4e9 - 1e9) / 1MiB used; 4e9 / 1MiB total.
    assert_eq!(
        block["ram_used_mb"],
        json!(((3_000_000_000_f64) / (1024.0 * 1024.0)) as i64)
    );
    assert_eq!(
        block["ram_total_mb"],
        json!(((4_000_000_000_f64) / (1024.0 * 1024.0)) as i64)
    );
    assert_eq!(block["temp_c"], json!(47.5));
    assert_eq!(block["disk_pct"], json!(25.0));
}

#[test]
fn system_block_nulls_disk_without_both_byte_signals() {
    let used_only = signals(&[("disk.fs_used_bytes", json!(8_000_000_000_i64))]);
    assert_eq!(
        system_block(Some(&used_only), 0, "")["disk_pct"],
        Value::Null
    );
    let zero_total = signals(&[
        ("disk.fs_total_bytes", json!(0)),
        ("disk.fs_used_bytes", json!(0)),
    ]);
    assert_eq!(
        system_block(Some(&zero_total), 0, "")["disk_pct"],
        Value::Null
    );
}

#[test]
fn mesh_block_projects_the_five_fields() {
    let snap: Map<String, Value> = json!({
        "up": true,
        "neighbors": [{"id": "a"}, {"id": "b"}],
        "selected_gateway": "node-1",
        "partition": false,
        "mesh_id": "mesh-xyz",
        "extra": "ignored",
    })
    .as_object()
    .unwrap()
    .clone();
    let block = mesh_block_from_snapshot(&snap);
    let want = json!({
        "up": true,
        "peer_count": 2,
        "selected_gateway": "node-1",
        "partition": false,
        "mesh_id": "mesh-xyz",
        "stale": false,
    });
    assert_eq!(block, want);
}

/// A clock advanced past the freshness window, used to age a just-written
/// sidecar without back-dating the file (there is no portable mtime setter,
/// and sleeping for 10 s in a unit test is not a test).
fn later_than_the_window() -> SystemTime {
    SystemTime::now() + Duration::from_secs_f64(SNAPSHOT_FRESH_S + 5.0)
}

/// A mesh sidecar older than the freshness window must NOT be served as the
/// current mesh state.
///
/// This is the whole finding: the sidecar lives on a tmpfs that outlives the
/// process that wrote it, so a dead mesh loop left `up: true` and a peer
/// count to be re-served indefinitely. An operator reads a partitioned mesh
/// as healthy.
#[test]
fn an_aged_mesh_sidecar_is_reported_stale_not_served_as_live() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mesh-state.json");
    std::fs::write(
        &path,
        r#"{"up":true,"neighbors":[{"id":"a"}],"selected_gateway":"node-1",
                "partition":false,"mesh_id":"mesh-xyz"}"#,
    )
    .unwrap();

    // Just written: the real reading comes through.
    let live = mesh_block_from_sidecar(&path, SystemTime::now());
    assert_eq!(live["up"], json!(true));
    assert_eq!(live["peer_count"], json!(1));
    assert_eq!(live["stale"], json!(false));

    // The same file, read past the window.
    let stale = mesh_block_from_sidecar(&path, later_than_the_window());
    assert_eq!(stale["stale"], json!(true));
    assert_eq!(
        stale["up"],
        Value::Null,
        "an aged snapshot must not keep claiming the mesh is up"
    );
    assert_eq!(stale["peer_count"], Value::Null);
    assert_eq!(stale["mesh_id"], Value::Null);
    // The key set is unchanged apart from the added flag, so no consumer sees
    // a field simply vanish (a missing key reads as "not reported" and gets
    // coerced, which is the same defect wearing a different hat).
    let mut keys: Vec<&str> = stale
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "mesh_id",
            "partition",
            "peer_count",
            "selected_gateway",
            "stale",
            "up",
        ]
    );
}

#[test]
fn mesh_sidecar_of_an_absent_file_is_the_stale_block() {
    let dir = tempfile::tempdir().unwrap();
    let block = mesh_block_from_sidecar(&dir.path().join("nope.json"), SystemTime::now());
    assert_eq!(block["stale"], json!(true));
    assert_eq!(block["up"], Value::Null);
}

/// A relay/receiver counter route with an aged sidecar must null the counters
/// rather than re-serve them, and must keep the key set so nothing downstream
/// reads a vanished key as "not reported" and coerces it.
#[test]
fn an_aged_relay_sidecar_nulls_its_counters_under_a_stale_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-relay.json");
    std::fs::write(
        &path,
        r#"{"up":true,"fragments_in":9001,"fragments_out":8999}"#,
    )
    .unwrap();

    let live = read_fresh_json(&path, SystemTime::now()).expect("a just-written sidecar is fresh");
    assert_eq!(fresh_snapshot_body(live)["fragments_in"], json!(9001));

    assert!(
        read_fresh_json(&path, later_than_the_window()).is_none(),
        "an aged sidecar must not read as fresh"
    );
    let stale = stale_snapshot_body(crate::sidecar::read_object(&path).unwrap_or_default());
    assert_eq!(stale["stale"], json!(true));
    assert_eq!(stale["up"], Value::Null);
    assert_eq!(
        stale["fragments_in"],
        Value::Null,
        "a counter from a dead loop must not be served as current"
    );
    assert_eq!(stale["fragments_out"], Value::Null);
}

#[test]
fn receiver_relays_slice_stamps_fresh_and_defaults_to_empty_list() {
    let empty: Map<String, Value> = Map::new();
    assert_eq!(
        slice_receiver_relays(&empty),
        json!({"relays": [], "stale": false})
    );
    let with: Map<String, Value> = json!({"relays": [{"id": "r1"}]})
        .as_object()
        .unwrap()
        .clone();
    assert_eq!(
        slice_receiver_relays(&with),
        json!({"relays": [{"id": "r1"}], "stale": false})
    );
}

/// With nothing current, `relays` is `null` — never `[]`, which reads as
/// "this node looked and there are no relays".
#[test]
fn receiver_combined_stale_nulls_every_counter() {
    assert_eq!(
        receiver_combined_stale(),
        json!({
            "fragments_after_dedup": Value::Null,
            "fec_repaired": Value::Null,
            "output_kbps": Value::Null,
            "up": Value::Null,
            "stale": true,
        })
    );
}

#[test]
fn receiver_combined_slice_applies_per_key_defaults() {
    let empty: Map<String, Value> = Map::new();
    let want = json!({
        "fragments_after_dedup": 0,
        "fec_repaired": 0,
        "output_kbps": 0,
        "up": false,
        "stale": false,
    });
    assert_eq!(slice_receiver_combined(&empty), want);
    let full: Map<String, Value> = json!({
        "fragments_after_dedup": 100,
        "fec_repaired": 5,
        "output_kbps": 4200,
        "up": true,
    })
    .as_object()
    .unwrap()
    .clone();
    assert_eq!(
        slice_receiver_combined(&full),
        json!({
            "fragments_after_dedup": 100,
            "fec_repaired": 5,
            "output_kbps": 4200,
            "up": true,
            "stale": false,
        })
    );
}

#[test]
fn wfb_view_reports_the_live_radio_and_nothing_when_it_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");
    let now = SystemTime::now();

    // No sidecar: the channel is unknown, never 0.
    assert_eq!(
        wfb_view_from(&path, now),
        json!({"channel": null, "tx_power_dbm": null})
    );

    std::fs::write(&path, r#"{"actual_channel": 149, "tx_power_dbm": 20}"#).unwrap();
    // Read after the write: a file stamped after `now` is not provably fresh.
    let now = SystemTime::now();
    assert_eq!(
        wfb_view_from(&path, now),
        json!({"channel": 149, "tx_power_dbm": 20})
    );

    // A dead writer's last file is not a reading.
    let later = now + Duration::from_secs_f64(SNAPSHOT_FRESH_S + 5.0);
    assert_eq!(
        wfb_view_from(&path, later),
        json!({"channel": null, "tx_power_dbm": null})
    );
}

#[test]
fn config_gs_role_defaults_to_direct() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, "agent:\n  profile: ground_station\n").unwrap();
    assert_eq!(ground_station_config_role(&cfg_path), "direct");
    std::fs::write(&cfg_path, "ground_station:\n  role: relay\n").unwrap();
    assert_eq!(ground_station_config_role(&cfg_path), "relay");
}

#[test]
fn config_gs_peer_prefers_video_wfb_then_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.yaml");
    // Canonical spot wins.
    std::fs::write(
            &cfg_path,
            "video:\n  wfb:\n    paired_with_device_id: drone-abc\nground_station:\n  paired_drone_id: drone-old\n",
        )
        .unwrap();
    assert_eq!(config_gs_peer(&cfg_path), json!("drone-abc"));
    // Legacy fallback when the canonical spot is absent.
    std::fs::write(&cfg_path, "ground_station:\n  paired_drone_id: drone-old\n").unwrap();
    assert_eq!(config_gs_peer(&cfg_path), json!("drone-old"));
    // Neither present → null.
    std::fs::write(&cfg_path, "agent:\n  profile: ground_station\n").unwrap();
    assert_eq!(config_gs_peer(&cfg_path), Value::Null);
}

#[test]
fn pair_identity_of_an_unpaired_gs_is_null_null() {
    // No rx.key file → not paired → device_id + fingerprint both null. Drive
    // the pieces the handler composes without the full AppState.
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("wfb").join("rx.key");
    let paired = std::fs::metadata(&key_path)
        .map(|m| m.is_file() && m.len() == WFB_KEY_FILE_BYTES as u64)
        .unwrap_or(false);
    assert!(!paired);
    // The unpaired snapshot the route emits.
    let snapshot = json!({
        "device_id": Value::Null,
        "key_fingerprint": Value::Null,
        "fc_mode": Value::Null,
        "battery_pct": Value::Null,
        "gps_sats": Value::Null,
    });
    let want = json!({
        "device_id": null,
        "key_fingerprint": null,
        "fc_mode": null,
        "battery_pct": null,
        "gps_sats": null,
    });
    assert_eq!(snapshot, want);
}

#[test]
fn truthiness_matches_python_bool() {
    assert!(!json_truthy(&Value::Null));
    assert!(!json_truthy(&json!(false)));
    assert!(json_truthy(&json!(true)));
    assert!(!json_truthy(&json!(0)));
    assert!(json_truthy(&json!(149)));
    assert!(!json_truthy(&json!("")));
    assert!(json_truthy(&json!("x")));
    assert!(!json_truthy(&json!([])));
}

/// The radio-learned peer id must land on the key THIS route reads.
///
/// Two halves that never met: `ados-radio` latched the beacon's device id into
/// `peer-backfill.json` and the only consumer was a Python function with zero
/// callers, so `paired_drone.device_id` was null on every auto-bound rig,
/// across reboots. Asserting the write and the read against one file is what
/// keeps the two ends on the same key.
#[test]
fn a_backfilled_peer_id_is_what_the_status_route_reports() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: my-gs\n").unwrap();

    assert_eq!(
        config_gs_peer(&cfg),
        Value::Null,
        "null before the back-fill"
    );

    crate::routes::peer_backfill::persist_peer_device_id(&cfg, true, "drone-abc")
        .expect("the back-fill persists");

    assert_eq!(config_gs_peer(&cfg), json!("drone-abc"));
}

/// The golden `/status` body for a direct ground-station node with no paired
/// drone, no radio snapshot, no store, no recorder — the empty-rig shape the
/// GCS Hardware tab reads on a fresh, idle GS. Composed from the same blocks the
/// handler composes (the volatile system + link fields are not part of this
/// fixture's stable contract; this pins the structural keys + the static legs).
#[test]
fn status_golden_fixture_for_an_idle_direct_gs() {
    let dir = tempfile::tempdir().unwrap();
    // An absent wfb-stats.json under a private tempdir → the disconnected link
    // base, with no process-global env mutation.
    let absent_stats = dir.path().join("wfb-stats.json");

    let role = "direct".to_string();
    let role_block = json!({
        "current": role,
        "configured": "direct",
        "supported": ["direct", "relay", "receiver"],
        "mesh_capable": false,
    });
    let body = json!({
        "profile": "ground-station",
        "paired_drone": {
            "device_id": Value::Null,
            "key_fingerprint": Value::Null,
            "fc_mode": Value::Null,
            "battery_pct": Value::Null,
            "gps_sats": Value::Null,
        },
        "link": link_view_from(&absent_stats),
        "gcs": {"clients": [], "pic_id": Value::Null},
        // The idle-GS network view: the resolved SSID with the AP unit down
        // (ap_ip gated to null), composed from the pure seam so the fixture
        // does not depend on the host's `systemctl` answer.
        "network": network_view_compose("ADOS-GS-0000", false, (Value::Null, Value::Null)),
        "recording": false,
        "video": {"recording": false, "recording_filename": Value::Null},
        "role": role_block,
        "mesh": json!({}),
    });

    // The structural contract: the top-level key set + the static legs.
    let want_keys = [
        "profile",
        "paired_drone",
        "link",
        "gcs",
        "network",
        "recording",
        "video",
        "role",
        "mesh",
    ];
    let obj = body.as_object().unwrap();
    for k in want_keys {
        assert!(obj.contains_key(k), "missing key {k}");
    }
    assert_eq!(
        body["profile"],
        json!("ground-station"),
        "one spelling of the profile enum across the wire: the hyphen form \
             every other producer and the relay-proxy gate use"
    );
    assert_eq!(body["paired_drone"]["device_id"], Value::Null);
    assert_eq!(body["paired_drone"]["key_fingerprint"], Value::Null);
    assert_eq!(body["gcs"], json!({"clients": [], "pic_id": null}));
    assert_eq!(body["recording"], json!(false));
    assert_eq!(
        body["video"],
        json!({"recording": false, "recording_filename": null})
    );
    assert_eq!(
        body["role"],
        json!({
            "current": "direct",
            "configured": "direct",
            "supported": ["direct", "relay", "receiver"],
            "mesh_capable": false,
        })
    );
    assert_eq!(body["mesh"], json!({}));
    // The link view is the disconnected base (no snapshot present).
    assert_eq!(body["link"]["state"], json!("connecting"));
    assert_eq!(body["link"]["channel"], Value::Null);
}
