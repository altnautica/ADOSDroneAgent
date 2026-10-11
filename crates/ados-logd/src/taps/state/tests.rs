use super::*;
use serde_json::json;
use std::io::Cursor;

/// Drive `process_stream` against an in-memory snapshot stream and collect
/// every frame it emits. A never-firing shutdown lets the stream run to EOF.
async fn run_against(lines: &str) -> Vec<IngestFrame> {
    run_against_bytes(lines.to_string().into_bytes()).await
}

/// Byte-level sibling of [`run_against`] for wire frames that are not valid
/// UTF-8 (v2 length-prefixed msgpack), so a test can feed the exact on-wire
/// bytes through the same process/emit path.
async fn run_against_bytes(bytes: Vec<u8>) -> Vec<IngestFrame> {
    let (tx, mut rx) = mpsc::channel::<IngestFrame>(256);
    let reader = BufReader::new(Cursor::new(bytes));
    // `Default` samples every snapshot, so these tests exercise the emit
    // mapping without the cadence in the way. The sampling itself is proved
    // separately by `metrics_are_sampled_but_transitions_never_are`.
    let mut prev = PrevState::default();
    let mut shutdown = Shutdown::never();
    process_stream(reader, &tx, &mut prev, &mut shutdown).await;
    drop(tx);
    let mut out = Vec::new();
    while let Some(f) = rx.recv().await {
        out.push(f);
    }
    out
}

fn metric<'a>(frames: &'a [IngestFrame], key: &str) -> Option<&'a TelemetryFrame> {
    frames.iter().find_map(|f| match f {
        IngestFrame::Telemetry(t) if t.metric == key => Some(t),
        _ => None,
    })
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

/// The snapshot shape the MAVLink router publishes. This test used to feed a
/// hand-written shape (`gps.fix`, `gps.sats`, top-level `groundspeed`, a
/// `link` block) that the producer never emits, so the tap looked right
/// while recording no GPS, altitude or speed at all on a real node.
#[tokio::test]
async fn snapshot_emits_the_documented_scalar_metrics() {
    let line = serde_json::to_string(&json!({
            "armed": false,
            "mode": "STABILIZE",
            "position": {"lat": 12.9716, "lon": 77.5946, "alt_msl": 920.5, "alt_rel": 30.2, "heading": 90.0},
            "velocity": {"vx": 1.0, "vy": 0.0, "vz": 0.0, "groundspeed": 5.5, "airspeed": 6.0, "climb": 0.1},
            "battery": {"voltage": 16.4, "current": 12.1, "remaining": 87, "temperature": null, "cell_voltages": []},
            "gps": {"fix_type": 3, "satellites": 14, "eph": 0.9, "epv": null},
            "attitude": {"roll": 0.01, "pitch": -0.02, "yaw": 1.57}
        }))
        .unwrap();
    let frames = run_against(&format!("{line}\n")).await;

    // Every mapped, present field becomes a metric with the dotted key.
    for (key, want) in [
        ("battery.voltage.v", 16.4),
        ("battery.current.a", 12.1),
        ("battery.remaining.pct", 87.0),
        ("gps.fix", 3.0),
        ("gps.sats", 14.0),
        ("gps.lat", 12.9716),
        ("gps.lon", 77.5946),
        ("altitude.msl_m", 920.5),
        ("altitude.rel_m", 30.2),
        ("groundspeed.ms", 5.5),
        ("airspeed.ms", 6.0),
        ("attitude.yaw", 1.57),
    ] {
        assert_eq!(metric(&frames, key).map(|m| m.value), Some(want), "{key}");
    }
    // The sample tag is set so the read edge can tell state-tap rows apart.
    assert_eq!(
        metric(&frames, "battery.voltage.v")
            .and_then(|m| m.tags.get("sample"))
            .and_then(|v| v.as_str()),
        Some("state_tick")
    );

    // The first snapshot reports a disarmed flag, which is a transition from
    // the unknown initial state, so a disarm event is emitted once.
    let arm = events(&frames, "state.arm");
    assert_eq!(arm.len(), 1);
    assert_eq!(
        arm[0].detail.get("reason").and_then(|v| v.as_str()),
        Some("disarm")
    );
    // The first mode is a transition from unknown, so one mode.change.
    let mode = events(&frames, "mode.change");
    assert_eq!(mode.len(), 1);
    assert_eq!(
        mode[0].detail.get("to").and_then(|v| v.as_str()),
        Some("STABILIZE")
    );
    assert!(!mode[0].detail.contains_key("from"));
}

#[tokio::test]
async fn arm_then_disarm_emits_exactly_two_transition_events_with_writer_reasons() {
    let armed = serde_json::to_string(&json!({"armed": true, "mode": "GUIDED"})).unwrap();
    let still_armed = serde_json::to_string(&json!({"armed": true, "mode": "GUIDED"})).unwrap();
    let disarmed = serde_json::to_string(&json!({"armed": false, "mode": "GUIDED"})).unwrap();
    let stream = format!("{armed}\n{still_armed}\n{disarmed}\n");
    let frames = run_against(&stream).await;

    // The unchanged middle snapshot emits no transition: one arm, one disarm.
    let arm = events(&frames, "state.arm");
    assert_eq!(arm.len(), 2, "one arm, one disarm");
    assert_eq!(
        arm[0].detail.get("reason").and_then(|v| v.as_str()),
        Some("arm")
    );
    assert_eq!(
        arm[1].detail.get("reason").and_then(|v| v.as_str()),
        Some("disarm")
    );
    // The mode never changed after the first snapshot, so exactly one
    // mode.change (the unknown -> GUIDED transition).
    assert_eq!(events(&frames, "mode.change").len(), 1);
}

#[tokio::test]
async fn mode_change_records_the_from_and_to_pair() {
    let a = serde_json::to_string(&json!({"armed": false, "mode": "STABILIZE"})).unwrap();
    let b = serde_json::to_string(&json!({"armed": false, "mode": "ALT_HOLD"})).unwrap();
    let frames = run_against(&format!("{a}\n{b}\n")).await;
    let mode = events(&frames, "mode.change");
    assert_eq!(mode.len(), 2);
    // Second mode change carries both ends.
    assert_eq!(
        mode[1].detail.get("from").and_then(|v| v.as_str()),
        Some("STABILIZE")
    );
    assert_eq!(
        mode[1].detail.get("to").and_then(|v| v.as_str()),
        Some("ALT_HOLD")
    );
}

#[tokio::test]
async fn absent_fields_emit_no_metric_and_malformed_lines_are_skipped() {
    // A snapshot with only battery voltage present plus a malformed line.
    let ok = serde_json::to_string(&json!({"battery": {"voltage": 15.0}})).unwrap();
    let stream = format!("{ok}\n{{ not json }}\n\n");
    let frames = run_against(&stream).await;
    assert_eq!(
        metric(&frames, "battery.voltage.v").map(|m| m.value),
        Some(15.0)
    );
    // No GPS metrics from a snapshot that has no gps block.
    assert!(metric(&frames, "gps.fix").is_none());
    // The malformed and empty lines produced no events.
    assert!(events(&frames, "state.arm").is_empty());
}

#[tokio::test]
async fn v2_msgpack_frames_drive_the_same_transitions_as_v1() {
    use ados_protocol::state::{encode_v1, encode_v2};

    // The same snapshot on either wire form must drive the identical arm
    // event, mode.change, and voltage metric through the process/emit path.
    // Since the installer already ships v2 msgpack, a v1-only reader would
    // silently emit nothing here; this asserts v2 is now read too.
    let snapshot = json!({"armed": true, "mode": "GUIDED", "battery": {"voltage": 16.0}});
    let via_v1 = run_against_bytes(encode_v1(&snapshot).unwrap()).await;
    let via_v2 = run_against_bytes(encode_v2(&snapshot).unwrap()).await;

    for frames in [&via_v1, &via_v2] {
        let arm = events(frames, "state.arm");
        assert_eq!(arm.len(), 1, "one arm transition from the unknown state");
        assert_eq!(
            arm[0].detail.get("reason").and_then(|v| v.as_str()),
            Some("arm")
        );
        let mode = events(frames, "mode.change");
        assert_eq!(mode.len(), 1);
        assert_eq!(
            mode[0].detail.get("to").and_then(|v| v.as_str()),
            Some("GUIDED")
        );
        assert_eq!(
            metric(frames, "battery.voltage.v").map(|m| m.value),
            Some(16.0)
        );
    }
}

#[tokio::test]
async fn process_stream_stops_when_the_writer_channel_closes() {
    // Drop the receiver before processing: the first send fails and the
    // stream processor returns rather than spinning.
    let (tx, rx) = mpsc::channel::<IngestFrame>(1);
    drop(rx);
    let line = serde_json::to_string(&json!({"armed": true})).unwrap();
    let reader = BufReader::new(Cursor::new(format!("{line}\n").into_bytes()));
    let mut prev = PrevState::default();
    let mut shutdown = Shutdown::never();
    // Returns promptly; if it looped forever the test would hang.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        process_stream(reader, &tx, &mut prev, &mut shutdown),
    )
    .await
    .expect("process_stream returns when the channel is closed");
}

#[tokio::test]
async fn metrics_are_sampled_but_transitions_never_are() {
    // The state hub publishes at ~10 Hz and each snapshot carries ~17
    // numeric fields. Recording all of them put ~15 million rows a day on
    // flash for a store whose own rollups are minute-grained. Metrics are
    // sampled; transitions are not, because a dropped arm or mode change is
    // a lost fact, not a lost sample.
    let (tx, mut rx) = mpsc::channel::<IngestFrame>(256);
    let mut prev = PrevState::new(1.0);

    // First snapshot: armed, in GUIDED, carrying a metric. All three land.
    let first = json!({"armed": true, "mode": "GUIDED", "battery": {"voltage": 12.4}});
    emit_snapshot(&first, &mut prev, &tx).await.unwrap();

    // Second snapshot, immediately after: the metric is not due, but the
    // disarm and the mode change are facts and must still be recorded.
    let second = json!({"armed": false, "mode": "LAND", "battery": {"voltage": 12.1}});
    emit_snapshot(&second, &mut prev, &tx).await.unwrap();

    drop(tx);
    let mut frames = Vec::new();
    while let Some(f) = rx.recv().await {
        frames.push(f);
    }

    let voltages: Vec<f64> = frames
        .iter()
        .filter_map(|f| match f {
            IngestFrame::Telemetry(t) if t.metric == "battery.voltage.v" => Some(t.value),
            _ => None,
        })
        .collect();
    assert_eq!(
        voltages,
        vec![12.4],
        "only the due sample is recorded, not every snapshot"
    );

    assert_eq!(
        events(&frames, "state.arm").len(),
        2,
        "both the arm and the disarm are recorded, sampling notwithstanding"
    );
    assert_eq!(
        events(&frames, "mode.change").len(),
        2,
        "both mode changes are recorded, sampling notwithstanding"
    );
}

#[test]
fn a_due_sample_advances_the_schedule_and_a_bad_rate_falls_back() {
    let mut prev = PrevState::new(1.0);
    let t0 = Instant::now();
    assert!(prev.metrics_due(t0), "the first sample is always due");
    assert!(
        !prev.metrics_due(t0 + Duration::from_millis(500)),
        "within the interval, not due"
    );
    assert!(
        prev.metrics_due(t0 + Duration::from_millis(1_100)),
        "past the interval, due again"
    );

    // A rate of zero would mean "never sample", which is never what was
    // meant, so it falls back rather than silently disabling the tap.
    for bad in [0.0, -5.0, f64::NAN] {
        assert_eq!(
            sample_interval(bad),
            Duration::from_secs_f64(1.0 / DEFAULT_SAMPLE_HZ),
            "a bad rate falls back to the default"
        );
    }
    assert_eq!(sample_interval(2.0), Duration::from_secs_f64(0.5));
}

#[tokio::test]
async fn lookup_number_walks_dotted_paths_and_coerces_bool() {
    let v = json!({"a": {"b": 3}, "flag": true, "text": "x"});
    assert_eq!(lookup_number(&v, "a.b"), Some(3.0));
    assert_eq!(lookup_number(&v, "flag"), Some(1.0));
    assert_eq!(lookup_number(&v, "text"), None);
    assert_eq!(lookup_number(&v, "a.missing"), None);
}

#[tokio::test]
async fn run_state_tap_against_an_absent_socket_retries_then_stops_on_shutdown() {
    // No socket exists at this path; the tap must back off and retry without
    // crashing, then stop promptly when shutdown fires.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sock");
    let (tx, _rx) = mpsc::channel::<IngestFrame>(8);
    let (stop, shutdown) = Shutdown::pair();
    let handle = tokio::spawn(run_state_tap(path, tx, DEFAULT_SAMPLE_HZ, shutdown));
    // Let it attempt a connect and enter the backoff wait.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    stop.fire();
    tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("state tap stops within the bound")
        .expect("state tap task did not panic");
}

#[tokio::test]
async fn run_state_tap_reads_a_live_socket_then_reconnects_on_eof() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sock");
    let listener = UnixListener::bind(&path).unwrap();

    let (tx, mut rx) = mpsc::channel::<IngestFrame>(64);
    let (stop, shutdown) = Shutdown::pair();
    let handle = tokio::spawn(run_state_tap(path.clone(), tx, DEFAULT_SAMPLE_HZ, shutdown));

    // First connection: send one snapshot, then close to force a reconnect.
    let (mut a, _addr) = listener.accept().await.unwrap();
    let line =
        serde_json::to_string(&json!({"armed": true, "battery": {"voltage": 16.0}})).unwrap();
    a.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    a.flush().await.unwrap();
    drop(a); // EOF -> the tap reconnects

    // The arm event and the voltage metric arrived from the first connection.
    let mut saw_arm = false;
    let mut saw_voltage = false;
    for _ in 0..20 {
        match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
            Ok(Some(IngestFrame::Event(e))) if e.kind == "state.arm" => saw_arm = true,
            Ok(Some(IngestFrame::Telemetry(t))) if t.metric == "battery.voltage.v" => {
                saw_voltage = true
            }
            Ok(Some(_)) => continue,
            _ => {}
        }
        if saw_arm && saw_voltage {
            break;
        }
    }
    assert!(saw_arm, "arm event from the first connection");
    assert!(saw_voltage, "voltage metric from the first connection");

    // The tap reconnects: a second accept succeeds within the bound.
    let second = tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
        .await
        .expect("the tap reconnects after EOF");
    assert!(second.is_ok());

    stop.fire();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}
