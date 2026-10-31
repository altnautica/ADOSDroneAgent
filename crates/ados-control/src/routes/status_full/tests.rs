use super::*;
use ados_protocol::wfb_status::WfbStatusConfig;
// Test-only: the base block is the shared module's, exercised here because
// this is where the radio block's null-verdict contract is pinned.
use ados_protocol::wfb_status::wfb_base_block;

fn signals(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

// -------- resources --------

#[test]
fn resources_of_an_absent_store_is_an_empty_object() {
    assert_eq!(derive_resources_subset(None), json!({}));
}

#[test]
fn resources_missing_an_essential_field_is_an_empty_object() {
    // cpu present but the memory + disk spine missing → empty object (the most
    // degraded Python path, no psutil on this daemon).
    let s = signals(&[("cpu.util.all", json!(10.0))]);
    assert_eq!(derive_resources_subset(Some(&s)), json!({}));
}

#[test]
fn resources_derives_the_thirteen_field_subset() {
    let s = signals(&[
        ("cpu.util.all", json!(12.34)),
        ("mem.total_bytes", json!(4.0 * BYTES_PER_MB)),
        ("mem.avail_bytes", json!(1.0 * BYTES_PER_MB)),
        ("mem.cache_bytes", json!(0.5 * BYTES_PER_MB)),
        ("mem.swap_total_bytes", json!(2.0 * BYTES_PER_MB)),
        ("mem.swap_free_bytes", json!(0.5 * BYTES_PER_MB)),
        ("disk.fs_total_bytes", json!(100.0 * BYTES_PER_GB)),
        ("disk.fs_used_bytes", json!(25.0 * BYTES_PER_GB)),
        ("thermal.primary_c", json!(47.5)),
    ]);
    let r = derive_resources_subset(Some(&s));
    let obj = r.as_object().unwrap();

    // Exactly the 13 keys the consolidated route selects — NOT `temperatures`
    // or `load_avg`.
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "cpu_percent",
            "disk_percent",
            "disk_total_gb",
            "disk_used_gb",
            "memory_available_mb",
            "memory_cache_mb",
            "memory_percent",
            "memory_total_mb",
            "memory_used_mb",
            "swap_percent",
            "swap_total_mb",
            "swap_used_mb",
            "temperature",
        ]
    );
    assert_eq!(obj["cpu_percent"], json!(12.3));
    assert_eq!(obj["memory_total_mb"], json!(4));
    assert_eq!(obj["memory_used_mb"], json!(3)); // 4 - 1 avail
    assert_eq!(obj["memory_available_mb"], json!(1));
    // 0.5 MB cache: Python round(0.5) is round-half-to-even → 0.
    assert_eq!(obj["memory_cache_mb"], json!(0));
    assert_eq!(obj["memory_percent"], json!(75.0)); // (4-1)/4 * 100
    assert_eq!(obj["swap_total_mb"], json!(2));
    // (2 - 0.5) = 1.5 swap used: round(1.5) is round-half-to-even → 2.
    assert_eq!(obj["swap_used_mb"], json!(2));
    assert_eq!(obj["swap_percent"], json!(75.0)); // 1.5/2 * 100
    assert_eq!(obj["disk_total_gb"], json!(100.0));
    assert_eq!(obj["disk_used_gb"], json!(25.0));
    assert_eq!(obj["disk_percent"], json!(25.0));
    assert_eq!(obj["temperature"], json!(47.5));
}

#[test]
fn resources_missing_temperature_is_null() {
    let s = signals(&[
        ("cpu.util.all", json!(5.0)),
        ("mem.total_bytes", json!(BYTES_PER_MB)),
        ("mem.avail_bytes", json!(BYTES_PER_MB)),
        ("disk.fs_total_bytes", json!(BYTES_PER_GB)),
        ("disk.fs_used_bytes", json!(0.0)),
    ]);
    let r = derive_resources_subset(Some(&s));
    assert_eq!(r["temperature"], Value::Null);
    // Zero-total guards never divide-by-zero: a 1 MB total avail=total → 0%.
    assert_eq!(r["memory_percent"], json!(0.0));
    assert_eq!(r["swap_percent"], json!(0.0)); // no swap signals → 0
}

#[test]
fn resources_populate_from_a_representative_merged_signal_map() {
    // The merged hardware-signal map a live board produces carries the canonical
    // resource keys (`cpu.util.all`, `mem.*_bytes`, `disk.fs_*_bytes`,
    // `thermal.primary_c`) alongside a tail of per-core / per-sensor / scheduler /
    // pressure signals the consolidated route does not consume. The block must
    // read the canonical keys out of that noisy map and come back fully
    // populated, NOT the most-degraded empty object — the regression guard for a
    // read keyed on the wrong signal names finding nothing.
    let s = signals(&[
        // Canonical keys the subset reads.
        ("cpu.util.all", json!(23.7)),
        ("mem.total_bytes", json!(8.0 * BYTES_PER_GB)),
        ("mem.avail_bytes", json!(6.0 * BYTES_PER_GB)),
        ("mem.cache_bytes", json!(2.0 * BYTES_PER_GB)),
        ("mem.swap_total_bytes", json!(4.0 * BYTES_PER_GB)),
        ("mem.swap_free_bytes", json!(3.0 * BYTES_PER_GB)),
        ("disk.fs_total_bytes", json!(64.0 * BYTES_PER_GB)),
        ("disk.fs_used_bytes", json!(16.0 * BYTES_PER_GB)),
        ("thermal.primary_c", json!(52.4)),
        // The non-consumed tail a real merged map also carries: per-core CPU, an
        // alternate thermal-zone name, load averages, and a pressure-stall line.
        ("cpu.util.0", json!(20.1)),
        ("cpu.util.1", json!(25.0)),
        ("cpu.util.2", json!(22.6)),
        ("cpu.util.3", json!(27.0)),
        ("thermal.cpu_thermal_c", json!(52.4)),
        ("sched.loadavg_1", json!(0.42)),
        ("sched.loadavg_5", json!(0.31)),
        ("sched.loadavg_15", json!(0.27)),
        ("mem.psi.cpu.some.avg10", json!(0.0)),
    ]);
    let r = derive_resources_subset(Some(&s));
    let obj = r.as_object().unwrap();

    // Populated, not the degraded empty object.
    assert!(
        !obj.is_empty(),
        "resources must populate from a live signal map"
    );

    // Exactly the 13 consolidated keys — the per-core / load / pressure tail and
    // the `temperatures` / `load_avg` Python-only fields are not carried.
    assert_eq!(obj.len(), 13);

    // Each canonical key reads its real value from the merged map (the read is
    // keyed on the correct signal names), not a default / null.
    assert_eq!(obj["cpu_percent"], json!(23.7));
    assert_eq!(obj["memory_total_mb"], json!(8192)); // 8 GiB in MiB
    assert_eq!(obj["memory_used_mb"], json!(2048)); // (8 - 6) GiB avail
    assert_eq!(obj["memory_available_mb"], json!(6144));
    assert_eq!(obj["memory_cache_mb"], json!(2048));
    assert_eq!(obj["memory_percent"], json!(25.0)); // (8-6)/8 * 100
    assert_eq!(obj["swap_total_mb"], json!(4096));
    assert_eq!(obj["swap_used_mb"], json!(1024)); // (4 - 3) GiB used
    assert_eq!(obj["swap_percent"], json!(25.0)); // 1/4 * 100
    assert_eq!(obj["disk_total_gb"], json!(64.0));
    assert_eq!(obj["disk_used_gb"], json!(16.0));
    assert_eq!(obj["disk_percent"], json!(25.0)); // 16/64 * 100
    assert_eq!(obj["temperature"], json!(52.4));
}

// -------- services --------

#[test]
fn fallback_line_parses_a_running_unit_into_the_consolidated_shape() {
    let row = "ados-mavlink.service loaded active running ADOS MAVLink router";
    let svc = parse_fallback_line(row).unwrap();
    assert_eq!(svc["name"], json!("ados-mavlink"));
    assert_eq!(svc["state"], json!("active"));
    assert_eq!(svc["sub_state"], json!("running"));
    assert_eq!(svc["task_done"], json!(false));
    assert!(svc.get("uptimeSeconds").is_none());
}

#[test]
fn fallback_line_keeps_active_state_and_substate_apart() {
    // The ActiveState and the sub-state travel separately, so a consumer
    // can tell a stopped unit from a crash-looping one.
    let row = "ados-cloud.service loaded inactive dead ADOS cloud";
    let svc = parse_fallback_line(row).unwrap();
    assert_eq!(svc["name"], json!("ados-cloud"));
    assert_eq!(svc["state"], json!("inactive"));
    assert_eq!(svc["sub_state"], json!("dead"));
    assert_eq!(svc["task_done"], json!(true));

    let looping =
        parse_fallback_line("ados-video.service loaded activating auto-restart V").unwrap();
    assert_eq!(looping["state"], json!("activating"));
    assert_eq!(looping["sub_state"], json!("auto-restart"));
}

#[test]
fn fallback_line_keeps_a_failed_unit_behind_a_status_glyph() {
    // A failed unit is the row the dashboard most needs, and systemd prefixes
    // it with a glyph token (`●`, or `*` under LANG=C). It must not vanish.
    for row in [
        "● ados-x.service loaded failed failed X",
        "* ados-x.service loaded failed failed X",
    ] {
        let svc = parse_fallback_line(row).unwrap_or_else(|| panic!("dropped: {row}"));
        assert_eq!(svc["name"], json!("ados-x"));
        assert_eq!(svc["state"], json!("failed"));
        assert_eq!(svc["sub_state"], json!("failed"));
        assert_eq!(svc["task_done"], json!(true));
    }
}

#[test]
fn fallback_line_skips_short_and_non_service_rows() {
    assert!(parse_fallback_line("").is_none());
    assert!(parse_fallback_line("ados-x.service loaded").is_none());
    assert!(parse_fallback_line("ados-thing.timer loaded active waiting A timer").is_none());
}

#[test]
fn consolidated_service_entry_carries_memory_mb() {
    // Build a representative entry through the same code the route runs and
    // assert the full five-key consolidated shape.
    let mut entry =
        vec![parse_fallback_line("ados-video.service loaded active running ADOS Video").unwrap()];
    crate::routes::services::apply_service_memory(&mut entry, &Default::default());
    let obj = entry[0].as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["memory_mb", "name", "state", "sub_state", "task_done"]
    );
    // Nothing measured this unit, so its memory is unknown, not zero.
    assert!(obj["memory_mb"].is_null());
}

#[test]
fn condition_skipped_unit_reads_idle_condition_unmet() {
    let show = "Id=ados-vision-hailo.service\nConditionResult=no\n\
                    ConditionTimestampMonotonic=65848616590\n\n\
                    Id=ados-oled.service\nConditionResult=no\nConditionTimestampMonotonic=0\n\n\
                    Id=ados-cloud.service\nConditionResult=yes\n\
                    ConditionTimestampMonotonic=5032496\n";
    let unmet = condition_unmet_units(show);
    // Never evaluated (timestamp 0) and a met condition are not unmet.
    assert_eq!(unmet.len(), 1);
    assert!(unmet.contains("ados-vision-hailo"));

    let mut rows: Vec<Value> = [
        "ados-vision-hailo.service loaded inactive dead Hailo",
        "ados-oled.service loaded inactive dead OLED",
        "ados-cloud.service loaded inactive dead Cloud",
    ]
    .iter()
    .map(|r| parse_fallback_line(r).unwrap())
    .collect();
    apply_condition_unmet(&mut rows, &unmet);
    assert_eq!(rows[0]["state"], json!("idle"));
    assert_eq!(rows[0]["reason"], json!("condition_unmet"));
    assert_eq!(rows[0]["sub_state"], json!("dead"));
    for row in &rows[1..] {
        assert_eq!(row["state"], json!("inactive"));
        assert!(row.get("reason").is_none());
    }
}

// -------- radio --------

#[test]
fn radio_absent_block_is_the_full_null_skeleton() {
    let block = build_radio_block(None);
    assert_eq!(block["state"], json!("absent"));
    assert_eq!(block["paired"], json!(false));
    assert_eq!(block["bandwidth_mhz"], Value::Null);
    assert_eq!(block["rssi_dbm"], Value::Null);
    assert_eq!(block["channel"], Value::Null);
    // With no radio view there is no adapter or PHY verdict: null, never a
    // confident false that would read as a measured green USB link / red
    // no-injection / unmuted PHY for hardware nothing examined.
    for key in ["adapter_injection_ok", "adapter_usb_degraded", "phy_muted"] {
        assert!(
            block[key].is_null(),
            "{key} must be null on the absent block, got {:?}",
            block[key]
        );
    }
}

#[test]
fn radio_block_adapter_and_phy_verdicts_null_when_not_reported() {
    // A view that carries no boolean reading for a verdict (a receive-side
    // view, an older sidecar, a junk type) forwards null — the three-state
    // consumer's unknown — never a fabricated false.
    let no_fields = signals(&[("state", json!("connected"))]);
    let block = build_radio_block(Some(&no_fields));
    for key in ["adapter_injection_ok", "adapter_usb_degraded", "phy_muted"] {
        assert!(
            block[key].is_null(),
            "{key} absent from the view must forward as null, got {:?}",
            block[key]
        );
    }

    let junk = signals(&[
        ("state", json!("connected")),
        ("adapter_injection_ok", json!("yes")),
        ("adapter_usb_degraded", json!(1)),
        ("phy_muted", json!("no")),
    ]);
    let block = build_radio_block(Some(&junk));
    for key in ["adapter_injection_ok", "adapter_usb_degraded", "phy_muted"] {
        assert!(
            block[key].is_null(),
            "{key} with a non-boolean reading must forward as null"
        );
    }

    // Real booleans forward verbatim in BOTH directions, so a measured
    // warning and a measured all-clear both stay live.
    let measured = signals(&[
        ("state", json!("connected")),
        ("adapter_injection_ok", json!(false)),
        ("adapter_usb_degraded", json!(true)),
        ("phy_muted", json!(false)),
    ]);
    let block = build_radio_block(Some(&measured));
    assert_eq!(block["adapter_injection_ok"], json!(false));
    assert_eq!(block["adapter_usb_degraded"], json!(true));
    assert_eq!(block["phy_muted"], json!(false));
}

#[test]
fn radio_block_over_the_no_sidecar_base_carries_no_fabricated_verdicts() {
    // When neither a stored status nor a sidecar exists, the status view
    // falls back to the config-seeded base block. That base must never
    // seed a boolean for the adapter / PHY verdicts: `bool_or_null`
    // forwards a real boolean verbatim, so a false planted there would
    // resurface as a measured no-injection / healthy-USB claim for a rig
    // whose radio never scanned anything.
    let base = wfb_base_block(&WfbStatusConfig::default());
    for key in ["adapter_injection_ok", "adapter_usb_degraded", "phy_muted"] {
        assert!(
            base.get(key).map(Value::is_null).unwrap_or(true),
            "base block must not seed a boolean for {key}, got {:?}",
            base.get(key)
        );
    }
    let block = build_radio_block(Some(&base));
    for key in ["adapter_injection_ok", "adapter_usb_degraded", "phy_muted"] {
        assert!(
            block[key].is_null(),
            "{key} from the bare base must forward as null, got {:?}",
            block[key]
        );
    }
}

#[test]
fn radio_block_maps_a_live_status() {
    let status = signals(&[
        ("state", json!("active")),
        ("interface", json!("wlan1")),
        ("actual_channel", json!(149)),
        ("rssi_dbm", json!(-55.0)),
        ("bitrate_kbps", json!(8000)),
        ("paired", json!(true)),
        ("adapter_injection_ok", json!(true)),
        ("snr_db", json!(22.0)),
    ]);
    let block = build_radio_block(Some(&status));
    assert_eq!(block["state"], json!("active"));
    assert_eq!(block["iface"], json!("wlan1"));
    assert_eq!(block["channel"], json!(149));
    assert_eq!(block["freq_mhz"], json!(5745));
    assert_eq!(block["bandwidth_mhz"], json!(20));
    assert_eq!(block["rssi_dbm"], json!(-55.0));
    assert_eq!(block["bitrate_kbps"], json!(8000));
    assert_eq!(block["paired"], json!(true));
    assert_eq!(block["adapter_injection_ok"], json!(true));
    assert_eq!(block["snr_db"], json!(22.0));
    // fec_lost is the camel-free snake_case key sourced from fec_failed.
    assert_eq!(block["fec_lost"], Value::Null);
}

#[test]
fn radio_block_forwards_the_rf_unverified_verdict() {
    // The radio's authoritative verdict reaches the status surface verbatim
    // in both directions, so a consumer never re-derives it.
    let unverified = signals(&[
        ("state", json!("rf_unverified")),
        ("rf_unverified", json!(true)),
        ("channel_locked", json!(false)),
    ]);
    let block = build_radio_block(Some(&unverified));
    assert_eq!(block["rf_unverified"], json!(true));
    assert_eq!(block["state"], json!("rf_unverified"));

    let proven = signals(&[
        ("state", json!("connected")),
        ("rf_unverified", json!(false)),
        ("channel_locked", json!(true)),
    ]);
    assert_eq!(
        build_radio_block(Some(&proven))["rf_unverified"],
        json!(false)
    );
}

#[test]
fn radio_block_rf_unverified_is_null_when_it_cannot_be_sourced() {
    // Absent from the view (receive side / older sidecar): unknown, never a
    // confident false that would claim the transmit path was proven.
    let no_field = signals(&[("state", json!("connected"))]);
    assert_eq!(
        build_radio_block(Some(&no_field))["rf_unverified"],
        Value::Null
    );

    // Stale snapshot: the last-known value cannot describe the link NOW.
    let stale = signals(&[("state", json!("stale")), ("rf_unverified", json!(false))]);
    assert_eq!(
        build_radio_block(Some(&stale))["rf_unverified"],
        Value::Null
    );

    // No radio view at all.
    assert_eq!(build_radio_block(None)["rf_unverified"], Value::Null);
}

#[test]
fn radio_to_camel_exposes_rf_unverified() {
    // The generic snake→camel remap must land the field as `rfUnverified`,
    // the name the status consumers read.
    let status = signals(&[
        ("state", json!("rf_unverified")),
        ("rf_unverified", json!(true)),
    ]);
    let camel = radio_to_camel(build_radio_block(Some(&status)));
    assert_eq!(camel["rfUnverified"], json!(true));
}

#[test]
fn radio_sentinel_rssi_becomes_null() {
    let status = signals(&[("state", json!("searching")), ("rssi_dbm", json!(-100.0))]);
    let block = build_radio_block(Some(&status));
    assert_eq!(block["rssi_dbm"], Value::Null);
}

#[test]
fn radio_falsey_channel_and_bitrate_become_null() {
    let status = signals(&[
        ("state", json!("disabled")),
        ("actual_channel", json!(0)),
        ("bitrate_kbps", json!(0)),
    ]);
    let block = build_radio_block(Some(&status));
    assert_eq!(block["channel"], Value::Null);
    assert_eq!(block["freq_mhz"], Value::Null);
    assert_eq!(block["bitrate_kbps"], Value::Null);
}

#[test]
fn radio_to_camel_renames_snake_keys() {
    let snake = json!({
        "state": "active",
        "freq_mhz": 5745,
        "adapter_injection_ok": true,
        "tx_power_max_dbm": null,
        "valid_rx_packets_per_s": 100,
    });
    let camel = radio_to_camel(snake);
    let obj = camel.as_object().unwrap();
    assert!(obj.contains_key("state")); // single segment unchanged
    assert!(obj.contains_key("freqMhz"));
    assert!(obj.contains_key("adapterInjectionOk"));
    assert!(obj.contains_key("txPowerMaxDbm"));
    assert!(obj.contains_key("validRxPacketsPerS"));
}

// -------- video gate --------

#[test]
fn gs_video_delivering_requires_link_state_and_a_positive_rate() {
    // Not delivering: no status, wrong state, or zero rate.
    assert!(!gs_video_delivering(None));
    let searching = signals(&[("state", json!("searching"))]);
    assert!(!gs_video_delivering(Some(&searching)));
    let active_zero = signals(&[
        ("state", json!("active")),
        ("valid_rx_packets_per_s", json!(0)),
    ]);
    assert!(!gs_video_delivering(Some(&active_zero)));
    // Delivering: active + a positive valid-decode rate.
    let active_live = signals(&[
        ("state", json!("active")),
        ("valid_rx_packets_per_s", json!(120.0)),
    ]);
    assert!(gs_video_delivering(Some(&active_live)));
    // Or connected + a positive packet count.
    let connected = signals(&[
        ("state", json!("connected")),
        ("packets_received", json!(50)),
    ]);
    assert!(gs_video_delivering(Some(&connected)));
}

/// No sidecar the reader would trust: absent, unparseable, or stale past
/// `CAMERA_STATE_FRESH_S`.
fn no_pipeline() -> PipelineSidecar {
    PipelineSidecar::default()
}

/// A FRESH sidecar from a legacy ados-video: the writer is alive, but names
/// no `pipeline_state`.
fn legacy_pipeline() -> PipelineSidecar {
    PipelineSidecar {
        fresh: true,
        ..PipelineSidecar::default()
    }
}

fn pipeline(state: &str, reason: Option<&str>, encoder: Option<&str>) -> PipelineSidecar {
    PipelineSidecar {
        fresh: true,
        state: Some(state.to_string()),
        reason: reason.map(str::to_string),
        encoder: encoder.map(str::to_string),
        encoder_hw: None,
        encoder_forced_software: None,
    }
}

#[test]
fn video_block_with_no_mediamtx_is_default_on_drone() {
    // mediamtx absent (readiness injected false) → not_initialized, no whep.
    // The readiness is threaded in explicitly, so the assertion holds
    // deterministically regardless of whatever answers 9997/8889 on the host.
    let v = build_video_block_with("drone", &None, false, vec![], no_pipeline());
    assert_eq!(v["state"], json!("not_initialized"));
    assert_eq!(v["whep_url"], Value::Null);
    assert_eq!(v["recording"], json!(false));
    assert_eq!(v["recording_filename"], Value::Null);
}

#[test]
fn video_block_on_drone_with_a_streaming_pipeline_is_running() {
    let v = build_video_block_with(
        "drone",
        &None,
        true,
        vec![],
        PipelineSidecar {
            encoder_hw: Some(true),
            encoder_forced_software: Some(false),
            ..pipeline("streaming", None, Some("rpicam-vid"))
        },
    );
    assert_eq!(v["state"], json!("running"));
    assert_eq!(
        v["whep_url"],
        json!("/whep"),
        "relative: resolved against whatever host reached the agent"
    );
    assert_eq!(
        v["hls_url"],
        json!("/hls/main/index.m3u8"),
        "HLS fallback for remote / mixed-content viewers"
    );
    assert_eq!(
        v["encoder"],
        json!("rpicam-vid"),
        "the encoder identity is back"
    );
    assert_eq!(
        v["encoder_hw"],
        json!(true),
        "and whether it is hardware-backed"
    );
    assert_eq!(
        v["pipeline_state"],
        json!("streaming"),
        "the sidecar's own word"
    );
    // A pipeline that says it is publishing needs no readiness alibi.
    assert!(v.get("whep_url_basis").is_none());
    // No sidecar streams → no `streams` key (single-stream nodes unchanged).
    assert!(v.get("streams").is_none());
    assert_eq!(
        v["encoder_forced_software"],
        json!(false),
        "the forced-software pin is surfaced"
    );
}

#[test]
fn forced_software_is_read_from_the_camera_state_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_000.0;
    std::fs::write(
        dir.path().join("camera-state.json"),
        json!({
            "pipeline_state": "streaming",
            "encoder": "gstreamer-x264enc",
            "encoder_hw": false,
            "encoder_forced_software": true,
            "updated_at_unix": now,
        })
        .to_string(),
    )
    .unwrap();
    let p = read_pipeline_outcome_in(dir.path(), now);
    assert_eq!(p.encoder_forced_software, Some(true));
    let v = build_video_block_with("drone", &None, true, vec![], p);
    assert_eq!(v["encoder_forced_software"], json!(true));

    // A writer that predates the field leaves it absent, not false.
    std::fs::write(
        dir.path().join("camera-state.json"),
        json!({"pipeline_state": "streaming", "updated_at_unix": now}).to_string(),
    )
    .unwrap();
    let p = read_pipeline_outcome_in(dir.path(), now);
    assert_eq!(p.encoder_forced_software, None);
    let v = build_video_block_with("drone", &None, true, vec![], p);
    assert!(v.get("encoder_forced_software").is_none());
}

/// The customer report, as a test. A camera is detected and mediamtx is
/// bound, but the pipeline never started: the old code answered
/// `state: "running"` with a dialable `whep_url`, which is how a node shows
/// a confident camera card and no video.
#[test]
fn a_failed_pipeline_is_never_reported_as_running() {
    let v = build_video_block_with(
        "drone",
        &None,
        true,
        vec![],
        pipeline(
            "error",
            Some("no encoder backend is available for this camera"),
            None,
        ),
    );
    assert_eq!(v["state"], json!("error"));
    assert_eq!(
        v["whep_url"],
        Value::Null,
        "a failed pipeline gets no playable endpoint"
    );
    assert_eq!(
        v["reason"],
        json!("no encoder backend is available for this camera"),
        "the operator is told WHY, not just that something is wrong"
    );
}

/// A LEGACY ados-video: the sidecar is fresh, so the writer is alive, but it
/// names no `pipeline_state`. Answering `connecting` here would cost a
/// mid-upgrade fleet its cloud video AND its auto-retry, both of which the
/// cockpit gates on `videoState === "running"`, so the previous answer
/// stands — labelled with the basis it actually rests on.
#[test]
fn a_fresh_sidecar_with_no_pipeline_state_is_a_live_legacy_agent() {
    let v = build_video_block_with("drone", &None, true, vec![], legacy_pipeline());
    assert_eq!(v["state"], json!("running"));
    assert_eq!(v["whep_url"], json!("/whep"));
    assert_eq!(v["whep_url_basis"], json!("mediamtx_readiness_only"));
    assert_eq!(
        v["pipeline_state"],
        json!("unknown"),
        "it is alive but cannot say"
    );
}

/// The same legacy agent with mediamtx unbound: there is nothing to dial, so
/// nothing is advertised and no basis is claimed.
#[test]
fn a_legacy_agent_whose_mediamtx_is_not_bound_is_connecting() {
    let v = build_video_block_with("drone", &None, false, vec![], legacy_pipeline());
    assert_eq!(v["state"], json!("connecting"));
    assert_eq!(v["whep_url"], Value::Null);
    assert!(v.get("whep_url_basis").is_none());
}

/// No sidecar at all: ados-video has never run this boot. mediamtx readiness
/// alone is not evidence of a publisher — the WHEP probe accepts an endpoint
/// mediamtx serves with zero publishers.
#[test]
fn an_absent_sidecar_is_not_initialized_even_with_a_ready_mediamtx() {
    let v = build_video_block_with("drone", &None, true, vec![], no_pipeline());
    assert_eq!(v["state"], json!("not_initialized"));
    assert_eq!(v["whep_url"], Value::Null);
    assert!(
        v.get("pipeline_state").is_none(),
        "nothing answered, so the block claims no verdict"
    );
}

#[test]
fn a_streaming_pipeline_whose_mediamtx_is_not_ready_is_connecting() {
    let v = build_video_block_with(
        "drone",
        &None,
        false,
        vec![],
        pipeline("streaming", None, Some("ffmpeg-h264_v4l2m2m")),
    );
    assert_eq!(v["state"], json!("connecting"));
    assert_eq!(v["whep_url"], Value::Null);
    assert_eq!(v["encoder"], json!("ffmpeg-h264_v4l2m2m"));
    assert_eq!(v["pipeline_state"], json!("streaming"));
}

#[test]
fn video_block_advertises_per_leg_streams_when_present() {
    let streams = vec![
        json!({ "id": "main", "role": "eo", "codec": "h265", "whepUrl": "/whep?camera=main" }),
        json!({ "id": "ir", "role": "ir", "codec": "h264", "whepUrl": "/whep?camera=ir" }),
    ];
    let v = build_video_block_with(
        "drone",
        &None,
        true,
        streams,
        pipeline("streaming", None, None),
    );
    assert_eq!(v["state"], json!("running"));
    let legs = v["streams"].as_array().unwrap();
    assert_eq!(legs.len(), 2);
    assert_eq!(legs[1]["id"], json!("ir"));
    assert_eq!(legs[1]["whepUrl"], json!("/whep?camera=ir"));
}

#[test]
fn video_block_on_gs_without_a_live_link_is_stopped() {
    // A ground station whose WFB link is not delivering reports stopped, no
    // whep — regardless of mediamtx reachability (the gate is the link). Inject
    // mediamtx ready=true to prove the link gate dominates even then.
    let v = build_video_block_with("ground-station", &None, true, vec![], no_pipeline());
    assert_eq!(v["state"], json!("stopped"));
    assert_eq!(v["whep_url"], Value::Null);
}

#[test]
fn video_block_on_gs_delivering_but_no_mediamtx_is_connecting() {
    // Link delivering frames but mediamtx WHEP not yet serving → connecting.
    let wfb = Some(signals(&[
        ("state", json!("active")),
        ("valid_rx_packets_per_s", json!(120.0)),
    ]));
    let v = build_video_block_with("ground-station", &wfb, false, vec![], no_pipeline());
    assert_eq!(v["state"], json!("connecting"));
    assert_eq!(v["whep_url"], Value::Null);
}

/// A ground station has no local pipeline, but if one somehow reported an
/// error the surface must still not claim a playable endpoint.
#[test]
fn a_gs_with_a_live_link_still_reports_a_failed_local_pipeline() {
    let wfb = Some(signals(&[
        ("state", json!("active")),
        ("valid_rx_packets_per_s", json!(120.0)),
    ]));
    let v = build_video_block_with(
        "ground-station",
        &wfb,
        true,
        vec![],
        pipeline("error", Some("mediamtx failed to start"), None),
    );
    assert_eq!(v["state"], json!("error"));
    assert_eq!(v["whep_url"], Value::Null);
}

/// The writer died. mediamtx stays bound (a separate unit) and the sidecar
/// left behind still names a state, which is how a dead node stayed dialable
/// forever. A stale file is evidence of nothing, so nothing in it — not even
/// its `error` — reaches the block.
#[test]
fn a_stale_camera_state_sidecar_answers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    // 10 minutes old > the 300 s window.
    let stale = now - 600.0;
    std::fs::write(
            dir.path().join("camera-state.json"),
            format!(
                r#"{{"version":2,"state":"ready","pipeline_state":"error","pipeline_reason":"the encoder exited","updated_at_unix":{stale}}}"#
            ),
        )
        .unwrap();
    let sidecar = read_pipeline_outcome_in(dir.path(), now);
    assert!(!sidecar.fresh);
    assert_eq!(sidecar.state, None);
    let v = build_video_block_with("drone", &None, true, vec![], sidecar);
    assert_eq!(
        v["state"],
        json!("not_initialized"),
        "not `error`: nobody is asserting it"
    );
    assert_eq!(v["whep_url"], Value::Null);
    assert!(v.get("pipeline_state").is_none());
}

/// A fresh sidecar is read whole, under the run dir, including the encoder
/// identity and the hardware flag the GCS needs.
#[test]
fn a_fresh_camera_state_sidecar_carries_the_verdict_and_the_encoder() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    let stamped = now - 1.0;
    std::fs::write(
            dir.path().join("camera-state.json"),
            format!(
                r#"{{"version":2,"state":"ready","pipeline_state":"streaming","encoder":"ffmpeg-h264_v4l2m2m","encoder_hw":true,"updated_at_unix":{stamped}}}"#
            ),
        )
        .unwrap();
    let sidecar = read_pipeline_outcome_in(dir.path(), now);
    assert!(sidecar.fresh);
    assert_eq!(sidecar.state.as_deref(), Some("streaming"));
    assert_eq!(sidecar.encoder.as_deref(), Some("ffmpeg-h264_v4l2m2m"));
    assert_eq!(sidecar.encoder_hw, Some(true));
    let v = build_video_block_with("drone", &None, true, vec![], sidecar);
    assert_eq!(v["state"], json!("running"));
    assert_eq!(v["encoder_hw"], json!(true));
    assert_eq!(v["pipeline_state"], json!("streaming"));
}

#[test]
fn an_absent_camera_state_sidecar_answers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = read_pipeline_outcome_in(dir.path(), 1_700_000_000.0);
    assert!(!sidecar.fresh);
    assert_eq!(sidecar.state, None);
    assert_eq!(sidecar.encoder, None);
}

// -------- mesh --------

#[tokio::test]
async fn mesh_block_is_empty_for_a_drone_or_auto_profile() {
    assert_eq!(build_mesh_block("drone").await, json!({}));
    assert_eq!(build_mesh_block("auto").await, json!({}));
    assert_eq!(build_mesh_block("").await, json!({}));
}

#[test]
fn mesh_block_on_a_direct_ground_station_carries_role_and_capable() {
    let dir = tempfile::tempdir().unwrap();
    // direct role (sentinel absent) → role + mesh_capable, no mesh-state keys.
    let mesh = build_mesh_block_at(
        "ground_station",
        &dir.path().join("absent.role"),
        false,
        &dir.path().join("absent.mesh"),
    );
    let obj = mesh.as_object().unwrap();
    assert_eq!(obj["role"], json!("direct"));
    assert_eq!(obj["mesh_capable"], json!(false));
    assert!(!obj.contains_key("peer_count"));
}

#[test]
fn mesh_block_on_a_relay_folds_in_the_mesh_state() {
    let dir = tempfile::tempdir().unwrap();
    let role = dir.path().join("role");
    let state = dir.path().join("mesh-state.json");
    std::fs::write(&role, "relay\n").unwrap();
    std::fs::write(
            &state,
            r#"{"up":true,"neighbors":[{"id":"a"},{"id":"b"}],"selected_gateway":"a","partition":false}"#,
        )
        .unwrap();
    let mesh = build_mesh_block_at("ground_station", &role, true, &state);
    let obj = mesh.as_object().unwrap();
    assert_eq!(obj["role"], json!("relay"));
    assert_eq!(obj["mesh_capable"], json!(true));
    assert_eq!(obj["up"], json!(true));
    assert_eq!(obj["peer_count"], json!(2));
    assert_eq!(obj["selected_gateway"], json!("a"));
    assert_eq!(obj["partition"], json!(false));
}

// -------- reconciler verdicts --------

/// Helper: collect the reconciler keys from a tempdir at `now`.
fn reconciler_map(dir: &Path, now: f64) -> Map<String, Value> {
    read_reconciler_status_in(dir, now).into_iter().collect()
}

#[test]
fn a_fresh_management_link_sidecar_becomes_the_camelcase_block() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    std::fs::write(
            dir.path().join("mgmt-link.json"),
            format!(
                r#"{{"version":1,"state":"degraded","iface":"eth0","transport":"ethernet","backend":"networkd","carrier":true,"has_lease":true,"gateway_reachable":false,"repairing":true,"last_rung":"renew_dhcp","last_repair_at_unix":1699999990,"repairs_in_window":2,"updated_at_unix":{now}}}"#
            ),
        )
        .unwrap();
    let map = reconciler_map(dir.path(), now);
    let ml = map.get("managementLink").unwrap();
    assert_eq!(ml["state"], json!("degraded"));
    assert_eq!(ml["iface"], json!("eth0"));
    assert_eq!(ml["gatewayReachable"], json!(false));
    assert_eq!(ml["hasLease"], json!(true));
    assert_eq!(ml["repairing"], json!(true));
    assert_eq!(ml["lastRung"], json!("renew_dhcp"));
    assert_eq!(ml["repairsInWindow"], json!(2));
}

#[test]
fn a_stale_or_absent_or_unknown_management_link_is_omitted() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    // Absent.
    assert!(!reconciler_map(dir.path(), now).contains_key("managementLink"));
    // Stale: a stopped supervisor must not render its last verdict as live.
    let stale = now - MGMT_LINK_FRESH_S - 1.0;
    std::fs::write(
        dir.path().join("mgmt-link.json"),
        format!(r#"{{"version":1,"state":"healthy","updated_at_unix":{stale}}}"#),
    )
    .unwrap();
    assert!(!reconciler_map(dir.path(), now).contains_key("managementLink"));
    // Fresh but an unrecognised verdict the GCS clamp would drop anyway.
    std::fs::write(
        dir.path().join("mgmt-link.json"),
        format!(r#"{{"version":1,"state":"weird","updated_at_unix":{now}}}"#),
    )
    .unwrap();
    assert!(!reconciler_map(dir.path(), now).contains_key("managementLink"));
}

#[test]
fn a_fresh_failover_sidecar_becomes_the_mode_and_its_two_scalars() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    std::fs::write(
            dir.path().join("mgmt-failover.json"),
            format!(
                r#"{{"version":1,"mgmt_link_mode":"wifi_heartbeat","mgmt_failover_iface":"wlan0","mgmt_failover_reason":"primary_carrier_down","updated_at_unix":{now}}}"#
            ),
        )
        .unwrap();
    let map = reconciler_map(dir.path(), now);
    assert_eq!(map.get("mgmtLinkMode"), Some(&json!("wifi_heartbeat")));
    assert_eq!(map.get("mgmtFailoverIface"), Some(&json!("wlan0")));
    assert_eq!(
        map.get("mgmtFailoverReason"),
        Some(&json!("primary_carrier_down"))
    );

    // The primary mode carries nulls for both scalars, not omissions: the
    // operator needs "on the wired primary, no failover" to read distinctly
    // from "this agent does not report reach-back at all".
    std::fs::write(
            dir.path().join("mgmt-failover.json"),
            format!(
                r#"{{"version":1,"mgmt_link_mode":"primary","mgmt_failover_iface":null,"mgmt_failover_reason":null,"updated_at_unix":{now}}}"#
            ),
        )
        .unwrap();
    let map = reconciler_map(dir.path(), now);
    assert_eq!(map.get("mgmtLinkMode"), Some(&json!("primary")));
    assert_eq!(map.get("mgmtFailoverIface"), Some(&Value::Null));
}

#[test]
fn a_fresh_usb_rehome_sidecar_becomes_the_state_and_attempt_scalars() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    std::fs::write(
            dir.path().join("usb-rehome.json"),
            format!(
                r#"{{"version":1,"usb_rehome_state":"rehoming","usb_rehome_attempts":2,"usb_rehome_cooldown_s":60,"usb_rehome_last_result":"retry","updated_at_unix":{now}}}"#
            ),
        )
        .unwrap();
    let map = reconciler_map(dir.path(), now);
    assert_eq!(map.get("usbRehomeState"), Some(&json!("rehoming")));
    assert_eq!(map.get("usbRehomeAttempts"), Some(&json!(2)));
    assert_eq!(map.get("usbRehomeCooldownSeconds"), Some(&json!(60)));
    // The recovery machine has no terminal state, so the surface must
    // not advertise one: a `usbRehomeMaxAttempts` on the wire would tell
    // an operator the vehicle has a give-up point.
    assert!(map.get("usbRehomeMaxAttempts").is_none());
    assert_eq!(map.get("usbRehomeLastResult"), Some(&json!("retry")));

    let stale = now - USB_REHOME_FRESH_S - 1.0;
    std::fs::write(
        dir.path().join("usb-rehome.json"),
        format!(r#"{{"version":1,"usb_rehome_state":"rehoming","updated_at_unix":{stale}}}"#),
    )
    .unwrap();
    assert!(!reconciler_map(dir.path(), now).contains_key("usbRehomeState"));
}

#[test]
fn the_powersave_interface_map_becomes_the_array_the_gcs_clamp_requires() {
    // The one shape change of the four: the sidecar keys its snapshots by
    // interface name, the GCS requires `interfaces` to be an array whose
    // entries carry their own `iface`. A map shipped verbatim would fail the
    // clamp and render nothing.
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    std::fs::write(
            dir.path().join("wifi-powersave.json"),
            r#"{"version":1,"interfaces":{"wlan0":{"powersave_on":false,"reasserts":3,"last_reassert":"2026-08-13T04:00:00+00:00","signal_dbm":-52,"link_state":"connected"}}}"#,
        )
        .unwrap();
    let map = reconciler_map(dir.path(), now);
    let ps = map.get("wifiPowersave").unwrap();
    let rows = ps["interfaces"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["iface"], json!("wlan0"));
    assert_eq!(rows[0]["powersaveOn"], json!(false));
    assert_eq!(rows[0]["reasserts"], json!(3));
    assert_eq!(rows[0]["signalDbm"], json!(-52));
    assert_eq!(rows[0]["linkState"], json!("connected"));
}

#[test]
fn a_powersave_sidecar_with_no_interfaces_is_omitted() {
    // A profile with no managed WiFi interface must hide the card, not render
    // an empty one.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("wifi-powersave.json"),
        r#"{"version":1,"interfaces":{}}"#,
    )
    .unwrap();
    assert!(!reconciler_map(dir.path(), 1_700_000_000.0).contains_key("wifiPowersave"));
}

#[test]
fn an_absent_run_dir_folds_no_reconciler_keys_at_all() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_reconciler_status_in(dir.path(), 1_700_000_000.0).is_empty());
}

// -------- camera --------

#[test]
fn camera_status_folds_in_fresh_state() {
    // The path-injectable core sidesteps the process-global ADOS_RUN_DIR, so
    // this never races a sibling test mutating the same env var.
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    std::fs::write(
        dir.path().join("camera-state.json"),
        format!(r#"{{"state":"ready","updated_at_unix":{now}}}"#),
    )
    .unwrap();
    let map: Map<String, Value> = read_camera_status_in(dir.path(), now).into_iter().collect();
    assert_eq!(map.get("cameraState"), Some(&json!("ready")));
}

#[test]
fn camera_status_folds_in_a_fresh_usb_recovery_block() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    std::fs::write(
            dir.path().join("camera-usb-recovery.json"),
            format!(
                r#"{{"camera_usb_recovery_state":"rebinding","case":"wedged","attempts":2,"cooldown_s":60,"camera_present":false,"expected":true,"ppps_capable":true,"updated_at_unix":{now}}}"#
            ),
        )
        .unwrap();
    let map: Map<String, Value> = read_camera_status_in(dir.path(), now).into_iter().collect();
    let rec = map.get("cameraUsbRecovery").unwrap().as_object().unwrap();
    assert_eq!(rec["state"], json!("rebinding"));
    assert_eq!(rec["case"], json!("wedged"));
    assert_eq!(rec["attempts"], json!(2));
    assert_eq!(rec["cooldownSeconds"], json!(60));
    assert!(rec.get("maxAttempts").is_none());
    assert_eq!(rec["cameraPresent"], json!(false));
    assert_eq!(rec["expected"], json!(true));
    assert_eq!(rec["pppsCapable"], json!(true));
    // The power-contention fields surface; default safe when omitted.
    assert_eq!(rec["powerContention"], json!(false));
    assert_eq!(rec["contentionPeer"], Value::Null);
}

#[test]
fn camera_status_surfaces_power_contention() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    std::fs::write(
            dir.path().join("camera-usb-recovery.json"),
            format!(
                r#"{{"camera_usb_recovery_state":"needs_hub_reset","case":"absent","attempts":0,"cooldown_s":60,"camera_present":true,"expected":true,"ppps_capable":false,"power_contention":true,"contention_peer":"1-1.2","updated_at_unix":{now}}}"#
            ),
        )
        .unwrap();
    let map: Map<String, Value> = read_camera_status_in(dir.path(), now).into_iter().collect();
    let rec = map.get("cameraUsbRecovery").unwrap().as_object().unwrap();
    assert_eq!(rec["powerContention"], json!(true));
    assert_eq!(rec["contentionPeer"], json!("1-1.2"));
}

#[test]
fn camera_status_drops_a_stale_state() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    // 10 minutes old > the 300 s window → dropped.
    let stale = now - 600.0;
    std::fs::write(
        dir.path().join("camera-state.json"),
        format!(r#"{{"state":"missing","updated_at_unix":{stale}}}"#),
    )
    .unwrap();
    assert!(read_camera_status_in(dir.path(), now).is_empty());
}

/// The customer symptom through the camera field: a discovered camera whose
/// pipeline failed used to keep a confident `ready` pill, and the cockpit
/// raises its no-camera overlay only for `missing` — so the operator saw a
/// healthy card above a dead pane and no warning anywhere.
#[test]
fn camera_status_downgrades_ready_to_error_for_a_failed_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    std::fs::write(
        dir.path().join("camera-state.json"),
        format!(r#"{{"state":"ready","pipeline_state":"error","updated_at_unix":{now}}}"#),
    )
    .unwrap();
    let map: Map<String, Value> = read_camera_status_in(dir.path(), now).into_iter().collect();
    assert_eq!(map.get("cameraState"), Some(&json!("error")));
}

#[test]
fn camera_status_keeps_ready_while_the_pipeline_is_streaming() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    std::fs::write(
        dir.path().join("camera-state.json"),
        format!(r#"{{"state":"ready","pipeline_state":"streaming","updated_at_unix":{now}}}"#),
    )
    .unwrap();
    let map: Map<String, Value> = read_camera_status_in(dir.path(), now).into_iter().collect();
    assert_eq!(map.get("cameraState"), Some(&json!("ready")));
}

/// Discovery stays authoritative for `missing`: a node with no camera does
/// not get re-labelled by a pipeline that also failed.
#[test]
fn camera_status_leaves_missing_alone_when_the_pipeline_failed() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    std::fs::write(
        dir.path().join("camera-state.json"),
        format!(r#"{{"state":"missing","pipeline_state":"error","updated_at_unix":{now}}}"#),
    )
    .unwrap();
    let map: Map<String, Value> = read_camera_status_in(dir.path(), now).into_iter().collect();
    assert_eq!(map.get("cameraState"), Some(&json!("missing")));
}

// -------- crsf RC-lane fold --------

/// A node running the CRSF lane carries the `crsf` block on `/api/status/full`
/// — the full snake_case sidecar field set served verbatim, including the
/// LAN-only richer set (`flyable` + `pic` + `fc_command_down_gated`) the cloud
/// heartbeat projection drops. Mirrors the handler's
/// `if let Some(crsf) = read_crsf_status()` fold.
#[test]
fn crsf_sidecar_folds_the_full_field_set_onto_the_body() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crsf-stats.json");
    let body = json!({
        "v": 1,
        "state": "link_ok",
        "rssi_dbm": -51,
        "lq_uplink": 99,
        "lq_downlink": 97,
        "snr_db": 8,
        "band": null,
        "packet_rate_hz": 150,
        "tx_power_mw": 100,
        "tx_frames_per_s": 149.6,
        "rx_frames_per_s": 12.0,
        "rf_unverified": false,
        "flyable": true,
        "mode": "crsf_rc",
        "channel_source": "inject",
        "pic": "unclaimed",
        "relay_role": null,
        "fc_command_down_gated": null,
    });
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();

    let crsf =
        read_crsf_status_in(&path, std::time::SystemTime::now()).expect("fresh sidecar → Some");

    // Fold into a representative payload exactly as the handler does.
    let mut payload = Map::new();
    payload.insert("crsf".to_string(), crsf);
    let out = &payload["crsf"];

    // The whole sidecar body rides verbatim — the GCS reads these snake_case
    // keys directly, including the LAN-only richer set.
    assert_eq!(out, &body);
    assert_eq!(out["flyable"], json!(true));
    assert_eq!(out["pic"], json!("unclaimed"));
    assert!(out["fc_command_down_gated"].is_null());
    assert_eq!(out["rssi_dbm"], json!(-51));
    assert_eq!(out["state"], json!("link_ok"));
}

/// A node with no CRSF lane (no sidecar) folds NO `crsf` key — never a
/// fabricated all-null block for a node without the lane.
#[test]
fn crsf_absent_when_the_lane_has_no_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let mut payload = Map::new();
    if let Some(crsf) = read_crsf_status_in(
        &dir.path().join("crsf-stats.json"),
        std::time::SystemTime::now(),
    ) {
        payload.insert("crsf".to_string(), crsf);
    }
    assert!(
        !payload.contains_key("crsf"),
        "a node without the lane carries no crsf key"
    );
}

/// A stale sidecar (a dead lane's orphaned file, past the staleness window)
/// folds NO `crsf` key, so a dropped lane clears the tab rather than pinning a
/// frozen reading.
#[test]
fn crsf_absent_when_the_sidecar_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crsf-stats.json");
    std::fs::write(&path, r#"{"v":1,"state":"link_ok","flyable":true}"#).unwrap();
    let future =
        std::time::SystemTime::now() + CRSF_STATS_STALE + std::time::Duration::from_secs(5);
    assert!(read_crsf_status_in(&path, future).is_none());
}

/// A malformed sidecar folds NO `crsf` key (never a 500, never a garbage
/// block).
#[test]
fn crsf_absent_when_the_sidecar_is_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crsf-stats.json");
    std::fs::write(&path, b"not json {{{").unwrap();
    assert!(read_crsf_status_in(&path, std::time::SystemTime::now()).is_none());
}

/// A non-object (well-formed JSON but an array/scalar) reads absent, not a
/// 500 — the same posture the sibling sidecar readers hold.
#[test]
fn crsf_absent_when_the_sidecar_is_not_an_object() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crsf-stats.json");
    std::fs::write(&path, b"[1,2,3]").unwrap();
    assert!(read_crsf_status_in(&path, std::time::SystemTime::now()).is_none());
}

// -------- linked WFB peers fold --------

/// A ground node decoding fresh PresenceBeacons folds the peer list + the
/// freshest-peer scalars onto the body. The array entries carry EXACTLY the
/// snake_case key set the GCS `FullStatusResponse.linked_peers` reader consumes,
/// with the sidecar's `last_seen_unix` remapped to `seen_at_unix`. This pins the
/// LAN producer to the GCS reader so the two never drift apart.
#[test]
fn linked_peers_fold_the_fresh_set_and_scalars_snake_case() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    let older = now - 5.0;
    std::fs::write(
            dir.path().join("linked-peers.json"),
            format!(
                r#"{{"version":1,"wall_time_unix":{now},"peers":[
                    {{"device_id":"drone-a","role":"drone","channel":149,"rssi_dbm":-51,"last_seen_unix":{now},"slot":2}},
                    {{"device_id":"drone-b","role":"drone","channel":157,"rssi_dbm":-63,"last_seen_unix":{older},"slot":3}}
                ]}}"#
            ),
        )
        .unwrap();

    let map: Map<String, Value> = read_linked_peers_in(&dir.path().join("linked-peers.json"), now)
        .into_iter()
        .collect();

    let peers = map.get("linked_peers").unwrap().as_array().unwrap();
    assert_eq!(peers.len(), 2);

    // The freshest peer (drone-a) leads the list (writer newest-first).
    let entry = peers[0].as_object().unwrap();
    // EXACTLY the snake_case entry key set the GCS reader keys on.
    let mut keys: Vec<&str> = entry.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "channel",
            "device_id",
            "role",
            "rssi_dbm",
            "seen_at_unix",
            "slot"
        ]
    );
    // The sidecar's `last_seen_unix` was remapped — never surfaced verbatim.
    assert!(!entry.contains_key("last_seen_unix"));
    assert_eq!(entry["device_id"], json!("drone-a"));
    assert_eq!(entry["role"], json!("drone"));
    assert_eq!(entry["channel"], json!(149));
    assert_eq!(entry["rssi_dbm"], json!(-51));
    assert_eq!(entry["seen_at_unix"], json!(now));
    assert_eq!(entry["slot"], json!(2));

    // The camelCase scalar fallback carries the freshest peer.
    assert_eq!(map.get("peerDeviceId"), Some(&json!("drone-a")));
    assert_eq!(map.get("peerRssiDbm"), Some(&json!(-51)));
}

/// Every entry past the prune window → nothing folded, so a quiet / dead lane
/// clears the surface rather than pinning ghost peers. The window is the
/// radio's own 25 s peer-stale threshold: a drone silent for 30 s has
/// already returned home and is not linked.
#[test]
fn linked_peers_absent_when_every_entry_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    let stale = now - 30.0;
    std::fs::write(
            dir.path().join("linked-peers.json"),
            format!(
                r#"{{"version":1,"peers":[{{"device_id":"drone-a","role":"drone","channel":149,"rssi_dbm":-51,"last_seen_unix":{stale}}}]}}"#
            ),
        )
        .unwrap();
    assert!(read_linked_peers_in(&dir.path().join("linked-peers.json"), now).is_empty());
}

/// An id-less peer is dropped; a file of only id-less entries folds nothing.
#[test]
fn linked_peers_drop_idless_entries() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000.0;
    std::fs::write(
            dir.path().join("linked-peers.json"),
            format!(
                r#"{{"peers":[{{"device_id":"","role":"drone","channel":149,"rssi_dbm":-51,"last_seen_unix":{now}}}]}}"#
            ),
        )
        .unwrap();
    assert!(read_linked_peers_in(&dir.path().join("linked-peers.json"), now).is_empty());
}

/// A missing sidecar (a drone / a peerless node) folds nothing — never a
/// fabricated block.
#[test]
fn linked_peers_absent_when_the_sidecar_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        read_linked_peers_in(&dir.path().join("linked-peers.json"), 1_700_000_000.0).is_empty()
    );
}

/// A malformed sidecar folds nothing (never a 500, never garbage keys).
#[test]
fn linked_peers_absent_when_the_sidecar_is_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("linked-peers.json");
    std::fs::write(&path, b"not json {{{").unwrap();
    assert!(read_linked_peers_in(&path, 1_700_000_000.0).is_empty());
}

#[test]
fn a_sidecar_stamped_in_the_future_is_not_fresh() {
    let now = 1_000_000.0;
    let at = |t: f64| json!({ "updated_at_unix": t }).as_object().unwrap().clone();
    assert!(sidecar_fresh(&at(now - 5.0), now, 60.0));
    assert!(
        sidecar_fresh(&at(now + 1.0), now, 60.0),
        "rounding is not a clock step"
    );
    assert!(!sidecar_fresh(&at(now + 3600.0), now, 60.0));
    assert!(!sidecar_fresh(&at(now - 61.0), now, 60.0));
}

// -------- envelope --------

/// The handler names the FC firmware family the way `/api/status` does and
/// carries no `capabilities` block: the GCS derives compute capability from
/// the top-level perception fields instead.
#[tokio::test]
async fn the_full_status_names_the_firmware_and_carries_no_capabilities_block() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let state = AppState::new(
        std::sync::Arc::new(crate::auth::PairingState::with_path(d.join("pairing.json"))),
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(d.join("absent-mavlink.sock")),
        crate::ipc::LogdQueryClient::new(d.join("absent-logd.sock")),
        d.join("board.json"),
        crate::state::PairingPaths {
            config: d.join("config.yaml"),
            pairing_json: d.join("pairing.json"),
            wfb_key_dir: d.join("wfb"),
            bind_state: d.join("bind-state.json"),
            profile_conf: d.join("profile.conf"),
            mesh_role: d.join("mesh-role"),
            relay_secret: d.join("relay-peer-secret"),
        },
        std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            d.join("dashboard-pin.json"),
        )),
        std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
            d.join("mcp-token.json"),
        )),
    );
    state
        .state
        .set_snapshot_for_test(json!({"fc_firmware": "ardupilot", "armed": false}));
    let body = serde_json::to_value(get_full_status(State(state)).await.0).unwrap();
    assert_eq!(body["fcFirmware"], json!("ardupilot"));
    assert!(body.get("capabilities").is_none());
    for key in [
        "npuTops",
        "hasAccelerator",
        "perceptionTier",
        "perceptionOffloadTarget",
    ] {
        assert!(body.get(key).is_some(), "{key} is the capability source");
    }
}

// -------- macStability --------

/// The per-adapter verdicts must reach the status body from the state file.
///
/// Six GCS readers and three cards consumed `macStability` while no transport
/// produced it, so the whole feature rendered nothing. The projection is
/// `ados-macpin`'s (the crate that owns the file), shared with the cloud
/// heartbeat, so this pins the shape both transports carry.
#[test]
fn the_mac_pin_verdicts_project_to_the_block_the_gcs_reads() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("mac-pins.state");
    std::fs::write(
        &state,
        r#"{"version":1,"adapters":[
                {"name":"wlan1","vidpid":"0bda:a81a","usb_path":"1-1.2","state":"pinned",
                 "applied_live":true,"pinned_mac":"00:11:22:33:44:55","source":"learner"},
                {"name":"wlan2","vidpid":"0bda:8812","usb_path":"1-1.3","state":"stable",
                 "applied_live":false,"pinned_mac":null}
            ]}"#,
    )
    .unwrap();

    let block = ados_macpin::engine::adapters_camel_from(&state).expect("a pinned node");
    let adapters = block["adapters"].as_array().unwrap();
    assert_eq!(adapters.len(), 2);
    assert_eq!(adapters[0]["usbPath"], json!("1-1.2"));
    assert_eq!(adapters[0]["pinnedMac"], json!("00:11:22:33:44:55"));
    assert_eq!(adapters[0]["appliedLive"], json!(true));
    assert_eq!(adapters[1]["appliedLive"], json!(false));
    // A null source value omits the key rather than rendering it null.
    assert!(
        !adapters[1].as_object().unwrap().contains_key("pinnedMac"),
        "a null pinned_mac must omit the key, not claim an empty pin"
    );
}

#[test]
fn a_node_that_pinned_nothing_omits_the_mac_stability_block() {
    let dir = tempfile::tempdir().unwrap();
    // Absent state file.
    assert!(ados_macpin::engine::adapters_camel_from(&dir.path().join("nope.state")).is_none());
    // Present but empty: the card must stay hidden rather than claim the node
    // looked and found no adapter.
    let empty = dir.path().join("empty.state");
    std::fs::write(&empty, r#"{"version":1,"adapters":[]}"#).unwrap();
    assert!(ados_macpin::engine::adapters_camel_from(&empty).is_none());
}

// -------- shared slow blocks --------

/// Two requests inside the TTL share one computation, so one
/// `systemctl list-units` serves both; the seam counts computations.
#[tokio::test]
async fn status_full_two_requests_one_list_units() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cache = TtlCache::new();
    let list_units_calls = AtomicUsize::new(0);
    let calls = &list_units_calls;
    let probe = move || async move {
        calls.fetch_add(1, Ordering::SeqCst);
        json!({"services": [{"name": "ados-cloud"}], "folds": {}})
    };
    let (a, b) = tokio::join!(
        cache.get_or_compute(SLOW_BLOCKS_TTL, probe),
        cache.get_or_compute(SLOW_BLOCKS_TTL, probe),
    );
    assert_eq!(a, b);
    assert_eq!(list_units_calls.load(Ordering::SeqCst), 1);
    // An expired slot recomputes.
    cache.get_or_compute(Duration::ZERO, probe).await;
    assert_eq!(list_units_calls.load(Ordering::SeqCst), 2);
}

// -------- fc_identity + router extras --------

fn state_in(d: &Path) -> AppState {
    AppState::new(
        std::sync::Arc::new(crate::auth::PairingState::with_path(d.join("pairing.json"))),
        crate::ipc::StateIpcClient::disconnected(),
        crate::ipc::MavlinkIpcClient::new(d.join("absent-mavlink.sock")),
        crate::ipc::LogdQueryClient::new(d.join("absent-logd.sock")),
        d.join("board.json"),
        crate::state::PairingPaths {
            config: d.join("config.yaml"),
            pairing_json: d.join("pairing.json"),
            wfb_key_dir: d.join("wfb"),
            bind_state: d.join("bind-state.json"),
            profile_conf: d.join("profile.conf"),
            mesh_role: d.join("mesh-role"),
            relay_secret: d.join("relay-peer-secret"),
        },
        std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            d.join("dashboard-pin.json"),
        )),
        std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
            d.join("mcp-token.json"),
        )),
    )
    .with_run_dir(d.to_path_buf())
}

#[tokio::test]
async fn fc_identity_block_reads_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let state = state_in(d);
    // Before the reconciler's first tick the block is null.
    let body = serde_json::to_value(get_full_status(State(state.clone())).await.0).unwrap();
    assert_eq!(body["fc_identity"], Value::Null);

    std::fs::write(
            d.join("fc-identity.json"),
            r#"{"version":1,"enabled":false,"outcome":"disabled","wanted_system_id":null,"current_system_id":7,"at_ms":1}"#,
        )
        .unwrap();
    state.state.set_snapshot_for_test(json!({
            "armed": false,
            "mavlink_proxy_posture": {"tcp_listening": true, "udp_listening": [true], "ws_listening": false},
            "agent_commands_unsigned_rejected_risk": true,
            "injector_arbitration": "off",
            "agent_signing": "active",
        }));
    let body = serde_json::to_value(get_full_status(State(state)).await.0).unwrap();
    assert_eq!(body["fc_identity"]["outcome"], json!("disabled"));
    assert_eq!(body["fc_identity"]["current_system_id"], json!(7));
    assert_eq!(body["mavlink_proxy_posture"]["tcp_listening"], json!(true));
    assert_eq!(
        body["mavlink_proxy_posture"]["udp_listening"],
        json!([true])
    );
    assert_eq!(body["agent_commands_unsigned_rejected_risk"], json!(true));
    assert_eq!(body["injector_arbitration"], json!("off"));
    assert_eq!(body["agent_signing"], json!("active"));
}

#[test]
fn router_extras_are_null_until_published() {
    for (_, v) in router_status_extras(None) {
        assert_eq!(v, Value::Null);
    }
    let extras = router_status_extras(Some(&json!({"injector_arbitration": "active"})));
    assert_eq!(extras.len(), 4);
    assert!(extras.contains(&("injector_arbitration".to_string(), json!("active"))));
}
