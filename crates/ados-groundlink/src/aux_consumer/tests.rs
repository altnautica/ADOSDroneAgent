use super::dispatch::{autopilot_heartbeat_system_id, mavlink_system_id};
use super::run::{set_recv_buffer, AUX_RECV_BUFFER_BYTES};
use super::*;

#[tokio::test]
async fn the_consumer_asks_for_a_larger_receive_buffer() {
    // The drone end of this same lane already asked for one and the ground
    // end did not, so the two ends disagreed about how much burst they
    // could absorb. A drop from a short buffer happens below the
    // application: `recv_errors` never sees it, so the loss is invisible on
    // both sides.
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let before = socket2::SockRef::from(&sock).recv_buffer_size().unwrap();
    set_recv_buffer(&sock);
    let after = socket2::SockRef::from(&sock).recv_buffer_size().unwrap();
    assert!(
        after >= before,
        "the request shrank the buffer: {before} -> {after}"
    );
}

#[test]
fn the_two_ends_of_the_lane_ask_for_the_same_buffer() {
    // Stated as a number rather than a comment, so a change on one side
    // that forgets the other is visible here.
    assert_eq!(AUX_RECV_BUFFER_BYTES, 4 * 1024 * 1024);
}
use ados_protocol::frame::HEADER_SIZE;
use std::path::PathBuf;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use tokio::sync::mpsc;

/// A stand-in for the MAVLink router's republish seam: accepts one
/// connection and decodes the length-prefixed frames off it onto a channel,
/// exactly as the real seam's reader does. Frames arriving here have
/// reached the republish plane.
fn fake_seam(path: PathBuf) -> (mpsc::Receiver<Vec<u8>>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(32);
    let handle = tokio::spawn(async move {
        let listener = UnixListener::bind(&path).unwrap();
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        loop {
            let mut header = [0u8; HEADER_SIZE];
            if stream.read_exact(&mut header).await.is_err() {
                return;
            }
            let len = u32::from_be_bytes(header) as usize;
            let mut payload = vec![0u8; len];
            if len > 0 && stream.read_exact(&mut payload).await.is_err() {
                return;
            }
            if tx.send(payload).await.is_err() {
                return;
            }
        }
    });
    (rx, handle)
}

/// A heartbeat frame, as the drone's tee would put on the lane.
fn heartbeat() -> Vec<u8> {
    vec![
        0xFD, 0x09, 0x00, 0x00, 0x07, 0x01, 0x01, 0x00, 0x00, 0x00, 0xAA, 0xBB,
    ]
}

#[tokio::test]
async fn a_mavlink_datagram_reaches_the_republish_seam_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink-ingest.sock");
    let (mut seam, server) = fake_seam(sock.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ingest = MavlinkIngest::new(&sock);
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    let frame = heartbeat();
    let datagram = aux_mux::encode(AuxChannel::Mavlink, &frame).unwrap();
    dispatch(
        TEST_SLOT,
        &datagram,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    assert_eq!(seam.recv().await.unwrap(), frame);
    let snap = counters.snapshot();
    assert_eq!(snap.datagrams_received, 1);
    assert_eq!(snap.mavlink_frames, 1);
    assert_eq!(snap.mavlink_republished, 1);
    assert_eq!(snap.republish_errors, 0);
    server.abort();
}

/// A whole MAVLink v2 HEARTBEAT from an ArduPilot quadrotor with an
/// explicit system id, so two drones can be given the SAME one — which is
/// the shipped default.
fn heartbeat_from(system_id: u8) -> Vec<u8> {
    let mut f = vec![
        0xFD, 0x09, 0x00, 0x00, 0x07, system_id, 0x01, 0x00, 0x00, 0x00,
    ];
    // custom_mode, type = quadrotor (2), autopilot = ArduPilot (3),
    // base_mode, system_status, mavlink_version.
    f.extend_from_slice(&[0, 0, 0, 0, 2, 3, 0, 4, 3]);
    f.extend_from_slice(&[0xAA, 0xBB]);
    f
}

/// The same HEARTBEAT in MAVLink v1 framing: STX, LEN, SEQ, SYSID, COMPID,
/// MSGID, then the payload.
fn heartbeat_v1_from(system_id: u8) -> Vec<u8> {
    let mut f = vec![0xFE, 0x09, 0x00, system_id, 0x01, 0x00];
    f.extend_from_slice(&[0, 0, 0, 0, 2, 3, 0, 4, 3]);
    f.extend_from_slice(&[0xAA, 0xBB]);
    f
}

#[test]
fn the_system_id_is_read_from_each_version_s_own_header_offset() {
    // v1 puts the message id at byte 5; reading the system id there made
    // every v1 HEARTBEAT (message id 0) look like system 0.
    assert_eq!(mavlink_system_id(&heartbeat_v1_from(7)), Some(7));
    assert_eq!(
        autopilot_heartbeat_system_id(&heartbeat_v1_from(7)),
        Some(7)
    );
    assert_eq!(mavlink_system_id(&heartbeat_from(7)), Some(7));
    assert_eq!(autopilot_heartbeat_system_id(&heartbeat_from(7)), Some(7));
    assert_eq!(mavlink_system_id(&[0x55, 0, 0, 0, 0, 9]), None);
}

#[test]
fn only_a_flight_controller_heartbeat_names_the_slot() {
    // A ground station's own HEARTBEAT (type 6) forwarded by the FC, and a
    // companion's (autopilot 8), are not the vehicle.
    let mut gcs = heartbeat_from(255);
    gcs[14] = 6;
    assert_eq!(autopilot_heartbeat_system_id(&gcs), None);
    let mut companion = heartbeat_from(1);
    companion[15] = 8;
    assert_eq!(autopilot_heartbeat_system_id(&companion), None);
    // Any other message id is not a HEARTBEAT.
    let mut other = heartbeat_from(1);
    other[7] = 30;
    assert_eq!(autopilot_heartbeat_system_id(&other), None);
}

#[tokio::test]
async fn two_v1_drones_on_distinct_system_ids_are_not_a_collision() {
    // Both v1 HEARTBEATs carry message id 0 at byte 5. Read as a system id
    // that was a collision on every pair of v1 drones.
    let ingest = MavlinkIngest::with_timeout("/nonexistent/ingest.sock", Duration::from_millis(20));
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();
    for (slot, system_id) in [(1u8, 7u8), (2, 8)] {
        let d = aux_mux::encode(AuxChannel::Mavlink, &heartbeat_v1_from(system_id)).unwrap();
        dispatch(
            slot,
            &d,
            &AuxSinks::mavlink_only(&ingest),
            &counters,
            &mut gate,
            &peers,
        )
        .await;
    }
    assert_eq!(counters.snapshot().system_id_collisions, 0);
    assert_eq!(peers.system_ids_by_slot(), vec![(1, 7), (2, 8)]);
}

/// The slot every single-drone test dispatches on. Collision tests name
/// their slots explicitly instead.
const TEST_SLOT: u8 = 1;

#[tokio::test]
async fn two_drones_on_one_system_id_are_reported_as_a_collision() {
    // Was a characterisation test: it pinned the fact that two aircraft on
    // the shipped default system id arrived as ONE vehicle and that nothing
    // downstream could tell them apart. The frames are still byte-identical
    // at the seam -- rewriting a system id in flight would change what every
    // ground station and flight controller sees, and that is the aircraft's
    // own decision to make, not this consumer's.
    //
    // What has changed is that the collision is now OBSERVABLE. The slot is
    // threaded to the dispatch that was already binding a per-slot port and
    // discarding the number, so a second slot presenting an identity the
    // first already claimed is counted and named rather than passing
    // silently.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink-ingest.sock");
    let (mut seam, server) = fake_seam(sock.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ingest = MavlinkIngest::new(&sock);
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    // Two DIFFERENT slots, both presenting the shipped default system id.
    let frame = heartbeat_from(1);
    let datagram = aux_mux::encode(AuxChannel::Mavlink, &frame).unwrap();
    dispatch(
        1,
        &datagram,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;
    dispatch(
        2,
        &datagram,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    let first = seam.recv().await.unwrap();
    let second = seam.recv().await.unwrap();
    assert_eq!(
        first, second,
        "the frames themselves are still forwarded verbatim"
    );

    let snap = counters.snapshot();
    assert_eq!(snap.mavlink_republished, 2, "both were still delivered");
    assert_eq!(
        snap.system_id_collisions, 1,
        "and the second slot claiming the first slot's identity is reported"
    );
    assert_eq!(
        peers.system_ids_by_slot(),
        vec![(1, 1), (2, 1)],
        "with both slots' claimed identities readable"
    );
    server.abort();
}

#[tokio::test]
async fn two_drones_on_distinct_system_ids_are_not_a_collision() {
    // The steady state a correctly-addressed fleet is in, which must stay
    // silent: a warning that fires for every healthy fleet is one nobody
    // reads when it matters.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink-ingest.sock");
    let (_seam, server) = fake_seam(sock.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ingest = MavlinkIngest::new(&sock);
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    for (slot, system_id) in [(1u8, 1u8), (2, 2)] {
        let d = aux_mux::encode(AuxChannel::Mavlink, &heartbeat_from(system_id)).unwrap();
        dispatch(
            slot,
            &d,
            &AuxSinks::mavlink_only(&ingest),
            &counters,
            &mut gate,
            &peers,
        )
        .await;
    }

    assert_eq!(counters.snapshot().system_id_collisions, 0);
    server.abort();
}

#[tokio::test]
async fn a_damaged_frame_of_ours_is_counted_as_damage_not_dropped_silently() {
    // Past the magic and version checks, so this is our frame arriving
    // wrong. It must be distinguishable from foreign traffic, because one
    // is a transport fault and the other is normal.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink-ingest.sock");
    let (mut seam, server) = fake_seam(sock.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ingest = MavlinkIngest::new(&sock);
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    let mut truncated = aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap();
    truncated.truncate(truncated.len() - 3);
    dispatch(
        TEST_SLOT,
        &truncated,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.datagrams_received, 1);
    assert_eq!(snap.decode_damaged, 1, "damage must be counted as damage");
    assert_eq!(snap.decode_foreign, 0, "damage is not foreign traffic");
    assert_eq!(snap.mavlink_republished, 0);

    // A healthy frame after the damaged one still gets through: the lane
    // does not latch shut on one bad datagram.
    let good = aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap();
    dispatch(
        TEST_SLOT,
        &good,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;
    assert_eq!(seam.recv().await.unwrap(), heartbeat());
    server.abort();
}

#[tokio::test]
async fn a_batched_datagram_republishes_every_frame_it_carried() {
    // The drone packs several frames into one datagram because the lane
    // loses packets rather than bytes. Each must reach the seam separately
    // or the republished stream is one corrupt blob.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink-ingest.sock");
    let (mut seam, server) = fake_seam(sock.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ingest = MavlinkIngest::new(&sock);
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    // A WELL-FORMED v2 frame: 10 header bytes, a 9-byte payload, 2 checksum.
    // The `heartbeat()` fixture above declares a 9-byte payload but is only
    // 12 bytes long, so it is not a whole frame and cannot be split out of a
    // batch. That is fine for the single-frame test (the fallback forwards
    // an unsplittable payload intact) but a batch needs real boundaries.
    let mut frame = vec![0xFD, 0x09, 0x00, 0x00, 0x07, 0x01, 0x01, 0x00, 0x00, 0x00];
    frame.extend_from_slice(&[0x11; 9]);
    frame.extend_from_slice(&[0xAA, 0xBB]);
    assert_eq!(frame.len(), 21, "a v2 frame with a 9-byte payload");

    let mut batch = Vec::new();
    for _ in 0..3 {
        batch.extend_from_slice(&frame);
    }
    let datagram = aux_mux::encode(AuxChannel::Mavlink, &batch).unwrap();
    dispatch(
        TEST_SLOT,
        &datagram,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    for _ in 0..3 {
        assert_eq!(seam.recv().await.unwrap(), frame);
    }
    let snap = counters.snapshot();
    assert_eq!(snap.datagrams_received, 1, "one datagram arrived");
    assert_eq!(snap.mavlink_frames, 3, "carrying three frames");
    assert_eq!(snap.mavlink_republished, 3);
    assert_eq!(snap.republish_errors, 0);
    server.abort();
}

#[tokio::test]
async fn a_payload_that_is_not_splittable_is_still_forwarded() {
    // A drone build predating batching sends one frame per datagram. If the
    // splitter cannot measure it we forward it intact rather than drop it.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink-ingest.sock");
    let (mut seam, server) = fake_seam(sock.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ingest = MavlinkIngest::new(&sock);
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    let opaque = b"not-a-mavlink-frame".to_vec();
    let datagram = aux_mux::encode(AuxChannel::Mavlink, &opaque).unwrap();
    dispatch(
        TEST_SLOT,
        &datagram,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    assert_eq!(seam.recv().await.unwrap(), opaque);
    assert_eq!(counters.snapshot().mavlink_republished, 1);
    server.abort();
}

#[tokio::test]
async fn foreign_traffic_is_counted_apart_from_damage_and_republishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink-ingest.sock");
    let ingest = MavlinkIngest::with_timeout(&sock, Duration::from_millis(50));
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    dispatch(
        TEST_SLOT,
        b"some other application's datagram",
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;
    dispatch(
        TEST_SLOT,
        b"\xAD",
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.decode_foreign, 1);
    assert_eq!(snap.decode_runt, 1, "a runt is neither foreign nor damage");
    assert_eq!(snap.decode_damaged, 0);
    assert_eq!(snap.mavlink_frames, 0);
    assert_eq!(snap.mavlink_republished, 0);
}

#[tokio::test]
async fn each_framing_failure_mode_lands_in_its_own_counter() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink-ingest.sock");
    let ingest = MavlinkIngest::with_timeout(&sock, Duration::from_millis(50));
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    let mut wrong_version = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
    wrong_version[2] = 0x99;
    dispatch(
        TEST_SLOT,
        &wrong_version,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    let mut unknown_channel = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
    unknown_channel[3] = 0x7F;
    dispatch(
        TEST_SLOT,
        &unknown_channel,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    let mut overlong = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
    overlong[4] = 0xFF;
    overlong[5] = 0xFF;
    dispatch(
        TEST_SLOT,
        &overlong,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.decode_unsupported_version, 1);
    assert_eq!(snap.decode_unknown_channel, 1);
    assert_eq!(snap.decode_overlong, 1);
    assert_eq!(snap.datagrams_received, 3);
}

#[tokio::test]
async fn the_other_channels_are_counted_and_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink-ingest.sock");
    let (mut seam, server) = fake_seam(sock.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ingest = MavlinkIngest::new(&sock);
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    for ch in [AuxChannel::Status, AuxChannel::Identity] {
        let d = aux_mux::encode(ch, b"a compact snapshot").unwrap();
        dispatch(
            TEST_SLOT,
            &d,
            &AuxSinks::mavlink_only(&ingest),
            &counters,
            &mut gate,
            &peers,
        )
        .await;
    }
    // The MAVLink one after them still republishes, proving the ignore is
    // per-channel and not a lane that stopped.
    let d = aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap();
    dispatch(
        TEST_SLOT,
        &d,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    assert_eq!(seam.recv().await.unwrap(), heartbeat());
    let snap = counters.snapshot();
    assert_eq!(snap.status_frames, 1);
    assert_eq!(snap.identity_frames, 1);
    assert_eq!(snap.mavlink_republished, 1);
    server.abort();
}

#[tokio::test]
async fn an_absent_seam_backs_off_instead_of_retrying_every_frame() {
    // With no router listening, the first frame attempts and fails; the
    // frames behind it are dropped against the backoff rather than each
    // paying a fresh connection attempt.
    let ingest = Arc::new(MavlinkIngest::with_timeout(
        "/nonexistent/mavlink-ingest.sock",
        Duration::from_millis(20),
    ));
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    let d = aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap();
    for _ in 0..5 {
        dispatch(
            TEST_SLOT,
            &d,
            &AuxSinks::mavlink_only(&ingest),
            &counters,
            &mut gate,
            &peers,
        )
        .await;
    }

    let snap = counters.snapshot();
    assert_eq!(snap.mavlink_frames, 5, "every frame is still accounted for");
    assert_eq!(snap.republish_errors, 1, "only the first attempt is made");
    assert_eq!(
        snap.republish_lane_down, 4,
        "the rest drop against the gate"
    );
    assert_eq!(snap.mavlink_republished, 0);
}

#[tokio::test]
async fn every_datagram_lands_in_exactly_one_bucket() {
    // The lane's tally is only trustworthy if it accounts for the whole
    // stream. Feed one datagram of every kind the consumer can meet and
    // assert the outcome counters sum back to the number that arrived, so a
    // path that forgot to count would show as a shortfall rather than
    // quietly making the lane look emptier than it was.
    let ingest = MavlinkIngest::with_timeout(
        "/nonexistent/mavlink-ingest.sock",
        Duration::from_millis(20),
    );
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    let mut wrong_version = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
    wrong_version[2] = 0x99;
    let mut unknown_channel = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
    unknown_channel[3] = 0x7F;
    let mut overlong = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
    overlong[4] = 0xFF;
    overlong[5] = 0xFF;
    let mut damaged = aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap();
    damaged.truncate(damaged.len() - 3);

    let datagrams: Vec<Vec<u8>> = vec![
        aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap(),
        // A second MAVLink frame: the first fails the absent seam, this one
        // drops against the backoff, so both republish outcomes are covered.
        aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap(),
        aux_mux::encode(AuxChannel::Status, b"status").unwrap(),
        aux_mux::encode(AuxChannel::Identity, b"identity").unwrap(),
        aux_mux::encode(AuxChannel::AppStream, b"app-stream").unwrap(),
        aux_mux::encode(AuxChannel::AppCommand, b"app-command").unwrap(),
        b"foreign application traffic".to_vec(),
        b"\xAD".to_vec(),
        wrong_version,
        unknown_channel,
        overlong,
        damaged,
    ];
    let sent = datagrams.len() as u64;
    for d in &datagrams {
        dispatch(
            TEST_SLOT,
            d,
            &AuxSinks::mavlink_only(&ingest),
            &counters,
            &mut gate,
            &peers,
        )
        .await;
    }

    let snap = counters.snapshot();
    assert_eq!(snap.datagrams_received, sent);
    assert_eq!(
        snap.accounted(),
        snap.datagrams_received,
        "every datagram must be accounted for exactly once: {snap:?}"
    );
    // And the split really is populated, so the equality above is not being
    // satisfied by two empty sums.
    assert_eq!(snap.republish_errors, 1);
    assert_eq!(snap.republish_lane_down, 1);
    assert_eq!(snap.decode_damaged, 1);
    assert_eq!(snap.decode_foreign, 1);
    assert_eq!(snap.app_stream, 1);
    assert_eq!(
        snap.app_stream_undelivered, 1,
        "no app-stream sink is wired"
    );
    assert_eq!(snap.app_command_unexpected, 1);
}

#[tokio::test]
async fn the_full_loop_carries_a_datagram_from_the_wire_to_the_seam() {
    // End to end over real sockets: a datagram on the lane's loopback port,
    // decoded, and the MAVLink frame arriving at the republish seam.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mavlink-ingest.sock");
    let (mut seam, server) = fake_seam(sock.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Port 0 lets the kernel choose, so the test never collides with a
    // running service or another test.
    let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let counters = AuxCounters::new();
    let cancel = Shutdown::new();
    let task = tokio::spawn(run_aux_consumer(
        TEST_SLOT,
        port,
        AuxSinksOwned {
            mavlink: Arc::new(MavlinkIngest::new(&sock)),
            rpc_response: None,
            config_tunnel: None,
            app_stream: None,
            local_plugins: None,
        },
        counters.clone(),
        AuxPeerCache::new(),
        cancel.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let tx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let frame = heartbeat();
    let datagram = aux_mux::encode(AuxChannel::Mavlink, &frame).unwrap();
    tx.send_to(&datagram, (Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    // A datagram that is not ours, to prove the loop keeps going past one.
    tx.send_to(b"foreign", (Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), seam.recv())
            .await
            .expect("the frame must reach the seam")
            .unwrap(),
        frame
    );

    cancel.trigger();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("cancellation must stop the consumer")
        .unwrap()
        .unwrap();

    let snap = counters.snapshot();
    assert_eq!(snap.mavlink_republished, 1);
    assert!(snap.bytes_received >= datagram.len() as u64);
    server.abort();
}

#[tokio::test]
async fn a_port_that_cannot_be_bound_is_reported_rather_than_silently_idle() {
    // A consumer that never started listening must be visible as such: a
    // bind failure counted, not an absent counter that reads as "nothing
    // arrived".
    let held = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = held.local_addr().unwrap().port();

    let counters = AuxCounters::new();
    let cancel = Shutdown::new();
    let ingest = Arc::new(MavlinkIngest::with_timeout(
        "/nonexistent/mavlink-ingest.sock",
        Duration::from_millis(20),
    ));

    assert!(
        run_aux_consumer(
            TEST_SLOT,
            port,
            AuxSinksOwned {
                mavlink: ingest.clone(),
                rpc_response: None,
                config_tunnel: None,
                app_stream: None,
                local_plugins: None,
            },
            counters.clone(),
            AuxPeerCache::new(),
            cancel.clone()
        )
        .await
        .is_err(),
        "binding a taken port must report an error"
    );

    // The supervisor turns that error into a counted, retried outage rather
    // than a task that quietly disappears. Aborted rather than cancelled
    // here: the cancellation path is asserted deterministically in the
    // full-loop test above, and racing a notification against a task that
    // is between its two await points would only make this one flaky.
    let task = tokio::spawn(supervise_aux_consumer(
        TEST_SLOT,
        port,
        AuxSinksOwned {
            mavlink: ingest,
            rpc_response: None,
            config_tunnel: None,
            app_stream: None,
            local_plugins: None,
        },
        counters.clone(),
        AuxPeerCache::new(),
        cancel.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    task.abort();

    assert!(
        counters.snapshot().bind_failures >= 1,
        "the failed bind must be counted"
    );
    drop(held);
}

/// A status frame must actually REACH the cache, not merely be counted.
///
/// The counter and the cache are updated in the same arm, so a wiring
/// regression that dropped the decode would still leave `status_frames`
/// climbing and the surface silently empty. This asserts the surface.
#[tokio::test]
async fn status_and_identity_frames_reach_the_peer_cache() {
    let ingest = MavlinkIngest::new("/nonexistent/ingest.sock");
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    let (status_body, _) = ados_protocol::node_status::NodeStatus::new("drone-a", 3)
        .with_fc(Some(true), Some(true), None, Some("ardupilot"))
        .encode()
        .unwrap();
    let identity_body = ados_protocol::node_status::NodeIdentity::build(
        "drone-a",
        Some("Alpha"),
        Some("drone"),
        Some("1.2.3"),
        &[],
    )
    .encode()
    .unwrap();

    for (channel, body) in [
        (AuxChannel::Status, status_body),
        (AuxChannel::Identity, identity_body),
    ] {
        let framed = aux_mux::encode(channel, &body).unwrap();
        dispatch(
            TEST_SLOT,
            &framed,
            &AuxSinks::mavlink_only(&ingest),
            &counters,
            &mut gate,
            &peers,
        )
        .await;
    }

    let snap = counters.snapshot();
    assert_eq!(snap.status_frames, 1);
    assert_eq!(snap.identity_frames, 1);
    assert_eq!(snap.status_undecodable, 0);
    assert_eq!(snap.identity_undecodable, 0);

    let published = peers.peers_payload(crate::aux_peers::tests::test_now());
    assert_eq!(published.len(), 1, "the peer must be published");
    assert_eq!(published[0]["device_id"], "drone-a");
    assert_eq!(published[0]["name"], "Alpha");
    assert_eq!(published[0]["status"]["fc_firmware"], "ardupilot");
}

/// A status body this build cannot read is counted, not silently dropped:
/// a peer running a newer schema is a version mismatch worth seeing.
#[tokio::test]
async fn an_undecodable_status_body_is_counted_separately() {
    let ingest = MavlinkIngest::new("/nonexistent/ingest.sock");
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    let framed = aux_mux::encode(AuxChannel::Status, b"not msgpack").unwrap();
    dispatch(
        TEST_SLOT,
        &framed,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.status_frames, 1, "the datagram is still accounted for");
    assert_eq!(snap.status_undecodable, 1);
    assert!(peers.peers_payload(0.0).is_empty());
    // The sub-tally must not disturb the whole-lane accounting invariant.
    assert_eq!(snap.accounted(), snap.datagrams_received);
}

/// The tally read a permanent 0 before this: nothing ever bumped it, so a
/// relay lane carrying answers and one carrying none looked identical on
/// the ground rig's counters.
#[tokio::test]
async fn a_response_forwarded_to_the_proxy_is_counted_as_dispatched() {
    use ados_protocol::aux_rpc::encode_response_fragment;
    use ados_protocol::aux_rpc_proxy::AuxRpcResponseIngest;

    let ingest = MavlinkIngest::new("/nonexistent/ingest.sock");
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();
    let proxy = AuxRpcResponseIngest::new("/nonexistent/proxy.sock");

    let fragment =
        encode_response_fragment(b"0a1b2c3d4e5f", 9, 200, 0, 1, 11, br#"{"ok":true}"#).unwrap();
    let framed = aux_mux::encode(AuxChannel::Response, &fragment).unwrap();
    dispatch(
        TEST_SLOT,
        &framed,
        &AuxSinks {
            mavlink: &ingest,
            rpc_response: Some(&proxy),
            config_tunnel: None,
            app_stream: None,
            local_plugins: None,
        },
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.response_frames, 1);
    assert_eq!(snap.response_dispatched, 1);
    assert_eq!(snap.response_orphan, 0, "an ingest was registered");
    assert_eq!(snap.accounted(), snap.datagrams_received);
}

/// The ground half of the config-over-radio bridge: a reply from the linked
/// drone is handed to the local config-tunnel service with its aux framing
/// intact, so that service re-checks the channel rather than trusting this
/// hop.
#[tokio::test]
async fn a_config_tunnel_frame_is_forwarded_to_the_local_service_verbatim() {
    use ados_protocol::config_tunnel_ingest::ConfigTunnelIngest;

    let ingest = MavlinkIngest::new("/nonexistent/ingest.sock");
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    // Stands in for the config-tunnel service's own loopback ingress.
    let service = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let tunnel = ConfigTunnelIngest::new(service.local_addr().unwrap().port());

    let framed = aux_mux::encode(AuxChannel::ConfigTunnel, b"a-tunnel-frame").unwrap();
    dispatch(
        TEST_SLOT,
        &framed,
        &AuxSinks {
            mavlink: &ingest,
            rpc_response: None,
            config_tunnel: Some(&tunnel),
            app_stream: None,
            local_plugins: None,
        },
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    let mut buf = [0u8; 256];
    let (n, _) = service.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], framed.as_slice(), "framing must survive the hop");

    let snap = counters.snapshot();
    assert_eq!(snap.config_tunnel_frames, 1);
    assert_eq!(snap.config_tunnel_forwarded, 1);
    assert_eq!(snap.config_tunnel_undelivered, 0);
    assert_eq!(snap.accounted(), snap.datagrams_received);
}

/// The channel is opt-in, so most rigs run no config-tunnel service at all.
/// Such a frame must be counted and dropped: silence here would read as a
/// lane carrying nothing, which is the opposite of what happened.
#[tokio::test]
async fn a_config_tunnel_frame_with_no_local_service_is_counted_not_lost() {
    let ingest = MavlinkIngest::new("/nonexistent/ingest.sock");
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    let framed = aux_mux::encode(AuxChannel::ConfigTunnel, b"a-tunnel-frame").unwrap();
    dispatch(
        TEST_SLOT,
        &framed,
        &AuxSinks::mavlink_only(&ingest),
        &counters,
        &mut gate,
        &peers,
    )
    .await;

    let snap = counters.snapshot();
    assert_eq!(snap.config_tunnel_frames, 1);
    assert_eq!(snap.config_tunnel_forwarded, 0);
    assert_eq!(snap.config_tunnel_undelivered, 1);
    assert_eq!(snap.accounted(), snap.datagrams_received);
}

/// Two halves of a plugin on different contract versions must be flagged,
/// not left to disagree on the wire in silence. A plugin the peer runs but
/// this node does not is ignored: there is nothing here to disagree with.
#[tokio::test]
async fn a_plugin_contract_version_mismatch_is_flagged() {
    use std::collections::HashMap;
    let ingest = MavlinkIngest::new("/nonexistent/ingest.sock");
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    // This node runs the plugin at contract version 3.
    let local: HashMap<String, u16> = [("ados.example.two-halves".to_string(), 3u16)]
        .into_iter()
        .collect();
    // The peer advertises the same plugin at version 2, plus a second plugin
    // this node does not run (which must not count).
    let peer_plugins = vec![
        ("ados.example.two-halves".to_string(), 2u16),
        ("ados.example.other".to_string(), 9u16),
    ];
    let identity = NodeIdentity::build("drone-a", None, Some("drone"), None, &peer_plugins);
    let framed = aux_mux::encode(AuxChannel::Identity, &identity.encode().unwrap()).unwrap();

    let mut sinks = AuxSinks::mavlink_only(&ingest);
    sinks.local_plugins = Some(&local);
    dispatch(TEST_SLOT, &framed, &sinks, &counters, &mut gate, &peers).await;

    let snap = counters.snapshot();
    assert_eq!(snap.identity_frames, 1);
    assert_eq!(
        snap.identity_plugin_version_mismatch, 1,
        "only the shared plugin with a differing version is a mismatch"
    );
}

/// The same plugin at the same version on both halves is not a mismatch.
#[tokio::test]
async fn matching_plugin_contract_versions_are_not_flagged() {
    use std::collections::HashMap;
    let ingest = MavlinkIngest::new("/nonexistent/ingest.sock");
    let counters = AuxCounters::new();
    let mut gate = SeamGate::default();
    let peers = AuxPeerCache::new();

    let local: HashMap<String, u16> = [("ados.example.two-halves".to_string(), 3u16)]
        .into_iter()
        .collect();
    let identity = NodeIdentity::build(
        "drone-a",
        None,
        Some("drone"),
        None,
        &[("ados.example.two-halves".to_string(), 3u16)],
    );
    let framed = aux_mux::encode(AuxChannel::Identity, &identity.encode().unwrap()).unwrap();

    let mut sinks = AuxSinks::mavlink_only(&ingest);
    sinks.local_plugins = Some(&local);
    dispatch(TEST_SLOT, &framed, &sinks, &counters, &mut gate, &peers).await;

    let snap = counters.snapshot();
    assert_eq!(snap.identity_frames, 1);
    assert_eq!(snap.identity_plugin_version_mismatch, 0);
}
