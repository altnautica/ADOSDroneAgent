// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Altnautica — ADOS Drone Agent
//! The cloud heartbeat's radio block, derived from the WFB stats sidecar.
//!
//! The heartbeat hardcoded `RadioBlock::absent()`, so a cloud-relayed node
//! rendered an empty link card in the GCS forever while `/run/ados/wfb-stats.json`
//! held the data on disk and the LAN routes read it fine. The fix routes the cloud
//! transport through `ados_protocol::wfb_status` — the one derivation both LAN
//! routes already call — and deserializes its output into `RadioBlock`.
//!
//! That seam is the thing worth pinning. `build_radio_block` emits 40 keys and
//! `RadioBlock` declares 40 fields, but nothing forced them to agree: a field added
//! to the derivation without a matching struct field would simply vanish off the
//! cloud wire, and a type the struct declares more narrowly than the sidecar
//! carries would drop the WHOLE block. The round-trip identity test below closes
//! both: it asserts the derivation's output deserializes AND re-serializes to
//! exactly the same JSON.

use serde_json::{json, Value};

use ados_cloud::heartbeat::RadioBlock;
use ados_protocol::wfb_status::{
    build_radio_block, build_status_from_stats_file_at, WfbStatusConfig,
};

/// A realistic sidecar body: a live 5.8 GHz link with the verdict fields present.
///
/// A raw string rather than `json!`: the macro hits its recursion limit on a
/// 40-key literal, and raising the limit crate-wide for one fixture is the wrong
/// trade.
fn live_sidecar_body() -> &'static str {
    r#"{
        "version": 1,
        "state": "active",
        "interface": "wlan1",
        "channel": 149,
        "frequency_mhz": 5745,
        "bandwidth_mhz": 20,
        "adapter_chipset": "rtl8812eu",
        "adapter_injection_ok": true,
        "adapter_usb_speed_mbps": 480,
        "adapter_usb_degraded": false,
        "phy_muted": false,
        "rssi_dbm": -40.0,
        "noise_dbm": -95.0,
        "snr_db": 55.0,
        "packets_received": 120000,
        "packets_lost": 12,
        "loss_percent": 0.01,
        "fec_recovered": 7,
        "fec_failed": 0,
        "bitrate_kbps": 4057,
        "rx_silent_seconds": 0.0,
        "restart_count": 1,
        "samples": 900,
        "tx_power_dbm": 20,
        "tx_power_max_dbm": 30,
        "topology": "one-to-one",
        "mcs_index": 3,
        "paired": true,
        "paired_with_device_id": "drone-abc",
        "paired_at": "2026-08-01T10:00:00+00:00",
        "public_key_fingerprint": "ab12cd34",
        "auto_pair_enabled": true,
        "tx_video_stalled": false,
        "tx_video_stall_kills": 0,
        "tx_video_recvq_bytes": 0,
        "acquire_state": "locked",
        "channel_locked": true,
        "rf_unverified": false,
        "reacquire_kills": 0,
        "valid_rx_packets_per_s": 812.5,
        "tx_zombie_kills": 0,
        "tx_bytes_per_s": 507125.0
    }"#
}

fn write_sidecar(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("wfb-stats.json");
    std::fs::write(&path, body).unwrap();
    path
}

/// The derivation's output and the struct must be the same 40 fields, with types
/// the struct actually accepts.
#[test]
fn the_derived_block_round_trips_through_the_struct_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_sidecar(dir.path(), live_sidecar_body());

    let status = build_status_from_stats_file_at(&WfbStatusConfig::default(), &path);
    let derived = build_radio_block(Some(status.as_object().unwrap()));

    let block: RadioBlock = serde_json::from_value(derived.clone()).unwrap_or_else(|e| {
        panic!("the derivation's output does not fit RadioBlock: {e}\n{derived:#}")
    });
    let reserialized = serde_json::to_value(&block).unwrap();

    assert_eq!(
        reserialized, derived,
        "a field the derivation emits and the struct drops (or renames, or narrows) \
         vanishes off the cloud wire silently"
    );
    assert_eq!(
        derived.as_object().unwrap().len(),
        40,
        "the populated radio block is the 40-field set"
    );

    // Spot-check the legs an operator reads on the link card, so a round trip that
    // is merely self-consistent cannot pass while carrying nothing.
    assert_eq!(block.state.as_deref(), Some("active"));
    assert_eq!(block.iface.as_deref(), Some("wlan1"));
    assert_eq!(block.channel, Some(149));
    assert_eq!(block.freq_mhz, Some(5745));
    assert_eq!(block.rssi_dbm, Some(-40.0));
    assert_eq!(block.bitrate_kbps, Some(4057));
    assert!(block.paired);
    assert_eq!(block.adapter_injection_ok, Some(true));
    assert_eq!(block.rf_unverified, Some(false));
    assert_eq!(block.tx_bytes_per_s, Some(507_125.0));
}

/// The absent skeleton must deserialize too — it is what a node with no radio
/// serves, and the 37-key form is what the frozen heartbeat fixture pins.
#[test]
fn the_absent_skeleton_round_trips_and_matches_the_struct_constructor() {
    let derived = ados_protocol::wfb_status::radio_absent_block();
    assert_eq!(derived.as_object().unwrap().len(), 37);

    let block: RadioBlock = serde_json::from_value(derived.clone()).unwrap();
    assert_eq!(
        block,
        RadioBlock::absent(),
        "the shared derivation's absent skeleton and the struct's own constructor \
         must be the same block, or the two transports disagree about 'no radio'"
    );
    assert_eq!(serde_json::to_value(&block).unwrap(), derived);
}

/// A sidecar older than the ceiling must not reach the wire: a frozen snapshot
/// renders a dead link as if it were live.
#[test]
fn a_stale_sidecar_is_not_read_into_the_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_sidecar(dir.path(), live_sidecar_body());

    // Fresh: the derivation produces a live block.
    let fresh = build_status_from_stats_file_at(&WfbStatusConfig::default(), &path);
    assert_eq!(fresh["state"], json!("active"));

    // Back-dated past the ten-second ceiling the LAN route applies.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old)
        .unwrap();

    let stale = build_status_from_stats_file_at(&WfbStatusConfig::default(), &path);
    assert_eq!(
        stale["state"],
        json!("stale"),
        "a snapshot past the ceiling must be marked stale, not served as live"
    );
    // And the transmit-proof verdict goes to null rather than carrying a stale
    // `false` — the healthy-looking dead link that field exists to expose.
    let block = build_radio_block(Some(stale.as_object().unwrap()));
    assert_eq!(block["rf_unverified"], Value::Null);
}

/// An absent sidecar derives the disabled base, which is not a live reading.
#[test]
fn an_absent_sidecar_derives_no_live_link() {
    let dir = tempfile::tempdir().unwrap();
    let status =
        build_status_from_stats_file_at(&WfbStatusConfig::default(), &dir.path().join("nope.json"));
    assert_eq!(status["state"], json!("disabled"));
    let block: RadioBlock =
        serde_json::from_value(build_radio_block(Some(status.as_object().unwrap()))).unwrap();
    assert_eq!(block.state.as_deref(), Some("disabled"));
    assert!(!block.paired);
    // No scan happened, so no verdict is claimed.
    assert_eq!(block.adapter_injection_ok, None);
}

/// A sidecar value whose type the struct declares more narrowly must fail loudly
/// at the seam rather than ship a half-populated block.
#[test]
fn a_type_the_struct_narrows_fails_the_whole_block_rather_than_half_of_it() {
    let dir = tempfile::tempdir().unwrap();
    // `tx_power_dbm` is `Option<i64>` on the struct; a float here is the realistic
    // drift (a producer switching to a measured value).
    let body = json!({"state": "active", "tx_power_dbm": 20.5}).to_string();
    let path = write_sidecar(dir.path(), &body);

    let status = build_status_from_stats_file_at(&WfbStatusConfig::default(), &path);
    let derived = build_radio_block(Some(status.as_object().unwrap()));
    let parsed = serde_json::from_value::<RadioBlock>(derived);

    assert!(
        parsed.is_err(),
        "a narrowed type must be an error at the seam; the caller then serves the \
         honest absent skeleton instead of a block missing one field"
    );
}
