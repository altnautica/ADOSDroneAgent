use super::*;
use crate::config::MavlinkConfig;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tokio::sync::Mutex;

/// A write half whose first `write_all` fails, standing in for a serial
/// port that drops writes while reads keep flowing (the failure mode that
/// used to latch the FC "disconnected" forever).
struct FailingWriter;

impl AsyncWrite for FailingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected write failure",
        )))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn test_connection() -> std::sync::Arc<FcConnection> {
    let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(Mutex::new(ParamCache::new("/tmp/ados-test-params.json")));
    FcConnection::new(MavlinkConfig::default(), state, params)
}

/// A write half that forwards everything written to it down a channel, so a
/// send cadence can be asserted on the frames it actually put on the wire.
/// A channel rather than a shared buffer keeps the sync `poll_write` free of
/// any lock.
struct CapturingWriter(std::sync::mpsc::Sender<Vec<u8>>);

impl AsyncWrite for CapturingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let _ = self.0.send(data.to_vec());
        Poll::Ready(Ok(data.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Run one `tick_streams` on an open link and return the MAVLink message ids
/// it wrote, in order.
async fn stream_request_message_ids(legacy_stream_request: bool) -> Vec<u32> {
    let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(Mutex::new(ParamCache::new("/tmp/ados-test-params.json")));
    let cfg = MavlinkConfig {
        legacy_stream_request,
        ..MavlinkConfig::default()
    };
    let conn = FcConnection::new(cfg, state, params);
    let (tx, rx) = std::sync::mpsc::channel();
    conn.connected.store(true, Ordering::Relaxed);
    *conn.writer.lock().await = Some(Box::pin(CapturingWriter(tx)));

    conn.tick_streams().await;

    // Drop the writer so the channel closes and the drain terminates.
    *conn.writer.lock().await = None;
    let mut buf: Vec<u8> = rx.into_iter().flatten().collect();
    crate::connection::framing::extract_frames(&mut buf)
        .iter()
        .filter_map(|frame| crate::aux_tee::mavlink_message_id(frame))
        .collect()
}

/// `COMMAND_LONG` (the `SET_MESSAGE_INTERVAL` carrier) and the legacy
/// `REQUEST_DATA_STREAM`.
const COMMAND_LONG_ID: u32 = 76;
const REQUEST_DATA_STREAM_ID: u32 = 66;

#[tokio::test]
async fn stream_refresh_omits_the_legacy_request_by_default() {
    let ids = stream_request_message_ids(false).await;
    assert_eq!(
        ids.iter().filter(|&&id| id == COMMAND_LONG_ID).count(),
        STREAM_RATES.len(),
        "every per-message interval request must still be sent"
    );
    assert!(
        !ids.contains(&REQUEST_DATA_STREAM_ID),
        "the legacy REQUEST_DATA_STREAM loop must be off by default: measured \
             ingest of 66.5 f/s against the 29 Hz asked for showed ArduPilot honoring \
             both paths, so sending both doubles the telemetry on a shared radio"
    );
}

#[tokio::test]
async fn stream_refresh_sends_the_legacy_request_when_the_flag_is_on() {
    let ids = stream_request_message_ids(true).await;
    assert_eq!(
        ids.iter().filter(|&&id| id == COMMAND_LONG_ID).count(),
        STREAM_RATES.len(),
        "the interval requests are unaffected by the flag"
    );
    assert_eq!(
        ids.iter()
            .filter(|&&id| id == REQUEST_DATA_STREAM_ID)
            .count(),
        STREAM_GROUPS.len(),
        "the rollback for a firmware that honors only REQUEST_DATA_STREAM must \
             send every legacy group"
    );
}

#[tokio::test]
async fn an_unidentified_ardupilot_is_asked_for_its_banner_at_once_then_every_retry() {
    let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
    {
        let mut s = state.lock().await;
        s.autopilot = MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA as i64;
        s.last_heartbeat = "2026-01-01T00:00:00Z".into();
    }
    let params = std::sync::Arc::new(Mutex::new(ParamCache::new("/tmp/ados-test-params.json")));
    let conn = FcConnection::new(MavlinkConfig::default(), state.clone(), params);
    let (tx, rx) = std::sync::mpsc::channel();
    conn.connected.store(true, Ordering::Relaxed);
    *conn.writer.lock().await = Some(Box::pin(CapturingWriter(tx)));

    conn.tick_banner_request().await;
    conn.tick_banner_request().await;
    // Rewind the last request past the retry: the next tick asks again.
    *conn.last_banner_req.lock().await = Some(Instant::now() - BANNER_RETRY);
    conn.tick_banner_request().await;
    // Identified: no more requests.
    state.lock().await.firmware = Some(ados_protocol::flight_modes::ArduPilotFirmware::Copter);
    *conn.last_banner_req.lock().await = None;
    conn.tick_banner_request().await;

    *conn.writer.lock().await = None;
    let mut buf: Vec<u8> = rx.into_iter().flatten().collect();
    let ids: Vec<u32> = crate::connection::framing::extract_frames(&mut buf)
        .iter()
        .filter_map(|frame| crate::aux_tee::mavlink_message_id(frame))
        .collect();
    assert_eq!(ids, vec![COMMAND_LONG_ID, COMMAND_LONG_ID]);
}

#[tokio::test]
async fn write_failure_does_not_latch_disconnected_and_signals_reconnect() {
    let conn = test_connection();

    // Simulate a live link with a writer that will fail on the next write.
    conn.connected.store(true, Ordering::Relaxed);
    *conn.writer.lock().await = Some(Box::pin(FailingWriter));

    conn.send_bytes(b"\xfd\x00").await;

    // The failing writer is dropped so the run loop reinstalls a fresh one.
    assert!(
        conn.writer.lock().await.is_none(),
        "writer must be cleared after a write failure"
    );

    // The write path must NOT declare the FC permanently disconnected; the
    // run loop owns the transport-open flag and clears it only on a real
    // teardown.
    assert!(
        conn.transport_open(),
        "send_bytes must not latch the transport closed on a transient write error"
    );

    // The reconnect signal must have been raised, tagged with the link
    // generation the write failed on, so run() rebuilds that link.
    let signalled = tokio::time::timeout(
        Duration::from_millis(100),
        conn.reconnect_requested(conn.link_gen()),
    )
    .await
    .is_ok();
    assert!(signalled, "a write failure must signal a reconnect");
}

/// A write failure recorded against a link that is already gone must not
/// tear down its replacement: the run loop waits on the CURRENT
/// generation only.
#[tokio::test(start_paused = true)]
async fn a_stale_generation_failure_does_not_tear_down_the_new_link() {
    let conn = test_connection();
    conn.connected.store(true, Ordering::Relaxed);
    *conn.writer.lock().await = Some(Box::pin(FailingWriter));
    let old = conn.link_gen();
    conn.send_bytes(b"\xfd\x00").await;
    // The run loop re-opens: a fresh writer under a new generation.
    let fresh = conn.next_link_gen();
    assert_ne!(old, fresh);
    let torn_down = tokio::time::timeout(
        super::super::RECONNECT_INTERVAL * 2,
        conn.reconnect_requested(fresh),
    )
    .await
    .is_ok();
    assert!(
        !torn_down,
        "the new link must survive the old link's write failure"
    );
    // A failure on the new link still signals.
    conn.request_reconnect(fresh);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), conn.reconnect_requested(fresh))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn send_bytes_is_a_noop_when_no_writer() {
    let conn = test_connection();
    // No writer installed: send_bytes does nothing and raises no reconnect.
    conn.send_bytes(b"\xfd\x00").await;
    let signalled = tokio::time::timeout(
        Duration::from_millis(50),
        conn.reconnect_requested(conn.link_gen()),
    )
    .await
    .is_ok();
    assert!(
        !signalled,
        "no writer means nothing to fail and no reconnect to raise"
    );
}

/// Backdate `param_last_request` past `PARAM_RATE_LIMIT` without mocking
/// the clock (this crate's tokio features don't include `test-util`) —
/// `Instant::elapsed()` reads real wall-clock time regardless, so a
/// stored `Instant` in the past makes the rate-limit check `due` exactly
/// as if real time had elapsed.
fn long_ago() -> Instant {
    Instant::now()
        .checked_sub(PARAM_RATE_LIMIT + Duration::from_secs(1))
        .expect("PARAM_RATE_LIMIT + 1s must not underflow Instant::now()")
}

#[tokio::test]
async fn sweep_defers_while_a_clients_own_download_is_making_progress() {
    let conn = test_connection();
    conn.connected.store(true, Ordering::Relaxed);
    conn.state.lock().await.param_count = 100;

    // A sweep already fired once, and PARAM_RATE_LIMIT has fully elapsed
    // since — a naive time-only gate would fire again here. But the cache
    // has grown since the last check, via a downstream client's own
    // PARAM_REQUEST_LIST landing real, ongoing progress.
    let backdated = long_ago();
    *conn.param_last_request.lock().await = Some(backdated);
    conn.param_priming.store(true, Ordering::Relaxed);
    for i in 0..40 {
        conn.params.lock().await.set(&format!("P{i}"), i as f64, 9);
    }

    conn.tick_param_sweep().await;

    assert_eq!(
        *conn.param_last_request.lock().await,
        Some(backdated),
        "a growing cache must not be interrupted by the sweep's own competing \
             PARAM_REQUEST_LIST — the FC tracks queued-parameter state per link, so a \
             second request would restart its enumeration from index 0"
    );
}

#[tokio::test]
async fn sweep_fires_again_once_genuinely_stalled() {
    let conn = test_connection();
    conn.connected.store(true, Ordering::Relaxed);
    conn.state.lock().await.param_count = 100;

    let backdated = long_ago();
    *conn.param_last_request.lock().await = Some(backdated);
    conn.param_priming.store(true, Ordering::Relaxed);
    // No cache growth at all since the last check — genuinely stalled.

    conn.tick_param_sweep().await;

    let after = *conn.param_last_request.lock().await;
    assert!(
        after.is_some_and(|t| t != backdated),
        "a genuinely stalled sweep must still retry once the rate limit elapses"
    );
}

// Exercises the spec-superseded legacy request on purpose; see `tick_streams`.
#[allow(deprecated)]
#[test]
fn request_data_stream_serializes_and_round_trips() {
    // The legacy stream request must be a real ardupilotmega variant and must
    // round-trip through the same v2 codec the send path uses.
    let msg = MavMessage::REQUEST_DATA_STREAM(REQUEST_DATA_STREAM_DATA {
        target_system: 1,
        target_component: 1,
        req_stream_id: 6, // POSITION
        req_message_rate: 5,
        start_stop: 1,
    });
    let bytes = mavlink::serialize_v2(
        MavHeader {
            system_id: 191,
            component_id: 1,
            sequence: 0,
        },
        &msg,
    )
    .expect("REQUEST_DATA_STREAM serializes");
    let (_h, parsed) = mavlink::parse_v2(&bytes).expect("round-trips");
    match parsed {
        MavMessage::REQUEST_DATA_STREAM(d) => {
            assert_eq!(d.req_stream_id, 6);
            assert_eq!(d.req_message_rate, 5);
            assert_eq!(d.start_stop, 1);
        }
        other => panic!("expected REQUEST_DATA_STREAM, got {other:?}"),
    }
}

fn command_long_bytes() -> Vec<u8> {
    let msg = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
        target_system: 1,
        target_component: 1,
        command: ados_protocol::mavlink::ardupilotmega::MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
        confirmation: 0,
        param1: 1.0,
        param2: 0.0,
        param3: 0.0,
        param4: 0.0,
        param5: 0.0,
        param6: 0.0,
        param7: 0.0,
    });
    mavlink::serialize_v2(
        MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 0,
        },
        &msg,
    )
    .unwrap()
}

fn param_request_list_bytes() -> Vec<u8> {
    let msg = MavMessage::PARAM_REQUEST_LIST(PARAM_REQUEST_LIST_DATA {
        target_system: 1,
        target_component: 1,
    });
    mavlink::serialize_v2(
        MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 0,
        },
        &msg,
    )
    .unwrap()
}

/// Spawns a real aux-uplink sender against a loopback listener, the same
/// setup `aux_uplink`'s own tests use, so `send_client_bytes`'s fallback
/// path is exercised end to end rather than mocked.
async fn test_uplink() -> (crate::aux_uplink::AuxUplinkSender, tokio::net::UdpSocket) {
    let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (crate::aux_uplink::spawn(port), listener)
}

/// A connection whose aux uplink is armed, with the origin gate set either
/// way. `MavlinkConfig` is the only input, so this is the same construction
/// the router performs from a config file.
fn conn_with_origin_gate(enforce: bool) -> std::sync::Arc<FcConnection> {
    let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
        "/tmp/ados-origin-gate-params.json",
    )));
    FcConnection::new(
        MavlinkConfig {
            aux_uplink_enforce_origin: enforce,
            ..MavlinkConfig::default()
        },
        state,
        params,
    )
}

/// Send one frame from `caller` into an armed aux uplink and report whether
/// it crossed, asserting the bytes are exact when it did.
async fn crosses_the_uplink(enforce: bool, caller: ClientOrigin) -> bool {
    let conn = conn_with_origin_gate(enforce);
    let (sender, listener) = test_uplink().await;
    conn.set_aux_uplink(sender).await;

    let frame = command_long_bytes();
    conn.send_client_bytes(&frame, caller, None).await;

    let mut buf = [0u8; 256];
    match tokio::time::timeout(Duration::from_millis(300), listener.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => {
            let (_, payload) = ados_protocol::aux_mux::decode(&buf[..n]).unwrap();
            assert_eq!(payload, frame, "whatever crosses must be the exact bytes");
            true
        }
        _ => false,
    }
}

/// A frame the aux uplink dropped on a full queue is not reported as sent.
#[tokio::test]
async fn a_full_uplink_queue_makes_send_client_bytes_return_false() {
    let conn = conn_with_origin_gate(false);
    let (sender, _rx) = crate::aux_uplink::AuxUplinkSender::detached(1);
    conn.set_aux_uplink(sender.clone()).await;
    let frame = command_long_bytes();
    assert!(
        conn.send_client_bytes(&frame, ClientOrigin::Trusted, None)
            .await
    );
    assert!(
        !conn
            .send_client_bytes(&frame, ClientOrigin::Trusted, None)
            .await,
        "a dropped frame must not be counted as injected"
    );
    assert_eq!(sender.queue_full_dropped(), 1);
}

/// A linked ground agent holds full authority over the node it relays, so
/// every frame a connected client sends reaches the uplink. This used to be
/// filtered by a message-id denylist that covered only this MAVLink lane
/// while the relay's HTTP lane carried the same vehicle commands ungated,
/// so it filtered one door and left the other open. Authority now belongs
/// to the relay's authentication boundary.
///
/// The amplifying case: with no local vehicle these bytes leave over the
/// radio to one that may be flying, so the reach is longer than the
/// local-controller case — it needs no proximity to the aircraft. That is
/// what makes the gate worth having at all, and what makes it a flag rather
/// than a default.
#[tokio::test]
async fn the_default_still_relays_an_anonymous_client_and_records_it() {
    // Unchanged shipped behaviour: these ports are advertised for
    // third-party ground stations, so with the flag off a refusal here would
    // break the workflow at the operator rather than at install time. The
    // provenance is the only thing the classification changed.
    assert!(
        crosses_the_uplink(false, ClientOrigin::Unauthenticated).await,
        "the shipped default must keep relaying an anonymous client"
    );
}

#[tokio::test]
async fn the_flag_drops_an_anonymous_client_before_the_radio() {
    // The whole point of the flag: a fleet whose clients all authenticate
    // can stop radiating an unidentified caller's commands to an airborne
    // vehicle.
    assert!(
        !crosses_the_uplink(true, ClientOrigin::Unauthenticated).await,
        "with the flag on an anonymous frame must not reach the radio"
    );
}

#[tokio::test]
async fn a_relayed_frame_crosses_under_both_settings() {
    // The exemption, pinned so a later sweep cannot quietly fold `Relayed`
    // in with `Unauthenticated`. `Relayed` is the cloud relay's own declared
    // forwarding on an IPC socket it had to be granted, and that relay runs
    // on this same ground-station profile: the frames it labels are the
    // production remote-command path for a drone this node is relaying.
    // Refusing them deletes remote piloting rather than closing a hole.
    for enforce in [false, true] {
        assert!(
            crosses_the_uplink(enforce, ClientOrigin::Relayed).await,
            "enforce={enforce}: a relayed frame must always cross"
        );
    }
}

#[tokio::test]
async fn a_trusted_frame_crosses_under_both_settings() {
    for enforce in [false, true] {
        assert!(
            crosses_the_uplink(enforce, ClientOrigin::Trusted).await,
            "enforce={enforce}: a locally produced frame must always cross"
        );
    }
}

#[test]
fn the_origins_are_distinguishable() {
    // If any two of these compare equal the recording is silently useless,
    // which is worse than not recording at all.
    assert_ne!(ClientOrigin::Trusted, ClientOrigin::Unauthenticated);
    assert_ne!(ClientOrigin::Trusted, ClientOrigin::Relayed);
    assert_ne!(ClientOrigin::Relayed, ClientOrigin::Unauthenticated);
}

/// The on-box socket's provenance was a constant, so a writer forwarding
/// broker traffic and a writer producing its own bytes recorded the same
/// thing. The first must not read as the second.
#[test]
fn a_forwarding_writer_is_not_read_as_a_local_producer() {
    use ados_protocol::ipc::IpcPeer;

    let local = IpcPeer {
        uid: Some(0),
        gid: Some(0),
        pid: Some(1234),
        off_box_source: false,
        injector: None,
    };
    assert_eq!(ClientOrigin::from_ipc_peer(&local), ClientOrigin::Trusted);

    let forwarder = IpcPeer {
        off_box_source: true,
        injector: None,
        ..local
    };
    assert_ne!(
        ClientOrigin::from_ipc_peer(&forwarder),
        ClientOrigin::Trusted,
        "bytes that reached the node over a broker must not be recorded as \
             produced on it"
    );
    assert_eq!(
        ClientOrigin::from_ipc_peer(&forwarder),
        ClientOrigin::Relayed
    );
}

/// The raw MSP byte lane called the plain FC writer, so it recorded no
/// provenance at all and could not have told a broker-sourced command from
/// a local one. It must also never reach the aux uplink: that lane frames
/// MAVLink, and MSP bytes put on it are not carried, they are corrupt.
#[tokio::test]
async fn msp_client_bytes_never_reach_the_aux_uplink() {
    let conn = test_connection();
    let (sender, listener) = test_uplink().await;
    conn.set_aux_uplink(sender).await;

    conn.send_client_raw(b"$M<\x00\x64\x64", ClientOrigin::Relayed, None)
        .await;

    let mut buf = [0u8; 256];
    assert!(
        tokio::time::timeout(Duration::from_millis(200), listener.recv_from(&mut buf))
            .await
            .is_err(),
        "MSP bytes must not be radiated on the MAVLink aux lane"
    );
}

#[tokio::test]
async fn a_command_frame_reaches_the_uplink() {
    let conn = test_connection();
    let (sender, listener) = test_uplink().await;
    conn.set_aux_uplink(sender).await;

    let frame = command_long_bytes();
    conn.send_client_bytes(&frame, ClientOrigin::Trusted, None)
        .await;

    let mut buf = [0u8; 256];
    let (n, _) = tokio::time::timeout(Duration::from_millis(300), listener.recv_from(&mut buf))
        .await
        .expect("a command frame must reach the aux uplink")
        .unwrap();
    let (_, payload) = ados_protocol::aux_mux::decode(&buf[..n]).unwrap();
    assert_eq!(payload, frame, "the exact bytes must reach the uplink");
}

#[tokio::test]
async fn a_param_frame_reaches_the_uplink() {
    let conn = test_connection();
    let (sender, listener) = test_uplink().await;
    conn.set_aux_uplink(sender).await;

    let frame = param_request_list_bytes();
    conn.send_client_bytes(&frame, ClientOrigin::Trusted, None)
        .await;

    let mut buf = [0u8; 256];
    let (n, _) = tokio::time::timeout(Duration::from_millis(300), listener.recv_from(&mut buf))
        .await
        .expect("a param frame must reach the aux uplink")
        .unwrap();
    let (_, payload) = ados_protocol::aux_mux::decode(&buf[..n]).unwrap();
    assert_eq!(payload, frame);
}

/// A raw TCP read is not frame-aligned, so one call can carry several
/// frames concatenated. Nothing splits them any more, so the buffer must
/// cross whole and byte-identical rather than being reassembled or dropped.
#[tokio::test]
async fn a_concatenated_buffer_crosses_whole() {
    let conn = test_connection();
    let (sender, listener) = test_uplink().await;
    conn.set_aux_uplink(sender).await;

    let mut concatenated = param_request_list_bytes();
    concatenated.extend_from_slice(&command_long_bytes());

    conn.send_client_bytes(&concatenated, ClientOrigin::Trusted, None)
        .await;

    let mut buf = [0u8; 256];
    let (n, _) = tokio::time::timeout(Duration::from_millis(300), listener.recv_from(&mut buf))
        .await
        .expect("the concatenated buffer must reach the aux uplink")
        .unwrap();
    let (_, payload) = ados_protocol::aux_mux::decode(&buf[..n]).unwrap();
    assert_eq!(
        payload, concatenated,
        "the buffer must cross whole, not be split or filtered"
    );
}
