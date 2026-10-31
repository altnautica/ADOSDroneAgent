use super::state::{age_out_relays, churn_step, upsert_relays};
use super::*;
use std::collections::HashMap;
use std::net::Ipv4Addr;

#[test]
fn aggregate_args_with_local_nic() {
    // wfb_rx -p 0 -i <link> -c 127.0.0.1 -u 5600 -a 5800 -K <rx.key> <iface>...
    let a = aggregate_args(
        &["wlan0".to_string(), "wlan1".to_string()],
        5800,
        true,
        Path::new("/etc/ados/wfb/rx.key"),
        ados_radio::config::link_id(1, 1),
    );
    assert_eq!(
        a,
        vec![
            "-p",
            "0",
            "-i",
            "257",
            "-c",
            "127.0.0.1",
            "-u",
            "5600",
            "-a",
            "5800",
            "-K",
            "/etc/ados/wfb/rx.key",
            "wlan0",
            "wlan1"
        ]
    );
}

#[test]
fn aggregate_args_without_local_nic_drops_iface() {
    let a = aggregate_args(&["wlan0".to_string()], 5800, false, Path::new("/k"), 257);
    assert!(!a.contains(&"wlan0".to_string()));
    // The aggregator still listens on the relay forward port.
    let ai = a.iter().position(|x| x == "-a").unwrap();
    assert_eq!(a[ai + 1], "5800");
}

/// The aggregator's stats line is `wfb_rx`'s own eleven-field `PKT` line on
/// stdout (`vendor/wfb-ng/src/rx.cpp:501`); nothing else carries counters.
#[test]
fn the_aggregator_stats_line_yields_dedup_fec_and_output() {
    assert_eq!(
        parse_receiver_stats_line("1750000000000\tPKT\t120:180000:0:1:110:100:4:0:0:100:125000"),
        Some(AggregatorPkt {
            unique: 100,
            fec_recovered: 4,
            bytes_out: 125_000,
        })
    );
    assert_eq!(
        parse_receiver_stats_line("999 PKT n_out:1500 fec_rec:12"),
        None
    );
    assert_eq!(
        parse_receiver_stats_line("1\tRX_ANT\t5745:1:20\t0\t1:-50:-49:-48:20:21:22"),
        None
    );
}

#[test]
fn receiver_state_json_shape_flattens_relays() {
    let mut s = ReceiverState::default();
    s.relays.push(RelayStats {
        mac: "aa:bb:cc:dd:ee:ff".into(),
        last_seen_ms: 123,
        fragments: Some(500),
    });
    let v = serde_json::to_value(&s).unwrap();
    for k in [
        "role",
        "drone_iface",
        "listen_port",
        "accept_local_nic",
        "mesh_iface",
        "relays",
        "fragments_after_dedup",
        "fec_repaired",
        "output_kbps",
        "up",
        "mesh_adapter",
    ] {
        assert!(v.get(k).is_some(), "missing key {k}");
    }
    assert_eq!(v["relays"][0]["mac"], "aa:bb:cc:dd:ee:ff");
    assert_eq!(v["relays"][0]["fragments"], 500);
}

#[test]
fn upsert_relays_adds_new_and_refreshes_existing() {
    let mut s = ReceiverState::default();
    // First sight of two relays → both reported new.
    let newly = upsert_relays(&mut s, &["aa".into(), "bb".into()], 1_000);
    assert_eq!(newly.len(), 2);
    assert_eq!(s.relays.len(), 2);
    // Re-sight one + a third → only the third is new, last_seen refreshed.
    let newly = upsert_relays(&mut s, &["aa".into(), "cc".into()], 5_000);
    assert_eq!(newly, vec!["cc".to_string()]);
    assert_eq!(s.relays.len(), 3);
    let aa = s.relays.iter().find(|r| r.mac == "aa").unwrap();
    assert_eq!(aa.last_seen_ms, 5_000);
}

#[test]
fn age_out_relays_removes_only_stale() {
    let mut s = ReceiverState::default();
    upsert_relays(&mut s, &["fresh".into(), "stale".into()], 0);
    // Mark "fresh" as recently seen, leave "stale" old.
    upsert_relays(&mut s, &["fresh".into()], 10_000);
    let removed = age_out_relays(&mut s, 10_000 + 1);
    // "stale" last seen at 0, now 10_001 → > RELAY_GRACE_MS (4000) → removed.
    assert_eq!(removed, vec!["stale".to_string()]);
    assert_eq!(s.relays.len(), 1);
    assert_eq!(s.relays[0].mac, "fresh");
}

#[test]
fn churn_step_drives_connect_then_disconnect() {
    // Drive the exact decision the churn watcher emits on: first sight of two
    // relays reports both as newly connected (→ relay_connected); a later
    // poll seeing only one ages the other out past the grace window (→
    // relay_disconnected) while the still-present one is neither.
    let mut s = ReceiverState::default();

    let (newly, removed) = churn_step(&mut s, &["aa".into(), "bb".into()], &BTreeSet::new(), 1_000);
    assert_eq!(newly, vec!["aa".to_string(), "bb".to_string()]);
    assert!(removed.is_empty());

    // Re-poll past "bb"'s grace window seeing only "aa".
    let later = 1_000 + RELAY_GRACE_MS + 1;
    let (newly, removed) = churn_step(&mut s, &["aa".into()], &BTreeSet::new(), later);
    assert!(newly.is_empty(), "aa already known → not newly connected");
    assert_eq!(removed, vec!["bb".to_string()]);
    // Only the live relay remains in the map.
    assert_eq!(s.relays.len(), 1);
    assert_eq!(s.relays[0].mac, "aa");
}

#[test]
fn churn_step_no_neighbors_ages_out_all() {
    // An empty neighbor list past the grace window disconnects every relay.
    let mut s = ReceiverState::default();
    churn_step(&mut s, &["aa".into(), "bb".into()], &BTreeSet::new(), 0);
    let (newly, removed) = churn_step(&mut s, &[], &BTreeSet::new(), RELAY_GRACE_MS + 1);
    assert!(newly.is_empty());
    assert_eq!(removed.len(), 2);
    assert!(removed.contains(&"aa".to_string()));
    assert!(removed.contains(&"bb".to_string()));
    assert!(s.relays.is_empty());
}

#[test]
fn age_out_then_reconnect_emits_again() {
    // A relay that ages out and later reappears is reported new again.
    let mut s = ReceiverState::default();
    upsert_relays(&mut s, &["r1".into()], 0);
    let removed = age_out_relays(&mut s, RELAY_GRACE_MS + 1);
    assert_eq!(removed, vec!["r1".to_string()]);
    let newly = upsert_relays(&mut s, &["r1".into()], RELAY_GRACE_MS + 2);
    assert_eq!(newly, vec!["r1".to_string()]);
}

/// Stats lines accumulate: the fragment and FEC totals sum the per-interval
/// counts, the output rate is the latest interval's, and every stats line
/// advances the line counter the stall window watches.
#[tokio::test]
async fn tail_folds_aggregator_counters() {
    let state = Arc::new(Mutex::new(ReceiverState::default()));
    let lines = Arc::new(AtomicU64::new(0));
    let script =
        "printf '1\\tPKT\\t10:0:0:0:10:8:1:0:0:8:1000\\n2\\tPKT\\t10:0:0:0:10:7:2:0:0:7:2000\\n'";
    let mut child = tokio::process::Command::new("sh")
        .args(["-c", script])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sh");
    tail_aggregator_stats(child.stdout.take().unwrap(), state.clone(), lines.clone()).await;
    let _ = child.wait().await;
    let s = state.lock().await;
    assert_eq!(s.fragments_after_dedup, 15);
    assert_eq!(s.fec_repaired, 3);
    assert_eq!(s.output_kbps, 16);
    assert_eq!(lines.load(Ordering::Relaxed), 2);
}

#[test]
fn receiver_fixture_round_trips_with_python_shape() {
    // The exact JSON the Python `_write_state` produced for a receiver
    // aggregating the local NIC plus one relay forward. Deserialize into the
    // Rust struct, then re-serialize and assert the key set + values are
    // preserved (no field drift), including the nested relay entry.
    let fixture = r#"{
        "role": "receiver",
        "drone_iface": "wlan1",
        "listen_port": 5800,
        "accept_local_nic": true,
        "mesh_iface": "bat0",
        "relays": [
            {"mac": "aa:bb:cc:dd:ee:ff", "last_seen_ms": 1717000000000, "fragments": 4096}
        ],
        "fragments_after_dedup": 8000,
        "fec_repaired": 24,
        "output_kbps": 4200,
        "up": true,
        "mesh_adapter": "wlan3"
    }"#;
    let s: ReceiverState = serde_json::from_str(fixture).expect("deserialize receiver fixture");
    assert_eq!(s.role, "receiver");
    assert_eq!(s.drone_iface, "wlan1");
    assert_eq!(s.listen_port, 5800);
    assert!(s.accept_local_nic);
    assert_eq!(s.mesh_iface, "bat0");
    assert_eq!(s.relays.len(), 1);
    assert_eq!(s.relays[0].mac, "aa:bb:cc:dd:ee:ff");
    assert_eq!(s.relays[0].last_seen_ms, 1_717_000_000_000);
    assert_eq!(s.relays[0].fragments, Some(4096));
    assert_eq!(s.fragments_after_dedup, 8000);
    assert_eq!(s.fec_repaired, 24);
    assert_eq!(s.output_kbps, 4200);
    assert!(s.up);
    assert_eq!(s.mesh_adapter.as_deref(), Some("wlan3"));

    // Round-trip back to the same key set + values as the fixture.
    let re = serde_json::to_value(&s).unwrap();
    let orig: serde_json::Value = serde_json::from_str(fixture).unwrap();
    assert_eq!(re, orig);
}

#[test]
fn unmeasured_relay_serializes_null() {
    let mut s = ReceiverState::default();
    upsert_relays(&mut s, &["aa:bb:cc:dd:ee:01".into()], 1);
    let v = serde_json::to_value(&s).unwrap();
    assert!(v["relays"][0]["fragments"].is_null());
    assert!(v["relays"][0]
        .as_object()
        .unwrap()
        .contains_key("fragments"));
}

#[test]
fn relay_fragments_count_only_resolved_relays() {
    let mut s = ReceiverState::default();
    upsert_relays(
        &mut s,
        &["aa:bb:cc:dd:ee:01".into(), "aa:bb:cc:dd:ee:02".into()],
        1,
    );
    let relay_ip = Ipv4Addr::new(192, 168, 1, 50);
    let arp = HashMap::from([(relay_ip, "aa:bb:cc:dd:ee:01".to_string())]);
    let per_source = HashMap::from([(relay_ip, 40u64)]);
    fold_relay_fragments(&mut s, &per_source, &arp);
    fold_relay_fragments(&mut s, &per_source, &arp);
    // A quiet interval still counts as measured: zero added.
    fold_relay_fragments(&mut s, &HashMap::new(), &arp);
    assert_eq!(s.relays[0].fragments, Some(80));
    // No resolved address: stays unmeasured.
    assert_eq!(s.relays[1].fragments, None);
}

#[test]
fn revoked_mac_excluded_from_relays() {
    let mut s = ReceiverState::default();
    churn_step(
        &mut s,
        &["aa:bb:cc:dd:ee:01".into(), "aa:bb:cc:dd:ee:02".into()],
        &BTreeSet::new(),
        0,
    );
    let revoked = BTreeSet::from(["AA:BB:CC:DD:EE:02".to_string()]);
    let (newly, removed) = churn_step(
        &mut s,
        &["aa:bb:cc:dd:ee:01".into(), "aa:bb:cc:dd:ee:02".into()],
        &revoked,
        1_000,
    );
    assert!(newly.is_empty());
    assert_eq!(removed, vec!["aa:bb:cc:dd:ee:02".to_string()]);
    assert_eq!(s.relays.len(), 1);
    assert_eq!(s.relays[0].mac, "aa:bb:cc:dd:ee:01");
    // A revoked neighbour seen first never enters the map.
    let mut fresh = ReceiverState::default();
    let (newly, _) = churn_step(&mut fresh, &["aa:bb:cc:dd:ee:02".into()], &revoked, 0);
    assert!(newly.is_empty());
    assert!(fresh.relays.is_empty());
}

#[test]
fn receiver_fixture_empty_relays_locks_list() {
    // A receiver with no relays seen yet writes `relays: []`; the Rust
    // `Vec<RelayStats>` must accept it as an empty list (not an error).
    let fixture = r#"{
        "role": "receiver",
        "drone_iface": "",
        "listen_port": 5800,
        "accept_local_nic": false,
        "mesh_iface": "bat0",
        "relays": [],
        "fragments_after_dedup": 0,
        "fec_repaired": 0,
        "output_kbps": 0,
        "up": false
    }"#;
    let s: ReceiverState = serde_json::from_str(fixture).expect("deserialize empty relays");
    // A sidecar written before the mesh adapter was published reads as none.
    assert!(s.mesh_adapter.is_none());
    assert!(s.relays.is_empty());
    assert!(!s.accept_local_nic);
    assert!(!s.up);
    let re = serde_json::to_value(&s).unwrap();
    assert!(re["relays"].as_array().unwrap().is_empty());
}

#[test]
fn receiver_state_write_to_writes_the_sidecar() {
    // The write seam persists the state to the given path. No env mutation:
    // the temp path is threaded in explicitly, so this test cannot race any
    // other test under the parallel runner.
    let dir = tempfile::tempdir().unwrap();
    let mut s = ReceiverState {
        listen_port: 5800,
        up: true,
        ..Default::default()
    };
    upsert_relays(&mut s, &["aa:bb:cc:dd:ee:ff".into()], 42);
    let path = dir.path().join("wfb-receiver.json");
    s.write_to(&path).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(v["up"], true);
    assert_eq!(v["relays"][0]["mac"], "aa:bb:cc:dd:ee:ff");
}

#[tokio::test]
async fn write_and_emit_enqueues_one_event_with_an_emitter_and_none_without() {
    // The emitting write ships exactly one gs.receiver_state event with an
    // emitter and nothing with None, regardless of the best-effort file
    // write result. The on-disk file path is covered by
    // `receiver_state_write_to_writes_the_sidecar`; this test threads the
    // temp path in explicitly (no `ADOS_RUN_DIR` mutation) so the best-effort
    // write stays inside its own temp tree and cannot race a sibling test.
    let dir = tempfile::tempdir().unwrap();
    let mut s = ReceiverState {
        listen_port: 5800,
        up: true,
        ..Default::default()
    };
    upsert_relays(&mut s, &["aa:bb:cc:dd:ee:ff".into()], 42);
    let path = dir.path().join("wfb-receiver.json");

    let emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
        "ados-groundlink",
        dir.path().join("ingest.sock"),
    );
    let stats = emitter.stats();
    let _ = s.write_and_emit_to(&path, Some(&emitter));
    assert_eq!(stats.enqueued(), 1);

    let none_emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
        "ados-groundlink",
        dir.path().join("ingest2.sock"),
    );
    let none_stats = none_emitter.stats();
    let _ = s.write_and_emit_to(&path, None);
    assert_eq!(none_stats.enqueued(), 0);
}
