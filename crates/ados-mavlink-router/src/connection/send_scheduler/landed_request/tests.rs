use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ados_protocol::mavlink;
use ados_protocol::mavlink::ardupilotmega::{
    MavLandedState, MavMessage, MavVtolState, EXTENDED_SYS_STATE_DATA,
};

use super::super::tests::CapturingWriter;
use super::*;
use crate::config::MavlinkConfig;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

#[test]
fn requests_once_per_link_up_after_the_grace() {
    let t0 = Instant::now();
    let mut r = LandedStateRequest::default();
    assert!(!r.due(false, false, t0), "no link");
    assert!(!r.due(true, false, t0), "link-up starts the grace");
    assert!(!r.due(
        true,
        false,
        t0 + LANDED_STATE_GRACE - Duration::from_millis(1)
    ));
    assert!(
        r.due(true, false, t0 + LANDED_STATE_GRACE),
        "absent after the grace"
    );
    assert!(!r.due(true, false, t0 + LANDED_STATE_GRACE * 4), "once");
    // A link drop and a new link-up decide again.
    assert!(!r.due(false, false, t0 + LANDED_STATE_GRACE * 5));
    let t1 = t0 + LANDED_STATE_GRACE * 6;
    assert!(!r.due(true, false, t1));
    assert!(r.due(true, false, t1 + LANDED_STATE_GRACE));
}

#[test]
fn never_requests_while_it_arrives() {
    let t0 = Instant::now();
    let mut r = LandedStateRequest::default();
    assert!(!r.due(true, true, t0));
    // Settled for this link-up even if it later stops.
    assert!(!r.due(true, false, t0 + LANDED_STATE_GRACE * 2));
}

/// A linked FC of `profile` whose link-up the request state saw 10 s ago.
async fn linked(
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
    *conn.last_heartbeat_at.lock().await = Some(Instant::now());
    conn.landed_state_req
        .lock()
        .unwrap()
        .linked_since(Instant::now() - Duration::from_secs(10));
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
async fn link_up_without_the_message_requests_it_at_1_hz_once() {
    let (conn, rx) = linked("ground-station").await;
    conn.tick_landed_state_request().await;
    assert_eq!(intervals(&rx), vec![(245, 1_000_000.0)]);
    conn.tick_landed_state_request().await;
    assert!(intervals(&rx).is_empty(), "sent once per link-up");
}

#[tokio::test]
async fn an_arriving_message_is_not_requested() {
    let (conn, rx) = linked("ground-station").await;
    let msg = MavMessage::EXTENDED_SYS_STATE(EXTENDED_SYS_STATE_DATA {
        vtol_state: MavVtolState::MAV_VTOL_STATE_UNDEFINED,
        landed_state: MavLandedState::MAV_LANDED_STATE_ON_GROUND,
    });
    conn.state
        .lock()
        .await
        .update_from_message(&msg, "2026-01-01T00:00:00Z");
    conn.tick_landed_state_request().await;
    assert!(intervals(&rx).is_empty());
}

#[tokio::test]
async fn a_stream_table_that_carries_it_owns_the_rate() {
    let (conn, rx) = linked("drone").await;
    conn.tick_landed_state_request().await;
    assert!(intervals(&rx).is_empty());
}
