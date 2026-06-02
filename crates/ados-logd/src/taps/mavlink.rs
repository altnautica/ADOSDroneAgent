//! The raw-frame tap.
//!
//! Connects to the MAVLink frame broadcast, deframes the 4-byte big-endian
//! length-prefixed raw frames, and records a rate-limited sample of them as
//! [`EventFrame`]s for deep diagnosis. This is the durable counterpart to the
//! raw frame stream that is otherwise only on the network relay and lost the
//! moment the link drops.
//!
//! Deliberately shallow: it reads only the frame header (magic, length, sysid,
//! compid, msgid) and a short truncated head of the payload. It does NOT run a
//! semantic message decoder. The semantic arm/mode/flight-session signals come
//! from the state tap; this tap is the byte-level flight recorder, sampled so it
//! does not write the full firehose to flash.
//!
//! Connection management and frame processing are split for testability:
//! [`process_frames`] consumes any async byte source (a fixture file, a socket
//! half, an in-memory pipe), so a test feeds synthetic length-prefixed frames
//! without a live socket. [`run_mavlink_tap`] wraps it with the connect-then-
//! reconnect-with-backoff loop the daemon spawns. The socket being absent is
//! normal (no agent on a host, an idle agent before the router is up) and is
//! logged at debug, not as an error. It only ever reads.

use std::path::Path;
use std::time::Duration;

use base64::Engine;
use tokio::io::AsyncRead;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::time::Instant;

use ados_protocol::frame::MAVLINK_MAX_FRAME;
use ados_protocol::ipc::read_length_prefixed;
use ados_protocol::logd::{EventFrame, IngestFrame, Level};

use super::backoff::ReconnectBackoff;
use super::{Shutdown, SOURCE_MAVLINK};
use crate::writer::now_us;

/// MAVLink v1 start-of-frame magic.
const MAGIC_V1: u8 = 0xfe;
/// MAVLink v2 start-of-frame magic.
const MAGIC_V2: u8 = 0xfd;

/// How many leading payload bytes are captured in the event detail. Enough to
/// tell the message apart and see a hint of its contents during diagnosis,
/// small enough that the sampled stream is cheap on flash.
const PAYLOAD_HEAD_BYTES: usize = 32;

/// Default sample rate: emit at most one frame event per second. Sized so the
/// raw recorder is a low-rate trail, not the full firehose; tunable per the
/// config surface.
pub const DEFAULT_SAMPLE_HZ: f64 = 1.0;

/// The decoded-enough view of one raw frame: the fields the tap records without
/// running a semantic decoder.
struct FrameHead {
    /// The MAVLink protocol version inferred from the magic byte (1 or 2).
    version: u8,
    /// Message id.
    msgid: u32,
    /// System id of the sender.
    sysid: u8,
    /// Component id of the sender.
    compid: u8,
    /// Declared payload length.
    payload_len: usize,
}

/// Run the raw-frame tap until `shutdown` resolves.
///
/// Connects to `socket_path`, samples the frame stream at `sample_hz`, and
/// reconnects with capped backoff on any disconnect or an absent socket. A
/// non-positive `sample_hz` is clamped to the default so a misconfiguration
/// cannot disable sampling silently or divide by zero.
pub async fn run_mavlink_tap(
    socket_path: impl AsRef<Path>,
    tx: mpsc::Sender<IngestFrame>,
    sample_hz: f64,
    mut shutdown: Shutdown,
) {
    let socket_path = socket_path.as_ref();
    let interval = sample_interval(sample_hz);
    let mut backoff = ReconnectBackoff::default();
    tracing::info!(
        path = %socket_path.display(),
        sample_hz,
        "frame tap started"
    );
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => {
                tracing::info!("frame tap stopping");
                return;
            }
            connected = connect(socket_path) => {
                match connected {
                    Some(stream) => {
                        backoff.reset();
                        process_frames(stream, &tx, interval, &mut shutdown).await;
                        if tx.is_closed() {
                            return;
                        }
                    }
                    None => {
                        let wait = backoff.next_delay();
                        tokio::select! {
                            _ = shutdown.recv() => {
                                tracing::info!("frame tap stopping");
                                return;
                            }
                            _ = tokio::time::sleep(wait) => {}
                        }
                    }
                }
            }
        }
    }
}

/// The minimum spacing between emitted frame events for a given sample rate.
fn sample_interval(sample_hz: f64) -> Duration {
    let hz = if sample_hz.is_finite() && sample_hz > 0.0 {
        sample_hz
    } else {
        DEFAULT_SAMPLE_HZ
    };
    Duration::from_secs_f64(1.0 / hz)
}

/// Try once to connect. Returns `None` (logged at debug) when the socket is
/// absent or refuses, so the caller backs off and retries.
async fn connect(path: &Path) -> Option<UnixStream> {
    match UnixStream::connect(path).await {
        Ok(s) => {
            tracing::debug!(path = %path.display(), "frame socket connected");
            Some(s)
        }
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "frame socket absent; will retry");
            None
        }
    }
}

/// Deframe and sample a raw-frame stream to EOF (or until the writer channel
/// closes or shutdown fires). At most one frame is emitted per `interval`; the
/// rest advance the deframer but produce no row, so the sampled trail stays
/// bounded regardless of the link rate.
///
/// This is the injectable seam: `reader` is any async byte source.
async fn process_frames<R>(
    mut reader: R,
    tx: &mpsc::Sender<IngestFrame>,
    interval: Duration,
    shutdown: &mut Shutdown,
) where
    R: AsyncRead + Unpin,
{
    // Allow the first frame immediately by seeding the last-emit time one full
    // interval in the past.
    let mut last_emit = Instant::now()
        .checked_sub(interval)
        .unwrap_or_else(Instant::now);
    loop {
        let next = tokio::select! {
            biased;
            _ = shutdown.recv() => return,
            r = read_length_prefixed(&mut reader, MAVLINK_MAX_FRAME, false) => r,
        };
        match next {
            Ok(Some(payload)) => {
                let now = Instant::now();
                if now.duration_since(last_emit) < interval {
                    // Within the sample window: count nothing, drop the frame.
                    continue;
                }
                last_emit = now;
                let event = build_event(&payload);
                if tx.send(IngestFrame::Event(event)).await.is_err() {
                    return; // writer side gone
                }
            }
            Ok(None) => return, // clean EOF
            Err(e) => {
                tracing::debug!(error = %e, "frame stream read error");
                return;
            }
        }
    }
}

/// Build the event row for one sampled raw frame. A frame too short to carry a
/// header still yields an event (with `payload_len=0` and whatever head bytes
/// exist) so a truncated or malformed frame is recorded rather than dropped
/// silently.
fn build_event(payload: &[u8]) -> EventFrame {
    let ts = now_us();
    let mut ev = EventFrame::new(ts, "mavlink.frame", SOURCE_MAVLINK, Level::Trace);
    let head_len = payload.len().min(PAYLOAD_HEAD_BYTES);
    let head_b64 = base64::engine::general_purpose::STANDARD.encode(&payload[..head_len]);
    ev.detail.insert(
        "frame_bytes".to_string(),
        rmpv::Value::from(payload.len() as u64),
    );
    ev.detail
        .insert("payload_head".to_string(), rmpv::Value::from(head_b64));
    if let Some(h) = parse_header(payload) {
        ev.detail.insert(
            "mav_version".to_string(),
            rmpv::Value::from(h.version as u64),
        );
        ev.detail
            .insert("msgid".to_string(), rmpv::Value::from(h.msgid as u64));
        ev.detail
            .insert("sysid".to_string(), rmpv::Value::from(h.sysid as u64));
        ev.detail
            .insert("compid".to_string(), rmpv::Value::from(h.compid as u64));
        ev.detail.insert(
            "payload_len".to_string(),
            rmpv::Value::from(h.payload_len as u64),
        );
    }
    ev
}

/// Parse just enough of a raw frame header to identify the message. Supports
/// both wire versions; returns `None` for an unrecognized magic or a frame too
/// short to carry the version's header. No semantic payload decoding.
///
/// v1 header: `fe len seq sysid compid msgid` (6 bytes), msgid one byte.
/// v2 header: `fd len incompat compat seq sysid compid msgid(3 LE)` (10 bytes),
/// msgid three little-endian bytes.
fn parse_header(payload: &[u8]) -> Option<FrameHead> {
    match payload.first().copied()? {
        MAGIC_V1 if payload.len() >= 6 => Some(FrameHead {
            version: 1,
            payload_len: payload[1] as usize,
            sysid: payload[3],
            compid: payload[4],
            msgid: payload[5] as u32,
        }),
        MAGIC_V2 if payload.len() >= 10 => {
            let msgid =
                (payload[7] as u32) | ((payload[8] as u32) << 8) | ((payload[9] as u32) << 16);
            Some(FrameHead {
                version: 2,
                payload_len: payload[1] as usize,
                sysid: payload[5],
                compid: payload[6],
                msgid,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
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
}
