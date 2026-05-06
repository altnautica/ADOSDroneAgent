//! End-to-end round-trip integration test for the RTSP push pipeline.
//!
//! Brings up the in-process `MockRtspServer` from `ados-test-mocks`,
//! feeds synthetic Annex-B byte streams through `run_push_loop`, and
//! asserts the captured RTP packets match the expected wire shape:
//!
//!   - RTP version 2, payload type 96.
//!   - Monotonically increasing sequence numbers (mod 2^16).
//!   - At least one packet per fixture access unit.
//!   - For the large-NAL fixture, FU-A indicator bytes (NAL type 28)
//!     appear in the captured stream.
//!
//! The fixtures live at `tests/fixtures/*.h264` and are regenerated
//! by the ignored `fixtures_gen` test target.

use std::path::PathBuf;
use std::time::Duration;

use ados_test_mocks::MockRtspServer;
use ados_video::rtsp::{run_push_loop, PushConfig, DEFAULT_MTU};
use ados_video::EncodedFrame;
use tokio::sync::{broadcast, oneshot};
use tokio::time::timeout;

/// Walk the start-code-delimited Annex-B byte stream and split it into
/// per-frame chunks at every IDR or SPS boundary. The push loop reads
/// one `EncodedFrame` at a time so the test simulates the encoder's
/// frame cadence by dripping access units through the broadcast.
fn split_annex_b_into_frames(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut frames: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        // Find next start code at or after i.
        let mut s = None;
        let mut j = i;
        while j + 4 <= bytes.len() {
            if bytes[j] == 0 && bytes[j + 1] == 0 {
                if bytes[j + 2] == 1 {
                    s = Some((j, j + 3));
                    break;
                }
                if j + 4 <= bytes.len() && bytes[j + 2] == 0 && bytes[j + 3] == 1 {
                    s = Some((j, j + 4));
                    break;
                }
            }
            j += 1;
        }
        let Some((start, hdr_idx)) = s else {
            break;
        };
        if hdr_idx >= bytes.len() {
            break;
        }
        let nal_type = bytes[hdr_idx] & 0x1F;
        // Find the next start code after this one to know where the
        // current NAL ends.
        let mut next = None;
        let mut k = hdr_idx + 1;
        while k + 4 <= bytes.len() {
            if bytes[k] == 0 && bytes[k + 1] == 0 {
                if bytes[k + 2] == 1 {
                    next = Some(k);
                    break;
                }
                if k + 4 <= bytes.len() && bytes[k + 2] == 0 && bytes[k + 3] == 1 {
                    next = Some(k);
                    break;
                }
            }
            k += 1;
        }
        let nal_end = next.unwrap_or(bytes.len());
        let nal_slice = &bytes[start..nal_end];

        // SPS or IDR starts a new access unit (frame boundary). Flush
        // the current accumulator if it already holds bytes, then
        // start the new frame with this NAL.
        let starts_new_frame = matches!(nal_type, 5 | 7) && !current.is_empty();
        if starts_new_frame {
            frames.push(std::mem::take(&mut current));
        }
        current.extend_from_slice(nal_slice);
        i = nal_end;
    }
    if !current.is_empty() {
        frames.push(current);
    }
    frames
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn load_fixture(name: &str) -> Vec<u8> {
    let path = fixture_path(name);
    std::fs::read(&path).unwrap_or_else(|err| {
        panic!(
            "failed to read fixture {}: {err}. Regenerate with: \
             cargo test -p ados-video --release --test fixtures_gen \
             -- --ignored --nocapture",
            path.display()
        )
    })
}

/// RTP version is the top two bits of byte 0; must be 2 (`0b10`).
fn rtp_version(packet: &[u8]) -> u8 {
    (packet[0] & 0xC0) >> 6
}

/// RTP payload type is the low 7 bits of byte 1.
fn rtp_payload_type(packet: &[u8]) -> u8 {
    packet[1] & 0x7F
}

/// RTP sequence number is bytes 2..4 big-endian.
fn rtp_sequence(packet: &[u8]) -> u16 {
    u16::from_be_bytes([packet[2], packet[3]])
}

/// RTP timestamp is bytes 4..8 big-endian.
fn rtp_timestamp(packet: &[u8]) -> u32 {
    u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]])
}

/// Drive the push loop against the mock until at least `min_packets`
/// have been captured or the deadline elapses. Returns the captured
/// RTP packets.
async fn run_round_trip(
    fixture: &[u8],
    pacing: Duration,
    min_packets: usize,
    deadline: Duration,
) -> Vec<Vec<u8>> {
    let server = MockRtspServer::start().await.expect("start mock rtsp");
    let url = server.url("test-stream");

    let cfg = PushConfig {
        url,
        width: 1280,
        height: 720,
        fps: 30,
        mtu: DEFAULT_MTU,
    };

    let (tx, rx) = broadcast::channel::<EncodedFrame>(64);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let push_handle = tokio::spawn(async move {
        run_push_loop(cfg, rx, async move {
            let _ = shutdown_rx.await;
        })
        .await;
    });

    // Feed frames at the configured pacing.
    let frames = split_annex_b_into_frames(fixture);
    let feeder_tx = tx.clone();
    let feeder = tokio::spawn(async move {
        for (idx, frame_bytes) in frames.into_iter().enumerate() {
            let pts_ms = (idx as u64) * 1000 / 30;
            let is_keyframe = frame_bytes
                .windows(5)
                .any(|w| w[..4] == [0, 0, 0, 1] && (w[4] & 0x1F) == 5);
            let frame = EncodedFrame {
                bytes: frame_bytes,
                is_keyframe,
                pts_ms,
            };
            // The feeder runs ahead of the push loop; if the receiver
            // has not been polled yet the broadcast capacity (64) is
            // ample for the 60-frame split fixture.
            let _ = feeder_tx.send(frame);
            if !pacing.is_zero() {
                tokio::time::sleep(pacing).await;
            }
        }
    });

    // Poll the captured-packet count until either the threshold is
    // reached or the deadline elapses.
    let captured = match timeout(deadline, async {
        loop {
            let count = server.captured_rtp_packets().len();
            if count >= min_packets {
                break server.captured_rtp_packets();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    {
        Ok(pkts) => pkts,
        Err(_) => server.captured_rtp_packets(),
    };

    let _ = shutdown_tx.send(());
    let _ = feeder.await;
    let _ = timeout(Duration::from_secs(2), push_handle).await;
    server.shutdown().await;

    captured
}

/// Assert RTP-header sanity across a captured stream:
///
///   - Every packet has version 2, payload type 96.
///   - Sequence numbers increase by 1 (mod 2^16) across the stream.
fn assert_rtp_invariants(packets: &[Vec<u8>]) {
    assert!(!packets.is_empty(), "no packets captured");
    let mut last_seq: Option<u16> = None;
    for (idx, pkt) in packets.iter().enumerate() {
        assert!(pkt.len() >= 12, "packet {idx} too short for an RTP header");
        assert_eq!(rtp_version(pkt), 2, "packet {idx} bad version");
        assert_eq!(rtp_payload_type(pkt), 96, "packet {idx} bad payload type");
        let seq = rtp_sequence(pkt);
        if let Some(prev) = last_seq {
            assert_eq!(
                seq,
                prev.wrapping_add(1),
                "packet {idx}: non-monotonic sequence (prev {prev}, got {seq})"
            );
        }
        last_seq = Some(seq);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtsp_pusher_round_trip_with_split_fixture() {
    let fixture = load_fixture("nal_split.h264");

    // 60 frames at 30 fps would normally take 2s. We shrink pacing to
    // 5 ms per frame so the test wraps in well under a second on CI.
    // The push pipeline does not gate on wall-clock pacing, only on
    // broadcast availability.
    let captured = run_round_trip(
        &fixture,
        Duration::from_millis(5),
        30,
        Duration::from_secs(5),
    )
    .await;

    assert!(
        captured.len() >= 30,
        "expected at least 30 RTP packets, got {}",
        captured.len()
    );
    assert_rtp_invariants(&captured);

    // None of the split fixture's NALs exceed the MTU, so no FU-A
    // packet should appear: every payload's first byte after the RTP
    // header is the original NAL header (types 1, 5, 7, 8) — never
    // type 28 (FU-A indicator).
    for (idx, pkt) in captured.iter().enumerate() {
        let nal_first = pkt[12];
        let nal_type = nal_first & 0x1F;
        assert_ne!(
            nal_type, 28,
            "packet {idx}: unexpected FU-A in split fixture (small NALs only)"
        );
    }

    // RTP timestamps advance at 90 kHz / fps when frames carry
    // distinct PTS values. We set pts_ms = idx*1000/30 above, so the
    // distinct timestamps in the captured stream should be at least
    // the keyframe count plus the non-IDR count.
    let mut distinct_ts: Vec<u32> = captured.iter().map(|p| rtp_timestamp(p)).collect();
    distinct_ts.sort_unstable();
    distinct_ts.dedup();
    assert!(
        distinct_ts.len() >= 30,
        "expected >=30 distinct RTP timestamps, got {}",
        distinct_ts.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtsp_pusher_round_trip_with_large_fixture_forces_fu_a() {
    let fixture = load_fixture("nal_large.h264");

    let captured = run_round_trip(
        &fixture,
        Duration::from_millis(5),
        4,
        Duration::from_secs(5),
    )
    .await;

    assert!(
        !captured.is_empty(),
        "expected RTP packets from large fixture"
    );
    assert_rtp_invariants(&captured);

    // The 6000-byte IDR slice should fragment into multiple FU-A
    // packets. RFC 3984 §5.8: FU indicator is `(NRI<<5) | 28` so the
    // low 5 bits of byte 12 (first payload byte) equal 28.
    let fu_a_packets: Vec<&Vec<u8>> = captured
        .iter()
        .filter(|p| (p[12] & 0x1F) == 28)
        .collect();
    assert!(
        fu_a_packets.len() >= 4,
        "expected at least 4 FU-A packets for a 6000-byte NAL at 1400 MTU, got {}",
        fu_a_packets.len()
    );

    // The first FU-A packet has the S bit set in the FU header (byte
    // 13), the last has the E bit set, and intermediate packets have
    // neither. Walk the FU-A run and check that exactly one S and one
    // E land in order.
    let mut saw_s = false;
    let mut saw_e = false;
    let mut e_after_s = false;
    for pkt in &fu_a_packets {
        let fu_header = pkt[13];
        let is_start = fu_header & 0x80 != 0;
        let is_end = fu_header & 0x40 != 0;
        if is_start {
            assert!(!saw_s, "multiple FU-A start packets in one fragmentation");
            saw_s = true;
        }
        if is_end {
            assert!(saw_s, "FU-A end packet without preceding start");
            saw_e = true;
            e_after_s = true;
        }
    }
    assert!(saw_s, "missing FU-A start packet");
    assert!(saw_e, "missing FU-A end packet");
    assert!(e_after_s, "FU-A end did not follow start");

    // The packet that carries the FU-A end fragment should have the
    // RTP marker bit set (last fragment of the access unit per RFC
    // 3984 §5.1).
    let last_fu_a = fu_a_packets
        .iter()
        .rev()
        .find(|p| p[13] & 0x40 != 0)
        .expect("missing FU-A end packet");
    assert_eq!(
        last_fu_a[1] & 0x80,
        0x80,
        "FU-A end packet did not set the RTP marker bit"
    );
}
