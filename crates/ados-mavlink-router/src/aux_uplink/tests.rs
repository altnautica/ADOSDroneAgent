use super::*;
use tokio::net::UdpSocket as TestSocket;

/// A MAVLink v2 frame carrying `payload_len` bytes.
fn mav2(payload_len: u8, seq: u8) -> Vec<u8> {
    let mut f = vec![0xFD, payload_len, 0, 0, seq, 1, 1, 0, 0, 0];
    f.extend(std::iter::repeat_n(0xAB, payload_len as usize));
    f.extend_from_slice(&[0x00, 0x00]); // checksum
    f
}

#[test]
fn a_chunk_that_fits_is_passed_through_whole() {
    let c = mav2(10, 1);
    let empty: &[u8] = &[];
    assert_eq!(split_oversize(&c), (vec![c.as_slice()], empty));
}

#[test]
fn an_oversize_chunk_is_split_on_frame_boundaries_rather_than_dropped() {
    // A mission or parameter burst arrives as one TCP read and used to be
    // handed to the encoder whole, rejected, and dropped with a warning and
    // no counter — a silent loss of exactly the traffic an operator is most
    // likely to be watching at the time.
    let mut chunk = Vec::new();
    let mut expected = 0usize;
    while chunk.len() <= AUX_MAX_PAYLOAD {
        chunk.extend_from_slice(&mav2(200, expected as u8));
        expected += 1;
    }
    let (pieces, remainder) = split_oversize(&chunk);
    assert!(remainder.is_empty());
    assert_eq!(pieces.len(), expected, "every frame must survive the split");
    for p in &pieces {
        assert!(
            p.len() <= AUX_MAX_PAYLOAD,
            "a piece still cannot be encoded"
        );
        assert_eq!(p[0], 0xFD, "each piece starts on a frame boundary");
    }
}

#[test]
fn an_oversize_chunk_carries_its_partial_tail_instead_of_losing_it() {
    let mut chunk = Vec::new();
    while chunk.len() <= AUX_MAX_PAYLOAD {
        chunk.extend_from_slice(&mav2(200, 0));
    }
    let next = mav2(200, 1);
    let tail = &next[..20];
    chunk.extend_from_slice(tail);
    let partial = AtomicU64::new(0);
    let pieces = pieces_to_send(&chunk, &partial);
    assert_eq!(pieces.last().copied(), Some(tail), "the tail is carried");
    assert_eq!(partial.load(Ordering::Relaxed), 0);
}

#[test]
fn a_tail_too_large_for_one_datagram_is_dropped_and_counted() {
    let mut chunk = mav2(10, 0);
    chunk.extend(std::iter::repeat_n(0x00u8, AUX_MAX_PAYLOAD + 10));
    let partial = AtomicU64::new(0);
    let pieces = pieces_to_send(&chunk, &partial);
    assert_eq!(pieces, vec![&chunk[..mav2(10, 0).len()]]);
    assert_eq!(partial.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_full_queue_reports_the_drop() {
    let (sender, _rx) = AuxUplinkSender::detached(2);
    assert!(sender.send(b"a"));
    assert!(sender.send(b"b"));
    assert!(!sender.send(b"c"), "a full queue must report the drop");
    assert_eq!(sender.queue_full_dropped(), 1);
}

#[test]
fn an_unparseable_oversize_chunk_is_passed_through_rather_than_guessed_at() {
    // Cutting at an arbitrary offset would deliver two halves that each
    // fail CRC on the far side and read as line noise. An honest encoder
    // rejection is better than a fabricated boundary.
    let chunk = vec![0x00u8; AUX_MAX_PAYLOAD + 50];
    let (pieces, remainder) = split_oversize(&chunk);
    assert_eq!(pieces.len(), 1);
    assert!(remainder.is_empty());
}

/// `start_paused` drives virtual time, so a steady stream can be simulated
/// faster than the batch window without the test sleeping for real.
#[tokio::test(start_paused = true)]
async fn a_steady_stream_still_flushes_on_the_window() {
    // The regression: the window was rebuilt on every loop iteration, so
    // each arriving frame reset it. A client sending faster than the window
    // never let it elapse and nothing went out until the batch happened to
    // reach the payload ceiling — seconds of added command latency on a
    // control link, clearing only when the client went quiet.
    let listener = TestSocket::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let sender = spawn(port);

    // Send steadily at half the batch window for well over one window.
    for i in 0..12u8 {
        sender.send(&mav2(4, i));
        tokio::time::sleep(BATCH_WINDOW / 2).await;
    }

    // Something must already have gone out: the window elapsed several
    // times over, and the batch is nowhere near the payload ceiling.
    let mut buf = [0u8; 4096];
    let got = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        listener.recv_from(&mut buf),
    )
    .await;
    assert!(
        got.is_ok(),
        "a steady stream never flushed; the batch window is being reset by \
             every arrival instead of running from the first frame"
    );
}

#[tokio::test]
async fn a_sent_frame_arrives_framed_on_the_target_port() {
    let listener = TestSocket::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let sender = spawn(port);
    sender.send(b"\xfdhello-frame-bytes");

    let mut buf = [0u8; 256];
    let (n, _) = tokio::time::timeout(Duration::from_millis(500), listener.recv_from(&mut buf))
        .await
        .expect("no datagram arrived within the batch window")
        .unwrap();

    let (channel, payload) =
        aux_mux::decode(&buf[..n]).expect("must decode as a valid aux datagram");
    assert_eq!(channel, AuxChannel::Mavlink);
    assert_eq!(payload, b"\xfdhello-frame-bytes");
}

#[tokio::test]
async fn several_quick_frames_batch_into_one_datagram() {
    let listener = TestSocket::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let sender = spawn(port);
    sender.send(b"AAA");
    sender.send(b"BBB");
    sender.send(b"CCC");

    let mut buf = [0u8; 256];
    let (n, _) = tokio::time::timeout(Duration::from_millis(500), listener.recv_from(&mut buf))
        .await
        .expect("no datagram arrived")
        .unwrap();

    let (_, payload) = aux_mux::decode(&buf[..n]).unwrap();
    assert_eq!(payload, b"AAABBBCCC");

    // Only one datagram — the three frames shared one batch window rather
    // than each triggering its own radio transmission.
    let none_more =
        tokio::time::timeout(Duration::from_millis(80), listener.recv_from(&mut buf)).await;
    assert!(none_more.is_err(), "expected no second datagram");
}
