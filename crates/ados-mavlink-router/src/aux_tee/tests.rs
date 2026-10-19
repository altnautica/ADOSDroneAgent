use super::*;
use ados_protocol::aux_mux;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UdpSocket, UnixListener};

/// A minimal MAVLink v2 frame carrying `msg_id` and `payload_len` bytes of
/// payload. Only the header fields the tee reads need to be real.
fn v2_frame(msg_id: u32, payload_len: usize) -> Vec<u8> {
    let mut f = vec![
        STX_V2,
        payload_len as u8,
        0, // incompat flags: unsigned
        0, // compat flags
        0, // sequence
        1, // system id
        1, // component id
        (msg_id & 0xFF) as u8,
        ((msg_id >> 8) & 0xFF) as u8,
        ((msg_id >> 16) & 0xFF) as u8,
    ];
    f.extend(std::iter::repeat_n(0xAB, payload_len));
    f.extend_from_slice(&[0, 0]); // checksum
    f
}

/// A fake radio command socket. `disabled` makes every open refuse the way
/// the operator dead switch does; otherwise opens succeed and point at
/// `tx_port`.
fn fake_radio(sock_path: PathBuf, tx_port: u16, disabled: bool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let listener = UnixListener::bind(&sock_path).unwrap();
        while let Ok((stream, _)) = listener.accept().await {
            let (rx, mut tx) = stream.into_split();
            let mut line = String::new();
            let _ = BufReader::new(rx).read_line(&mut line).await;
            let reply = if disabled {
                "{\"ok\":false,\"error\":\"E_AUX_DISABLED\"}\n".to_string()
            } else {
                format!("{{\"ok\":true,\"active\":true,\"tx_port\":{tx_port},\"rx_port\":5603}}\n")
            };
            let _ = tx.write_all(reply.as_bytes()).await;
        }
    })
}

/// Wait until the fake radio's socket exists, so a test never races it.
async fn await_socket(path: &Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("fake radio never bound its socket");
}

// ---- message id and shed classification ----

#[test]
fn reads_the_message_id_from_both_frame_versions() {
    assert_eq!(mavlink_message_id(&v2_frame(0, 4)), Some(0));
    assert_eq!(mavlink_message_id(&v2_frame(33, 8)), Some(33));
    // A three-byte id exercises all of the little-endian span.
    assert_eq!(mavlink_message_id(&v2_frame(0x0A_BC_DE, 2)), Some(0x0ABCDE));
    // v1: magic, length, sequence, system, component, id.
    let v1 = vec![STX_V1, 1, 0, 1, 1, 30, 0xAB, 0, 0];
    assert_eq!(mavlink_message_id(&v1), Some(30));
}

#[test]
fn an_unreadable_header_has_no_message_id_and_is_shed_first() {
    assert_eq!(mavlink_message_id(&[]), None);
    assert_eq!(mavlink_message_id(b"not a frame"), None);
    // Truncated below the header: refuse rather than read past the end.
    assert_eq!(mavlink_message_id(&[STX_V2, 1, 0]), None);
    assert_eq!(mavlink_message_id(&[STX_V1, 1]), None);
    assert_eq!(frame_priority(b"not a frame"), Priority::Bulk);
}

#[test]
fn flight_critical_frames_outrank_the_high_rate_streams() {
    // Heartbeat, position and status text keep the vehicle legible.
    assert_eq!(frame_priority(&v2_frame(0, 9)), Priority::Critical);
    assert_eq!(frame_priority(&v2_frame(33, 28)), Priority::Critical);
    assert_eq!(frame_priority(&v2_frame(253, 51)), Priority::Critical);
    // A transfer stays whole rather than stalling half downloaded.
    assert_eq!(frame_priority(&v2_frame(22, 25)), Priority::Critical);
    assert_eq!(frame_priority(&v2_frame(73, 37)), Priority::Critical);
    // The noisy sensor and servo streams are what a congested lane sheds.
    assert_eq!(frame_priority(&v2_frame(27, 26)), Priority::Bulk); // RAW_IMU
    assert_eq!(frame_priority(&v2_frame(36, 21)), Priority::Bulk); // SERVO_OUTPUT_RAW
    assert_eq!(frame_priority(&v2_frame(241, 32)), Priority::Bulk); // VIBRATION
}

// ---- rate shaping ----

#[test]
fn a_burst_is_admitted_until_the_bucket_empties() {
    let cfg = ShaperConfig {
        budget_bytes_per_sec: 100.0,
        burst_bytes: 1000.0,
        bulk_reserve_bytes: 0.0,
    };
    let start = Instant::now();
    let mut shaper = RateShaper::starting_at(cfg, start);
    // Ten 100-byte frames exactly drain a 1000-byte bucket.
    for i in 0..10 {
        assert!(
            shaper.admit(start, 100, Priority::Critical),
            "frame {i} is within the burst"
        );
    }
    assert!(
        !shaper.admit(start, 100, Priority::Critical),
        "the eleventh exceeds the cap and is shed, not queued"
    );
}

#[test]
fn the_bucket_refills_at_the_configured_rate_and_stops_at_the_burst() {
    let cfg = ShaperConfig {
        budget_bytes_per_sec: 100.0,
        burst_bytes: 1000.0,
        bulk_reserve_bytes: 0.0,
    };
    let start = Instant::now();
    let mut shaper = RateShaper::starting_at(cfg, start);
    assert!(shaper.admit(start, 1000, Priority::Critical));
    assert!(!shaper.admit(start, 100, Priority::Critical));
    // Two seconds buys exactly 200 bytes back.
    let later = start + Duration::from_secs(2);
    assert!(shaper.admit(later, 200, Priority::Critical));
    assert!(!shaper.admit(later, 1, Priority::Critical));
    // A long idle refills to the burst size and no further, so the cap
    // cannot be banked indefinitely and spent in one go.
    let much_later = start + Duration::from_secs(3600);
    shaper.admit(much_later, 0, Priority::Critical);
    assert!((shaper.tokens() - 1000.0).abs() < 1e-6);
}

#[test]
fn a_saturating_bulk_stream_starves_before_critical_traffic_does() {
    // This is the property that keeps a vehicle legible on a congested
    // lane: bulk runs the bucket down to the reserve and stops, and what
    // is left is available only to critical frames.
    let cfg = ShaperConfig {
        budget_bytes_per_sec: 100.0,
        burst_bytes: 1000.0,
        bulk_reserve_bytes: 400.0,
    };
    let start = Instant::now();
    let mut shaper = RateShaper::starting_at(cfg, start);

    let mut bulk_admitted = 0;
    for _ in 0..100 {
        if shaper.admit(start, 100, Priority::Bulk) {
            bulk_admitted += 1;
        }
    }
    // Bulk spends only the 600 bytes above the reserve.
    assert_eq!(bulk_admitted, 6);
    assert!((shaper.tokens() - 400.0).abs() < 1e-6);

    // Critical traffic still flows on the very same saturated bucket.
    for i in 0..4 {
        assert!(
            shaper.admit(start, 100, Priority::Critical),
            "critical frame {i} still gets through"
        );
    }
    // And the hard cap still binds: it is a ceiling for everyone.
    assert!(!shaper.admit(start, 100, Priority::Critical));
}

#[test]
fn the_shipped_defaults_can_carry_a_whole_aux_frame() {
    // A burst smaller than one frame would shed every frame forever.
    let cfg = ShaperConfig::default();
    assert!(cfg.burst_bytes >= (AUX_HEADER_LEN + AUX_MAX_PAYLOAD) as f64);
    assert!(cfg.bulk_reserve_bytes < cfg.burst_bytes);
}

// ---- the running tee ----

/// Drive the tee over a fake radio, returning the counters and everything
/// the aux transmit port received.
async fn run_tee(
    frames_to_send: Vec<Vec<u8>>,
    disabled: bool,
    shaper: ShaperConfig,
) -> (TeeCountersSnapshot, Vec<Vec<u8>>) {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("radio-aux.sock");
    let aux_rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let tx_port = aux_rx.local_addr().unwrap().port();
    let radio = fake_radio(sock_path.clone(), tx_port, disabled);
    await_socket(&sock_path).await;

    let (tx, rx) = broadcast::channel::<crate::rx_frame::RxFrame>(64);
    let counters = Arc::new(TeeCounters::default());
    let handle = tokio::spawn(run(
        rx,
        AuxEgress::with_timeout(&sock_path, Duration::from_millis(200)),
        counters.clone(),
        shaper,
        Shutdown::new(),
    ));

    for frame in frames_to_send {
        tx.send(crate::rx_frame::RxFrame::new(0, frame.into()))
            .unwrap();
    }
    // Closing the source ends the loop, which is deterministic where a
    // cancellation notify would race the select.
    drop(tx);
    handle.await.unwrap();

    let mut received = Vec::new();
    let mut buf = vec![0u8; 4096];
    while let Ok(Ok((n, _))) =
        tokio::time::timeout(Duration::from_millis(50), aux_rx.recv_from(&mut buf)).await
    {
        received.push(buf[..n].to_vec());
    }
    radio.abort();
    (counters.snapshot(), received)
}

#[tokio::test]
async fn a_flight_controller_frame_reaches_the_aux_transmit_port_intact() {
    let heartbeat = v2_frame(0, 9);
    let (counters, received) =
        run_tee(vec![heartbeat.clone()], false, ShaperConfig::default()).await;

    assert_eq!(counters.frames_forwarded, 1);
    assert_eq!(
        counters.bytes_forwarded,
        (AUX_HEADER_LEN + heartbeat.len()) as u64
    );
    assert_eq!(received.len(), 1);
    let (channel, payload) = aux_mux::decode(&received[0]).unwrap();
    assert_eq!(channel, AuxChannel::Mavlink);
    assert_eq!(
        payload,
        &heartbeat[..],
        "the frame crosses the lane byte for byte"
    );
}

#[tokio::test]
async fn an_oversized_frame_is_dropped_whole_and_never_truncated() {
    // Larger than one aux frame can carry. Sending a prefix of it would put
    // a corrupt frame on air; the tee must drop it and say so.
    let too_big = vec![STX_V2; AUX_MAX_PAYLOAD + 1];
    let ok = v2_frame(0, 9);
    let (counters, received) =
        run_tee(vec![too_big, ok.clone()], false, ShaperConfig::default()).await;

    assert_eq!(counters.dropped_oversize, 1);
    assert_eq!(counters.frames_forwarded, 1, "the healthy frame still goes");
    assert_eq!(received.len(), 1, "nothing partial was emitted");
    assert_eq!(aux_mux::decode(&received[0]).unwrap().1, &ok[..]);
}

#[tokio::test]
async fn several_frames_ride_one_datagram() {
    // The reason batching exists: the lane loses PACKETS, not bytes. On a
    // live link the identical byte rate lost 15% as ten small datagrams a
    // second and 0% as two large ones. So a burst of frames must leave as
    // markedly fewer datagrams than frames, and every frame must survive.
    let frame = v2_frame(0, 9);
    let shaper = ShaperConfig {
        budget_bytes_per_sec: 1_000_000.0,
        burst_bytes: 1_000_000.0,
        bulk_reserve_bytes: 0.0,
    };
    let (counters, received) = run_tee(vec![frame.clone(); 20], false, shaper).await;

    assert_eq!(
        counters.frames_forwarded, 20,
        "no frame is lost to batching"
    );
    assert!(
        received.len() < 20,
        "20 frames must not leave as 20 datagrams, got {}",
        received.len()
    );

    // And the batch splits back into exactly what went in.
    let out: Vec<Vec<u8>> = received
        .iter()
        .flat_map(|d| {
            aux_mux::split_frames(aux_mux::decode(d).unwrap().1)
                .into_iter()
                .map(|f| f.to_vec())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(out.len(), 20);
    assert!(out.iter().all(|f| *f == frame));
}

#[tokio::test]
async fn no_datagram_exceeds_the_lane_payload_budget() {
    // A batch must flush before it would overflow, or the encode refuses and
    // the whole batch is lost rather than one frame.
    let frame = v2_frame(0, 9);
    let shaper = ShaperConfig {
        budget_bytes_per_sec: 1_000_000.0,
        burst_bytes: 1_000_000.0,
        bulk_reserve_bytes: 0.0,
    };
    // 60 frames of ~21 bytes is over the 1200-byte payload budget, so the
    // batch must flush mid-run. Kept under the harness broadcast channel's
    // 64 slots: pushing more sheds frames as lagged before the tee sees
    // them, which would test the harness rather than the batching.
    let (counters, received) = run_tee(vec![frame; 60], false, shaper).await;
    assert_eq!(counters.frames_forwarded + counters.dropped_lagged, 60);
    assert!(
        received.len() > 1,
        "the batch must flush before overflowing"
    );
    for d in &received {
        let (_, payload) = aux_mux::decode(d).expect("every datagram decodes");
        assert!(
            payload.len() <= AUX_MAX_PAYLOAD,
            "datagram payload {} exceeds the lane budget",
            payload.len()
        );
    }
}

#[tokio::test]
async fn frames_over_the_byte_budget_are_shed_rather_than_queued() {
    // A bucket that admits two frames and no more within the test's span.
    let frame = v2_frame(0, 9);
    let cost = (AUX_HEADER_LEN + frame.len()) as f64;
    let shaper = ShaperConfig {
        budget_bytes_per_sec: 1.0,
        burst_bytes: cost * 2.0,
        bulk_reserve_bytes: 0.0,
    };
    let (counters, received) = run_tee(vec![frame; 10], false, shaper).await;

    assert_eq!(counters.frames_forwarded, 2);
    assert_eq!(counters.dropped_rate_limit, 8);
    // Frames are batched into datagrams, so count FRAMES on the wire rather
    // than datagrams: the budget is a ceiling on what is admitted, and the
    // carrier it rides is a separate concern.
    let on_wire: usize = received
        .iter()
        .map(|d| aux_mux::split_frames(aux_mux::decode(d).unwrap().1).len())
        .sum();
    assert_eq!(
        on_wire, 2,
        "the budget is a ceiling: the rest are dropped, not buffered"
    );
}

#[tokio::test]
async fn a_congested_lane_sheds_the_noisy_stream_and_keeps_the_heartbeat() {
    // One frame of headroom above the reserve, so bulk gets exactly one
    // through and every heartbeat still crosses.
    let bulk = v2_frame(27, 26); // RAW_IMU
    let beat = v2_frame(0, 9); // HEARTBEAT
    let bulk_cost = (AUX_HEADER_LEN + bulk.len()) as f64;
    let beat_cost = (AUX_HEADER_LEN + beat.len()) as f64;
    let shaper = ShaperConfig {
        budget_bytes_per_sec: 1.0,
        burst_bytes: bulk_cost + beat_cost * 3.0,
        bulk_reserve_bytes: beat_cost * 3.0,
    };

    let mut frames = vec![bulk.clone(); 6];
    frames.extend(vec![beat.clone(); 3]);
    let (counters, received) = run_tee(frames, false, shaper).await;

    assert_eq!(counters.dropped_rate_limit, 5, "five bulk frames shed");
    assert_eq!(counters.frames_forwarded, 4, "one bulk plus three beats");
    // Split each datagram back into frames: a batch may carry several, and
    // the guarantee under test is about frames surviving, not datagrams.
    let payloads: Vec<Vec<u8>> = received
        .iter()
        .flat_map(|d| {
            aux_mux::split_frames(aux_mux::decode(d).unwrap().1)
                .into_iter()
                .map(|f| f.to_vec())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        payloads.iter().filter(|p| *p == &beat).count(),
        3,
        "every heartbeat survives the congestion"
    );
}

#[tokio::test]
async fn a_disabled_lane_forwards_nothing_and_stops_retrying() {
    let (counters, received) =
        run_tee(vec![v2_frame(0, 9); 5], true, ShaperConfig::default()).await;

    assert_eq!(counters.frames_forwarded, 0);
    assert!(received.is_empty(), "nothing is put on a forbidden lane");
    assert_eq!(
        counters.dropped_lane_down, 5,
        "every frame is accounted for"
    );
    assert_eq!(
        counters.open_errors, 0,
        "an operator's dead switch is a state, not a fault"
    );
}

#[tokio::test]
async fn a_missing_radio_backs_off_instead_of_reconnecting_per_frame() {
    // No radio at all: the first frame attempts an open, and the backoff
    // means the rest are dropped without touching the command socket.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.sock");
    let (tx, rx) = broadcast::channel::<crate::rx_frame::RxFrame>(64);
    let counters = Arc::new(TeeCounters::default());
    let handle = tokio::spawn(run(
        rx,
        AuxEgress::with_timeout(&missing, Duration::from_millis(50)),
        counters.clone(),
        ShaperConfig::default(),
        Shutdown::new(),
    ));
    for _ in 0..8 {
        tx.send(crate::rx_frame::RxFrame::new(0, v2_frame(0, 9).into()))
            .unwrap();
    }
    drop(tx);
    handle.await.unwrap();

    let snap = counters.snapshot();
    assert_eq!(snap.frames_forwarded, 0);
    assert_eq!(snap.dropped_lane_down, 8);
    assert_eq!(
        snap.open_errors, 1,
        "one open attempt, then the gate holds the rest off"
    );
}

#[tokio::test]
async fn cancelling_stops_the_tee() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.sock");
    let (_tx, rx) = broadcast::channel::<crate::rx_frame::RxFrame>(8);
    let cancel = Shutdown::new();
    let handle = tokio::spawn(run(
        rx,
        AuxEgress::with_timeout(&missing, Duration::from_millis(50)),
        Arc::new(TeeCounters::default()),
        ShaperConfig::default(),
        cancel.clone(),
    ));
    // Let the task reach its select before notifying.
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel.trigger();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("the tee shuts down promptly")
        .unwrap();
}
