//! [`RtspSource`] against a stream served over RTSP: a minimal server on
//! loopback (RTSP-over-TCP, RTP interleaved on channel 0) sending an H.264
//! stream the real `ffmpeg` encodes first.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;
const FPS: u32 = 20;
const PICTURES: usize = 200;
/// RTP payload size at most, as a server forwarding 1440-byte packets sends.
const MAX_PAYLOAD: usize = 1400;
/// The gap between access units sent: five times the stream's rate, as a
/// camera paces them (all at once would outrun the pairing's
/// [`MAX_PENDING`] before the decoder's first frame).
const SEND_INTERVAL: Duration = Duration::from_millis(10);

/// The encoded stream's parameter sets and its access units with every SPS
/// and PPS taken out.
struct Stream {
    sps: Vec<u8>,
    pps: Vec<u8>,
    access_units: Vec<Vec<Vec<u8>>>,
}

fn encode() -> Stream {
    let size = format!("testsrc=size={WIDTH}x{HEIGHT}:rate={FPS}");
    let encoded = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &size,
        ])
        .args(["-frames:v", &PICTURES.to_string()])
        .args(["-c:v", "libx264", "-bf", "0", "-g", "10", "-f", "h264", "-"])
        .output()
        .expect("ffmpeg is installed");
    assert!(encoded.status.success() && !encoded.stdout.is_empty());
    let mut scanner = AnnexBScanner::default();
    scanner.push(&encoded.stdout);
    // A trailing start code completes the last NAL.
    scanner.push(&[0, 0, 1]);
    let (mut sps, mut pps) = (None, None);
    let mut access_units: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut prefix = Vec::new();
    while let Some(nal) = scanner.next_nal() {
        match nal_type(&nal) {
            NAL_SPS => {
                sps.get_or_insert(nal);
            }
            NAL_PPS => {
                pps.get_or_insert(nal);
            }
            crate::sei::NAL_SLICE | NAL_IDR if starts_picture(&nal) => {
                let mut au = std::mem::take(&mut prefix);
                au.push(nal);
                access_units.push(au);
            }
            crate::sei::NAL_SLICE | NAL_IDR => access_units
                .last_mut()
                .expect("a slice after its picture's first")
                .push(nal),
            _ => prefix.push(nal),
        }
    }
    assert_eq!(access_units.len(), PICTURES);
    Stream {
        sps: sps.expect("the stream has an SPS"),
        pps: pps.expect("the stream has a PPS"),
        access_units,
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The RTP payloads of one NAL: the NAL itself, or FU-A fragments of it.
fn payloads(nal: &[u8]) -> Vec<Vec<u8>> {
    if nal.len() <= MAX_PAYLOAD {
        return vec![nal.to_vec()];
    }
    let indicator = (nal[0] & 0xe0) | 28;
    let chunks: Vec<&[u8]> = nal[1..].chunks(MAX_PAYLOAD - 2).collect();
    let last = chunks.len() - 1;
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let start = if i == 0 { 0x80 } else { 0 };
            let end = if i == last { 0x40 } else { 0 };
            let mut p = vec![indicator, start | end | (nal[0] & 0x1f)];
            p.extend_from_slice(chunk);
            p
        })
        .collect()
}

/// One interleaved RTP packet on channel 0.
fn interleaved(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
    let len = u16::try_from(12 + payload.len()).expect("packet fits a frame");
    let mut out = vec![b'$', 0];
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&[0x80, if marker { 0x80 | 96 } else { 96 }]);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&ts.to_be_bytes());
    out.extend_from_slice(&0x5745_0001u32.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// The next request's method and `CSeq`, skipping interleaved frames the
/// client sends (RTCP receiver reports); `None` once the client hangs up.
async fn request(conn: &mut TcpStream, buf: &mut Vec<u8>) -> Option<(String, String)> {
    loop {
        if buf.first() == Some(&b'$') && buf.len() >= 4 {
            let frame = 4 + usize::from(u16::from_be_bytes([buf[2], buf[3]]));
            if buf.len() >= frame {
                buf.drain(..frame);
                continue;
            }
        } else if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            buf.drain(..end + 4);
            let method = head.split_whitespace().next()?.to_string();
            let cseq = head
                .lines()
                .find_map(|l| l.strip_prefix("CSeq:"))
                .map(|v| v.trim().to_string())
                .unwrap_or_default();
            return Some((method, cseq));
        }
        let mut chunk = [0u8; 4096];
        match conn.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Where the served stream carries its parameter sets.
#[derive(Clone, Copy, PartialEq)]
enum ParameterSets {
    /// In the SDP only (`sprop-parameter-sets`).
    SdpOnly,
    /// In-band only, before every IDR picture; the SDP names none.
    InBandOnly,
}

/// Serve `stream` until one client has received all of it, taking clients one
/// after another (a demuxer that gives up and reconnects gets a new session).
async fn serve(listener: TcpListener, stream: Stream, sets: ParameterSets) {
    loop {
        let (conn, _) = listener.accept().await.expect("the demuxer connects");
        if serve_one(conn, &stream, sets).await {
            return;
        }
    }
}

/// Serve one client: OPTIONS, DESCRIBE, SETUP over TCP, PLAY, then every
/// access unit one [`SEND_INTERVAL`] apart. `true` once all of it was sent.
async fn serve_one(mut conn: TcpStream, stream: &Stream, sets: ParameterSets) -> bool {
    let mut buf = Vec::new();
    let sprop = match sets {
        ParameterSets::SdpOnly => format!(
            ";sprop-parameter-sets={},{}",
            base64(&stream.sps),
            base64(&stream.pps)
        ),
        ParameterSets::InBandOnly => String::new(),
    };
    while let Some((method, cseq)) = request(&mut conn, &mut buf).await {
        let reply = match method.as_str() {
            "DESCRIBE" => {
                let sdp = format!(
                    "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=cam\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
                     m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
                     a=fmtp:96 packetization-mode=1{sprop}\r\n\
                     a=control:trackID=0\r\n"
                );
                format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\n\
                     Content-Length: {}\r\n\r\n{sdp}",
                    sdp.len()
                )
            }
            "SETUP" => format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 1\r\n\
                 Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
            ),
            _ => format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 1\r\n\
                 Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n\r\n"
            ),
        };
        if conn.write_all(reply.as_bytes()).await.is_err() {
            return false;
        }
        if method == "PLAY" {
            break;
        }
    }
    let mut seq = 0u16;
    let mut pace = tokio::time::interval(SEND_INTERVAL);
    for (i, au) in stream.access_units.iter().enumerate() {
        pace.tick().await;
        let ts = u32::try_from(i).expect("few pictures") * (90_000 / FPS);
        let idr = au.iter().any(|nal| nal_type(nal) == NAL_IDR);
        let in_band = (sets == ParameterSets::InBandOnly && idr)
            .then_some([&stream.sps, &stream.pps])
            .into_iter()
            .flatten();
        let packets: Vec<Vec<u8>> = in_band
            .chain(au.iter())
            .flat_map(|nal| payloads(nal))
            .collect();
        for (j, payload) in packets.iter().enumerate() {
            let packet = interleaved(seq, ts, j + 1 == packets.len(), payload);
            if conn.write_all(&packet).await.is_err() {
                return false;
            }
            seq = seq.wrapping_add(1);
        }
    }
    true
}

/// Pull the stream served with `sets` through an [`RtspSource`]: the frames
/// decoded, and the error that ended the stream.
async fn decode_served(sets: ParameterSets) -> (usize, String) {
    let stream = encode();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let server = tokio::spawn(serve(listener, stream, sets));
    let mut source = RtspSource::new(
        "cam",
        RtspSettings {
            url: format!("rtsp://127.0.0.1:{port}/cam"),
            width: WIDTH,
            height: HEIGHT,
            format: FrameFormat::Gray8,
            decode_threads: 1,
        },
        None,
        SimClock::default(),
    );
    let mut frames = 0usize;
    let end = loop {
        match tokio::time::timeout(Duration::from_secs(20), source.next_frame()).await {
            Ok(Ok(frame)) => {
                assert_eq!(frame.data.len(), (WIDTH * HEIGHT) as usize);
                frames += 1;
            }
            Ok(Err(e)) => break e.to_string(),
            Err(_) => panic!("no frame for 20 s after {frames} frames"),
        }
    };
    server.await.expect("server task");
    (frames, end)
}

/// A stream whose parameter sets travel only in the SDP decodes: an encoder
/// that writes global headers publishes them there alone, and a server that
/// forwards its RTP packets as sent keeps it so. The decoder on the pipe gets
/// the parameter sets in-band from the demuxer, so frames come out and pair
/// with their pictures to the end of the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_with_its_parameter_sets_only_in_the_sdp_decodes() {
    let (frames, end) = decode_served(ParameterSets::SdpOnly).await;
    // The pictures from the first IDR on reach the decoder.
    assert!(
        frames >= PICTURES / 4,
        "{frames} frames decoded of {PICTURES} sent, then: {end}"
    );
    assert!(!end.contains("pairing lost"), "{end}");
}

/// A stream whose parameter sets travel only in-band fails the quick open (its
/// size is unknown until the first SPS arrives), and the source reopens it with
/// the stream probe rather than reporting the source failed: frames come out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_with_its_parameter_sets_only_in_band_decodes() {
    let (frames, end) = decode_served(ParameterSets::InBandOnly).await;
    assert!(
        frames >= PICTURES / 4,
        "{frames} frames decoded of {PICTURES} sent, then: {end}"
    );
    assert!(!end.contains("pairing lost"), "{end}");
}
