use super::*;
use std::fs;
use std::time::Duration;

/// Write a sidecar file with `body`. Staleness is driven by the injected
/// `now_us`, not by rewriting mtimes (which std cannot do without a new
/// dependency): a poll with a present `now` reads a just-written file as
/// fresh, and a poll with a `now` far in the future reads it as stale. The
/// `_age` argument keeps the call sites self-documenting about intent.
fn write_with_age(root: &Path, name: &str, body: &str, _age: Duration) {
    fs::write(root.join(name), body).unwrap();
}

fn events<'a>(frames: &'a [IngestFrame], kind: &str) -> Vec<&'a EventFrame> {
    frames
        .iter()
        .filter_map(|f| match f {
            IngestFrame::Event(e) if e.kind == kind => Some(e),
            _ => None,
        })
        .collect()
}

fn metric<'a>(frames: &'a [IngestFrame], key: &str) -> Option<&'a TelemetryFrame> {
    frames.iter().find_map(|f| match f {
        IngestFrame::Telemetry(t) if t.metric == key => Some(t),
        _ => None,
    })
}

/// A `now` value far enough in the future that any freshly-written file is
/// older than its staleness budget.
fn future_now() -> i64 {
    now_us_systemtime() + Duration::from_secs(3600).as_micros() as i64
}

#[test]
fn fresh_sidecars_sample_their_scalars_with_the_source_tag() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_with_age(
        root,
        "wfb-stats.json",
        r#"{"rssi": -44, "snr": 21, "fec_loss": 0.02, "bitrate_mbps": 8.5}"#,
        Duration::ZERO,
    );
    let mut state = TailerState::default();
    // A present `now` keeps the just-written files fresh.
    let frames = poll_once(root, &mut state, now_us_systemtime());

    assert_eq!(
        metric(&frames, "link.rssi.dbm").map(|m| m.value),
        Some(-44.0)
    );
    assert_eq!(metric(&frames, "link.snr.db").map(|m| m.value), Some(21.0));
    assert_eq!(
        metric(&frames, "link.fec.loss_rate").map(|m| m.value),
        Some(0.02)
    );
    // The source tag distinguishes radio from video metrics.
    assert_eq!(
        metric(&frames, "link.rssi.dbm")
            .and_then(|m| m.tags.get("source"))
            .and_then(|v| v.as_str()),
        Some("wfb")
    );
    // Nothing was stale or dropped on a fresh poll.
    assert!(events(&frames, "sidecar.stale").is_empty());
    assert!(events(&frames, "sidecar.drop").is_empty());
}

#[test]
fn lookup_number_coerces_bools_and_rejects_non_numbers() {
    // Bools sample as 0/1 so a spec can declare a boolean field as a scalar.
    // No surviving spec declares one, which is why this asserts the rule on
    // the helper rather than through a fixture: it has to stay true for the
    // next spec that does.
    let json: Value = serde_json::from_str(
        r#"{"yes": true, "no": false, "num": 30.5, "text": "x", "nul": null,
                "nested": {"deep": 7}}"#,
    )
    .unwrap();
    assert_eq!(lookup_number(&json, "yes"), Some(1.0));
    assert_eq!(lookup_number(&json, "no"), Some(0.0));
    assert_eq!(lookup_number(&json, "num"), Some(30.5));
    assert_eq!(lookup_number(&json, "nested.deep"), Some(7.0));
    // A string, a null, and an absent path are all "no value", never zero.
    assert_eq!(lookup_number(&json, "text"), None);
    assert_eq!(lookup_number(&json, "nul"), None);
    assert_eq!(lookup_number(&json, "absent"), None);
}

#[test]
fn string_event_carries_its_fields_and_fires_once_per_transition() {
    // `lcd-latency.json` is the only spec left that carries a string event, so
    // this is the sole coverage of the transition machine: fire on first sight,
    // stay quiet while the value holds, fire again when it changes.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let body = |source: &str| {
        format!(
            r#"{{"latency_ms": 42.5, "latency_ewma_ms": 40.1, "samples": 7, "source": "{source}"}}"#
        )
    };
    write_with_age(root, "lcd-latency.json", &body("sei"), Duration::ZERO);
    let mut state = TailerState::default();

    // First fresh poll fires the state event carrying the field.
    let first = poll_once(root, &mut state, now_us_systemtime());
    let evts = events(&first, "video.latency_source");
    assert_eq!(evts.len(), 1);
    assert_eq!(
        evts[0].detail.get("source").and_then(|v| v.as_str()),
        Some("sei")
    );

    // A second poll of the same snapshot does NOT re-fire (steady state).
    let second = poll_once(root, &mut state, now_us_systemtime());
    assert!(events(&second, "video.latency_source").is_empty());

    // The value changes -> a new event fires.
    write_with_age(
        root,
        "lcd-latency.json",
        &body("unavailable"),
        Duration::ZERO,
    );
    let third = poll_once(root, &mut state, now_us_systemtime());
    let evts3 = events(&third, "video.latency_source");
    assert_eq!(evts3.len(), 1);
    assert_eq!(
        evts3[0].detail.get("source").and_then(|v| v.as_str()),
        Some("unavailable")
    );
}

#[test]
fn string_event_re_emits_on_the_keepalive_cadence_under_steady_state() {
    // The keep-alive path is driven by `now_us` vs the last emit time, which
    // is unit-testable directly without the mtime-vs-now tension `poll_once`
    // has (the staleness check is mtime-driven, the keep-alive is now-driven).
    // `lcd-latency.json` is the spec carrying the `video.latency_source` event.
    let spec = SPECS
        .iter()
        .find(|s| s.name == "lcd-latency.json")
        .expect("the lcd-latency sidecar spec is present");
    let evt = spec.string_event.as_ref().unwrap();
    let json: Value = serde_json::from_str(
        r#"{"latency_ms": 42.5, "latency_ewma_ms": 40.1, "samples": 7, "source": "sei"}"#,
    )
    .unwrap();
    let base = 1_000_000_000_i64;
    let keepalive_us = STRING_EVENT_KEEPALIVE.as_micros() as i64;

    // Same transition value, last emitted < keep-alive ago -> no re-emit.
    let mut out = Vec::new();
    let within = emit_string_event(
        &json,
        spec,
        evt,
        base + keepalive_us - 1,
        Some("sei"),
        Some(base),
        &mut out,
    );
    assert!(within.is_none(), "no re-emit inside the keep-alive window");
    assert!(events(&out, "video.latency_source").is_empty());

    // Same transition value, last emitted >= keep-alive ago -> re-emit, and
    // the returned timestamp is the new `now_us` so the next window restarts.
    let mut out = Vec::new();
    let due = emit_string_event(
        &json,
        spec,
        evt,
        base + keepalive_us,
        Some("sei"),
        Some(base),
        &mut out,
    );
    assert_eq!(due, Some(("sei".to_string(), base + keepalive_us)));
    let evts = events(&out, "video.latency_source");
    assert_eq!(evts.len(), 1, "the steady snapshot re-emits on keep-alive");
    assert_eq!(
        evts[0].detail.get("source").and_then(|v| v.as_str()),
        Some("sei")
    );

    // A never-emitted snapshot (no prior timestamp) always emits first time.
    let mut out = Vec::new();
    let first = emit_string_event(&json, spec, evt, base, None, None, &mut out);
    assert_eq!(first, Some(("sei".to_string(), base)));
    assert_eq!(events(&out, "video.latency_source").len(), 1);
}

#[test]
fn lcd_latency_samples_numerics_and_carries_source() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_with_age(
        root,
        "lcd-latency.json",
        r#"{"latency_ms": 42.5, "latency_ewma_ms": 40.1, "samples": 7, "source": "sei"}"#,
        Duration::ZERO,
    );
    let mut state = TailerState::default();
    let frames = poll_once(root, &mut state, now_us_systemtime());

    // Every scalar this spec declares must actually produce its metric. A
    // negative assertion alone cannot show that: deleting a ScalarMap makes
    // "no metric was recorded" pass MORE readily, so each mapping needs a
    // positive gate of its own.
    assert_eq!(
        metric(&frames, "video.latency.glass_ms").map(|m| m.value),
        Some(42.5)
    );
    assert_eq!(
        metric(&frames, "video.latency.ewma_ms").map(|m| m.value),
        Some(40.1)
    );
    assert_eq!(
        metric(&frames, "video.latency.samples").map(|m| m.value),
        Some(7.0)
    );
    // The source rides the latency-source event.
    let evts = events(&frames, "video.latency_source");
    assert_eq!(evts.len(), 1);
    assert_eq!(
        evts[0].detail.get("source").and_then(|v| v.as_str()),
        Some("sei")
    );
}

#[test]
fn a_null_scalar_reads_as_absent_not_as_zero() {
    // The generic rule for every scalar in every spec, asserted here on the
    // EWMA because it is the one nullable field a live producer actually
    // leaves null. Kept SEPARATE from the positive test above: folding the
    // two together (a null fixture plus an is_none assertion) is what let the
    // EWMA mapping lose its only positive gate — with the mapping deleted,
    // the is_none check passes and the whole module stays green.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_with_age(
        root,
        "lcd-latency.json",
        r#"{"latency_ms": 42.5, "latency_ewma_ms": null, "samples": 7, "source": "sei"}"#,
        Duration::ZERO,
    );
    let mut state = TailerState::default();
    let frames = poll_once(root, &mut state, now_us_systemtime());

    assert!(metric(&frames, "video.latency.ewma_ms").is_none());
    // Its siblings still record, so this is the null being skipped rather
    // than the whole snapshot failing to sample.
    assert_eq!(
        metric(&frames, "video.latency.glass_ms").map(|m| m.value),
        Some(42.5)
    );
}

#[test]
fn every_spec_window_is_its_registry_window() {
    for spec in SPECS {
        let row = paths::SIDECARS
            .iter()
            .find(|s| s.path.rsplit('/').next() == Some(spec.name))
            .unwrap_or_else(|| panic!("{} has no registry row", spec.name));
        assert_eq!(row.max_age(), Some(spec.staleness), "{}", spec.name);
    }
}

#[test]
fn a_stale_file_fires_one_event_per_episode_not_per_poll() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_with_age(root, "wfb-stats.json", r#"{"rssi": -44}"#, Duration::ZERO);
    let mut state = TailerState::default();
    let now = future_now(); // makes the file read as stale

    // First stale poll fires the event.
    let first = poll_once(root, &mut state, now);
    let s1 = events(&first, "sidecar.stale");
    assert_eq!(s1.len(), 1);
    assert_eq!(
        s1[0].detail.get("name").and_then(|v| v.as_str()),
        Some("wfb-stats.json")
    );
    assert!(s1[0].detail.get("age_s").and_then(|v| v.as_i64()).unwrap() >= 3600);
    // A stale file is not sampled.
    assert!(metric(&first, "link.rssi.dbm").is_none());

    // Second stale poll does NOT re-fire: one event per episode.
    let second = poll_once(root, &mut state, now);
    assert!(events(&second, "sidecar.stale").is_empty());
}

#[test]
fn a_file_recovering_from_stale_can_fire_a_fresh_episode_again() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_with_age(root, "wfb-stats.json", r#"{"rssi": -44}"#, Duration::ZERO);
    let mut state = TailerState::default();

    // Stale episode 1.
    let stale1 = poll_once(root, &mut state, future_now());
    assert_eq!(events(&stale1, "sidecar.stale").len(), 1);
    // Recover: a present `now` reads the file as fresh and resets the episode.
    let fresh = poll_once(root, &mut state, now_us_systemtime());
    assert!(events(&fresh, "sidecar.stale").is_empty());
    assert_eq!(
        metric(&fresh, "link.rssi.dbm").map(|m| m.value),
        Some(-44.0)
    );
    // Stale episode 2 fires a new event.
    let stale2 = poll_once(root, &mut state, future_now());
    assert_eq!(events(&stale2, "sidecar.stale").len(), 1);
}

#[test]
fn a_disappearing_file_fires_drop_only_after_having_been_present() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let path = root.join("wfb-stats.json");
    fs::write(&path, r#"{"rssi": -44}"#).unwrap();
    let mut state = TailerState::default();

    // First poll: present and fresh, no drop.
    let first = poll_once(root, &mut state, now_us_systemtime());
    assert!(events(&first, "sidecar.drop").is_empty());

    // Remove it: the next poll fires exactly one drop event.
    fs::remove_file(&path).unwrap();
    let second = poll_once(root, &mut state, now_us_systemtime());
    let drops = events(&second, "sidecar.drop");
    assert_eq!(drops.len(), 1);
    assert_eq!(
        drops[0].detail.get("name").and_then(|v| v.as_str()),
        Some("wfb-stats.json")
    );

    // A file that was never present does not fire a drop on first sight.
    // (peer-presence.json was absent on both polls and produced nothing.)
    assert!(events(&first, "sidecar.drop").is_empty());
    let third = poll_once(root, &mut state, now_us_systemtime());
    assert!(events(&third, "sidecar.drop").is_empty(), "no repeat drop");
}

#[test]
fn a_malformed_fresh_file_yields_no_metric_and_no_crash() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("wfb-stats.json"), b"{ not json").unwrap();
    let mut state = TailerState::default();
    let frames = poll_once(root, &mut state, now_us_systemtime());
    // No metric was sampled, and the file is tracked as present.
    assert!(metric(&frames, "link.rssi.dbm").is_none());
    assert!(state.entry("wfb-stats.json").present);
}

#[test]
fn an_empty_root_produces_no_frames() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = TailerState::default();
    let frames = poll_once(dir.path(), &mut state, now_us_systemtime());
    assert!(frames.is_empty(), "no sidecars, no frames");
}

#[tokio::test]
async fn run_sidecar_tailer_emits_a_metric_then_stops_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::write(root.join("wfb-stats.json"), r#"{"rssi": -50}"#).unwrap();

    let (tx, mut rx) = mpsc::channel::<IngestFrame>(64);
    let (stop, shutdown) = Shutdown::pair();
    let handle = tokio::spawn(run_sidecar_tailer(root, tx, shutdown));

    // The first poll fires immediately (interval tick yields at t=0), so a
    // metric lands within a couple of poll intervals.
    let mut saw = false;
    for _ in 0..30 {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(IngestFrame::Telemetry(t))) if t.metric == "link.rssi.dbm" => {
                assert_eq!(t.value, -50.0);
                saw = true;
                break;
            }
            Ok(Some(_)) => continue,
            _ => {}
        }
    }
    assert!(saw, "the tailer sampled the fresh sidecar");

    stop.fire();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("the tailer stops within the bound")
        .expect("the tailer task did not panic");
}
