use super::*;
use crate::reg_gate::STATE_REG_BLOCKED;

/// Build a stats body over an injected link reading + bitrate snapshot,
/// with everything else at its default. Only the loss provenance is under
/// test here.
fn stats_body(link: &LinkStats, bitrate: &BitrateSnapshot) -> serde_json::Value {
    build_stats_value(
        "connected",
        &ChannelTruth::default(),
        &RegSnapshot::default(),
        5,
        None,
        link,
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        bitrate,
    )
}

#[test]
fn a_transmit_only_node_publishes_the_loss_its_peer_measured() {
    // A drone injects its own downlink and a single radio in monitor mode
    // cannot capture what it injects, so its own counters are the permanent
    // no-measurement sentinel and `loss_percent` reads 0 forever. The
    // receiving station's measurement of that same downlink already arrives
    // and already drives the ladder — it just never left the process, so
    // every delivery surface on a drone had nothing to show.
    let body = stats_body(
        &LinkStats::default(),
        &BitrateSnapshot {
            sample_source: "peer",
            sample_loss_percent: Some(24.29),
            ..BitrateSnapshot::default()
        },
    );
    assert_eq!(body["sample_source"], "peer");
    assert!((body["measured_loss_percent"].as_f64().unwrap() - 24.29).abs() < 0.001);
    // The local counter is untouched and still honestly zero, so the two
    // never get confused for one another.
    assert_eq!(body["packets_received"], 0);
}

#[test]
fn no_usable_sample_publishes_null_not_a_zero() {
    // Nobody measured. A zero here would render an unobserved link as a
    // clean one, which is the reading that trains an operator to distrust
    // the surface.
    let body = stats_body(&LinkStats::default(), &BitrateSnapshot::default());
    assert!(
        body["measured_loss_percent"].is_null(),
        "an unmeasured link must publish null, not 0"
    );
}

#[test]
fn reg_posture_default_is_unrestricted() {
    let p = RegPosture::default();
    assert_eq!(p.mode, "unrestricted");
    assert!(p.pinned_region.is_none());
}

#[test]
fn reg_posture_new_reflects_mode() {
    let unrestricted = RegPosture::new(true, "US");
    assert_eq!(unrestricted.mode, "unrestricted");
    assert!(unrestricted.pinned_region.is_none());

    let region = RegPosture::new(false, "DE");
    assert_eq!(region.mode, "region");
    assert_eq!(region.pinned_region.as_deref(), Some("DE"));
}

#[test]
fn fingerprint_none_when_key_absent_or_wrong_size() {
    let dir = tempfile::tempdir().unwrap();
    // Missing file → None.
    assert!(read_public_fingerprint(&dir.path().join("nope.key")).is_none());
    // Wrong size (not 64 bytes) → None.
    let short = dir.path().join("short.key");
    std::fs::write(&short, vec![0u8; 32]).unwrap();
    assert!(read_public_fingerprint(&short).is_none());
}

#[test]
fn fingerprint_is_16_hex_of_blake2b_8_over_public_half() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("tx.key");
    // 64-byte key: first 32 are the secret half, second 32 the public half.
    let mut data = vec![0u8; 64];
    for (i, b) in data.iter_mut().enumerate() {
        *b = i as u8;
    }
    std::fs::write(&key, &data).unwrap();
    let got = read_public_fingerprint(&key).unwrap();
    // Python `hashlib.blake2b(bytes(range(32, 64)), digest_size=8)`.
    assert_eq!(got, "e054fa6440b441f9");
    assert_eq!(got.len(), 16);
    assert!(got
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}

#[test]
fn channel_truth_configured_reports_rendezvous_for_all_channels() {
    // Before the interface exists, the truth reports the rendezvous home for
    // actual/rendezvous/operating, with the link not proven and no tx.
    let t = ChannelTruth::configured(149);
    assert_eq!(t.actual, 149);
    assert_eq!(t.rendezvous, 149);
    assert_eq!(t.operating, 149);
    assert!(!t.locked);
    assert!(!t.rf_unverified);
}

// Serialize tests that mutate the process-global `ADOS_RUN_DIR` env var; the
// lock is crate-wide because `paths` tests share this lib test binary.
use crate::paths::TEST_ENV_LOCK as SIDECAR_ENV_LOCK;

fn read_sidecar(dir: &std::path::Path) -> serde_json::Value {
    let body = std::fs::read(dir.join("wfb-stats.json")).unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn sidecar_carries_truthful_channel_and_reg_fields() {
    let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ADOS_RUN_DIR", dir.path());

    // A locked link on a live fallback channel under a verified domain: the
    // actual channel differs from the rendezvous home (the fallback-frequency
    // case), the link is proven, and rf_unverified is clear.
    let channels = ChannelTruth {
        actual: 161,
        rendezvous: 149,
        operating: 157,
        locked: true,
        rf_unverified: false,
    };
    let reg = RegSnapshot {
        domain: Some("US".to_string()),
        verified: true,
        enabled_channels: vec![149, 153, 157, 161, 165],
        ..RegSnapshot::default()
    };
    write_stats_sidecar(
        "connected",
        &channels,
        &reg,
        5,
        None,
        &LinkStats::default(),
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        &BitrateSnapshot::default(),
        None,
    );
    let v = read_sidecar(dir.path());

    assert_eq!(v["actual_channel"], 161);
    assert_eq!(v["rendezvous_channel"], 149);
    assert_eq!(v["operating_channel"], 157);
    assert_eq!(v["reg_domain"], "US");
    assert_eq!(v["reg_verified"], true);
    assert_eq!(
        v["enabled_channels"],
        serde_json::json!([149, 153, 157, 161, 165])
    );
    // channel_locked is the proof-derived value, not hardcoded true.
    assert_eq!(v["channel_locked"], true);
    assert_eq!(v["rf_unverified"], false);
    // The radio backend is the kernel monitor-mode path today (ADOS Direct
    // Link: the userspace USB backend is a later wave).
    assert_eq!(v["backend"], "kernel");
    // link_state mirrors the lifecycle state string. The default LinkStats
    // sentinel (0 decoded packets) is a transmit-dominant drone injecting
    // RF, not a degraded link, so it surfaces as connected.
    assert_eq!(v["link_state"], "connected");
    // No return signal decoded → the signal-strength fields are null, not
    // the -100 dBm sentinel, so a healthy injecting drone is never shown as
    // a weak-signal link. The counters stay 0 (honestly zero received).
    assert!(v["rssi_dbm"].is_null());
    assert!(v["snr_db"].is_null());
    assert!(v["noise_dbm"].is_null());
    assert_eq!(v["packets_received"], 0);
    // Default RegSnapshot posture: unrestricted with no pinned region. The
    // cross-surface contract keys carry the honest operating-region state.
    assert_eq!(v["regPosture"], "unrestricted");
    assert!(v["pinnedRegion"].is_null());
    assert_eq!(v["regVerified"], true);

    std::env::remove_var("ADOS_RUN_DIR");
}

#[test]
fn sidecar_surfaces_pinned_region_posture() {
    let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ADOS_RUN_DIR", dir.path());

    let reg = RegSnapshot {
        domain: Some("DE".to_string()),
        verified: true,
        enabled_channels: vec![149],
        posture: RegPosture::new(false, "DE"),
    };
    write_stats_sidecar(
        "connected",
        &ChannelTruth::configured(149),
        &reg,
        5,
        None,
        &LinkStats::default(),
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        &BitrateSnapshot::default(),
        None,
    );
    let v = read_sidecar(dir.path());
    assert_eq!(v["regPosture"], "region");
    assert_eq!(v["pinnedRegion"], "DE");
    assert_eq!(v["regVerified"], true);

    std::env::remove_var("ADOS_RUN_DIR");
}

#[test]
fn json_object_to_fields_round_trips_nulls_arrays_and_number_kinds() {
    // The store decodes the detail map back to JSON via `Fields` ->
    // serde_json, so a value built here must equal the original after the
    // round-trip: null stays null, the channel array stays an array, an int
    // stays an int (not a float), and a negative reading keeps its sign.
    let body = serde_json::json!({
        "pinnedRegion": serde_json::Value::Null,
        "enabled_channels": [149, 153, 157, 161, 165],
        "actual_channel": 161,
        "rssi_dbm": -53.0,
        "paired": true,
        "adapter_chipset": "RTL8812EU",
    });
    let fields = json_object_to_fields(&body);
    // Round-trip the detail map through the real wire path: frame it as an
    // event, encode it (the length-prefixed msgpack the store ingests), strip
    // the 4-byte length header the same way the ingest reader does, decode,
    // then map the decoded detail back to JSON the way the query layer does
    // (`serde_json::to_value` over the decoded `Fields`).
    use ados_protocol::frame::{decode_len, HEADER_SIZE};
    use ados_protocol::logd::{EventFrame, IngestFrame, Level, LOGD_MAX_FRAME};
    let mut frame = EventFrame::new(0, "link.wfb_status", "ados-radio", Level::Info);
    frame.detail = fields;
    let bytes = IngestFrame::Event(frame).encode().unwrap();
    let header: [u8; HEADER_SIZE] = bytes[..HEADER_SIZE].try_into().unwrap();
    let len = decode_len(header, LOGD_MAX_FRAME, true).unwrap();
    let decoded = match IngestFrame::decode(&bytes[HEADER_SIZE..HEADER_SIZE + len]).unwrap() {
        IngestFrame::Event(e) => e,
        other => panic!("expected an event frame, got {other:?}"),
    };
    let back = serde_json::to_value(decoded.detail).unwrap();
    assert_eq!(back, body);
    // The null channel must survive as a JSON null, not be dropped.
    assert!(back["pinnedRegion"].is_null());
    // The array round-trips as an array.
    assert_eq!(
        back["enabled_channels"],
        serde_json::json!([149, 153, 157, 161, 165])
    );
    // The integer channel stays an integer (a float would render 161.0).
    assert_eq!(back["actual_channel"], serde_json::json!(161));
}

#[test]
fn build_stats_value_matches_the_sidecar_file_body() {
    // The body shipped to the store must be the exact value written to the
    // sidecar file, so the durable read source is byte-identical to the live
    // fallback.
    let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ADOS_RUN_DIR", dir.path());
    let channels = ChannelTruth {
        actual: 161,
        rendezvous: 149,
        operating: 157,
        locked: true,
        rf_unverified: false,
    };
    let reg = RegSnapshot {
        domain: Some("US".to_string()),
        verified: true,
        enabled_channels: vec![149, 153, 157, 161, 165],
        ..RegSnapshot::default()
    };
    let built = build_stats_value(
        "connected",
        &channels,
        &reg,
        5,
        None,
        &LinkStats::default(),
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        &BitrateSnapshot::default(),
    );
    write_stats_sidecar(
        "connected",
        &channels,
        &reg,
        5,
        None,
        &LinkStats::default(),
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        &BitrateSnapshot::default(),
        None,
    );
    let on_disk = read_sidecar(dir.path());
    assert_eq!(built, on_disk);
    std::env::remove_var("ADOS_RUN_DIR");
}

#[tokio::test]
async fn write_stats_sidecar_emits_the_status_event_on_a_degraded_write() {
    // A degraded-state write (here `reg_blocked`, the bind/error class of
    // call site, not the heartbeat) must ship the same body to the store as a
    // `link.wfb_status` event so a store-first read never lags the file. The
    // emitter records every enqueue, so a single write with an emitter present
    // increments the enqueued counter by exactly one regardless of whether a
    // logging daemon is listening on the socket.
    let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ADOS_RUN_DIR", dir.path());

    let emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
        "ados-radio",
        dir.path().join("ingest.sock"),
    );
    let stats = emitter.stats();
    let before = stats.enqueued();
    write_stats_sidecar(
        STATE_REG_BLOCKED,
        &ChannelTruth::configured(149),
        &RegSnapshot::default(),
        5,
        None,
        &LinkStats::default(),
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        &BitrateSnapshot::default(),
        Some(&emitter),
    );
    // Exactly one event was enqueued by the degraded-state write.
    assert_eq!(stats.enqueued(), before + 1);
    // The file write is still byte-identical to the build (the live fallback).
    let on_disk = read_sidecar(dir.path());
    assert_eq!(on_disk["state"], "reg_blocked");

    // With no emitter the write still produces the file but enqueues nothing.
    let none_emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
        "ados-radio",
        dir.path().join("ingest2.sock"),
    );
    let none_stats = none_emitter.stats();
    write_stats_sidecar(
        STATE_REG_BLOCKED,
        &ChannelTruth::configured(149),
        &RegSnapshot::default(),
        5,
        None,
        &LinkStats::default(),
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        &BitrateSnapshot::default(),
        None,
    );
    assert_eq!(none_stats.enqueued(), 0);

    std::env::remove_var("ADOS_RUN_DIR");
}

#[test]
fn stats_report_no_adapter_verdicts_before_a_scan() {
    // No adapter resolved (the unpaired / pre-scan writes pass `None`):
    // the injection and USB-degraded verdicts must be JSON null — no
    // reading — never a confident false. A false here is exactly what an
    // unpaired rig used to write, and a downstream three-state consumer
    // rendered it as a measured-healthy green USB link for hardware
    // nothing ever enumerated.
    let body = build_stats_value(
        "unpaired",
        &ChannelTruth::configured(149),
        &RegSnapshot::default(),
        5,
        None,
        &LinkStats::default(),
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        &BitrateSnapshot::default(),
    );
    assert!(
        body["adapter_injection_ok"].is_null(),
        "no scan ⇒ no injection verdict, got {:?}",
        body["adapter_injection_ok"]
    );
    assert!(
        body["adapter_usb_degraded"].is_null(),
        "no enumeration ⇒ no USB-degraded verdict, got {:?}",
        body["adapter_usb_degraded"]
    );
    assert!(body["adapter_usb_speed_mbps"].is_null());
}

#[test]
fn stats_report_the_measured_tx_power_or_null() {
    let body = |counters: &WatchdogCounters| {
        build_stats_value(
            "connected",
            &ChannelTruth::configured(149),
            &RegSnapshot::default(),
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            counters,
            &TxRates::default(),
            &BitrateSnapshot::default(),
        )
    };
    assert!(body(&WatchdogCounters::default())["tx_power_readback_dbm"].is_null());
    let measured = WatchdogCounters {
        tx_power_readback_dbm: Some(9.0),
        ..WatchdogCounters::default()
    };
    let v = body(&measured);
    assert_eq!(v["tx_power_readback_dbm"], 9.0);
    assert_eq!(v["tx_power_dbm"], 5);
}

#[test]
fn stats_report_a_measured_no_adapter_verdict_after_a_scan() {
    // The scan ran and proved no injection-capable radio: the injection
    // verdict is a MEASURED false (the loud stranded-radio signal must
    // still fire), while the USB fields stay null — nothing enumerated.
    let body = build_stats_value(
        "no_adapter",
        &ChannelTruth::configured(149),
        &RegSnapshot::default(),
        5,
        Some(&AdapterInfo::none_found()),
        &LinkStats::default(),
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        &BitrateSnapshot::default(),
    );
    assert_eq!(body["adapter_injection_ok"], false);
    assert!(body["adapter_usb_degraded"].is_null());
    assert!(body["adapter_usb_speed_mbps"].is_null());
}

#[test]
fn stats_report_a_resolved_adapters_verdicts_as_real_booleans() {
    // The three-state contract must not swallow real readings: a selected
    // adapter with an enumerated speed reports actual booleans in both
    // directions, so the degraded warning and the healthy reading both
    // stay live.
    for (speed, degraded) in [(Some(12u32), true), (Some(480u32), false)] {
        let adapter = AdapterInfo {
            interface: "wlan1".to_string(),
            chipset: "RTL8812EU".to_string(),
            injection_ok: true,
            usb_speed_mbps: speed,
            usb_degraded: degraded,
        };
        let body = build_stats_value(
            "connected",
            &ChannelTruth::configured(149),
            &RegSnapshot::default(),
            5,
            Some(&adapter),
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
        );
        assert_eq!(body["adapter_injection_ok"], true);
        assert_eq!(body["adapter_usb_degraded"], degraded);
        assert_eq!(body["adapter_usb_speed_mbps"], speed.unwrap());
    }
    // An adapter whose USB speed could not be read has no degraded
    // verdict, even though it was resolved: `usb_degraded` derives from
    // the speed reading and must not outlive its absence.
    let unread = AdapterInfo {
        interface: "wlan1".to_string(),
        chipset: "RTL8812EU".to_string(),
        injection_ok: true,
        usb_speed_mbps: None,
        usb_degraded: false,
    };
    let body = build_stats_value(
        "connected",
        &ChannelTruth::configured(149),
        &RegSnapshot::default(),
        5,
        Some(&unread),
        &LinkStats::default(),
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        &BitrateSnapshot::default(),
    );
    assert_eq!(body["adapter_injection_ok"], true);
    assert!(body["adapter_usb_degraded"].is_null());
}

#[test]
fn sidecar_reports_rf_unverified_and_unlocked_when_transmitting_blind() {
    let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ADOS_RUN_DIR", dir.path());

    // Transmitting (tx advancing) but no confirmed reception: locked false,
    // rf_unverified true — the exact transmitting-zero-reception case.
    let channels = ChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
        locked: false,
        rf_unverified: true,
    };
    let reg = RegSnapshot {
        domain: Some("BO".to_string()),
        verified: false,
        enabled_channels: vec![],
        ..RegSnapshot::default()
    };
    write_stats_sidecar(
        STATE_REG_BLOCKED,
        &channels,
        &reg,
        5,
        None,
        &LinkStats::default(),
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        &BitrateSnapshot::default(),
        None,
    );
    let v = read_sidecar(dir.path());
    assert_eq!(v["channel_locked"], false);
    assert_eq!(v["rf_unverified"], true);
    // The forbidden domain the global set could not displace is visible.
    assert_eq!(v["reg_domain"], "BO");
    assert_eq!(v["reg_verified"], false);
    assert_eq!(v["enabled_channels"], serde_json::json!([]));
    assert_eq!(v["state"], "reg_blocked");

    std::env::remove_var("ADOS_RUN_DIR");
}

#[test]
fn sidecar_state_agrees_with_the_rf_unverified_boolean() {
    // The derived state string and the standalone boolean are two views of
    // one verdict; a body that says `rf_unverified: true` while the state
    // reads `connected` is the false-healthy surface this pairing exists to
    // prevent, so the two are asserted together on one body.
    let state = crate::link_state::derive_link_state(
        true,
        false,
        &LinkStats::default(),
        true,  // transmitting
        false, // no confirmed reception
    );
    assert_eq!(state, crate::link_state::LinkState::RfUnverified);

    let channels = ChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
        locked: false,
        rf_unverified: true,
    };
    let body = build_stats_value(
        state.as_str(),
        &channels,
        &RegSnapshot::default(),
        5,
        None,
        &LinkStats::default(),
        &WfbConfig::default(),
        0,
        &WatchdogCounters::default(),
        &TxRates::default(),
        &BitrateSnapshot::default(),
    );
    assert_eq!(body["state"], "rf_unverified");
    assert_eq!(body["link_state"], "rf_unverified");
    assert_eq!(body["rf_unverified"], true);
    assert_eq!(body["channel_locked"], false);
}
