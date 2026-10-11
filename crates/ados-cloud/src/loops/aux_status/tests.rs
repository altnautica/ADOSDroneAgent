use super::*;
use serde_json::json;

fn sample_with(enrichment: Value) -> Sample {
    Sample {
        enrichment,
        camera: Some("ready".to_string()),
        video: Some("streaming".to_string()),
        video_legs: Some(1),
        board: (
            Some("rock-5c-lite".to_string()),
            Some("rk3588s2".to_string()),
            Some(3),
        ),
    }
}

#[test]
fn only_failed_or_crash_looping_units_count_as_failures() {
    // A stopped unit is not a fault, and a finished one-shot reads running
    // from the producer; only `error` is broken.
    let out = "\
ados-control.service loaded active running ADOS control
ados-macpin.service loaded active exited ADOS MAC pinning
ados-net.service loaded inactive dead ADOS uplink
ados-vision.service loaded failed failed ADOS vision
ados-video.service loaded activating auto-restart ADOS video
";
    let services = crate::loops::enrichment::parse_systemctl_units(out);
    let s = summarize_services(&services);
    assert_eq!(s.running, 2);
    assert_eq!(s.failed, 2);
    assert_eq!(s.other, 1);
    assert_eq!(
        s.failed_names,
        vec!["ados-vision".to_string(), "ados-video".to_string()]
    );
}

#[test]
fn a_projected_snapshot_carries_the_live_fields_and_fits() {
    let sample = sample_with(json!({
        "cpuPercent": 12.5,
        "memoryPercent": 48.0,
        "diskPercent": 31.25,
        "temperature": 52.0,
        "fcConnected": true,
        "mavlinkAlive": false,
        "fcVariant": "betaflight",
        "fcFirmware": "betaflight",
    }));
    let services = ServiceSummary {
        running: 14,
        failed: 1,
        other: 3,
        failed_names: vec!["ados-vision".to_string()],
    };
    let s = project("abcdef123456", 7, 3600, "1.2.3", &sample, Some(&services));

    assert_eq!(s.id, "abcdef123456");
    assert_eq!(s.sq, 7);
    assert_eq!(s.fc, Some(true));
    assert_eq!(s.fa, Some(false));
    assert_eq!(s.cp, Some(12.5));
    assert_eq!(s.sr, Some(14));
    assert_eq!(s.sf, Some(1));
    assert_eq!(s.bn.as_deref(), Some("rock-5c-lite"));
    assert_eq!(s.cs.as_deref(), Some("ready"));
    assert_eq!(s.vs.as_deref(), Some("streaming"));

    let (bytes, trimmed) = s.encode().expect("encodes");
    assert_eq!(trimmed, 0, "a real snapshot must not need trimming");
    assert!(bytes.len() < 400, "snapshot is {} bytes", bytes.len());
}

#[test]
fn a_failed_source_leaves_its_field_absent_not_zero() {
    // The honesty invariant: an empty enrichment (every source failed) must
    // produce a snapshot that says nothing, not one claiming 0% CPU and a
    // disconnected flight controller.
    let sample = Sample {
        enrichment: json!({}),
        camera: None,
        video: None,
        video_legs: None,
        board: (None, None, None),
    };
    let s = project("abcdef123456", 1, 10, "1.2.3", &sample, None);
    assert_eq!(s.cp, None);
    assert_eq!(s.fc, None);
    assert_eq!(s.fa, None);
    assert_eq!(s.sr, None);
    assert_eq!(s.bn, None);
    assert_eq!(s.cs, None);
    // The core is still present, so the frame remains useful as a liveness
    // and identity signal even when every sensor read failed.
    assert_eq!(s.id, "abcdef123456");
    assert_eq!(s.sq, 1);
}

#[test]
fn the_video_leg_count_is_read_and_staleness_gated() {
    // A ground station has no other way to learn this: the radio carries no
    // IP, so it cannot ask the drone how many cameras it serves. Two legs
    // must arrive as two, and a sidecar nobody is refreshing must read as
    // unknown rather than keep asserting its last count.
    let dir = tempfile::tempdir().unwrap();
    let vid = dir.path().join("video-streams.json");
    let now = 1_700_000_000.0;
    std::fs::write(
        &vid,
        json!({"updated_at_unix": now, "streams": [
            {"id": "main", "live": true},
            {"id": "thermal", "live": true},
        ]})
        .to_string(),
    )
    .unwrap();
    assert_eq!(read_video_leg_count(vid.to_str().unwrap(), now), Some(2));
    assert_eq!(
        read_video_leg_count(vid.to_str().unwrap(), now + 120.0),
        None
    );
    assert_eq!(
        read_video_leg_count("/nonexistent/video-streams.json", now),
        None
    );

    // A node serving nothing reports zero legs, which is a real answer and
    // must not be confused with the unreadable case above.
    std::fs::write(
        &vid,
        json!({"updated_at_unix": now, "streams": []}).to_string(),
    )
    .unwrap();
    assert_eq!(read_video_leg_count(vid.to_str().unwrap(), now), Some(0));
}

#[test]
fn camera_and_video_states_are_staleness_gated() {
    let dir = tempfile::tempdir().unwrap();
    let cam = dir.path().join("camera-state.json");
    let vid = dir.path().join("video-streams.json");
    let now = 1_700_000_000.0;

    std::fs::write(
        &cam,
        json!({"state": "ready", "updated_at_unix": now}).to_string(),
    )
    .unwrap();
    std::fs::write(
        &vid,
        json!({"updated_at_unix": now, "streams": [{"id": "main", "live": true}]}).to_string(),
    )
    .unwrap();
    assert_eq!(
        read_camera_state(cam.to_str().unwrap(), now),
        Some("ready".into())
    );
    assert_eq!(
        read_video_state(vid.to_str().unwrap(), now),
        Some("streaming".into())
    );

    // A lingering sidecar from a stopped pipeline must read as unknown, not
    // keep advertising its last value.
    let later = now + 120.0;
    assert_eq!(read_camera_state(cam.to_str().unwrap(), later), None);
    assert_eq!(read_video_state(vid.to_str().unwrap(), later), None);
}

/// The cockpit raises its no-camera overlay only for `missing`, so a
/// discovered camera whose pipeline failed used to reach the operator as a
/// confident `ready` pill above a dead pane, with no warning anywhere.
#[test]
fn a_discovered_camera_with_a_failed_pipeline_reports_error() {
    let dir = tempfile::tempdir().unwrap();
    let cam = dir.path().join("camera-state.json");
    let now = 1_700_000_000.0;
    let write = |state: &str, pipeline: &str| {
        std::fs::write(
            &cam,
            json!({"state": state, "pipeline_state": pipeline, "updated_at_unix": now}).to_string(),
        )
        .unwrap();
    };

    write("ready", "error");
    assert_eq!(
        read_camera_state(cam.to_str().unwrap(), now),
        Some("error".into())
    );

    write("ready", "streaming");
    assert_eq!(
        read_camera_state(cam.to_str().unwrap(), now),
        Some("ready".into())
    );

    // Discovery stays authoritative for `missing`.
    write("missing", "error");
    assert_eq!(
        read_camera_state(cam.to_str().unwrap(), now),
        Some("missing".into())
    );
}

#[test]
fn video_state_distinguishes_flat_legs_from_unreported_ones() {
    let dir = tempfile::tempdir().unwrap();
    let vid = dir.path().join("video-streams.json");
    let now = 1_700_000_000.0;
    let write = |streams: Value| {
        std::fs::write(
            &vid,
            json!({"updated_at_unix": now, "streams": streams}).to_string(),
        )
        .unwrap();
    };

    // Every reporting leg flat is a real degradation.
    write(json!([{"id": "main", "live": false}]));
    assert_eq!(
        read_video_state(vid.to_str().unwrap(), now),
        Some("degraded".into())
    );
    // No leg reported liveness at all: idle, which is not the same claim.
    write(json!([{"id": "main"}]));
    assert_eq!(
        read_video_state(vid.to_str().unwrap(), now),
        Some("idle".into())
    );
    // No legs at all: unknown, not a fabricated state.
    write(json!([]));
    assert_eq!(read_video_state(vid.to_str().unwrap(), now), None);
}

#[test]
fn a_missing_or_malformed_sidecar_reads_unknown() {
    assert_eq!(read_camera_state("/nonexistent/camera.json", 1.0), None);
    assert_eq!(read_video_state("/nonexistent/video.json", 1.0), None);
    assert_eq!(read_board("/nonexistent/board.json"), (None, None, None));
}

#[test]
fn an_operator_disabled_lane_stands_down_but_a_refusal_backs_off() {
    // A deliberate operator choice and a transient refusal must not be
    // treated alike: the first stands down quietly, the second is counted
    // and retried sooner.
    let mut counters = ProducerCounters::default();
    let mut disabled_until = None;
    let mut backoff = None;
    let now = Instant::now();

    record_send_error(
        &AuxEgressError::Refused("no radio".into()),
        &mut counters,
        &mut disabled_until,
        &mut backoff,
        now,
    );
    assert!(
        disabled_until.is_none(),
        "a transient refusal must not stand the producer down"
    );
    assert_eq!(counters.refused, 1);
    assert!(backoff.is_some(), "a refusal must back off");

    record_send_error(
        &AuxEgressError::Disabled,
        &mut counters,
        &mut disabled_until,
        &mut backoff,
        now,
    );
    assert!(
        disabled_until.is_some(),
        "an operator-disabled lane must stand the producer down"
    );
}

#[test]
fn a_disabled_lane_is_retested_rather_than_latched_off_for_the_process() {
    // The switch is reversible. Latching meant status and identity never
    // came back after the operator re-enabled the lane — on a box they may
    // only be able to reach through that same lane.
    let mut counters = ProducerCounters::default();
    let mut disabled_until = None;
    let mut backoff = None;
    let now = Instant::now();

    record_send_error(
        &AuxEgressError::Disabled,
        &mut counters,
        &mut disabled_until,
        &mut backoff,
        now,
    );
    let until = disabled_until.expect("stood down");
    assert!(until > now, "the stand-down must be in the future");
    assert!(
        until <= now + DISABLED_RECHECK,
        "the stand-down must expire, so a re-enabled lane recovers on its own"
    );
}

#[test]
fn a_lane_left_disabled_extends_its_stand_down_without_relatching_the_log() {
    // Each retest that still finds the lane disabled pushes the window out
    // again, so a permanently disabled lane costs one attempt a minute
    // rather than one per status tick.
    let mut counters = ProducerCounters::default();
    let mut disabled_until = None;
    let mut backoff = None;
    let first = Instant::now();

    record_send_error(
        &AuxEgressError::Disabled,
        &mut counters,
        &mut disabled_until,
        &mut backoff,
        first,
    );
    let after_first = disabled_until.expect("stood down");

    let later = first + DISABLED_RECHECK;
    record_send_error(
        &AuxEgressError::Disabled,
        &mut counters,
        &mut disabled_until,
        &mut backoff,
        later,
    );
    let after_second = disabled_until.expect("still stood down");
    assert!(
        after_second > after_first,
        "a lane still disabled at the retest must stand down again"
    );
}

#[test]
fn the_configured_cadence_can_never_become_a_busy_loop() {
    use crate::config::{WfbSection, AUX_MIN_INTERVAL_S};
    // A zero or negative interval in config must be floored, not obeyed: the
    // lane is shared with video and a spinning producer would flood it.
    let mut w = WfbSection {
        aux_status_interval_s: 0.0,
        aux_identity_interval_s: -5.0,
        ..Default::default()
    };
    assert_eq!(w.status_interval().as_secs_f64(), AUX_MIN_INTERVAL_S);
    assert_eq!(w.identity_interval().as_secs_f64(), AUX_MIN_INTERVAL_S);

    // A sane value is respected.
    w.aux_status_interval_s = 2.5;
    assert_eq!(w.status_interval().as_secs_f64(), 2.5);
}

#[test]
fn defaults_are_a_one_second_status_and_ten_second_identity() {
    let w = crate::config::WfbSection::default();
    assert!(w.aux_status_enabled);
    assert_eq!(w.status_interval(), Duration::from_secs(1));
    assert_eq!(w.identity_interval(), Duration::from_secs(10));
}
