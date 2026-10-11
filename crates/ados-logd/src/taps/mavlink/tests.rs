use super::*;
use ados_protocol::frame::encode_frame;
use std::io::Cursor;

/// Build a minimal but valid v1 frame: magic, payload_len, seq, sysid,
/// compid, msgid, then `payload_len` payload bytes (CRC bytes omitted; the
/// tap never validates CRC, only reads the header).
fn v1_frame(sysid: u8, compid: u8, msgid: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![MAGIC_V1, payload.len() as u8, 0, sysid, compid, msgid];
    f.extend_from_slice(payload);
    f
}

/// Build a minimal v2 frame header with a 3-byte little-endian msgid.
fn v2_frame(sysid: u8, compid: u8, msgid: u32, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![
        MAGIC_V2,
        payload.len() as u8,
        0, // incompat flags
        0, // compat flags
        0, // seq
        sysid,
        compid,
        (msgid & 0xff) as u8,
        ((msgid >> 8) & 0xff) as u8,
        ((msgid >> 16) & 0xff) as u8,
    ];
    f.extend_from_slice(payload);
    f
}

/// Frame a sequence of raw payloads into a length-prefixed byte stream the
/// deframer reads, the same wire shape the broadcast uses.
fn framed_stream(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in frames {
        out.extend(encode_frame(f, MAVLINK_MAX_FRAME).unwrap());
    }
    out
}

async fn run_against(stream: Vec<u8>, interval: Duration) -> Vec<EventFrame> {
    let (tx, mut rx) = mpsc::channel::<IngestFrame>(256);
    let reader = Cursor::new(stream);
    let mut shutdown = Shutdown::never();
    process_frames(reader, &tx, interval, &mut shutdown).await;
    drop(tx);
    let mut out = Vec::new();
    while let Some(f) = rx.recv().await {
        if let IngestFrame::Event(e) = f {
            out.push(e);
        }
    }
    out
}

#[test]
fn parse_header_reads_v1_and_v2_identity() {
    let v1 = v1_frame(7, 1, 30, &[0xaa, 0xbb]);
    let h1 = parse_header(&v1).unwrap();
    assert_eq!(
        (h1.version, h1.sysid, h1.compid, h1.msgid, h1.payload_len),
        (1, 7, 1, 30, 2)
    );

    let v2 = v2_frame(9, 2, 0x12345, &[0x01, 0x02, 0x03]);
    let h2 = parse_header(&v2).unwrap();
    assert_eq!(
        (h2.version, h2.sysid, h2.compid, h2.msgid, h2.payload_len),
        (2, 9, 2, 0x12345, 3)
    );

    // An unrecognized magic or a too-short frame yields no header.
    assert!(parse_header(&[0x00, 0x01, 0x02]).is_none());
    assert!(parse_header(&[MAGIC_V1, 0x00]).is_none());
    assert!(parse_header(&[]).is_none());
}

#[tokio::test]
async fn a_frame_event_carries_identity_and_a_truncated_head() {
    // A long payload so the head is truncated to PAYLOAD_HEAD_BYTES.
    let payload: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();
    let stream = framed_stream(&[v1_frame(1, 1, 33, &payload)]);
    // A zero interval emits every frame; this stream has exactly one.
    let events = run_against(stream, Duration::ZERO).await;
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.kind, "mavlink.frame");
    assert_eq!(e.severity, Level::Trace);
    assert_eq!(e.detail.get("msgid").and_then(|v| v.as_u64()), Some(33));
    assert_eq!(
        e.detail.get("mav_version").and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        e.detail.get("payload_len").and_then(|v| v.as_u64()),
        Some(200)
    );
    // The head decodes to exactly the first PAYLOAD_HEAD_BYTES payload bytes.
    let head_b64 = e
        .detail
        .get("payload_head")
        .and_then(|v| v.as_str())
        .unwrap();
    let head = base64::engine::general_purpose::STANDARD
        .decode(head_b64)
        .unwrap();
    let frame = v1_frame(1, 1, 33, &payload);
    assert_eq!(head, &frame[..PAYLOAD_HEAD_BYTES]);
}

#[tokio::test]
async fn rate_limit_drops_frames_inside_the_sample_window() {
    // Ten frames back-to-back at a 10s sample interval: only the first is
    // emitted; the rest fall inside the window and are dropped.
    let frames: Vec<Vec<u8>> = (0..10u8).map(|i| v1_frame(1, 1, i, &[i])).collect();
    let stream = framed_stream(&frames);
    let events = run_against(stream, Duration::from_secs(10)).await;
    assert_eq!(events.len(), 1, "only one frame escapes the sample window");
    assert_eq!(
        events[0].detail.get("msgid").and_then(|v| v.as_u64()),
        Some(0)
    );
}

#[tokio::test]
async fn a_zero_interval_emits_every_frame() {
    let frames: Vec<Vec<u8>> = (0..5u8).map(|i| v1_frame(1, 1, i, &[i])).collect();
    let stream = framed_stream(&frames);
    let events = run_against(stream, Duration::ZERO).await;
    assert_eq!(events.len(), 5, "no rate limit: every frame is emitted");
}

#[tokio::test]
async fn a_short_frame_still_records_an_event_without_a_header() {
    // A two-byte frame cannot carry a header; it is still sampled, with the
    // raw byte count and head, so a malformed frame is not silently lost.
    let stream = framed_stream(&[vec![0x01, 0x02]]);
    let events = run_against(stream, Duration::ZERO).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].detail.get("frame_bytes").and_then(|v| v.as_u64()),
        Some(2)
    );
    // No header fields were added for an unparseable frame.
    assert!(!events[0].detail.contains_key("msgid"));
}

#[test]
fn sample_interval_clamps_a_bad_rate_to_the_default() {
    assert_eq!(
        sample_interval(0.0),
        Duration::from_secs_f64(1.0 / DEFAULT_SAMPLE_HZ)
    );
    assert_eq!(
        sample_interval(-5.0),
        Duration::from_secs_f64(1.0 / DEFAULT_SAMPLE_HZ)
    );
    assert_eq!(
        sample_interval(f64::NAN),
        Duration::from_secs_f64(1.0 / DEFAULT_SAMPLE_HZ)
    );
    // A valid rate is honoured.
    assert_eq!(sample_interval(2.0), Duration::from_secs_f64(0.5));
}

#[tokio::test]
async fn process_frames_stops_when_the_writer_channel_closes() {
    let (tx, rx) = mpsc::channel::<IngestFrame>(1);
    drop(rx);
    let stream = framed_stream(&[v1_frame(1, 1, 0, &[1, 2, 3])]);
    let reader = Cursor::new(stream);
    let mut shutdown = Shutdown::never();
    tokio::time::timeout(
        Duration::from_secs(2),
        process_frames(reader, &tx, Duration::ZERO, &mut shutdown),
    )
    .await
    .expect("process_frames returns when the channel is closed");
}

#[tokio::test]
async fn run_mavlink_tap_against_an_absent_socket_retries_then_stops_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let (tx, _rx) = mpsc::channel::<IngestFrame>(8);
    let (stop, shutdown) = Shutdown::pair();
    let handle = tokio::spawn(run_mavlink_tap(path, tx, DEFAULT_SAMPLE_HZ, shutdown));
    tokio::time::sleep(Duration::from_millis(50)).await;
    stop.fire();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("frame tap stops within the bound")
        .expect("frame tap task did not panic");
}

#[tokio::test]
async fn run_mavlink_tap_reads_a_live_socket_then_reconnects_on_eof() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();

    let (tx, mut rx) = mpsc::channel::<IngestFrame>(64);
    let (stop, shutdown) = Shutdown::pair();
    // A zero sample interval so the single frame is emitted at once.
    let handle = tokio::spawn(run_mavlink_tap(path.clone(), tx, 1_000_000.0, shutdown));

    let (mut a, _addr) = listener.accept().await.unwrap();
    let frame = encode_frame(&v1_frame(1, 1, 42, &[9, 9, 9]), MAVLINK_MAX_FRAME).unwrap();
    a.write_all(&frame).await.unwrap();
    a.flush().await.unwrap();
    drop(a); // EOF -> reconnect

    let mut saw = false;
    for _ in 0..20 {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(IngestFrame::Event(e))) if e.kind == "mavlink.frame" => {
                assert_eq!(e.detail.get("msgid").and_then(|v| v.as_u64()), Some(42));
                saw = true;
                break;
            }
            Ok(Some(_)) => continue,
            _ => {}
        }
    }
    assert!(saw, "frame event from the first connection");

    let second = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .expect("the tap reconnects after EOF");
    assert!(second.is_ok());

    stop.fire();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}
