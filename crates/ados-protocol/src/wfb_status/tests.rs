use super::*;
use std::time::{Duration, Instant};

#[test]
fn the_channel_table_covers_both_sub_bands_including_forty_and_forty_four() {
    // 40 and 44 are the regression: the radio block carried a second table
    // that omitted them, so a link on either rendered a null `freqMhz` in the
    // GCS beside a status body reporting the right frequency.
    assert_eq!(get_channel(36).unwrap().frequency_mhz, 5180);
    assert_eq!(get_channel(40).unwrap().frequency_mhz, 5200);
    assert_eq!(get_channel(44).unwrap().frequency_mhz, 5220);
    assert_eq!(get_channel(48).unwrap().frequency_mhz, 5240);
    assert_eq!(get_channel(165).unwrap().frequency_mhz, 5825);
    assert!(get_channel(1).is_none());
    for ch in STANDARD_CHANNELS {
        assert_eq!(ch.bandwidth_mhz, 20);
    }
}

#[test]
fn the_radio_block_resolves_a_channel_40_frequency() {
    let mut status = Map::new();
    status.insert("actual_channel".to_string(), json!(40));
    let block = build_radio_block(Some(&status));
    assert_eq!(block["freq_mhz"], json!(5200));
    let mut status44 = Map::new();
    status44.insert("actual_channel".to_string(), json!(44));
    assert_eq!(build_radio_block(Some(&status44))["freq_mhz"], json!(5220));
}

#[test]
fn the_absent_block_omits_exactly_the_three_skip_serializing_keys() {
    let absent = radio_absent_block();
    let populated = build_radio_block(Some(&Map::new()));
    let a = absent.as_object().unwrap();
    let p = populated.as_object().unwrap();
    assert_eq!(p.len(), 40, "the populated block is the 40-field set");
    assert_eq!(a.len(), 37, "the absent skeleton is the 37-field set");
    // `serde_json::Map` is a BTreeMap here, so compare the delta as a sorted
    // set rather than in declaration order.
    let mut only_populated: Vec<&str> = p
        .keys()
        .filter(|k| !a.contains_key(*k))
        .map(|k| k.as_str())
        .collect();
    only_populated.sort_unstable();
    assert_eq!(
        only_populated,
        vec!["restart_count", "tx_bytes_per_s", "tx_zombie_kills"]
    );
    assert!(
        a.keys().all(|k| p.contains_key(k)),
        "the absent skeleton must not carry a key the populated block lacks"
    );
}

#[test]
fn the_verdict_fields_are_null_rather_than_a_fabricated_false() {
    let block = build_radio_block(Some(&Map::new()));
    for key in [
        "adapter_injection_ok",
        "adapter_usb_degraded",
        "phy_muted",
        "rf_unverified",
    ] {
        assert_eq!(block[key], Value::Null, "{key} must not fabricate a false");
    }
    // A real reading forwards verbatim, in both polarities.
    let mut status = Map::new();
    status.insert("adapter_injection_ok".to_string(), json!(false));
    status.insert("phy_muted".to_string(), json!(true));
    let block = build_radio_block(Some(&status));
    assert_eq!(block["adapter_injection_ok"], json!(false));
    assert_eq!(block["phy_muted"], json!(true));
}

#[test]
fn a_stale_view_reports_no_rf_verdict() {
    let mut status = Map::new();
    status.insert("state".to_string(), json!("stale"));
    status.insert("rf_unverified".to_string(), json!(false));
    // A stale `false` is exactly the healthy-looking dead link this field
    // exists to expose.
    assert_eq!(
        build_radio_block(Some(&status))["rf_unverified"],
        Value::Null
    );
}

#[test]
fn the_rssi_sentinel_reads_as_no_reading() {
    let mut status = Map::new();
    status.insert("rssi_dbm".to_string(), json!(RSSI_SENTINEL));
    assert_eq!(build_radio_block(Some(&status))["rssi_dbm"], Value::Null);
    status.insert("rssi_dbm".to_string(), json!(-61.5));
    assert_eq!(build_radio_block(Some(&status))["rssi_dbm"], json!(-61.5));
}

#[test]
fn an_absent_stats_file_degrades_to_the_bare_base() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = WfbStatusConfig {
        channel: 149,
        ..Default::default()
    };
    let body = build_status_from_stats_file_at(&cfg, &dir.path().join("wfb-stats.json"));
    let obj = body.as_object().unwrap();
    assert_eq!(obj["state"], json!("disabled"));
    assert_eq!(obj["channel"], json!(149));
    assert_eq!(
        obj["actual_channel"],
        Value::Null,
        "no producer, no live channel"
    );
    // The bare base carries no finalize legs.
    assert!(!obj.contains_key("bitrate_mbps"));
    assert_eq!(obj["frequency_mhz"], json!(0));
}

#[test]
fn a_fresh_stats_file_merges_and_finalizes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");
    std::fs::write(
        &path,
        r#"{"state":"active","interface":"wlan1","actual_channel":40,"bitrate_kbps":4057,"rssi_dbm":-40.0}"#,
    )
    .unwrap();
    let body = build_status_from_stats_file_at(&WfbStatusConfig::default(), &path);
    let obj = body.as_object().unwrap();
    assert_eq!(obj["state"], json!("active"));
    // Channel 40 resolves through the one table.
    assert_eq!(obj["frequency_mhz"], json!(5200));
    assert_eq!(obj["bandwidth_mhz"], json!(20));
    assert_eq!(obj["bitrate_mbps"], json!(4.057));
}

#[test]
fn a_stale_stats_file_flips_state_and_drops_the_rf_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");
    std::fs::write(
        &path,
        r#"{"state":"active","rf_unverified":false,"actual_channel":149}"#,
    )
    .unwrap();
    // Back-date the file past the ceiling. `File::set_modified` keeps this
    // dependency-free — the crate carries no filetime dev-dependency.
    let old =
        std::time::SystemTime::now() - std::time::Duration::from_secs(WFB_STALE_AGE_S as u64 + 5);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old)
        .unwrap();

    let body = build_status_from_stats_file_at(&WfbStatusConfig::default(), &path);
    let obj = body.as_object().unwrap().clone();
    assert_eq!(obj["state"], json!("stale"));
    assert_eq!(build_radio_block(Some(&obj))["rf_unverified"], Value::Null);
}

#[test]
fn a_stored_event_older_than_the_ceiling_reads_stale() {
    let mut detail = Map::new();
    detail.insert("state".to_string(), json!("active"));
    let fresh = derive_wfb_status(&detail, now_unix_micros(), &WfbStatusConfig::default());
    assert_eq!(fresh["state"], json!("active"));

    let old_ts = now_unix_micros() - WFB_STALE_AGE_US - 1;
    let stale = derive_wfb_status(&detail, old_ts, &WfbStatusConfig::default());
    assert_eq!(stale["state"], json!("stale"));

    // A zero timestamp means "no event age known" and must not flip.
    let unknown = derive_wfb_status(&detail, 0, &WfbStatusConfig::default());
    assert_eq!(unknown["state"], json!("active"));
}

#[test]
fn the_config_slice_defaults_when_the_file_or_section_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.yaml");
    assert_eq!(WfbStatusConfig::load(&missing).channel, 149);

    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, "agent:\n  name: x\n").unwrap();
    assert_eq!(WfbStatusConfig::load(&cfg_path).channel, 149);

    std::fs::write(
        &cfg_path,
        "video:\n  wfb:\n    channel: 157\n    tx_power_dbm: 10\n",
    )
    .unwrap();
    let cfg = WfbStatusConfig::load(&cfg_path);
    assert_eq!(cfg.channel, 157);
    assert_eq!(cfg.tx_power_dbm, json!(10));
    // An absent key is the schema default the radio runs at, not null.
    assert_eq!(cfg.topology, json!("host_vbus"));
    assert_eq!(cfg.tx_power_max_dbm, json!(15));
}

#[test]
fn a_regulatory_reading_is_served_inside_its_window_and_falls_due_after_it() {
    // The radio panel polls this route about once a second and each poll
    // used to fork `iw` twice, inline, on the reactor. One reading serves
    // the window; past it the reading falls due so a real domain change
    // reaches the body rather than being pinned to whatever was true at
    // boot.
    let cache = RegDomainCache::new();
    let t0 = Instant::now();

    // Nothing read yet: the honest unknown, and a refresh is due.
    assert_eq!(
        cache.peek_at(t0, REG_DOMAIN_TTL),
        (REG_DOMAIN_UNKNOWN.to_string(), true)
    );

    cache.store_at(t0, "US".to_string());
    let t1 = t0 + Duration::from_secs(1);
    assert_eq!(cache.peek_at(t1, REG_DOMAIN_TTL), ("US".to_string(), false));
    assert_eq!(cache.peek_at(t1, REG_DOMAIN_TTL), ("US".to_string(), false));

    // Past the window the held reading is still served — a stale country is
    // better than a blank one, and it is a value this process did read —
    // but it is marked due so the refresh is scheduled.
    let t2 = t0 + REG_DOMAIN_TTL;
    assert_eq!(cache.peek_at(t2, REG_DOMAIN_TTL), ("US".to_string(), true));

    // A changed domain is served as soon as it is read, and a failed read
    // stores as the honest unknown rather than holding the last good answer
    // forever.
    cache.store_at(t2, "IN".to_string());
    assert_eq!(cache.peek_at(t2, REG_DOMAIN_TTL), ("IN".to_string(), false));
    cache.store_at(t2, REG_DOMAIN_UNKNOWN.to_string());
    assert_eq!(
        cache.peek_at(t2, REG_DOMAIN_TTL),
        (REG_DOMAIN_UNKNOWN.to_string(), false)
    );
}

#[test]
fn only_one_refresh_is_in_flight_at_a_time() {
    // A burst of concurrent polls past the TTL must schedule one `iw`, not
    // one per request: the whole point of moving the read off the caller's
    // path is that a poll storm cannot multiply into a fork storm.
    let cache = RegDomainCache::new();
    assert!(cache.claim_refresh());
    assert!(!cache.claim_refresh());
    assert!(!cache.claim_refresh());
    cache.release_refresh();
    assert!(cache.claim_refresh());
}

#[test]
fn regulatory_domain_never_spawns_on_the_callers_thread() {
    // Off a tokio runtime there is nothing to spawn onto, so the answer is
    // the honest unknown. The property under test is that the fallback is
    // NOT an inline fork: this assertion holds on a host where `iw reg get`
    // would have answered "US".
    assert_eq!(regulatory_domain(), REG_DOMAIN_UNKNOWN);
}

#[test]
fn the_reg_get_parse_reads_a_country_a_global_domain_and_nothing_else() {
    assert_eq!(parse_reg_get("global\ncountry US: DFS-FCC\n"), "global");
    assert_eq!(parse_reg_get("country US: DFS-FCC\n"), "US");
    assert_eq!(parse_reg_get("  country IN: DFS-ETSI\n"), "IN");
    // A country line with no code is not a domain.
    assert_eq!(parse_reg_get("country : DFS-FCC\n"), REG_DOMAIN_UNKNOWN);
    assert_eq!(parse_reg_get(""), REG_DOMAIN_UNKNOWN);
    assert_eq!(parse_reg_get("phy#0\n\tband 1:\n"), REG_DOMAIN_UNKNOWN);
}

#[test]
fn a_node_with_no_radio_reports_no_signal_measurement_at_all() {
    // The base block is the body a node with no adapter, no producer and no
    // sample serves. It used to carry `rssi_dbm: -100.0`, `noise_dbm:
    // -95.0`, `snr_db: 0.0` — a complete, credible weak link that the GCS
    // signal meter, the durable link series and the operator all read as a
    // measurement. Only `rssi_dbm` was folded back to null downstream, so
    // the noise floor and SNR rode the heartbeat verbatim.
    let base = wfb_base_block(&WfbStatusConfig::default());
    for key in ["rssi_dbm", "noise_dbm", "snr_db"] {
        assert_eq!(
            base.get(key),
            Some(&Value::Null),
            "{key} must not fabricate a reading on a node with no radio"
        );
    }

    // And the same through the projection every transport ships.
    let block = build_radio_block(Some(&base));
    for key in ["rssi_dbm", "noise_dbm", "snr_db"] {
        assert_eq!(block[key], Value::Null, "{key} reached the heartbeat");
    }

    // A real producer still overwrites the nulls on merge, including a
    // genuine 0.0 dB SNR, which is a measurement and must survive.
    let mut merged = wfb_base_block(&WfbStatusConfig::default());
    merged.insert("rssi_dbm".to_string(), json!(-62.0));
    merged.insert("noise_dbm".to_string(), json!(-94.0));
    merged.insert("snr_db".to_string(), json!(0.0));
    let live = build_radio_block(Some(&merged));
    assert_eq!(live["rssi_dbm"], json!(-62.0));
    assert_eq!(live["noise_dbm"], json!(-94.0));
    assert_eq!(live["snr_db"], json!(0.0));
}
