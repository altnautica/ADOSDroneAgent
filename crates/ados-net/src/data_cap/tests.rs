use super::*;

struct FixedSource(Option<UsageSample>);
#[async_trait]
impl UsageSource for FixedSource {
    async fn data_usage(&self) -> Option<UsageSample> {
        self.0.clone()
    }
}

fn sample_on(iface: &str, rx: u64, tx: u64) -> Arc<FixedSource> {
    Arc::new(FixedSource(Some(UsageSample {
        iface: iface.to_string(),
        bytes: UsageBytes {
            rx_bytes: rx,
            tx_bytes: tx,
        },
    })))
}

fn fixed(rx: u64, tx: u64) -> Arc<FixedSource> {
    sample_on("wwan0", rx, tx)
}

/// A tracker whose baseline is wwan0 at zero, so the first poll counts the
/// whole sample.
fn baselined(
    src: Arc<FixedSource>,
    bus: &Arc<UplinkEventBus>,
    cap_gb: f64,
    state_path: PathBuf,
) -> DataCapTracker {
    let mut t = DataCapTracker::with_config(src, Arc::clone(bus), cap_gb, state_path);
    t.state.last_iface = Some("wwan0".to_string());
    t
}

fn tracker(
    cap_gb: f64,
    state_path: PathBuf,
    rx: u64,
    tx: u64,
) -> (DataCapTracker, Arc<UplinkEventBus>) {
    let bus = Arc::new(UplinkEventBus::new());
    let t = baselined(fixed(rx, tx), &bus, cap_gb, state_path);
    (t, bus)
}

#[test]
fn classify_thresholds_match_python() {
    let cap = cap_to_bytes(1.0); // 1 GiB
                                 // Pick values comfortably inside each band (rounding `as u64` down can
                                 // pull an exact-boundary value back under the threshold, so nudge up).
    let at = |frac: f64| ((cap as f64 * frac) as u64) + 1;
    assert_eq!(classify(cap, 0), DataCapState::Ok);
    assert_eq!(classify(cap, at(0.79)), DataCapState::Ok);
    assert_eq!(classify(cap, at(0.80)), DataCapState::Warn80);
    assert_eq!(classify(cap, at(0.85)), DataCapState::Warn80);
    assert_eq!(classify(cap, at(0.95)), DataCapState::Throttle95);
    assert_eq!(classify(cap, at(0.97)), DataCapState::Throttle95);
    assert_eq!(classify(cap, cap), DataCapState::Blocked100);
    assert_eq!(classify(cap, cap + 1), DataCapState::Blocked100);
    // cap <= 0 → ok regardless.
    assert_eq!(classify(0, 999_999), DataCapState::Ok);
}

#[test]
fn render_json_has_a_fixed_key_order() {
    let st = UsageState {
        window_started_at: 1_700_000_000.0,
        cumulative_bytes: 123_456,
        last_rx: 1000,
        last_tx: 2000,
        last_iface: Some("wwan0".to_string()),
        last_reset_month: "2026-05".to_string(),
    };
    assert_eq!(
        st.render_json(),
        r#"{"window_started_at": 1700000000.0, "cumulative_bytes": 123456, "last_rx": 1000, "last_tx": 2000, "last_iface": "wwan0", "last_reset_month": "2026-05"}"#
    );
}

#[test]
fn save_then_load_round_trips_and_fsync_leaves_no_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let (mut t, _bus) = tracker(5.0, path.clone(), 0, 0);
    t.state.cumulative_bytes = 42;
    t.state.last_reset_month = "2026-05".to_string();
    t.save_state();
    // No torn tmp (with_suffix(".json.tmp")).
    assert!(!dir.path().join("modem-usage.json.tmp").exists());
    let loaded = load_state(&path);
    assert_eq!(loaded.cumulative_bytes, 42);
    assert_eq!(loaded.last_reset_month, "2026-05");
}

#[tokio::test]
async fn poll_counts_a_restarted_counter_from_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let bus = Arc::new(UplinkEventBus::new());
    let mut t = baselined(fixed(1000, 500), &bus, 5.0, path);
    t.poll_once().await;
    assert_eq!(t.state.cumulative_bytes, 1500);
    assert_eq!(t.state.last_rx, 1000);
    // The modem re-enumerated: the counter restarted, so its current value
    // is the traffic since the restart and is counted, not dropped.
    t.source = fixed(10, 5);
    t.poll_once().await;
    assert_eq!(t.state.cumulative_bytes, 1515);
    assert_eq!(t.state.last_rx, 10);
}

#[tokio::test]
async fn an_interface_switch_rebaselines_instead_of_counting_the_new_counter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let bus = Arc::new(UplinkEventBus::new());
    let mut t = baselined(fixed(1000, 500), &bus, 5.0, path);
    t.poll_once().await;
    assert_eq!(t.state.cumulative_bytes, 1500);
    // The modem now presents usb0, whose counter is unrelated to wwan0's.
    t.source = sample_on("usb0", 9_000_000, 9_000_000);
    t.poll_once().await;
    assert_eq!(t.state.cumulative_bytes, 1500);
    assert_eq!(t.state.last_iface.as_deref(), Some("usb0"));
    // Next sample on usb0 counts against the usb0 baseline.
    t.source = sample_on("usb0", 9_000_100, 9_000_000);
    t.poll_once().await;
    assert_eq!(t.state.cumulative_bytes, 1600);
}

#[tokio::test]
async fn an_unreadable_sample_keeps_the_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let bus = Arc::new(UplinkEventBus::new());
    let mut t = baselined(fixed(1000, 500), &bus, 5.0, path);
    t.poll_once().await;
    t.source = Arc::new(FixedSource(None));
    t.poll_once().await;
    assert_eq!(t.state.last_rx, 1000);
    assert_eq!(t.state.cumulative_bytes, 1500);
    // The next good sample counts only the growth since the last one.
    t.source = fixed(1100, 500);
    t.poll_once().await;
    assert_eq!(t.state.cumulative_bytes, 1600);
}

#[tokio::test]
async fn a_month_rollover_does_not_charge_the_since_boot_counter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let bus = Arc::new(UplinkEventBus::new());
    // A box that has carried 4 GiB since boot, sampled last month.
    let since_boot = 4 * 1024 * 1024 * 1024u64;
    let mut t = baselined(fixed(since_boot, 0), &bus, 5.0, path);
    t.state.last_rx = since_boot;
    t.state.cumulative_bytes = 3 * 1024 * 1024 * 1024;
    t.state.last_reset_month = "1999-01".to_string();
    assert!(t.check_month_reset());
    // 1 MiB more arrives in the new month.
    t.source = fixed(since_boot + 1024 * 1024, 0);
    t.poll_once().await;
    assert_eq!(t.state.cumulative_bytes, 1024 * 1024);
    assert_eq!(t.classify(), DataCapState::Ok);
}

#[tokio::test]
async fn poll_emits_threshold_event_only_on_transition() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    // cap 1 KiB so a small counter trips blocked_100 immediately.
    let bus = Arc::new(UplinkEventBus::new());
    let mut rx = bus.subscribe();
    let mut t = baselined(fixed(4096, 0), &bus, 1.0 / (1024.0 * 1024.0), path);
    t.poll_once().await;
    let evt = rx
        .try_recv()
        .expect("a threshold event on the first crossing");
    assert_eq!(evt.kind, UplinkEventKind::DataCapThreshold);
    assert_eq!(evt.data_cap_state, Some(DataCapState::Blocked100));
    // Second poll, same state → NO new event.
    t.poll_once().await;
    assert!(rx.try_recv().is_err(), "no event when state is unchanged");
}

#[test]
fn get_usage_keys_match_python() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let (t, _bus) = tracker(5.0, path, 0, 0);
    let u = t.get_usage();
    for k in [
        "data_used_mb",
        "cap_mb",
        "percent",
        "state",
        "window_reset_at",
        "last_reset_month",
    ] {
        assert!(u.get(k).is_some(), "missing key {k}");
    }
    assert_eq!(u["state"], "ok");
}

#[test]
fn get_usage_payload_values_match_python() {
    // 1 GiB cap, 100 MiB used → 100 MB used, 1024 MB cap, ~10 percent, ok.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let (mut t, _bus) = tracker(1.0, path, 0, 0);
    t.state.cumulative_bytes = 100 * 1024 * 1024;
    let u = t.get_usage();
    assert_eq!(u["data_used_mb"], 100);
    assert_eq!(u["cap_mb"], 1024);
    let pct = u["percent"].as_f64().unwrap();
    assert!((9.0..11.0).contains(&pct), "percent out of band: {pct}");
    assert_eq!(u["state"], "ok");
}

#[test]
fn set_cap_updates_cap_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let (mut t, _bus) = tracker(1.0, path, 0, 0);
    assert_eq!(t.cap_bytes, cap_to_bytes(1.0));
    t.set_cap(2.0);
    assert_eq!(t.cap_bytes, (2.0 * 1024.0 * 1024.0 * 1024.0) as u64);
}

#[test]
fn load_state_tolerates_missing_fields() {
    // An empty JSON object loads as a fresh window: zeros + empty month,
    // never a parse error. Mirrors `_UsageState.from_json({})`.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    std::fs::write(&path, b"{}").unwrap();
    let st = load_state(&path);
    assert_eq!(st.cumulative_bytes, 0);
    assert_eq!(st.last_rx, 0);
    assert_eq!(st.last_tx, 0);
    assert_eq!(st.last_reset_month, "");
}

#[test]
fn load_state_round_trips_all_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let written = UsageState {
        window_started_at: 1_700_000_000.0,
        cumulative_bytes: 12345,
        last_rx: 100,
        last_tx: 200,
        last_iface: Some("usb0".to_string()),
        last_reset_month: "2026-04".to_string(),
    };
    std::fs::write(&path, written.render_json()).unwrap();
    let st = load_state(&path);
    assert_eq!(st.window_started_at, 1_700_000_000.0);
    assert_eq!(st.cumulative_bytes, 12345);
    assert_eq!(st.last_rx, 100);
    assert_eq!(st.last_tx, 200);
    assert_eq!(st.last_reset_month, "2026-04");
    assert_eq!(st.last_iface.as_deref(), Some("usb0"));
}

#[tokio::test]
async fn two_polls_accumulate_against_the_previous_sample() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    // First poll counts from the zero wwan0 baseline → +1500.
    let bus = Arc::new(UplinkEventBus::new());
    let mut t = baselined(fixed(1000, 500), &bus, 1.0, path);
    t.poll_once().await;
    assert_eq!(t.state.cumulative_bytes, 1500);
    // Second poll: rx 1000→1700 (+700), tx 500→800 (+300) → +1000.
    t.source = fixed(1700, 800);
    t.poll_once().await;
    assert_eq!(t.state.cumulative_bytes, 2500);
}

#[tokio::test]
async fn poll_warn_80_emits_warn_threshold_event() {
    // A sample at 81 percent of the cap trips warn_80 on the first crossing.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let bus = Arc::new(UplinkEventBus::new());
    let mut rx = bus.subscribe();
    let cap = cap_to_bytes(1.0);
    let at_81 = ((cap as f64) * 0.81) as u64;
    let mut t = baselined(fixed(at_81, 0), &bus, 1.0, path);
    t.poll_once().await;
    let evt = rx.try_recv().expect("a warn_80 threshold event");
    assert_eq!(evt.kind, UplinkEventKind::DataCapThreshold);
    assert_eq!(evt.data_cap_state, Some(DataCapState::Warn80));
}

#[test]
fn flush_persists_bytes_accumulated_since_the_last_poll() {
    // A clean stop must persist the latest counter, not lose the bytes
    // observed mid-window. Mutate state without going through save_state,
    // then flush() must write them to disk.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let (mut t, _bus) = tracker(1.0, path.clone(), 0, 0);
    t.state.cumulative_bytes = 9_999_999;
    // The on-disk file does not yet reflect this (no save happened).
    assert!(!path.exists());
    t.flush();
    let persisted = load_state(&path);
    assert_eq!(persisted.cumulative_bytes, 9_999_999);
}

#[test]
fn flush_is_safe_before_any_poll() {
    // flush() must be callable even when poll_once was never run (a service
    // that crashes before its task is scheduled still flushes its counter).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let (mut t, _bus) = tracker(1.0, path.clone(), 0, 0);
    t.state.cumulative_bytes = 7;
    t.flush();
    assert_eq!(load_state(&path).cumulative_bytes, 7);
}

#[test]
fn flush_failure_is_swallowed_and_does_not_panic() {
    // A flush failure during shutdown must never propagate: the save logs
    // and returns. Force the failure by rooting the state file under a
    // PARENT that is a regular file, so create_dir_all errors and the
    // atomic write cannot proceed.
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"i am a file, not a directory").unwrap();
    // `blocker/sub/modem-usage.json` cannot be created: `blocker` is a file.
    let path = blocker.join("sub").join("modem-usage.json");
    let (mut t, _bus) = tracker(1.0, path.clone(), 0, 0);
    t.state.cumulative_bytes = 1234;
    // Must not panic; the error is logged inside save_state.
    t.flush();
    // And the unwritable target was indeed never created.
    assert!(!path.exists());
}

#[test]
fn current_month_is_year_dash_month() {
    let m = current_month();
    // YYYY-MM shape.
    assert_eq!(m.len(), 7);
    assert_eq!(&m[4..5], "-");
    assert!(m[0..4].chars().all(|c| c.is_ascii_digit()));
    assert!(m[5..7].chars().all(|c| c.is_ascii_digit()));
}

#[tokio::test]
async fn poll_emits_a_modem_usage_event_when_an_emitter_is_attached() {
    // Each poll ships one `net.modem_usage` event carrying the same usage
    // body the modem view serves, regardless of whether the threshold
    // changed. The emitter counts every enqueue, so the stats are the proof.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let bus = Arc::new(UplinkEventBus::new());
    let emitter = IngestEmitter::with_socket("ados-net", dir.path().join("ingest.sock"));
    let stats = emitter.stats();
    let mut t = baselined(fixed(1000, 500), &bus, 5.0, path).with_emitter(emitter);

    t.poll_once().await;
    assert_eq!(stats.enqueued(), 1);
    // A second poll (same state, no threshold transition) still emits the
    // fresh usage snapshot.
    t.poll_once().await;
    assert_eq!(stats.enqueued(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn sysfs_source_counts_only_wwan0_when_present() {
    // wwan0 present → count wwan0, never usb0 (even if usb0 also exists).
    let dir = tempfile::tempdir().unwrap();
    let net = dir.path().join("net");
    for (iface, driver, rx, tx) in [
        ("wwan0", "qmi_wwan", "100", "200"),
        ("usb0", "rndis_host", "9000", "9000"),
    ] {
        crate::managers::modem::rig_cellular(&net, iface, driver);
        let stats = net.join(iface).join("statistics");
        std::fs::create_dir_all(&stats).unwrap();
        std::fs::write(stats.join("rx_bytes"), rx).unwrap();
        std::fs::write(stats.join("tx_bytes"), tx).unwrap();
    }
    let src = SysfsUsageSource::with_roots(net, dir.path().join("no-gadget"));
    let u = src.data_usage().await.expect("wwan0 sample");
    assert_eq!(u.iface, "wwan0");
    assert_eq!(u.bytes.rx_bytes, 100);
    assert_eq!(u.bytes.tx_bytes, 200);
}

#[cfg(unix)]
#[tokio::test]
async fn sysfs_source_counts_usb0_as_modem_only_when_no_gadget() {
    // No wwan0, usb0 present, no provisioned gadget → usb0 IS the modem
    // RNDIS iface, so its counters are the cellular usage.
    let dir = tempfile::tempdir().unwrap();
    let net = dir.path().join("net");
    crate::managers::modem::rig_cellular(&net, "usb0", "rndis_host");
    let stats = net.join("usb0").join("statistics");
    std::fs::create_dir_all(&stats).unwrap();
    std::fs::write(stats.join("rx_bytes"), "300").unwrap();
    std::fs::write(stats.join("tx_bytes"), "400").unwrap();
    let src = SysfsUsageSource::with_roots(net, dir.path().join("no-gadget"));
    let u = src.data_usage().await.expect("usb0 sample");
    assert_eq!(u.iface, "usb0");
    assert_eq!(u.bytes.rx_bytes, 300);
    assert_eq!(u.bytes.tx_bytes, 400);
}

#[cfg(unix)]
#[tokio::test]
async fn usb0_only_board_with_no_modem_accrues_nothing_against_the_cap() {
    // The headline bug: usb0 is the USB-gadget tether NIC (a provisioned
    // gadget dir exists) and there is no modem. usb0 carries gigabytes of
    // a tethered laptop's traffic, but NONE of it is cellular, so the cap
    // tracker must see zero usage and never falsely throttle.
    let dir = tempfile::tempdir().unwrap();
    let net = dir.path().join("net");
    crate::managers::modem::rig_cellular(&net, "usb0", "rndis_host");
    let stats = net.join("usb0").join("statistics");
    std::fs::create_dir_all(&stats).unwrap();
    std::fs::write(stats.join("rx_bytes"), "5000000000").unwrap();
    std::fs::write(stats.join("tx_bytes"), "5000000000").unwrap();
    // A provisioned gadget marker → usb0 belongs to the tether, not a modem.
    let gadget = dir.path().join("gadget");
    std::fs::create_dir_all(&gadget).unwrap();
    let src = SysfsUsageSource::with_roots(net, gadget);
    assert_eq!(
        src.data_usage().await,
        None,
        "tether traffic must not count"
    );

    // And drive it through a poll: a 1 KiB cap stays OK because nothing
    // accrued, so the cap never crosses a throttle threshold.
    let bus = Arc::new(UplinkEventBus::new());
    let mut rx = bus.subscribe();
    let mut t = DataCapTracker::with_config(
        Arc::new(src),
        Arc::clone(&bus),
        1.0 / (1024.0 * 1024.0), // 1 KiB cap
        dir.path().join("modem-usage.json"),
    );
    t.poll_once().await;
    assert_eq!(t.state.cumulative_bytes, 0, "no cellular bytes accrued");
    assert_eq!(t.classify(), DataCapState::Ok);
    // The only state the cap ever reaches is `ok` (the first-poll baseline
    // transition), never a throttle/block, because nothing was counted.
    if let Ok(evt) = rx.try_recv() {
        assert_eq!(
            evt.data_cap_state,
            Some(DataCapState::Ok),
            "a no-modem box must never throttle"
        );
    }
    // A second poll at the same (still-zero) usage emits no further event.
    t.poll_once().await;
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn sysfs_source_with_no_ifaces_reads_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let src = SysfsUsageSource::with_roots(dir.path().join("net"), dir.path().join("gadget"));
    assert_eq!(src.data_usage().await, None);
}

#[cfg(unix)]
#[tokio::test]
async fn sysfs_source_with_an_unreadable_counter_reads_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let net = dir.path().join("net");
    crate::managers::modem::rig_cellular(&net, "wwan0", "qmi_wwan");
    let stats = net.join("wwan0").join("statistics");
    std::fs::create_dir_all(&stats).unwrap();
    std::fs::write(stats.join("rx_bytes"), "garbage").unwrap();
    std::fs::write(stats.join("tx_bytes"), "10").unwrap();
    let src = SysfsUsageSource::with_roots(net, dir.path().join("no-gadget"));
    assert_eq!(src.data_usage().await, None);
}

#[tokio::test]
async fn poll_without_an_emitter_enqueues_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modem-usage.json");
    let bus = Arc::new(UplinkEventBus::new());
    let probe = IngestEmitter::with_socket("ados-net", dir.path().join("probe.sock"));
    let stats = probe.stats();
    let mut t = baselined(fixed(1000, 500), &bus, 5.0, path);
    t.poll_once().await;
    assert_eq!(stats.enqueued(), 0);
}
