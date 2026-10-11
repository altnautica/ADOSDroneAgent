use super::*;
use crate::config::MavlinkConfig;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;
use ados_protocol::mavlink::ardupilotmega::{
    MavAutopilot, MavMessage, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
};
use ados_protocol::mavlink::{self, MavHeader};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tokio::sync::Mutex;

/// This drone's own device id, as `/etc/ados/device-id` would carry it.
const OWN_ID: &str = "0a1b2c3d4e5f";

fn heartbeat_bytes() -> Vec<u8> {
    let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_GCS,
        autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_ACTIVE,
        mavlink_version: 3,
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

/// Captures every write, standing in for a real FC serial port so a test
/// can assert exactly what reached "the flight controller".
struct CapturingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl AsyncWrite for CapturingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(data);
        Poll::Ready(Ok(data.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn test_connection() -> (Arc<FcConnection>, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
        "/tmp/ados-uplink-test-params.json",
    )));
    let fc = FcConnection::new(MavlinkConfig::default(), state, params);
    (fc, std::sync::Arc::new(std::sync::Mutex::new(Vec::new())))
}

fn dedupe() -> Arc<RequestDedupe> {
    Arc::new(RequestDedupe::new())
}

/// Never completes a write, standing in for a wedged or flow-controlled FC.
struct StallingWriter;
impl AsyncWrite for StallingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn a_decoded_uplink_frame_is_injected_into_the_fc_writer() {
    let (fc, captured) = test_connection();
    fc.connected
        .store(true, std::sync::atomic::Ordering::Relaxed);
    *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

    let counters = AuxUplinkConsumerCounters::new();
    let frame = heartbeat_bytes();
    let datagram = aux_mux::encode(AuxChannel::Mavlink, &frame).unwrap();

    dispatch(
        &datagram,
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    assert_eq!(*captured.lock().unwrap(), frame);
    let snap = counters.snapshot();
    assert_eq!(snap.mavlink_frames, 1);
    assert_eq!(snap.mavlink_injected, 1);
    assert_eq!(snap.decode_foreign, 0);
    assert_eq!(snap.decode_damaged, 0);
}

#[tokio::test]
async fn foreign_traffic_on_the_shared_lane_is_counted_not_alarmed() {
    let (fc, captured) = test_connection();
    fc.connected
        .store(true, std::sync::atomic::Ordering::Relaxed);
    *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

    let counters = AuxUplinkConsumerCounters::new();
    dispatch(
        b"not-an-aux-frame-at-all",
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    assert!(
        captured.lock().unwrap().is_empty(),
        "foreign bytes must never reach the FC"
    );
    assert_eq!(counters.snapshot().decode_foreign, 1);
}

#[tokio::test]
async fn a_batched_datagram_splits_into_separate_injections() {
    let (fc, captured) = test_connection();
    fc.connected
        .store(true, std::sync::atomic::Ordering::Relaxed);
    *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

    let counters = AuxUplinkConsumerCounters::new();
    let one = heartbeat_bytes();
    let mut batch = one.clone();
    batch.extend_from_slice(&one);
    let datagram = aux_mux::encode(AuxChannel::Mavlink, &batch).unwrap();

    dispatch(
        &datagram,
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    assert_eq!(
        *captured.lock().unwrap(),
        batch,
        "both frames must reach the FC, in order"
    );
    assert_eq!(counters.snapshot().mavlink_frames, 2);
    assert_eq!(counters.snapshot().mavlink_injected, 2);
}

/// An ARM command from a ground station, addressed to `target_system`.
fn arm_command_bytes(target_system: u8) -> Vec<u8> {
    use ados_protocol::mavlink::ardupilotmega::{MavCmd, COMMAND_LONG_DATA};
    let msg = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
        target_system,
        target_component: 1,
        command: MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
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

/// Dispatch one MAVLink datagram carrying `frame` to a live FC whose
/// latched identity is `fc_identity`, returning what reached the FC and the
/// counters.
async fn dispatch_to_fc(
    frame: &[u8],
    fc_identity: Option<(u8, u8)>,
) -> (Vec<u8>, AuxUplinkConsumerSnapshot) {
    let (fc, captured) = test_connection();
    fc.connected
        .store(true, std::sync::atomic::Ordering::Relaxed);
    *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));
    if let Some((sys, comp)) = fc_identity {
        fc.latch_fc_identity(sys, comp);
    }
    let counters = AuxUplinkConsumerCounters::new();
    let datagram = aux_mux::encode(AuxChannel::Mavlink, frame).unwrap();
    dispatch(
        &datagram,
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;
    let written = captured.lock().unwrap().clone();
    (written, counters.snapshot())
}

#[tokio::test]
async fn a_command_for_another_aircraft_never_reaches_this_fc() {
    let (written, snap) = dispatch_to_fc(&arm_command_bytes(2), Some((1, 1))).await;
    assert!(written.is_empty(), "system 2's ARM must not reach system 1");
    assert_eq!(snap.mavlink_foreign_target_dropped, 1);
    assert_eq!(snap.mavlink_injected, 0);
}

#[tokio::test]
async fn a_command_for_this_fc_or_broadcast_is_delivered() {
    for target in [1u8, 0] {
        let frame = arm_command_bytes(target);
        let (written, snap) = dispatch_to_fc(&frame, Some((1, 1))).await;
        assert_eq!(written, frame, "target {target}");
        assert_eq!(snap.mavlink_injected, 1, "target {target}");
        assert_eq!(snap.mavlink_foreign_target_dropped, 0, "target {target}");
    }
}

#[tokio::test]
async fn an_addressed_command_is_held_until_the_fc_identity_is_known() {
    let (written, snap) = dispatch_to_fc(&arm_command_bytes(1), None).await;
    assert!(written.is_empty());
    assert_eq!(snap.mavlink_target_unknown_dropped, 1);

    // A frame with no target field is not addressed and still passes.
    let hb = heartbeat_bytes();
    let (written, snap) = dispatch_to_fc(&hb, None).await;
    assert_eq!(written, hb);
    assert_eq!(snap.mavlink_target_unknown_dropped, 0);
}

/// `ADOS_RUN_DIR` is process-global, so the sidecar tests serialise on it
/// the same way the radio crate's sidecar tests do.
static RUN_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn feedback_datagram(fb: &ados_protocol::link_feedback::LinkFeedback) -> Vec<u8> {
    aux_mux::encode(AuxChannel::LinkFeedback, &fb.encode()).unwrap()
}

/// Write a config naming this node's slot, so the addressee check has
/// something to compare against. Returns the dir to keep it alive.
fn config_with_slot(dir: &std::path::Path, slot: u8) {
    std::fs::write(
        dir.join("config.yaml"),
        format!("video:\n  wfb:\n    fleet_slot: {slot}\n"),
    )
    .unwrap();
}

fn measured_feedback() -> ados_protocol::link_feedback::LinkFeedback {
    ados_protocol::link_feedback::LinkFeedback {
        loss_percent: 24.29,
        rssi_dbm: -36.0,
        snr_db: 12.4,
        packets_received: 485,
        fec_failed: 25,
        bitrate_kbps: 2242,
        has_measurement: true,
        target_slot: 1,
    }
}

// The env guard must span the dispatch await: it serialises a
// process-global variable, so releasing it before the call would let a
// sibling test retarget ADOS_RUN_DIR mid-dispatch.
/// Point both the run dir and the config at a temp tree, and name this
/// node's slot so the addressee check has something to compare against.
fn stage(slot: u8) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    config_with_slot(&dir, slot);
    std::env::set_var("ADOS_RUN_DIR", &dir);
    std::env::set_var("ADOS_CONFIG_YAML", dir.join("config.yaml"));
    (tmp, dir)
}

fn unstage() {
    std::env::remove_var("ADOS_RUN_DIR");
    std::env::remove_var("ADOS_CONFIG_YAML");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn a_link_feedback_report_for_this_slot_is_published_for_the_ladder() {
    let _guard = RUN_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, dir) = stage(1);

    let (fc, _captured) = test_connection();
    let counters = AuxUplinkConsumerCounters::new();
    dispatch(
        &feedback_datagram(&measured_feedback()),
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.link_feedback_frames, 1);
    assert_eq!(snap.link_feedback_not_for_us, 0);
    assert_eq!(snap.link_feedback_write_errors, 0);
    let written = ados_protocol::link_feedback::read_sidecar_from(&dir.join("link-feedback.json"))
        .expect("the ladder's input must be on disk");
    assert!((written.loss_percent - 24.29).abs() < 0.01);
    assert_eq!(written.target_slot, 1);
    unstage();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn requests_beyond_the_inflight_ceiling_are_shed_not_queued() {
    // Each accepted request spawns a task that opens a local HTTP call and
    // buffers its response, so an unbounded spawn let the uplink's datagram
    // rate set the memory ceiling on a board with a few hundred megabytes.
    // Shedding is safe: the ground retransmits, and the dedupe cache
    // answers the retry from the first attempt's work.
    let _guard = RUN_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, _dir) = stage(1);

    // Hold every permit, as saturated in-flight work would.
    let mut held = Vec::new();
    for _ in 0..MAX_INFLIGHT_REQUESTS {
        held.push(REQUEST_SLOTS.clone().try_acquire_owned().expect("permit"));
    }

    let (fc, _captured) = test_connection();
    let counters = AuxUplinkConsumerCounters::new();
    let payload = ados_protocol::aux_rpc::encode_request(
        ados_protocol::aux_rpc::RpcMethod::Get,
        9,
        OWN_ID.as_bytes(),
        &ados_protocol::aux_rpc::RequestParts {
            path: b"/api/status",
            ..Default::default()
        },
    )
    .unwrap()
    .remove(0);
    let datagram = aux_mux::encode(AuxChannel::Request, &payload).unwrap();
    dispatch(
        &datagram,
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    assert_eq!(
        counters.snapshot().rpc_requests_shed,
        1,
        "a request past the ceiling must be shed rather than spawned"
    );
    drop(held);
    unstage();
}

// Each in-flight request can hold a full response body, so the ceiling is
// what turns "bounded by the datagram rate" into a real number. Checked at
// compile time so raising it past what the board can hold is a build error.
const _: () = assert!(MAX_INFLIGHT_REQUESTS >= 1 && MAX_INFLIGHT_REQUESTS <= 16);

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn a_report_measuring_another_drone_is_refused() {
    // THE regression this whole change exists for. The ground transmits the
    // uplink ONCE for the whole fleet and measures only one slot, so an
    // unaddressed record handed every other drone the primary's loss — a
    // clean drone would shed video rate because a neighbour was at the edge
    // of the link. Worse than no sample, because a no-sample ladder holds.
    let _guard = RUN_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, dir) = stage(2);

    let (fc, _captured) = test_connection();
    let counters = AuxUplinkConsumerCounters::new();
    // The record measures slot 1; this node is slot 2.
    dispatch(
        &feedback_datagram(&measured_feedback()),
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.link_feedback_frames, 1, "it was received");
    assert_eq!(snap.link_feedback_not_for_us, 1, "and refused");
    assert!(
        ados_protocol::link_feedback::read_sidecar_from(&dir.join("link-feedback.json")).is_none(),
        "another drone's measurement must never reach this drone's ladder"
    );
    unstage();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn an_unprovisioned_node_accepts_no_report() {
    // With no slot in config a node cannot prove a record is about itself,
    // and guessing is exactly the failure the addressee prevents.
    let _guard = RUN_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    std::fs::write(
        dir.join("config.yaml"),
        "video:\n  wfb:\n    channel: 149\n",
    )
    .unwrap();
    std::env::set_var("ADOS_RUN_DIR", &dir);
    std::env::set_var("ADOS_CONFIG_YAML", dir.join("config.yaml"));

    let (fc, _captured) = test_connection();
    let counters = AuxUplinkConsumerCounters::new();
    dispatch(
        &feedback_datagram(&measured_feedback()),
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    assert_eq!(counters.snapshot().link_feedback_not_for_us, 1);
    assert!(
        ados_protocol::link_feedback::read_sidecar_from(&dir.join("link-feedback.json")).is_none()
    );
    unstage();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn a_damaged_link_feedback_report_is_counted_and_not_published() {
    let _guard = RUN_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    std::env::set_var("ADOS_RUN_DIR", &dir);

    let (fc, _captured) = test_connection();
    let counters = AuxUplinkConsumerCounters::new();
    // Truncated record: decoding it as zeros would hand the ladder an
    // artificially clean link and push the rate the wrong way.
    let truncated = aux_mux::encode(AuxChannel::LinkFeedback, &[1u8, 0, 0, 0]).unwrap();
    dispatch(
        &truncated,
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.link_feedback_frames, 1);
    assert_eq!(snap.link_feedback_undecodable, 1);
    assert!(
        ados_protocol::link_feedback::read_sidecar_from(&dir.join("link-feedback.json")).is_none(),
        "a damaged report must never reach the ladder"
    );

    std::env::remove_var("ADOS_RUN_DIR");
}

#[tokio::test]
async fn a_request_addressed_to_another_drone_is_dropped_unanswered() {
    let (fc, _captured) = test_connection();
    let counters = AuxUplinkConsumerCounters::new();
    let payload = ados_protocol::aux_rpc::encode_request(
        ados_protocol::aux_rpc::RpcMethod::Get,
        1,
        b"deadbeefcafe",
        &ados_protocol::aux_rpc::RequestParts {
            path: b"/api/pairing/info",
            ..Default::default()
        },
    )
    .unwrap()
    .remove(0);
    let datagram = aux_mux::encode(AuxChannel::Request, &payload).unwrap();

    dispatch(
        &datagram,
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.rpc_requests, 1);
    assert_eq!(
        snap.rpc_requests_not_for_us, 1,
        "answering another drone's request would hand the ground the wrong node's data"
    );
}

#[tokio::test]
async fn a_request_for_us_or_broadcast_is_accepted() {
    for target in [OWN_ID.as_bytes(), b""] {
        let (fc, _captured) = test_connection();
        let counters = AuxUplinkConsumerCounters::new();
        let payload = ados_protocol::aux_rpc::encode_request(
            ados_protocol::aux_rpc::RpcMethod::Get,
            1,
            target,
            &ados_protocol::aux_rpc::RequestParts {
                path: b"/api/pairing/info",
                ..Default::default()
            },
        )
        .unwrap()
        .remove(0);
        let datagram = aux_mux::encode(AuxChannel::Request, &payload).unwrap();

        dispatch(
            &datagram,
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        let snap = counters.snapshot();
        assert_eq!(snap.rpc_requests, 1);
        assert_eq!(snap.rpc_requests_not_for_us, 0, "target {target:?}");
    }
}

/// The regression every other target test misses: they all fix `OWN_ID`,
/// but a drone with no `/etc/ados/device-id` reports an empty own id and
/// used to drop every named request — 100% failure on an unprovisioned
/// node, while the startup log claimed the opposite.
#[tokio::test]
async fn a_named_request_is_accepted_when_our_own_id_is_unresolved() {
    let (fc, _captured) = test_connection();
    let counters = AuxUplinkConsumerCounters::new();
    let payload = ados_protocol::aux_rpc::encode_request(
        ados_protocol::aux_rpc::RpcMethod::Get,
        1,
        b"deadbeefcafe",
        &ados_protocol::aux_rpc::RequestParts {
            path: b"/api/pairing/info",
            ..Default::default()
        },
    )
    .unwrap()
    .remove(0);
    let datagram = aux_mux::encode(AuxChannel::Request, &payload).unwrap();

    dispatch(
        &datagram,
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: "".into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.rpc_requests, 1);
    assert_eq!(
        snap.rpc_requests_not_for_us, 0,
        "an unresolved local id cannot adjudicate a target, so it must fail open"
    );
}

/// The recv loop that feeds the relay-proxy Request lane must not park
/// behind a flight controller that has stopped accepting writes.
#[tokio::test(start_paused = true)]
async fn a_wedged_fc_write_is_bounded_and_counted() {
    let (fc, _captured) = test_connection();
    fc.connected
        .store(true, std::sync::atomic::Ordering::Relaxed);
    *fc.writer.lock().await = Some(Box::pin(StallingWriter));

    let counters = AuxUplinkConsumerCounters::new();
    let datagram = aux_mux::encode(AuxChannel::Mavlink, &heartbeat_bytes()).unwrap();

    dispatch(
        &datagram,
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.mavlink_write_timeouts, 1);
    assert_eq!(
        snap.mavlink_injected, 0,
        "a write that never completed is not an injection"
    );
}

/// The drone half of the config-over-radio bridge: a request from the ground
/// station is handed to the local config-tunnel service with its aux framing
/// intact, and never touches the flight controller.
#[tokio::test]
async fn a_config_tunnel_frame_is_forwarded_to_the_local_service_and_not_to_the_fc() {
    use ados_protocol::config_tunnel_ingest::ConfigTunnelIngest;

    let (fc, captured) = test_connection();
    fc.connected
        .store(true, std::sync::atomic::Ordering::Relaxed);
    *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

    // Stands in for the config-tunnel service's own loopback ingress.
    let service = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let tunnel = ConfigTunnelIngest::new(service.local_addr().unwrap().port());

    let counters = AuxUplinkConsumerCounters::new();
    let datagram = aux_mux::encode(AuxChannel::ConfigTunnel, b"a-tunnel-frame").unwrap();
    dispatch(
        &datagram,
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: Some(Arc::new(tunnel)),
        },
        &counters,
    )
    .await;

    let mut buf = [0u8; 256];
    let (n, _) = service.recv_from(&mut buf).await.unwrap();
    assert_eq!(
        &buf[..n],
        datagram.as_slice(),
        "framing must survive the hop"
    );
    assert!(
        captured.lock().unwrap().is_empty(),
        "a config frame must never reach the flight controller"
    );

    let snap = counters.snapshot();
    assert_eq!(snap.config_tunnel_frames, 1);
    assert_eq!(snap.config_tunnel_forwarded, 1);
    assert_eq!(snap.config_tunnel_undelivered, 0);
    assert_eq!(snap.mavlink_injected, 0);
}

/// The channel is opt-in, so most drones run no config-tunnel service. Such
/// a frame is counted and dropped rather than read as a fault — and it still
/// must not reach the FC.
#[tokio::test]
async fn a_config_tunnel_frame_with_no_local_service_is_counted_not_lost() {
    let (fc, captured) = test_connection();
    fc.connected
        .store(true, std::sync::atomic::Ordering::Relaxed);
    *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

    let counters = AuxUplinkConsumerCounters::new();
    let datagram = aux_mux::encode(AuxChannel::ConfigTunnel, b"a-tunnel-frame").unwrap();
    dispatch(
        &datagram,
        &UplinkDeps {
            fc,
            egress: None,
            own_device_id: OWN_ID.into(),
            dedupe: dedupe(),
            config_tunnel: None,
        },
        &counters,
    )
    .await;

    assert!(captured.lock().unwrap().is_empty());
    let snap = counters.snapshot();
    assert_eq!(snap.config_tunnel_frames, 1);
    assert_eq!(snap.config_tunnel_undelivered, 1);
    assert_eq!(
        snap.non_mavlink_channel, 0,
        "a known channel must not fall through to the unknown-channel tally"
    );
}

/// Ground → drone APPLICATION datagrams are plugin traffic, not flight
/// controller traffic.
///
/// Before they were routed they fell through to the unknown-channel tally
/// and were dropped, so an operator-side plugin could send to its on-board
/// counterpart and the counterpart never saw it, with nothing naming the
/// loss. Both application channels must now be recognised, counted, and —
/// the load-bearing half — must NEVER be written to the FC serial cable.
#[tokio::test]
async fn an_application_datagram_is_recognised_and_never_reaches_the_flight_controller() {
    for channel in [AuxChannel::AppStream, AuxChannel::AppCommand] {
        let (fc, captured) = test_connection();
        fc.connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

        let counters = AuxUplinkConsumerCounters::new();
        let datagram = aux_mux::encode(channel, b"plugin-payload").unwrap();
        dispatch(
            &datagram,
            &UplinkDeps {
                fc,
                // No radio command socket on this rig, so the republish
                // cannot complete — which is the case that must be COUNTED
                // rather than silently swallowed.
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        assert!(
            captured.lock().unwrap().is_empty(),
            "{channel:?} must never be written to the FC"
        );
        let snap = counters.snapshot();
        assert_eq!(snap.app_datagrams, 1, "{channel:?}");
        assert_eq!(snap.app_datagrams_undelivered, 1, "{channel:?}");
        assert_eq!(snap.mavlink_injected, 0, "{channel:?}");
        assert_eq!(
            snap.non_mavlink_channel, 0,
            "{channel:?} must not fall through to the unknown-channel tally"
        );
    }
}
