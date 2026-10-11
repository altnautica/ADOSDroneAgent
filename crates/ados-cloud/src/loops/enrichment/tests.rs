use super::*;

#[test]
fn parse_proc_stat_cpu_reads_the_aggregate_line() {
    // Aggregate line: user nice system idle iowait irq softirq steal ...
    let s = parse_proc_stat_cpu("cpu  100 0 50 800 50 0 0 0 0 0").unwrap();
    // total = 100+0+50+800+50 = 1000; idle = 800 + 50 (iowait) = 850.
    assert_eq!(s.total, 1000);
    assert_eq!(s.idle, 850);
    // A per-core line is not the aggregate and is rejected.
    assert!(parse_proc_stat_cpu("cpu0 1 2 3 4 5").is_none());
    // A non-cpu line is rejected.
    assert!(parse_proc_stat_cpu("intr 12345 0 0").is_none());
    // Exactly four fields (user nice system idle, no iowait) is the minimum
    // valid aggregate line — idle is index 3.
    let four = parse_proc_stat_cpu("cpu 1 2 3 4").unwrap();
    assert_eq!(four.total, 10);
    assert_eq!(four.idle, 4); // idle only; no iowait field present.
                              // Fewer than four fields cannot supply the idle slot → rejected.
    assert!(parse_proc_stat_cpu("cpu 1 2 3").is_none());
    assert!(parse_proc_stat_cpu("cpu 1 2").is_none());
}

#[test]
fn cpu_percent_computes_the_inter_tick_delta() {
    // Between the two samples: total advanced 1000, idle advanced 800 →
    // busy 200 → 20%.
    let prev = CpuSample {
        idle: 800,
        total: 1000,
    };
    let cur = CpuSample {
        idle: 1600,
        total: 2000,
    };
    let pct = cpu_percent(&prev, &cur).unwrap();
    assert!((pct - 20.0).abs() < 1e-9, "expected 20%, got {pct}");

    // A fully-busy interval: idle did not advance → 100%.
    let busy = CpuSample {
        idle: 800,
        total: 2000,
    };
    assert!((cpu_percent(&prev, &busy).unwrap() - 100.0).abs() < 1e-9);

    // No elapsed time (total unchanged) → None (undefined, omit the field).
    assert!(cpu_percent(&prev, &prev).is_none());

    // Counters ran backwards (a stat reset) → None.
    let backwards = CpuSample {
        idle: 0,
        total: 500,
    };
    assert!(cpu_percent(&prev, &backwards).is_none());
}

#[test]
fn parse_meminfo_derives_used_and_percent() {
    // 4 GiB total, 1 GiB available → 3 GiB used → 75%.
    let text = "\
MemTotal:        4194304 kB
MemFree:          524288 kB
MemAvailable:    1048576 kB
Buffers:          102400 kB
Cached:           409600 kB
SwapTotal:       2097152 kB
SwapFree:        1048576 kB
";
    let m = parse_meminfo(text).unwrap();
    assert_eq!(m.total_mb, 4096);
    assert_eq!(m.available_mb, 1024);
    assert_eq!(m.used_mb, 3072);
    // cache = Cached + Buffers = 409600 + 102400 = 512000 kB = 500 MiB.
    assert_eq!(m.cache_mb, 500);
    assert!((m.percent - 75.0).abs() < 1e-9);
    // Swap: 2 GiB total, 1 GiB free → 1 GiB used → 50%.
    assert_eq!(m.swap_total_mb, 2048);
    assert_eq!(m.swap_used_mb, 1024);
    assert!((m.swap_percent - 50.0).abs() < 1e-9);
}

#[test]
fn parse_meminfo_handles_no_swap_and_rejects_zero_total() {
    let text = "MemTotal: 1048576 kB\nMemAvailable: 524288 kB\n";
    let m = parse_meminfo(text).unwrap();
    assert_eq!(m.total_mb, 1024);
    assert_eq!(m.used_mb, 512);
    // No swap lines → zero swap, 0% (never a divide-by-zero).
    assert_eq!(m.swap_total_mb, 0);
    assert_eq!(m.swap_used_mb, 0);
    assert_eq!(m.swap_percent, 0.0);
    // A zero or absent MemTotal is a meaningless sample → None.
    assert!(parse_meminfo("MemTotal: 0 kB\n").is_none());
    assert!(parse_meminfo("MemFree: 100 kB\n").is_none());
}

#[test]
fn parse_df_root_reads_blocks_and_percent() {
    let out = "\
Filesystem     1024-blocks    Used Available Capacity Mounted on
/dev/root         62914560 6291456  56623104       10% /
";
    let d = parse_df_root(out).unwrap();
    // 62914560 KiB = 60 GiB total; 6291456 KiB = 6 GiB used; 10%.
    assert!((d.total_gb - 60.0).abs() < 1e-6, "total {}", d.total_gb);
    assert!((d.used_gb - 6.0).abs() < 1e-6, "used {}", d.used_gb);
    assert!((d.percent - 10.0).abs() < 1e-9, "percent {}", d.percent);
    // No data line → None.
    assert!(parse_df_root("Filesystem 1024-blocks Used Available Capacity Mounted on\n").is_none());
}

#[test]
fn parse_thermal_millidegrees_converts() {
    assert!((parse_thermal_millidegrees("47500\n").unwrap() - 47.5).abs() < 1e-9);
    assert!((parse_thermal_millidegrees("  52000 ").unwrap() - 52.0).abs() < 1e-9);
    assert!(parse_thermal_millidegrees("not-a-number").is_none());
}

#[test]
fn parse_systemctl_units_reports_the_rendered_status_vocabulary() {
    // The --no-legend output: UNIT LOAD ACTIVE SUB DESCRIPTION.
    let out = "\
ados-supervisor.service loaded active running ADOS process supervisor
ados-setup.service      loaded active exited  ADOS first-boot setup
ados-cloud.service      loaded inactive dead   ADOS cloud relay
ados-video.service      loaded activating auto-restart ADOS video pipeline
";
    let svcs = parse_systemctl_units(out);
    let status: Vec<(&str, &str)> = svcs
        .iter()
        .map(|s| (s["name"].as_str().unwrap(), s["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        status,
        [
            ("ados-supervisor", "running"),
            // A oneshot that finished did its job.
            ("ados-setup", "running"),
            ("ados-cloud", "stopped"),
            // A unit systemd keeps restarting is crash-looping.
            ("ados-video", "error"),
        ]
    );
    // list-units measures no per-unit accounting, so none is asserted.
    assert!(svcs[0].get("uptimeSeconds").is_none());
    assert!(svcs[0].get("memoryMb").is_none());
}

#[test]
fn parse_systemctl_units_tolerates_lead_glyph_and_blank_lines() {
    // A failed unit gets a `●` lead glyph; a blank line is skipped.
    let out = "\
● ados-net.service loaded failed failed ADOS uplink

ados-mavlink-router.service loaded active running ADOS MAVLink router
";
    let svcs = parse_systemctl_units(out);
    assert_eq!(svcs.len(), 2);
    assert_eq!(svcs[0]["name"], "ados-net");
    assert_eq!(svcs[0]["status"], "error");
    assert_eq!(svcs[1]["name"], "ados-mavlink-router");
    assert_eq!(svcs[1]["status"], "running");
}

#[test]
fn an_unknown_active_state_is_degraded_not_stopped() {
    assert_eq!(service_status("maintenance", "running"), "degraded");
}

#[test]
fn build_native_enrichment_omits_cpu_on_first_tick_and_is_an_object() {
    // The first call has no prior sample, so cpuPercent is omitted; the
    // sample is seeded for the next tick. The result is always an object.
    let mut prev: Option<CpuSample> = None;
    let v = build_native_enrichment(&mut prev);
    assert!(v.is_object());
    let obj = v.as_object().unwrap();
    assert!(
        !obj.contains_key("cpuPercent"),
        "first tick has no delta to report"
    );
    // The CPU sample is seeded (on a host with /proc/stat) so the next tick
    // can compute a delta. On a non-Linux dev host /proc/stat is absent and
    // prev stays None, which is fine — the field is simply never folded.
    if Path::new("/proc/stat").exists() {
        assert!(prev.is_some(), "the sample is seeded for the next tick");
    }
    // No key is ever a JSON null — every value is a real reading or omitted.
    for (k, val) in obj {
        assert!(!val.is_null(), "{k} must be a real value or omitted");
    }
}

/// Serializes the tests that steer `fold_fc` with `ADOS_STATE_SOCK`.
///
/// The variable is process-global while Rust runs tests on several threads
/// in one process, so without this the socket-present test can have its path
/// swapped out from under it by the socket-absent test and read `None` for
/// every FC field. It fails only under the load of a full workspace run,
/// which is the worst kind of flake: the narrow `-p ados-cloud` run that a
/// change gets checked with passes, and the gate goes red somewhere else.
/// Poisoning is recovered rather than propagated — one failing test should
/// not cascade into its neighbour.
static STATE_SOCK_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn read_state_snapshot_lifts_fc_fields_from_a_socket() {
    let _guard = STATE_SOCK_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Stand up a one-shot Unix socket that replays a state frame, point the
    // enrichment at it, and assert the FC fields fold in. Exercise BOTH wire
    // forms — v1 newline-JSON and v2 length-prefixed msgpack — because the
    // shared reader auto-detects per frame and the installer ships v2 (a
    // v1-only read would silently lift nothing against a v2 producer).
    let snap = json!({
        "armed": false,
        "fc_connected": true,
        "fc_port": "/dev/ttyACM0",
        "fc_baud": 115200,
        "transport_open": true,
        "mavlink_alive": true,
        "heartbeat_age_s": 0.5,
        "fc_source": "serial",
        "fc_link_hint": "none",
        "fc_command_down_gated": true,
        "fc_variant": "betaflight",
        "fc_firmware": "betaflight"
    });

    // Serve `wire` bytes on a fresh socket and return the folded enrichment.
    //
    // Retried, deliberately. `read_state_snapshot` gives the socket a 200 ms
    // read timeout, which is the right production bound but is a real wall
    // clock: under a full `cargo test --workspace` the writer thread is not
    // always scheduled inside it, the read times out, and every FC field
    // comes back absent. The assertion here is about which fields get lifted
    // off which wire version, not about latency, so a scheduling miss must
    // not be reported as a parsing failure. An earlier attempt at this blamed
    // the shared `ADOS_STATE_SOCK` env var — that race is real and is still
    // serialized above, but it was not what made this test red.
    let fold_over_wire_once = |tag: &str, wire: Vec<u8>| -> Map<String, Value> {
        let dir = std::env::temp_dir();
        let sock = dir.join(format!(
            "ados-enrich-state-{}-{}.sock",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let sock_for_thread = sock.clone();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Write;
                let _ = stream.write_all(&wire);
                let _ = stream.flush();
            }
            let _ = std::fs::remove_file(&sock_for_thread);
        });

        std::env::set_var("ADOS_STATE_SOCK", &sock);
        let mut obj = Map::new();
        fold_fc(&mut obj);
        std::env::remove_var("ADOS_STATE_SOCK");
        let _ = handle.join();
        obj
    };

    // Up to five attempts; in practice the first succeeds. Five consecutive
    // misses is not scheduler noise, it is a real regression.
    let fold_over_wire = |tag: &str, wire: Vec<u8>| -> Map<String, Value> {
        let mut last = Map::new();
        for attempt in 0..5 {
            last = fold_over_wire_once(tag, wire.clone());
            if last.contains_key("fcConnected") {
                return last;
            }
            std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
        }
        last
    };

    let assert_fc = |obj: &Map<String, Value>| {
        assert_eq!(obj.get("fcConnected"), Some(&json!(true)));
        assert_eq!(obj.get("fcPort"), Some(&json!("/dev/ttyACM0")));
        assert_eq!(obj.get("fcBaud"), Some(&json!(115200)));
        // The gated-truth detail lifts alongside the connection triple.
        assert_eq!(obj.get("transportOpen"), Some(&json!(true)));
        assert_eq!(obj.get("mavlinkAlive"), Some(&json!(true)));
        assert_eq!(obj.get("heartbeatAgeS"), Some(&json!(0.5)));
        assert_eq!(obj.get("fcSource"), Some(&json!("serial")));
        assert_eq!(obj.get("fcLinkHint"), Some(&json!("none")));
        assert_eq!(obj.get("fcCommandDownGated"), Some(&json!(true)));
        assert_eq!(obj.get("fcVariant"), Some(&json!("betaflight")));
        assert_eq!(obj.get("fcFirmware"), Some(&json!("betaflight")));
    };

    // v1 newline-JSON.
    let v1 = ados_protocol::state::encode_v1(&snap).unwrap();
    assert_fc(&fold_over_wire("v1", v1));

    // v2 length-prefixed msgpack (the shipped wire form).
    let v2 = ados_protocol::state::encode_v2(&snap).unwrap();
    assert_fc(&fold_over_wire("v2", v2));
}

#[test]
fn fold_fc_omits_everything_when_the_socket_is_absent() {
    let _guard = STATE_SOCK_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var("ADOS_STATE_SOCK", "/nonexistent/ados/state.sock");
    let mut obj = Map::new();
    fold_fc(&mut obj);
    std::env::remove_var("ADOS_STATE_SOCK");
    assert!(obj.is_empty(), "no FC fields without a state socket");
}
