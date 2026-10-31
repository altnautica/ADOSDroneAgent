use super::super::args::{STATE_ACTIVE, STATE_NO_INJECTION, STATE_REG_BLOCKED, STATE_SEARCHING};
use super::sidecars::unexamined_stats;
use super::*;

#[test]
fn gs_stats_carries_ground_station_profile_and_extras() {
    let cfg = WfbConfig::default();
    let snap = LinkStats::default();
    // A locked link on a live channel that drifted from the configured home
    // (the acquirer swept to 157), under a verified domain.
    let channels = GsChannelTruth {
        actual: 157,
        rendezvous: 149,
        operating: 149,
    };
    let reg = GsRegSnapshot {
        domain: Some("US".to_string()),
        verified: true,
        enabled_channels: vec![149, 153, 157, 161, 165],
        posture: Some(Posture {
            domain_to_set: "IN".to_string(),
            wanted_domain: "IN".to_string(),
            strict: true,
        }),
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
    let v = build_gs_stats(
        &snap,
        "wlan1",
        &adapter,
        channels,
        &reg,
        &cfg,
        STATE_ACTIVE,
        "locked",
        true,
        12.5,
        2,
        1,
        Some(0.3),
        508_000.0,
    );
    assert_eq!(v["profile"], "ground_station");
    assert_eq!(v["interface"], "wlan1");
    assert_eq!(v["adapter_chipset"], "rtl88x2eu");
    assert_eq!(v["adapter_injection_ok"], true);
    assert_eq!(v["actual_channel"], 157);
    assert_eq!(v["rendezvous_channel"], 149);
    assert_eq!(v["operating_channel"], 149);
    assert_eq!(v["reg_domain"], "US");
    assert_eq!(v["reg_verified"], true);
    assert_eq!(
        v["enabled_channels"],
        serde_json::json!([149, 153, 157, 161, 165])
    );
    assert_eq!(
        v["reg_posture"],
        serde_json::json!({"domain": "IN", "strict": true})
    );
    // The GS proves the link by valid decodes and never measures a transmit
    // path, so it reports NO transmit-proof verdict: the key is present for
    // schema symmetry but null, never a boolean.
    assert!(v.as_object().unwrap().contains_key("rf_unverified"));
    assert!(v["rf_unverified"].is_null());
    assert_eq!(v["link_state"], "active");
    assert_eq!(v["acquire_state"], "locked");
    assert_eq!(v["channel_locked"], true);
    assert_eq!(v["reacquire_kills"], 2);
    assert_eq!(v["rx_zombie_kills"], 1);
    assert_eq!(v["valid_rx_packets_per_s"], 12.5);
    assert_eq!(v["video_inbound_bytes_per_s"], 508000.0);
    assert_eq!(v["topology"], cfg.topology);
    // The MCS is MEASURED off-air, not mirrored from config. This snapshot is
    // `LinkStats::default()` (nothing decoded), so it must be null — reporting
    // `cfg.mcs_index` here would pin the GS's own configured uplink rung to a
    // block whose every other reading measures the drone's downlink, and it
    // would never move while the drone's adaptive ladder did.
    assert!(v["mcs_index"].is_null());
    assert!(v["bandwidth_mhz"].is_null());
}

/// With a real decode the measured PHY rate reaches the sidecar, and it is
/// the drone's rung — deliberately NOT the ground station's configured value.
#[test]
fn gs_stats_reports_the_measured_phy_rate_not_the_configured_one() {
    // Config says MCS 1; the drone's adaptive ladder has climbed to 3.
    let cfg = WfbConfig {
        mcs_index: 1,
        ..WfbConfig::default()
    };
    let snap = LinkStats {
        packets_received: 42,
        mcs_index: Some(3),
        bandwidth_mhz: Some(20),
        ..LinkStats::default()
    };
    let v = build_gs_stats(
        &snap,
        "wlan1",
        &GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480)),
        GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        },
        &GsRegSnapshot {
            domain: Some("US".to_string()),
            verified: true,
            enabled_channels: vec![149],
            posture: None,
        },
        &cfg,
        "active",
        "locked",
        true,
        0.0,  // valid_rx_packets_per_s
        0,    // reacquire_kills
        0,    // rx_zombie_kills
        None, // rx_silent_seconds
        0.0,  // video_inbound_bytes_per_s
    );
    assert_eq!(v["mcs_index"], 3, "must report the measured rung");
    assert_ne!(v["mcs_index"], cfg.mcs_index, "must not echo config");
    assert_eq!(v["bandwidth_mhz"], 20);
}

#[test]
fn gs_stats_never_claims_a_transmit_proof_verdict() {
    // A receive-side node has not measured a transmit path, in ANY state:
    // the `rf_unverified` key must read null (no verdict), never a boolean.
    // A boolean false here flows through every forwarder as the radio's own
    // measured proof that the transmit path was verified — the exact
    // false-healthy surface the verdict exists to expose — so this test
    // pins the producer to absence.
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
    for (state, locked, decoded) in [
        (STATE_ACTIVE, true, 25),
        (STATE_SEARCHING, false, 0),
        (STATE_REG_BLOCKED, false, 0),
    ] {
        let snap = LinkStats {
            packets_received: decoded,
            ..LinkStats::default()
        };
        let v = build_gs_stats(
            &snap,
            "wlan1",
            &adapter,
            channels,
            &GsRegSnapshot::default(),
            &WfbConfig::default(),
            state,
            "searching",
            locked,
            0.0,
            0,
            0,
            None,
            0.0,
        );
        assert!(
            v.as_object().unwrap().contains_key("rf_unverified"),
            "{state}: the key must ride for schema symmetry"
        );
        assert!(
            v["rf_unverified"].is_null(),
            "{state}: a receive-side node must report no transmit-proof \
             verdict, got {:?}",
            v["rf_unverified"]
        );
    }
}

#[test]
fn gs_stats_carries_top_level_state_noise_and_tx_power_ceiling() {
    // The drone-side sidecar writes a top-level `state`, a `noise_dbm`, and a
    // `tx_power_max_dbm`; the GS heartbeat reads the sidecar raw, so these
    // must be present on the ground sidecar too or the link block reports
    // null for them.
    let cfg = WfbConfig {
        tx_power_max_dbm: 30,
        ..WfbConfig::default()
    };
    // A measured snapshot (a packet decoded) so the signal-strength fields are
    // real readings, not the no-measurement sentinel.
    let snap = LinkStats {
        noise_dbm: -91.0,
        packets_received: 3,
        ..LinkStats::default()
    };
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
    let v = build_gs_stats(
        &snap,
        "wlan1",
        &adapter,
        channels,
        &GsRegSnapshot::default(),
        &cfg,
        STATE_ACTIVE,
        "searching",
        false,
        0.0,
        0,
        0,
        Some(7.5),
        0.0,
    );
    assert_eq!(v["state"], "active");
    assert_eq!(v["noise_dbm"], -91.0);
    assert_eq!(v["tx_power_max_dbm"], 30);
    assert_eq!(v["rx_silent_seconds"], 7.5);
    assert_eq!(v["backend"], "kernel");
    // The new keys must never be null on the ground sidecar.
    assert!(!v["state"].is_null());
    assert!(!v["noise_dbm"].is_null());
    assert!(!v["tx_power_max_dbm"].is_null());
    // An unknown regulatory snapshot reports a JSON null domain (not absent)
    // + an empty enabled set, never a fabricated value.
    assert!(v["reg_domain"].is_null());
    assert_eq!(v["reg_verified"], false);
    assert_eq!(v["enabled_channels"], serde_json::json!([]));
}

#[test]
fn gs_stats_nulls_signal_fields_and_surfaces_diag_when_deaf() {
    // A deaf ground station: wfb_rx alive and emitting a PKT line, but 0 decoded
    // and 0 captured off-air. The signal-strength fields must be null (not the
    // -100 sentinel dressed up as a reading), and the diagnostic counters +
    // link_diag must carry the real, legible cause.
    let snap = LinkStats {
        link_diag: ados_radio::link_quality::LinkDiag::Deaf,
        ..LinkStats::default() // packets_received == 0, packets_all == 0
    };
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
    let v = build_gs_stats(
        &snap,
        "wlan1",
        &adapter,
        channels,
        &GsRegSnapshot::default(),
        &WfbConfig::default(),
        STATE_SEARCHING,
        "searching",
        false,
        0.0,
        0,
        0,
        None,
        0.0,
    );
    // No fabricated weak-signal reading when nothing was decoded.
    assert!(v["rssi_dbm"].is_null());
    assert!(v["noise_dbm"].is_null());
    assert!(v["snr_db"].is_null());
    // The cause is legible: deaf + all counters honestly zero.
    assert_eq!(v["link_diag"], "deaf");
    assert_eq!(v["packets_all"], 0);
    assert_eq!(v["decrypt_errors"], 0);
    assert_eq!(v["state"], "searching");
}

#[test]
fn json_object_to_fields_round_trips_the_gs_body() {
    // The GS body shipped to the store must decode back to identical JSON, so
    // the durable read source matches the live sidecar fallback. Null domain,
    // the channel array, and integer-vs-float number kinds are the at-risk
    // legs.
    let cfg = WfbConfig::default();
    let snap = LinkStats::default();
    let channels = GsChannelTruth {
        actual: 157,
        rendezvous: 149,
        operating: 149,
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
    let body = build_gs_stats(
        &snap,
        "wlan1",
        &adapter,
        channels,
        &GsRegSnapshot::default(),
        &cfg,
        STATE_ACTIVE,
        "searching",
        false,
        0.0,
        0,
        0,
        None,
        0.0,
    );
    let fields = json_object_to_fields(&body);
    use ados_protocol::frame::{decode_len, HEADER_SIZE};
    use ados_protocol::logd::{EventFrame, IngestFrame, Level, LOGD_MAX_FRAME};
    let mut frame = EventFrame::new(0, "link.wfb_status", "ados-groundlink", Level::Info);
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
    // The unknown domain is a JSON null on the wire (not dropped), the empty
    // enabled set is an array, and the GS profile string survives.
    assert!(back["reg_domain"].is_null());
    assert_eq!(back["enabled_channels"], serde_json::json!([]));
    assert_eq!(back["profile"], "ground_station");
}

#[test]
fn reg_blocked_state_string_is_bland_and_stable() {
    // The sidecar surfaces this verbatim; keep it stable and tag-free.
    assert_eq!(STATE_REG_BLOCKED, "reg_blocked");
}

#[test]
fn reg_blocked_sidecar_carries_state_reason_and_no_injection() {
    // The reg-blocked sidecar is written from the run loop when the gate
    // fails; it must surface the bland reason + the blocked state and never
    // claim injection while no receive chain is running. Write to a tmp dir
    // to verify the JSON shape without touching /run.
    let cfg = WfbConfig::default();
    let snap = LinkStats::default();
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    // The forbidden domain the global set could not displace, surfaced.
    let reg = GsRegSnapshot {
        domain: Some("BO".to_string()),
        verified: false,
        enabled_channels: vec![],
        posture: None,
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), false, Some(480));
    let mut v = build_gs_stats(
        &snap,
        "wlan1",
        &adapter,
        channels,
        &reg,
        &cfg,
        STATE_REG_BLOCKED,
        crate::acquire::AcquireState::Searching.as_str(),
        false,
        0.0,
        0,
        0,
        None,
        0.0,
    );
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "reg_block_reason".to_string(),
            serde_json::Value::String("phy_override".to_string()),
        );
    }
    assert_eq!(v["state"], "reg_blocked");
    assert_eq!(v["reg_block_reason"], "phy_override");
    assert_eq!(v["adapter_injection_ok"], false);
    assert_eq!(v["channel_locked"], false);
    assert_eq!(v["valid_rx_packets_per_s"], 0.0);
    assert_eq!(v["profile"], "ground_station");
    // The live conflict is visible in the reg block sidecar.
    assert_eq!(v["reg_domain"], "BO");
    assert_eq!(v["reg_verified"], false);
}

#[test]
fn gs_stats_reports_the_adapter_usb_link_health() {
    // A healthy high-speed adapter: the real reading is surfaced and the
    // degraded flag stays clear, under the same keys the drone side uses.
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
    let v = build_gs_stats(
        &LinkStats::default(),
        "wlan1",
        &adapter,
        channels,
        &GsRegSnapshot::default(),
        &WfbConfig::default(),
        STATE_ACTIVE,
        "locked",
        true,
        1.0,
        0,
        0,
        None,
        0.0,
    );
    assert_eq!(v["adapter_usb_speed_mbps"], 480);
    assert_eq!(v["adapter_usb_degraded"], false);
}

#[test]
fn gs_stats_flags_a_slow_usb_adapter_so_a_deaf_receiver_has_a_cause() {
    // A full-speed (12 Mbps) enumeration cannot carry usable RF. The receive
    // plane must surface it, otherwise a deaf ground station reads as a bare
    // "0 received" with no legible cause.
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(12));
    let v = build_gs_stats(
        &LinkStats::default(),
        "wlan1",
        &adapter,
        channels,
        &GsRegSnapshot::default(),
        &WfbConfig::default(),
        STATE_SEARCHING,
        "searching",
        false,
        0.0,
        0,
        0,
        None,
        0.0,
    );
    assert_eq!(v["adapter_usb_speed_mbps"], 12);
    assert_eq!(v["adapter_usb_degraded"], true);
}

#[test]
fn gs_stats_reports_an_unknown_usb_speed_as_null_and_never_as_zero() {
    // No adapter resolved yet / a non-USB or unreadable adapter: the speed is
    // an explicit JSON null (no reading), never a confident 0 that would look
    // like a measured dead link — and with no speed reading there is no
    // degraded VERDICT either: the flag is null, never a false that a
    // downstream three-state consumer renders as a measured-healthy green.
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    let v = build_gs_stats(
        &LinkStats::default(),
        "wlan1",
        &GsAdapterInfo::default(),
        channels,
        &GsRegSnapshot::default(),
        &WfbConfig::default(),
        STATE_SEARCHING,
        "searching",
        false,
        0.0,
        0,
        0,
        None,
        0.0,
    );
    assert!(v["adapter_usb_speed_mbps"].is_null());
    assert_ne!(v["adapter_usb_speed_mbps"], 0);
    assert!(
        v["adapter_usb_degraded"].is_null(),
        "no enumeration ⇒ no degraded verdict, got {:?}",
        v["adapter_usb_degraded"]
    );
    // An unresolved adapter also makes no chipset or injection claim: both
    // read null, so the panel shows "not reported" instead of a fabricated
    // red no-injection (or green all-clear) about hardware never examined.
    assert!(v["adapter_chipset"].is_null());
    assert!(
        v["adapter_injection_ok"].is_null(),
        "no adapter resolved ⇒ no injection verdict, got {:?}",
        v["adapter_injection_ok"]
    );
}

#[test]
fn gs_stats_keeps_a_resolved_adapters_verdicts_as_real_booleans() {
    // The three-state contract must not swallow real readings: a resolved
    // adapter with an enumerated speed reports actual booleans, so the
    // loud degraded / no-injection signals still fire when measured.
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), false, Some(12));
    let v = build_gs_stats(
        &LinkStats::default(),
        "wlan1",
        &adapter,
        channels,
        &GsRegSnapshot::default(),
        &WfbConfig::default(),
        STATE_SEARCHING,
        "searching",
        false,
        0.0,
        0,
        0,
        None,
        0.0,
    );
    assert_eq!(v["adapter_injection_ok"], false);
    assert_eq!(v["adapter_usb_degraded"], true);
}

#[test]
fn gs_adapter_info_derives_degraded_from_the_shared_threshold() {
    // The ground plane must not restate the degraded threshold: every
    // constructed value agrees with the shared `usb_speed_degraded` helper,
    // so the two rigs can never disagree on what "degraded" means.
    for speed in [None, Some(12), Some(480), Some(5000)] {
        let info = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, speed);
        assert_eq!(
            info.usb_degraded,
            ados_radio::adapter::usb_speed_degraded(speed),
            "degraded flag disagreed with the shared threshold for {speed:?}"
        );
        assert_eq!(info.usb_speed_mbps, speed);
    }
}

#[test]
fn reg_blocked_sidecar_keeps_the_usb_facts_while_denying_injection() {
    // A regulatory-blocked gate runs no receive chain, so it claims no
    // injection — but the adapter's USB link health is a physical fact that
    // must survive, since a slow-USB adapter is exactly the kind of cause an
    // operator needs while the gate retries.
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(12));
    let blocked = GsAdapterInfo {
        injection_ok: false,
        ..adapter.clone()
    };
    let v = build_gs_stats(
        &LinkStats::default(),
        "wlan1",
        &blocked,
        channels,
        &GsRegSnapshot::default(),
        &WfbConfig::default(),
        STATE_REG_BLOCKED,
        "searching",
        false,
        0.0,
        0,
        0,
        None,
        0.0,
    );
    assert_eq!(v["adapter_injection_ok"], false);
    assert_eq!(v["adapter_usb_speed_mbps"], 12);
    assert_eq!(v["adapter_usb_degraded"], true);
}

#[test]
fn gs_adapter_info_carries_every_fact_off_the_selected_adapter() {
    // The resolved adapter record is the one source the sidecar reads, so no
    // fact may be dropped on the way through (the drop is what left the
    // ground sidecar reporting a default chipset and a fixed injection claim).
    let sel = ados_radio::adapter::DetectedAdapter {
        ifname: "wlan1".to_string(),
        mac: None,
        usb_path: None,
        driver: "rtl88x2eu".to_string(),
        chipset: "rtl88x2eu".to_string(),
        verified: true,
        usb_speed_mbps: Some(12),
        usb_degraded: true,
    };
    let info = GsAdapterInfo::from(&sel);
    assert_eq!(info.chipset.as_deref(), Some("rtl88x2eu"));
    assert!(info.injection_ok);
    assert_eq!(info.usb_speed_mbps, Some(12));
    assert!(info.usb_degraded);
}

#[test]
fn json_object_to_fields_round_trips_the_usb_link_health() {
    // The body shipped to the store must decode back identically, including a
    // null speed (absent reading) and the integer speed reading — the store is
    // the durable read source the panel falls back to.
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    for adapter in [
        GsAdapterInfo::default(),
        GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(12)),
    ] {
        let body = build_gs_stats(
            &LinkStats::default(),
            "wlan1",
            &adapter,
            channels,
            &GsRegSnapshot::default(),
            &WfbConfig::default(),
            STATE_SEARCHING,
            "searching",
            false,
            0.0,
            0,
            0,
            None,
            0.0,
        );
        let fields = json_object_to_fields(&body);
        let back = serde_json::to_value(fields).unwrap();
        assert_eq!(
            back["adapter_usb_speed_mbps"],
            body["adapter_usb_speed_mbps"]
        );
        assert_eq!(back["adapter_usb_degraded"], body["adapter_usb_degraded"]);
    }
}

#[test]
fn no_injection_state_string_is_bland_and_stable() {
    // The sidecar surfaces this verbatim; keep it stable and tag-free.
    assert_eq!(STATE_NO_INJECTION, "no_injection");
}

#[test]
fn blocked_unpaired_state_string_is_bland_and_stable() {
    // Consumed by name: the re-arm latch treats this state as "the receive
    // chain never ran, so there is no verdict on the key" and must not
    // confuse it with `searching`, which IS a verdict.
    assert_eq!(STATE_BLOCKED_UNPAIRED, "blocked_unpaired");
    assert_ne!(STATE_BLOCKED_UNPAIRED, STATE_SEARCHING);
    assert_ne!(STATE_BLOCKED_UNPAIRED, STATE_REG_BLOCKED);
    assert_ne!(STATE_BLOCKED_UNPAIRED, STATE_NO_INJECTION);
}

#[test]
fn blocked_unpaired_sidecar_names_the_cause_and_claims_nothing_it_did_not_examine() {
    // The gate sits ahead of adapter resolution, so nothing has been looked
    // at: every hardware verdict must be null, not a confident boolean. The
    // one thing the body asserts is WHY the plane is deaf.
    let v = build_gs_stats(
        &LinkStats::default(),
        "",
        &GsAdapterInfo::default(),
        GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        },
        &GsRegSnapshot::default(),
        &WfbConfig::default(),
        STATE_BLOCKED_UNPAIRED,
        crate::acquire::AcquireState::Searching.as_str(),
        false,
        0.0,
        0,
        0,
        None,
        0.0,
    );
    assert_eq!(v["state"], "blocked_unpaired");
    assert_eq!(v["link_state"], "blocked_unpaired");
    assert_eq!(v["profile"], "ground_station");
    // Nothing was examined, so nothing is claimed.
    assert!(v["adapter_chipset"].is_null());
    assert!(v["adapter_injection_ok"].is_null());
    assert!(v["adapter_usb_speed_mbps"].is_null());
    assert!(v["adapter_usb_degraded"].is_null());
    assert!(v["rssi_dbm"].is_null());
    // And the receive plane never measures a transmit path, on any state.
    assert!(v["rf_unverified"].is_null());
    assert_eq!(v["channel_locked"], false);
    assert_eq!(v["packets_received"], 0);
}

#[test]
fn a_missing_adapter_replaces_an_active_body_with_no_adapter() {
    // The arm used to write nothing, so a pulled adapter left the last
    // generation's `active` body in place.
    let v = unexamined_stats(
        "",
        149,
        &WfbConfig::default(),
        STATE_NO_ADAPTER,
        &GsRegSnapshot::default(),
    );
    assert_eq!(v["state"], "no_adapter");
    assert_eq!(v["link_state"], "no_adapter");
    assert!(v["adapter_chipset"].is_null());
    assert!(v["rssi_dbm"].is_null());
    assert_eq!(v["packets_received"], 0);
    // No posture resolved → null, not a fabricated one.
    assert!(v["reg_posture"].is_null());
    let unrestricted = unexamined_stats(
        "",
        149,
        &WfbConfig::default(),
        STATE_NO_ADAPTER,
        &GsRegSnapshot {
            posture: Some(Posture::resolve(
                &ados_radio::config::RegulatoryConfig::default(),
                None,
                true,
            )),
            ..GsRegSnapshot::default()
        },
    );
    assert_eq!(
        unrestricted["reg_posture"],
        serde_json::json!({"domain": null, "strict": false})
    );
}

#[tokio::test]
async fn blocked_unpaired_sidecar_emits_the_status_event_when_given_an_emitter() {
    // Same contract as the other two degraded-state writers: the body ships
    // to the store alongside the file, so a store-first read never lags.
    let dir = tempfile::tempdir().unwrap();
    let emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
        "ados-groundlink",
        dir.path().join("ingest.sock"),
    );
    let stats = emitter.stats();
    write_blocked_unpaired_sidecar("", 149, &WfbConfig::default(), Some(&emitter));
    assert_eq!(stats.enqueued(), 1);

    let none_emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
        "ados-groundlink",
        dir.path().join("ingest2.sock"),
    );
    let none_stats = none_emitter.stats();
    write_blocked_unpaired_sidecar("", 149, &WfbConfig::default(), None);
    assert_eq!(none_stats.enqueued(), 0);
}

#[test]
fn no_injection_sidecar_carries_the_state_and_denies_injection() {
    // The no-injection sidecar is written from the run loop when an adapter
    // resolved but injection setup did not establish. It must surface the
    // `no_injection` state and never claim injection while no receive chain
    // runs. Assert the JSON shape build_gs_stats produces for that state,
    // matching what the file writer serializes.
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), false, Some(480));
    let v = build_gs_stats(
        &LinkStats::default(),
        "wlan1",
        &adapter,
        channels,
        &GsRegSnapshot::default(),
        &WfbConfig::default(),
        STATE_NO_INJECTION,
        crate::acquire::AcquireState::Searching.as_str(),
        false,
        0.0,
        0,
        0,
        None,
        0.0,
    );
    assert_eq!(v["state"], "no_injection");
    assert_eq!(v["link_state"], "no_injection");
    assert_eq!(v["adapter_injection_ok"], false);
    assert_eq!(v["channel_locked"], false);
    assert_eq!(v["valid_rx_packets_per_s"], 0.0);
    assert_eq!(v["profile"], "ground_station");
}

#[test]
fn no_injection_sidecar_surfaces_the_slow_usb_cause() {
    // The whole point of the write: a slow-USB adapter — exactly what lands a
    // rig in the no-injection arm — must carry its USB speed + degraded flag
    // so the operator sees WHY the receive plane is stuck, instead of the arm
    // going silent.
    let channels = GsChannelTruth {
        actual: 149,
        rendezvous: 149,
        operating: 149,
    };
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), false, Some(12));
    let v = build_gs_stats(
        &LinkStats::default(),
        "wlan1",
        &adapter,
        channels,
        &GsRegSnapshot::default(),
        &WfbConfig::default(),
        STATE_NO_INJECTION,
        crate::acquire::AcquireState::Searching.as_str(),
        false,
        0.0,
        0,
        0,
        None,
        0.0,
    );
    assert_eq!(v["state"], "no_injection");
    assert_eq!(v["adapter_usb_speed_mbps"], 12);
    assert_eq!(v["adapter_usb_degraded"], true);
}

#[tokio::test]
async fn no_injection_sidecar_emits_the_status_event_when_given_an_emitter() {
    // The no-injection write is a GS degraded-state path: passing an emitter
    // must ship the same body to the store as a `link.wfb_status` event so a
    // store-first read never lags the on-disk sidecar. The emitter records
    // every enqueue regardless of a listening daemon; the unconditional emit
    // fires after the best-effort file write, so the assertion holds whether
    // or not the runtime sidecar path is writable in the test environment.
    let dir = tempfile::tempdir().unwrap();
    let emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
        "ados-groundlink",
        dir.path().join("ingest.sock"),
    );
    let stats = emitter.stats();
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), false, Some(12));
    write_no_injection_sidecar(
        "wlan1",
        &adapter,
        149,
        &WfbConfig::default(),
        Some(&emitter),
    );
    assert_eq!(stats.enqueued(), 1);

    // With no emitter the write enqueues nothing.
    let none_emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
        "ados-groundlink",
        dir.path().join("ingest2.sock"),
    );
    let none_stats = none_emitter.stats();
    write_no_injection_sidecar("wlan1", &adapter, 149, &WfbConfig::default(), None);
    assert_eq!(none_stats.enqueued(), 0);
}

#[tokio::test]
async fn reg_blocked_sidecar_emits_the_status_event_when_given_an_emitter() {
    // The reg-blocked write is the GS degraded-state path: passing an emitter
    // must ship the same body to the store as a `link.wfb_status` event so a
    // store-first read never lags the on-disk reg-blocked sidecar. The emitter
    // records every enqueue regardless of whether a daemon is listening, so one
    // write enqueues exactly one event. The unconditional emit fires after the
    // best-effort file write, so the assertion holds whether or not the runtime
    // sidecar path is writable in the test environment.
    let dir = tempfile::tempdir().unwrap();
    let emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
        "ados-groundlink",
        dir.path().join("ingest.sock"),
    );
    let stats = emitter.stats();
    let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
    write_reg_blocked_sidecar(
        "wlan1",
        &adapter,
        149,
        &WfbConfig::default(),
        &GsRegSnapshot::default(),
        "phy_override",
        Some(&emitter),
    );
    assert_eq!(stats.enqueued(), 1);

    // With no emitter the write enqueues nothing.
    let none_emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
        "ados-groundlink",
        dir.path().join("ingest2.sock"),
    );
    let none_stats = none_emitter.stats();
    write_reg_blocked_sidecar(
        "wlan1",
        &adapter,
        149,
        &WfbConfig::default(),
        &GsRegSnapshot::default(),
        "phy_override",
        None,
    );
    assert_eq!(none_stats.enqueued(), 0);
}
