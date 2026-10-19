use std::time::{Duration, Instant};

use super::tests::CapturingWriter;
use super::*;
use crate::config::MavlinkConfig;
use crate::connection::imu_fallback::{ImuStream, HIGHRES_TIMEOUT};
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

fn rate(table: &[(u32, f32)], id: u32) -> Option<f32> {
    let hits: Vec<f32> = table.iter().filter(|e| e.0 == id).map(|e| e.1).collect();
    assert!(hits.len() <= 1, "message {id} listed twice");
    hits.first().copied()
}

#[test]
fn base_table_outside_capture() {
    for (profile, capture) in [
        ("drone", false),
        ("ground-station", false),
        ("ground-station", true),
        ("workstation", true),
    ] {
        let t = stream_rates(profile, capture);
        assert_eq!(t, BASE_RATES, "{profile} capture={capture}");
        assert_eq!(rate(t, 105), None);
        assert_eq!(rate(t, 24), Some(2.0));
    }
}

#[test]
fn capture_table_for_visor_always_and_drone_while_capturing() {
    for (profile, capture) in [("visor", false), ("visor", true), ("drone", true)] {
        let t = stream_rates(profile, capture);
        assert_eq!(rate(t, 105), Some(200.0), "HIGHRES_IMU");
        assert_eq!(rate(t, 31), Some(100.0), "ATTITUDE_QUATERNION");
        assert_eq!(rate(t, 32), Some(50.0), "LOCAL_POSITION_NED");
        assert_eq!(
            rate(t, 24),
            Some(10.0),
            "GPS_RAW_INT raised, not duplicated"
        );
        assert_eq!(rate(t, 2), Some(1.0), "SYSTEM_TIME");
        assert_eq!(rate(t, 29), Some(10.0), "SCALED_PRESSURE");
        // Every base stream is still requested, at its base rate unless raised.
        for &(id, hz) in BASE_RATES {
            let got = rate(t, id).expect("base stream kept");
            assert!(got >= hz, "message {id}");
        }
    }
}

#[test]
fn highres_fallback_state_machine() {
    let t0 = Instant::now();
    let s = ImuStream::Idle;
    // Not requested: a stray HIGHRES_IMU and time change nothing.
    assert_eq!(s.highres_seen(), ImuStream::Idle);
    assert_eq!(s.poll(t0 + HIGHRES_TIMEOUT * 2), ImuStream::Idle);

    let waiting = s.requested(t0);
    assert_eq!(waiting, ImuStream::Waiting(t0));
    // A later request does not restart the clock.
    assert_eq!(waiting.requested(t0 + Duration::from_secs(3)), waiting);
    let just_before = t0 + HIGHRES_TIMEOUT - Duration::from_millis(1);
    assert_eq!(waiting.poll(just_before), waiting);
    assert!(!waiting.poll(just_before).substitutes_raw());

    let fallen = waiting.poll(t0 + HIGHRES_TIMEOUT);
    assert_eq!(fallen, ImuStream::RawFallback);
    assert!(fallen.substitutes_raw());
    // A late HIGHRES_IMU switches back for good.
    let highres = fallen.highres_seen();
    assert_eq!(highres, ImuStream::Highres);
    assert_eq!(highres.poll(t0 + HIGHRES_TIMEOUT * 10), ImuStream::Highres);
    // Answered inside the window: never falls back.
    let answered = waiting.highres_seen();
    assert_eq!(answered.poll(t0 + HIGHRES_TIMEOUT * 2), ImuStream::Highres);
}

/// A link of `profile` with an open transport and a capturing writer.
async fn link(
    profile: &str,
) -> (
    std::sync::Arc<FcConnection>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let state = std::sync::Arc::new(tokio::sync::Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(tokio::sync::Mutex::new(ParamCache::new(
        "/tmp/ados-test-params.json",
    )));
    let cfg = MavlinkConfig {
        profile: profile.into(),
        ..MavlinkConfig::default()
    };
    let conn = FcConnection::new(cfg, state, params);
    let (tx, rx) = std::sync::mpsc::channel();
    conn.connected.store(true, Ordering::Relaxed);
    *conn.writer.lock().await = Some(Box::pin(CapturingWriter(tx)));
    (conn, rx)
}

/// `(message id, interval µs)` of every SET_MESSAGE_INTERVAL written.
fn intervals(rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<(u32, f32)> {
    let mut buf: Vec<u8> = rx.try_iter().flatten().collect();
    crate::connection::framing::extract_frames(&mut buf)
        .iter()
        .filter_map(|f| match mavlink::parse_v2(f) {
            Ok((_, MavMessage::COMMAND_LONG(c))) => Some((c.param1 as u32, c.param2)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn unanswered_highres_request_switches_to_raw_imu_at_once() {
    let (conn, wrx) = link("visor").await;

    conn.tick_streams().await;
    let first = intervals(&wrx);
    assert!(
        first.contains(&(105, 5_000.0)),
        "HIGHRES_IMU at 200 Hz: {first:?}"
    );
    assert!(!first.iter().any(|e| e.0 == 27));
    assert!(matches!(
        conn.sensor_streams().imu_state(),
        ImuStream::Waiting(_)
    ));

    // Inside the window nothing is re-sent (the cadence is not due).
    conn.tick_streams().await;
    assert!(intervals(&wrx).is_empty());

    // Age the request past the timeout: the next tick re-requests at once
    // with RAW_IMU at 50 Hz in place of HIGHRES_IMU, and stops HIGHRES_IMU.
    let aged = Instant::now() - HIGHRES_TIMEOUT - Duration::from_millis(1);
    conn.sensor_streams().reset_link();
    conn.sensor_streams()
        .requested(true, stream_rates("visor", false), aged);
    conn.tick_streams().await;
    let second = intervals(&wrx);
    assert!(
        second.contains(&(27, 20_000.0)),
        "RAW_IMU at 50 Hz: {second:?}"
    );
    assert!(
        second.contains(&(105, -1.0)),
        "HIGHRES_IMU stopped: {second:?}"
    );
    assert!(!second.contains(&(105, 5_000.0)));
    assert_eq!(conn.sensor_streams().imu_state(), ImuStream::RawFallback);
    assert_eq!(
        serde_json::Value::Object(conn.sensor_streams().sensor_rates_hz()),
        serde_json::json!({"fc_raw_imu": 50.0, "fc_baro": 10.0})
    );

    // The FC starts answering HIGHRES_IMU: back to the HIGHRES request, and
    // RAW_IMU is stopped.
    conn.sensor_streams().highres_seen();
    conn.tick_streams().await;
    let third = intervals(&wrx);
    assert!(third.contains(&(105, 5_000.0)), "{third:?}");
    assert!(third.contains(&(27, -1.0)), "{third:?}");
}

#[tokio::test]
async fn capture_end_stops_every_capture_only_stream() {
    let (conn, wrx) = link("drone").await;
    conn.sensor_streams().set_capture_active(true);
    conn.tick_streams().await;
    assert!(intervals(&wrx).contains(&(105, 5_000.0)));
    assert_eq!(
        serde_json::Value::Object(conn.sensor_streams().sensor_rates_hz()),
        serde_json::json!({"fc_imu": 200.0, "fc_baro": 10.0})
    );

    conn.sensor_streams().set_capture_active(false);
    conn.tick_streams().await;
    let after = intervals(&wrx);
    let stopped: Vec<u32> = after.iter().filter(|e| e.1 == -1.0).map(|e| e.0).collect();
    assert_eq!(stopped, vec![105, 31, 32, 2, 29], "{after:?}");
    // GPS_RAW_INT stays, back at its base rate.
    assert!(after.contains(&(24, 500_000.0)), "{after:?}");
    assert!(conn.sensor_streams().sensor_rates_hz().is_empty());
    // Nothing more to stop on the next refresh.
    *conn.last_stream_req.lock().await = None;
    conn.tick_streams().await;
    let next = intervals(&wrx);
    assert!(!next.is_empty() && !next.iter().any(|e| e.1 == -1.0));
}
