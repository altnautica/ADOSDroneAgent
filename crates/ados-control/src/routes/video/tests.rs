use super::*;

// ----- /api/video/latency -----

#[test]
fn latency_live_of_an_absent_file_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let out = project_latency_live(&dir.path().join("lcd-latency.json"));
    assert_eq!(out, json!({"latency_ms": null, "source": "unavailable"}));
}

#[test]
fn latency_live_projects_the_file_fields_with_ewma_preference() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lcd-latency.json");
    std::fs::write(
        &path,
        r#"{"latency_ms": 82.5, "latency_ewma_ms": 80.0, "ewma_ms": 999,
                "samples": 30}"#,
    )
    .unwrap();
    let out = project_latency_live(&path);
    // The golden latency body the GCS reads (live-file path). `latency_ewma_ms`
    // wins over `ewma_ms`; `source` defaults to "sei" when absent.
    let want = json!({
        "latency_ms": 82.5,
        "ewma_ms": 80.0,
        "samples": 30,
        "source": "sei",
    });
    assert_eq!(out, want);
}

#[test]
fn latency_live_of_a_non_object_is_read_failed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lcd-latency.json");
    std::fs::write(&path, "[1,2,3]").unwrap();
    let out = project_latency_live(&path);
    assert_eq!(out, json!({"latency_ms": null, "source": "read_failed"}));
}

#[test]
fn latency_live_with_a_future_mtime_reads_stale() {
    // A clock that stepped backwards (an RTC-less SBC correcting after boot)
    // leaves the file dated after "now": its age is unprovable, so it is
    // stale, never a fresh reading.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lcd-latency.json");
    std::fs::write(&path, r#"{"latency_ms": 82.5, "samples": 3}"#).unwrap();
    let ahead = std::time::SystemTime::now() + std::time::Duration::from_secs(600);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(ahead))
        .unwrap();
    let out = project_latency_live(&path);
    assert_eq!(out["source"], json!("stale"));
    assert_eq!(out["latency_ms"], Value::Null);
    assert_eq!(
        out["age_s"],
        Value::Null,
        "a future mtime has no provable age"
    );
}

#[test]
fn a_sidecar_past_the_freshness_window_reads_stale_not_as_a_measurement() {
    // The bug this closes: nothing deletes /run/ados/lcd-latency.json when the
    // SEI probe is turned off and ados-video restarts, so the ungated read
    // kept answering with the last number the probe ever wrote. A consumer
    // polling at 1 Hz saw a plausible live figure for a pipeline that had no
    // injector in its byte path at all, and the only way to disprove it was to
    // SSH to the box and inspect the `ados-video` configuration.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lcd-latency.json");
    let body = r#"{"latency_ms": 82.5, "latency_ewma_ms": 80.0, "samples": 30, "source": "sei"}"#;

    // Inside the window the same bytes are a reading.
    write_sidecar_aged(&path, body, 2);
    let fresh = project_latency_live(&path);
    assert_eq!(fresh["latency_ms"], json!(82.5));
    assert_eq!(fresh["ewma_ms"], json!(80.0));
    assert_eq!(fresh["source"], json!("sei"));

    // Past it, every value reads null and the body SAYS it is stale (not
    // "unavailable", which would claim the probe never ran) and how old it is.
    write_sidecar_aged(&path, body, (LATENCY_STALE_AFTER_S as u64) + 5);
    let stale = project_latency_live(&path);
    assert_eq!(stale["latency_ms"], Value::Null);
    assert_eq!(stale["ewma_ms"], Value::Null);
    assert_eq!(stale["samples"], Value::Null);
    assert_eq!(stale["source"], json!("stale"));
    assert!(
        stale["age_s"].as_f64().unwrap() >= LATENCY_STALE_AFTER_S,
        "the stale body must carry the age that disqualified it: {stale}"
    );
}

#[test]
fn the_latency_window_matches_the_other_two_gates_on_the_same_file() {
    // Three surfaces gate on this data: the store's sidecar tailer stops
    // sampling lcd-latency.json past its budget, this route's live read stops
    // believing the file, and the sibling link block stops believing
    // wfb-stats.json. Different windows would mean the store path and the file
    // path disagree about whether there is a reading, which is the same
    // healthy-looking-dead-surface failure with an extra step. All three read
    // the one registry window.
    assert_eq!(LATENCY_STALE_AFTER_S, LINK_STALE_AFTER_S);
    assert_eq!(
        Some(secs(LATENCY_STALE_AFTER_S)),
        paths::max_age("lcd-latency")
    );
    let tailer = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ados-logd/src/taps/sidecar.rs")
        .canonicalize()
        .expect("the sidecar tailer must exist beside this crate");
    let body = std::fs::read_to_string(&tailer).unwrap();
    assert!(
        body.contains("paths::LCD_LATENCY_MAX_AGE_MS"),
        "{} must take its lcd-latency budget from the registry",
        tailer.display()
    );
}

#[test]
fn a_store_sample_past_the_window_is_dropped_so_the_route_falls_to_the_file() {
    // The store query is a 200-row page, not a time window, so on a quiet box
    // the last sample the probe ever recorded stays inside it indefinitely.
    // Gating only the file read would have moved the stale number from one
    // path to the other rather than removing it.
    let now_us = 1_800_000_000_000_000_i64;
    let names = ["video.latency.glass_ms", "video.latency.samples"];
    let row = |metric: &str, value: f64, age_s: i64| json!({"metric": metric, "value": value, "ts_us": now_us - age_s * 1_000_000});

    let fresh = fresh_metric_values(
        &[
            row("video.latency.glass_ms", 82.5, 1),
            row("video.latency.samples", 30.0, 1),
        ],
        &names,
        now_us,
        LATENCY_STALE_AFTER_S,
    );
    assert_eq!(fresh.get("video.latency.glass_ms"), Some(&json!(82.5)));
    assert!(collapse_empty_metrics(fresh).is_some());

    let stale = fresh_metric_values(
        &[
            row("video.latency.glass_ms", 82.5, 120),
            row("video.latency.samples", 30.0, 120),
        ],
        &names,
        now_us,
        LATENCY_STALE_AFTER_S,
    );
    assert!(stale.is_empty(), "a two-minute-old sample is not a reading");
    assert_eq!(collapse_empty_metrics(stale), None);

    // Newest-wins still holds inside the window: the page is ordered
    // newest-first, so a fresh sample is not displaced by an older duplicate.
    let mixed = fresh_metric_values(
        &[
            row("video.latency.glass_ms", 82.5, 1),
            row("video.latency.glass_ms", 900.0, 5),
        ],
        &names,
        now_us,
        LATENCY_STALE_AFTER_S,
    );
    assert_eq!(mixed.get("video.latency.glass_ms"), Some(&json!(82.5)));

    // A row whose age cannot be established is dropped, never shown as
    // current.
    let undatable = fresh_metric_values(
        &[json!({"metric": "video.latency.glass_ms", "value": 82.5})],
        &names,
        now_us,
        LATENCY_STALE_AFTER_S,
    );
    assert!(undatable.is_empty());
}

#[test]
fn the_display_services_framebuffer_stats_have_their_own_file() {
    // The display service's framebuffer-writer counters used to share
    // lcd-latency.json with the SEI tap; they now live in lcd-fb-stats.json,
    // so the latency file only ever holds a latency snapshot.
    assert_ne!(paths::LCD_FB_STATS_JSON, paths::LCD_LATENCY_JSON);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lcd-latency.json");

    // A running probe that has not produced a sample yet writes the key with
    // a null value. That IS a latency snapshot and still reads as "sei" — the
    // discrimination is on the key, not on the value.
    std::fs::write(
        &path,
        r#"{"latency_ms": null, "latency_ewma_ms": null, "samples": 0, "source": "sei"}"#,
    )
    .unwrap();
    let out = project_latency_live(&path);
    assert_eq!(out["latency_ms"], Value::Null);
    assert_eq!(out["source"], json!("sei"));
    assert_eq!(out["samples"], json!(0));
}

// ----- /api/video/config -----

#[test]
fn adaptive_block_carries_the_live_controller_state_from_wfb_stats() {
    // The dashboard reads `adaptive.adaptive_bitrate_enabled`. That key is
    // published by the radio into wfb-stats.json on every tick -- the reader
    // used to look for it in bitrate-controller.json, which has no writer at
    // all, so the row was blank forever and the key looked stale when the
    // real fault was the file.
    //
    // Scope, stated because it is not obvious: this covers the PROJECTION,
    // not which file the caller reads. Two other things cover that -- the
    // dead path's helper is deleted, so re-pointing at it takes a
    // hand-written join rather than an existing affordance, and the
    // api-conformance `video-config` case diffs this route against its Python
    // twin, so a one-sided repoint shows up there (on a live agent).
    let mut stats = Map::new();
    stats.insert("adaptive_bitrate_enabled".into(), json!(true));
    stats.insert("recommended_bitrate_kbps".into(), json!(3000));
    stats.insert("encoder_bitrate_kbps".into(), json!(2800));
    stats.insert("recommended_tier_idx".into(), json!(1));
    stats.insert("link_preset".into(), json!("conservative"));
    stats.insert("mcs_index".into(), json!(2));
    stats.insert("fec_k".into(), json!(8));
    stats.insert("fec_n".into(), json!(14));
    // A key the block does not carry must not leak through the filter.
    stats.insert("rssi".into(), json!(-48));

    let block = build_adaptive_block(true, Some(&stats));
    assert_eq!(block.get("available"), Some(&json!(true)));
    assert_eq!(block.get("adaptive_bitrate_enabled"), Some(&json!(true)));
    assert_eq!(block.get("recommended_bitrate_kbps"), Some(&json!(3000)));
    assert_eq!(block.get("encoder_bitrate_kbps"), Some(&json!(2800)));
    assert_eq!(block.get("recommended_tier_idx"), Some(&json!(1)));
    assert_eq!(block.get("link_preset"), Some(&json!("conservative")));
    assert_eq!(block.get("fec_k"), Some(&json!(8)));
    assert!(
        block.get("rssi").is_none(),
        "the block must carry only its own keys, not the whole sidecar"
    );
}

#[test]
fn a_stale_stats_sidecar_is_not_read_into_the_adaptive_block() {
    // The `link` block twelve lines below reads this same file behind a
    // freshness ceiling, for the reason its own docs give: a stale snapshot
    // describes a radio that may be dead now. The adaptive half shipped
    // without that gate, so a dead radio left a frozen
    // `adaptive_bitrate_enabled: true` next to a frozen recommended rung --
    // a live-looking adapting link with nothing behind it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");
    std::fs::write(
        &path,
        r#"{"adaptive_bitrate_enabled": true, "recommended_bitrate_kbps": 3000}"#,
    )
    .unwrap();

    // Fresh: the state is read.
    let fresh = read_stats_if_fresh(&path).expect("a just-written file is fresh");
    assert_eq!(fresh.get("adaptive_bitrate_enabled"), Some(&json!(true)));

    // Older than the ceiling: nothing is read, so the block falls to the stub.
    let stale =
        std::time::SystemTime::now() - std::time::Duration::from_secs_f64(LINK_STALE_AFTER_S + 5.0);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(stale)
        .unwrap();
    assert!(
        read_stats_if_fresh(&path).is_none(),
        "a snapshot past the staleness ceiling must not be merged"
    );
    let block = build_adaptive_block(true, read_stats_if_fresh(&path).as_ref());
    assert_eq!(block.len(), 1, "expected only `available`, got {block:?}");

    // An absent file is the same case, not an error.
    assert!(read_stats_if_fresh(&dir.path().join("nope.json")).is_none());
}

#[test]
fn adaptive_block_without_a_sidecar_is_the_config_stub() {
    // An absent or unparseable sidecar degrades to `available` alone rather
    // than erroring or fabricating a controller state.
    let block = build_adaptive_block(false, None);
    assert_eq!(block.get("available"), Some(&json!(false)));
    assert_eq!(block.len(), 1, "expected only `available`, got {block:?}");

    // A present sidecar carrying none of the adaptive keys is the same case.
    let mut unrelated = Map::new();
    unrelated.insert("rssi".into(), json!(-48));
    let block = build_adaptive_block(true, Some(&unrelated));
    assert_eq!(block.len(), 1, "expected only `available`, got {block:?}");
}

#[test]
fn config_of_an_empty_yaml_is_the_pydantic_default_shape() {
    // An empty config maps to the loaded Python config object's field defaults
    // for radio + encoder; the absent sidecars give the config-seeded stubs.
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, "agent:\n  profile: drone\n").unwrap();

    let cfg = VideoConfig::load_from(&cfg_path);
    let wfb = &cfg.video.wfb;
    let camera = &cfg.video.camera;
    assert_eq!(wfb.channel, 149);
    assert_eq!(wfb.band, "u-nii-3");
    assert_eq!(wfb.mcs_index, 1);
    assert_eq!(wfb.fec_k, 8);
    assert_eq!(wfb.fec_n, 12);
    assert_eq!(wfb.tx_power_dbm, 5);
    assert_eq!(wfb.wfb_link_preset, "conservative");
    assert!(wfb.adaptive_bitrate_enabled);
    assert!(wfb.auto_hop_enabled);
    assert_eq!(wfb.hop_period_seconds, 60);
    assert_eq!(camera.bitrate_kbps, 4000);
    assert_eq!(camera.width, 1280);
    assert_eq!(camera.height, 720);
    assert_eq!(camera.fps, 30);
    assert_eq!(camera.codec, "h264");

    // The hopping/link blocks are the config-seeded stubs with no sidecars
    // (point the sidecar paths at absent files in the tempdir).
    let hopping = match read_state_file(&dir.path().join("hop-supervisor.json")) {
        Some(snap) => Value::Object(snap),
        None => json!({
            "enabled": wfb.auto_hop_enabled,
            "band": wfb.band,
            "hop_period_seconds": wfb.hop_period_seconds,
            "history": [],
            "last_hop_at": 0.0,
        }),
    };
    assert_eq!(
        hopping,
        json!({
            "enabled": true,
            "band": "u-nii-3",
            "hop_period_seconds": 60,
            "history": [],
            "last_hop_at": 0.0,
        })
    );
    let link = link_snapshot(wfb.channel, &dir.path().join("wfb-stats.json"));
    // Every field present; channel falls back to the configured value.
    assert_eq!(link["channel"], json!(149));
    assert_eq!(link["acquire_state"], Value::Null);
    assert_eq!(link["tx_bytes_per_s"], Value::Null);
    // With no snapshot at all there is no verdict to report, and a false
    // here would claim a transmit path had been proven.
    assert_eq!(link["rf_unverified"], Value::Null);
}

#[test]
fn config_reads_explicit_radio_and_encoder_fields() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(
        &cfg_path,
        "video:\n  wfb:\n    channel: 165\n    mcs_index: 3\n    fec_k: 4\n    fec_n: 8\n    tx_power_dbm: 12\n    wfb_link_preset: aggressive\n  camera:\n    bitrate_kbps: 6000\n    width: 1920\n    height: 1080\n    fps: 60\n    codec: h265\n",
    )
    .unwrap();
    let cfg = VideoConfig::load_from(&cfg_path);
    assert_eq!(cfg.video.wfb.channel, 165);
    assert_eq!(cfg.video.wfb.mcs_index, 3);
    assert_eq!(cfg.video.wfb.fec_k, 4);
    assert_eq!(cfg.video.wfb.fec_n, 8);
    assert_eq!(cfg.video.wfb.tx_power_dbm, 12);
    assert_eq!(cfg.video.wfb.wfb_link_preset, "aggressive");
    assert_eq!(cfg.video.camera.bitrate_kbps, 6000);
    assert_eq!(cfg.video.camera.codec, "h265");
    // Absent fields still take their Python defaults.
    assert_eq!(cfg.video.wfb.band, "u-nii-3");
    assert_eq!(cfg.video.camera.fps, 60);
}

#[test]
fn link_snapshot_merges_present_stats_over_null_placeholders() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");
    std::fs::write(
        &path,
        r#"{"tx_bytes_per_s": 12345, "acquire_state": "locked", "actual_channel": 149,
                "channel_locked": true, "extra": "ignored"}"#,
    )
    .unwrap();
    let link = link_snapshot(36, &path);
    assert_eq!(link["tx_bytes_per_s"], json!(12345));
    assert_eq!(link["acquire_state"], json!("locked"));
    assert_eq!(link["channel_locked"], json!(true));
    // The sidecar's channel wins over the config fallback.
    assert_eq!(link["channel"], json!(149));
    // A field not in the stats file stays null.
    assert_eq!(link["video_inbound_bytes_per_s"], Value::Null);
    // Only the contract fields are present (the extra is dropped): the 16
    // merged fields, the live channel and the rf_unverified verdict.
    assert_eq!(link.as_object().unwrap().len(), 18);
}

#[test]
fn link_snapshot_folds_the_false_healthy_explainer_fields() {
    // The signals that explain why a carried counter should not be believed
    // ride the link block too, gated on the SAME freshness window as the
    // counters: a fresh snapshot surfaces the derived state, the muted-PHY +
    // USB-health flags, and the link_diag verdict with its backing RX counters;
    // a stale one reads them all null (unknown), never a frozen last-known.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");
    let body = r#"{"state": "connected", "link_state": "connected",
            "phy_muted": false, "adapter_usb_speed_mbps": 480,
            "adapter_usb_degraded": false, "link_diag": "healthy",
            "packets_all": 6400, "decrypt_errors": 0, "packets_bad": 2,
            "session_packets": 6398, "tx_bytes_per_s": 750000}"#;
    const EXPLAINERS: [&str; 10] = [
        "state",
        "link_state",
        "phy_muted",
        "adapter_usb_speed_mbps",
        "adapter_usb_degraded",
        "link_diag",
        "packets_all",
        "decrypt_errors",
        "packets_bad",
        "session_packets",
    ];

    write_sidecar_aged(&path, body, 2);
    let fresh = link_snapshot(149, &path);
    assert_eq!(fresh["state"], json!("connected"));
    assert_eq!(fresh["link_state"], json!("connected"));
    assert_eq!(fresh["phy_muted"], json!(false));
    assert_eq!(fresh["adapter_usb_speed_mbps"], json!(480));
    assert_eq!(fresh["adapter_usb_degraded"], json!(false));
    assert_eq!(fresh["link_diag"], json!("healthy"));
    assert_eq!(fresh["packets_all"], json!(6400));
    assert_eq!(fresh["decrypt_errors"], json!(0));
    assert_eq!(fresh["packets_bad"], json!(2));
    assert_eq!(fresh["session_packets"], json!(6398));

    write_sidecar_aged(&path, body, 30);
    let stale = link_snapshot(149, &path);
    for key in EXPLAINERS {
        assert_eq!(
            stale[key],
            Value::Null,
            "stale explainer {key} must read null"
        );
    }
}

/// Write a runtime sidecar and back-date its mtime by `age_s` seconds, so any
/// of this module's staleness gates can be driven without sleeping. Shared by
/// the `wfb-stats.json` gates and the `lcd-latency.json` one.
fn write_sidecar_aged(path: &Path, body: &str, age_s: u64) {
    std::fs::write(path, body).unwrap();
    if age_s > 0 {
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(age_s);
        let times = std::fs::FileTimes::new()
            .set_accessed(when)
            .set_modified(when);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(times)
            .unwrap();
    }
}

#[test]
fn link_snapshot_forwards_the_rf_unverified_verdict() {
    // The radio's authoritative verdict reaches the panel's link block
    // verbatim in both directions, so the panel never re-derives it from the
    // liveness counters beside it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");

    write_sidecar_aged(
        &path,
        r#"{"rf_unverified": true, "channel_locked": false, "tx_bytes_per_s": 750000}"#,
        0,
    );
    let link = link_snapshot(149, &path);
    assert_eq!(link["rf_unverified"], json!(true));
    // Its other half rides the same block: injecting blind is not locked.
    assert_eq!(link["channel_locked"], json!(false));

    write_sidecar_aged(
        &path,
        r#"{"rf_unverified": false, "channel_locked": true}"#,
        0,
    );
    assert_eq!(link_snapshot(149, &path)["rf_unverified"], json!(false));
}

#[test]
fn link_snapshot_rf_unverified_is_null_when_it_cannot_be_sourced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");

    // Absent from the snapshot (a sidecar written before the field existed):
    // unknown, never a confident false claiming the path was proven.
    write_sidecar_aged(&path, r#"{"channel_locked": true}"#, 0);
    assert_eq!(link_snapshot(149, &path)["rf_unverified"], Value::Null);

    // Present but not a boolean: a garbled body is no reading either.
    write_sidecar_aged(&path, r#"{"rf_unverified": "yes"}"#, 0);
    assert_eq!(link_snapshot(149, &path)["rf_unverified"], Value::Null);

    // An explicit JSON null is already no reading.
    write_sidecar_aged(&path, r#"{"rf_unverified": null}"#, 0);
    assert_eq!(link_snapshot(149, &path)["rf_unverified"], Value::Null);
}

#[test]
fn link_snapshot_rf_unverified_is_null_on_a_stale_snapshot() {
    // A snapshot older than the staleness ceiling cannot say whether the
    // radio is unverified NOW. Holding its last `false` is exactly the
    // healthy-looking dead link this field exists to expose, so it reads
    // null instead.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");
    let body = r#"{"rf_unverified": false, "channel_locked": true, "tx_bytes_per_s": 750000}"#;

    write_sidecar_aged(&path, body, 30);
    let stale = link_snapshot(149, &path);
    assert_eq!(stale["rf_unverified"], Value::Null);
    // The sibling liveness counters are gated on the SAME freshness window: a
    // stale advancing tx_bytes is the frozen-last-known reading a dead link
    // would show, so it reads null (unknown), never the last value.
    assert_eq!(stale["tx_bytes_per_s"], Value::Null);
    assert_eq!(stale["channel_locked"], Value::Null);

    // The same body inside the ceiling reports the real verdict AND the live
    // counters.
    write_sidecar_aged(&path, body, 2);
    let fresh = link_snapshot(149, &path);
    assert_eq!(fresh["rf_unverified"], json!(false));
    assert_eq!(fresh["tx_bytes_per_s"], json!(750000));
    assert_eq!(fresh["channel_locked"], json!(true));
}

#[test]
fn link_snapshot_stale_counters_read_null_not_frozen() {
    // A dead radio leaves its last-written sidecar on disk. Every liveness
    // counter must read null on a stale snapshot — a frozen last-known value
    // read as live is the healthy-looking dead link an honest surface forbids.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");
    let body = r#"{"tx_bytes_per_s": 750000, "valid_rx_packets_per_s": 42.5,
            "channel_locked": true, "acquire_state": "locked", "actual_channel": 165,
            "video_inbound_bytes_per_s": 900000, "rx_silent_seconds": 0.0}"#;

    write_sidecar_aged(&path, body, 30);
    let stale = link_snapshot(149, &path);
    for key in [
        "tx_bytes_per_s",
        "valid_rx_packets_per_s",
        "channel_locked",
        "acquire_state",
        "video_inbound_bytes_per_s",
        "rx_silent_seconds",
    ] {
        assert_eq!(stale[key], Value::Null, "stale {key} must read null");
    }
    // Channel is the exception: it falls back to the configured value (always
    // truthful) so the panel keeps a channel number, never the stale sidecar's.
    assert_eq!(stale["channel"], json!(149));

    // Inside the ceiling the same body reports the live counters verbatim.
    write_sidecar_aged(&path, body, 2);
    let fresh = link_snapshot(149, &path);
    assert_eq!(fresh["tx_bytes_per_s"], json!(750000));
    assert_eq!(fresh["valid_rx_packets_per_s"], json!(42.5));
    assert_eq!(fresh["channel_locked"], json!(true));
    assert_eq!(fresh["acquire_state"], json!("locked"));
    assert_eq!(fresh["channel"], json!(165));
}

#[test]
fn empty_metrics_window_collapses_to_none() {
    // The Python `latest_metrics` ends with `return out or None`, so a reachable
    // store whose window held none of the named metrics reads as `None` (no
    // data), NOT an empty snapshot. The latency route relies on that `None`: it
    // falls through to the live file read, which reports `source: "unavailable"`
    // when the SEI probe has produced nothing. An empty map collapsing to
    // `Some({})` instead would let the route synthesize a snapshot of nulls and
    // report it as a real measurement.
    assert_eq!(collapse_empty_metrics(Map::new()), None);

    // A non-empty map passes through unchanged so the route still builds a
    // snapshot when the store does carry the metrics.
    let mut m = Map::new();
    m.insert("video.latency.glass_ms".to_string(), json!(30.0));
    let collapsed = collapse_empty_metrics(m.clone());
    assert_eq!(collapsed, Some(m));
}

// ----- shared helpers -----

#[test]
fn metric_value_excludes_bools_and_non_numbers() {
    let mut m = Map::new();
    m.insert("a".to_string(), json!(12.5));
    m.insert("b".to_string(), json!(true));
    m.insert("c".to_string(), json!("x"));
    assert_eq!(metric_value(Some(&m), "a"), Some(12.5));
    assert_eq!(metric_value(Some(&m), "b"), None); // bool excluded
    assert_eq!(metric_value(Some(&m), "c"), None); // string excluded
    assert_eq!(metric_value(Some(&m), "absent"), None);
    assert_eq!(metric_value(None, "a"), None);
}

#[test]
fn read_state_file_tolerates_absent_and_non_object() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_state_file(&dir.path().join("absent.json")).is_none());
    std::fs::write(dir.path().join("arr.json"), "[1,2]").unwrap();
    assert!(read_state_file(&dir.path().join("arr.json")).is_none());
    std::fs::write(dir.path().join("obj.json"), r#"{"k":1}"#).unwrap();
    assert_eq!(
        read_state_file(&dir.path().join("obj.json")).unwrap()["k"],
        json!(1)
    );
}
