//! Stream, banner and landed-state requests wait for the FC identity, go to
//! it, and are re-sent at once when it latches or changes.

use std::time::{Duration, Instant};

use ados_protocol::mavlink::ardupilotmega::{MavAutopilot, MavCmd, MavMessage};

use super::tests::CapturingWriter;
use super::*;
use crate::config::MavlinkConfig;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

/// An open link of `profile` whose FC has heartbeated (alive, an ArduPilot
/// without a banner yet) but whose identity is not latched.
async fn link(
    profile: &str,
    legacy_stream_request: bool,
) -> (
    std::sync::Arc<FcConnection>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let state = std::sync::Arc::new(tokio::sync::Mutex::new(VehicleState::default()));
    {
        let mut s = state.lock().await;
        s.autopilot = MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA as i64;
        s.last_heartbeat = "2026-01-01T00:00:00Z".into();
    }
    let params = std::sync::Arc::new(tokio::sync::Mutex::new(ParamCache::new(
        "/tmp/ados-test-params.json",
    )));
    let cfg = MavlinkConfig {
        profile: profile.into(),
        legacy_stream_request,
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
        .absent_since(Instant::now() - Duration::from_secs(10));
    (conn, rx)
}

/// One written request: (message id, target system, target component, and
/// the COMMAND_LONG command when it is one).
type Req = (u32, u8, u8, Option<MavCmd>);

// Reads the spec-superseded legacy request on purpose; see `tick_streams`.
#[allow(deprecated)]
fn written(rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<Req> {
    let mut buf: Vec<u8> = rx.try_iter().flatten().collect();
    crate::connection::framing::extract_frames(&mut buf)
        .iter()
        .filter_map(|f| match mavlink::parse_v2(f) {
            Ok((_, MavMessage::COMMAND_LONG(c))) => {
                Some((76, c.target_system, c.target_component, Some(c.command)))
            }
            Ok((_, MavMessage::REQUEST_DATA_STREAM(r))) => {
                Some((66, r.target_system, r.target_component, None))
            }
            _ => None,
        })
        .collect()
}

fn all_to(reqs: &[Req], fc: (u8, u8)) -> bool {
    !reqs.is_empty() && reqs.iter().all(|r| (r.1, r.2) == fc)
}

#[tokio::test]
async fn nothing_is_requested_before_the_identity_latches() {
    let (conn, rx) = link("ground-station", true).await;
    conn.tick_streams().await;
    conn.tick_banner_request().await;
    conn.tick_landed_state_request().await;
    assert!(written(&rx).is_empty());
}

#[tokio::test]
async fn an_fc_on_sysid_2_gets_the_streams_addressed_to_it_at_once() {
    let (conn, rx) = link("drone", true).await;
    conn.tick_streams().await;
    assert!(written(&rx).is_empty());
    // A recent (pre-latch) stream refresh must not hold the first request
    // back to the cadence.
    *conn.last_stream_req.lock().await = Some(Instant::now());
    conn.latch_fc_identity(2, 1);
    conn.tick_streams().await;
    let reqs = written(&rx);
    let intervals = reqs
        .iter()
        .filter(|r| r.3 == Some(MavCmd::MAV_CMD_SET_MESSAGE_INTERVAL))
        .count();
    assert_eq!(intervals, stream_rates("drone", false).len(), "{reqs:?}");
    assert_eq!(
        reqs.iter().filter(|r| r.0 == 66).count(),
        STREAM_GROUPS.len()
    );
    assert!(all_to(&reqs, (2, 1)), "{reqs:?}");
}

#[tokio::test]
async fn an_identity_change_re_sends_the_streams_to_the_new_fc() {
    let (conn, rx) = link("drone", false).await;
    conn.latch_fc_identity(1, 1);
    conn.tick_streams().await;
    assert!(all_to(&written(&rx), (1, 1)));
    // Inside the cadence, same FC: nothing.
    conn.tick_streams().await;
    assert!(written(&rx).is_empty());
    // Another FC identity: the whole table again, at once, to it.
    conn.latch_fc_identity(3, 1);
    conn.tick_streams().await;
    let reqs = written(&rx);
    assert_eq!(reqs.len(), stream_rates("drone", false).len(), "{reqs:?}");
    assert!(all_to(&reqs, (3, 1)), "{reqs:?}");
}

#[tokio::test]
async fn the_banner_request_goes_to_the_fc_and_follows_an_identity_change() {
    let (conn, rx) = link("ground-station", false).await;
    conn.latch_fc_identity(2, 1);
    conn.tick_banner_request().await;
    conn.tick_banner_request().await;
    let reqs = written(&rx);
    assert_eq!(reqs.len(), 1, "once inside the retry: {reqs:?}");
    assert_eq!(reqs[0], (76, 2, 1, Some(MavCmd::MAV_CMD_DO_SEND_BANNER)));
    conn.latch_fc_identity(4, 1);
    conn.tick_banner_request().await;
    assert_eq!(
        written(&rx),
        vec![(76, 4, 1, Some(MavCmd::MAV_CMD_DO_SEND_BANNER))]
    );
}

#[tokio::test]
async fn the_landed_request_goes_to_the_fc_and_follows_an_identity_change() {
    let (conn, rx) = link("ground-station", false).await;
    conn.latch_fc_identity(2, 1);
    conn.tick_landed_state_request().await;
    conn.tick_landed_state_request().await;
    let reqs = written(&rx);
    assert_eq!(reqs.len(), 1, "once inside the cadence: {reqs:?}");
    assert!(all_to(&reqs, (2, 1)), "{reqs:?}");
    conn.latch_fc_identity(5, 1);
    conn.tick_landed_state_request().await;
    let reqs = written(&rx);
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert!(all_to(&reqs, (5, 1)), "{reqs:?}");
}
