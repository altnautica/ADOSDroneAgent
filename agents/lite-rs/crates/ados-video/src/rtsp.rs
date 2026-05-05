//! RTSP push client for the lite agent video pipeline.
//!
//! The lite agent does not run an RTSP *server* — it pushes its
//! encoded H.264 stream to a relay endpoint that the cloud (or a local
//! mediamtx instance) advertises. Push uses the standard RTSP/1.0
//! handshake (RFC 2326) followed by RTP payload (RFC 3984) framed
//! over the same TCP connection per RFC 2326 §10.12 (interleaved
//! binary frames with `$ <channel> <length>` framing).
//!
//! Resilience. The push client owns its own reconnect loop with
//! capped exponential backoff, mirroring the cloud relay client's
//! pattern. A dropped TCP connection makes the loop reconnect and
//! re-send `OPTIONS / ANNOUNCE / SETUP / RECORD` from the top.
//!
//! Implementation notes:
//!
//! - Single video track on RTP channel 0, RTCP on channel 1. We do
//!   not currently consume RTCP receiver reports; the relay's bitrate
//!   adaptation is done over a separate signalling channel today.
//! - FU-A fragmentation kicks in for NAL units > MTU (default 1400
//!   bytes). RFC 3984 §5.8 specifies the on-wire shape.
//! - Sequence number wraps at 2^16; SSRC is fixed for the lifetime of
//!   the connection and re-rolled on every reconnect.

use std::time::Duration;

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;

use crate::EncodedFrame;

/// Maximum RTP payload size before FU-A fragmentation kicks in. 1400
/// is a safe default for typical Ethernet/Wi-Fi links and matches what
/// FFmpeg's RTSP muxer uses by default.
pub const DEFAULT_MTU: usize = 1400;

/// RTP payload type for H.264 over RTP. RFC 3984 says 96-127 are
/// dynamic; we pick 96 to match what every RTSP server we tested
/// against advertises in its SDP.
pub const RTP_PAYLOAD_TYPE_H264: u8 = 96;

/// RTP clock rate for video tracks. RFC 3984 §8.2.1 mandates 90 kHz.
pub const RTP_CLOCK_HZ: u32 = 90_000;

/// Initial backoff after a reconnect failure.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Cap on the exponential backoff. The push client logs at WARN every
/// time it hits the cap so an operator can tell the difference between
/// "noisy network" and "cloud relay is permanently down".
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// RTSP method tokens we send.
mod method {
    pub const OPTIONS: &str = "OPTIONS";
    pub const ANNOUNCE: &str = "ANNOUNCE";
    pub const SETUP: &str = "SETUP";
    pub const RECORD: &str = "RECORD";
    pub const TEARDOWN: &str = "TEARDOWN";
}

/// Configuration for the RTSP push client.
#[derive(Debug, Clone)]
pub struct PushConfig {
    /// Full RTSP URL, e.g. `rtsp://relay.example.com:8554/drone-42`.
    pub url: String,
    /// Encoder width in pixels. Embedded in the SDP profile-level-id
    /// hint; the relay uses it for bitrate planning.
    pub width: u32,
    /// Encoder height in pixels.
    pub height: u32,
    /// Encoder frame rate. Sent as the `a=framerate:` SDP attribute.
    pub fps: u32,
    /// Maximum RTP payload size before FU-A fragmentation. Lower this
    /// if the operator's link reports MTU-related drops.
    pub mtu: usize,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            width: 1280,
            height: 720,
            fps: 30,
            mtu: DEFAULT_MTU,
        }
    }
}

/// Errors the RTSP push client surfaces. Reconnect failures are
/// folded into `Io` so the supervise loop can react uniformly.
#[derive(Debug, thiserror::Error)]
pub enum RtspError {
    #[error("invalid RTSP URL: {0}")]
    InvalidUrl(String),

    #[error("RTSP server returned status {code}: {reason}")]
    Status { code: u16, reason: String },

    #[error("RTSP I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("RTSP protocol error: {0}")]
    Protocol(String),
}

/// Parsed RTSP URL. Limited to the subset the push client uses: host,
/// port, path. We avoid pulling in a full URL crate so the dependency
/// graph stays small for the musl-static lite agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl ParsedUrl {
    /// Parse the subset of URL shapes the RTSP push client supports:
    /// `rtsp://host[:port]/path`. No userinfo, no fragments, no
    /// queries (the relay uses path components, not query strings,
    /// for the device id).
    pub fn parse(input: &str) -> Result<Self, RtspError> {
        let rest = input
            .strip_prefix("rtsp://")
            .ok_or_else(|| RtspError::InvalidUrl(format!("missing rtsp:// scheme in {input}")))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(RtspError::InvalidUrl(format!(
                "empty authority in {input}"
            )));
        }
        let (host, port) = match authority.rfind(':') {
            Some(i) => {
                let host = &authority[..i];
                let port_s = &authority[i + 1..];
                let port: u16 = port_s
                    .parse()
                    .map_err(|_| RtspError::InvalidUrl(format!("invalid port in {input}")))?;
                (host.to_string(), port)
            }
            None => (authority.to_string(), 554),
        };
        Ok(Self {
            host,
            port,
            path: path.to_string(),
        })
    }
}

/// Build the SDP body for an ANNOUNCE request describing one H.264
/// video track on RTP payload type 96. RFC 3984 §8 covers the
/// `fmtp:96 packetization-mode=1` line; mode 1 is the standard
/// FU-A-capable shape.
pub fn build_sdp(config: &PushConfig, parsed: &ParsedUrl) -> String {
    // Session-level lines first, then a single video media line.
    // The connection address is `0.0.0.0` for push because the
    // server, not the client, will allocate the receive port.
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 0.0.0.0\r\n\
         s=ADOS Drone Agent stream\r\n\
         c=IN IP4 0.0.0.0\r\n\
         t=0 0\r\n\
         a=tool:ados-agent-lite\r\n\
         a=range:npt=0-\r\n\
         a=control:rtsp://{}:{}{}\r\n\
         m=video 0 RTP/AVP {}\r\n\
         a=rtpmap:{} H264/{}\r\n\
         a=fmtp:{} packetization-mode=1\r\n\
         a=framerate:{}\r\n\
         a=control:trackID=0\r\n",
        parsed.host,
        parsed.port,
        parsed.path,
        RTP_PAYLOAD_TYPE_H264,
        RTP_PAYLOAD_TYPE_H264,
        RTP_CLOCK_HZ,
        RTP_PAYLOAD_TYPE_H264,
        config.fps
    )
}

/// Build a single RTSP request as a UTF-8 string. CSeq is required by
/// RFC 2326. Body is appended as-is; the caller computes
/// `Content-Length` for us.
pub fn build_request(
    method_tok: &str,
    url: &str,
    cseq: u32,
    extra: &[(&str, &str)],
    body: Option<&str>,
) -> String {
    let mut out = String::with_capacity(256);
    out.push_str(&format!("{method_tok} {url} RTSP/1.0\r\n"));
    out.push_str(&format!("CSeq: {cseq}\r\n"));
    out.push_str("User-Agent: ados-agent-lite\r\n");
    for (k, v) in extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        out.push_str("Content-Type: application/sdp\r\n");
        out.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    out.push_str("\r\n");
    if let Some(b) = body {
        out.push_str(b);
    }
    out
}

/// Parse an RTSP response head (everything up to and including the
/// blank line). Returns the status code, reason phrase, and any
/// `Session:` value. On the push path we don't currently parse
/// `Transport:` because we already pinned the channels; future
/// transport-aware code can extend this helper.
pub fn parse_response_head(head: &str) -> Result<RtspResponseHead, RtspError> {
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| RtspError::Protocol("empty response".into()))?;
    let mut parts = status_line.splitn(3, ' ');
    let _proto = parts
        .next()
        .ok_or_else(|| RtspError::Protocol("missing RTSP version".into()))?;
    let code_s = parts
        .next()
        .ok_or_else(|| RtspError::Protocol("missing status code".into()))?;
    let reason = parts.next().unwrap_or("").to_string();
    let code: u16 = code_s
        .parse()
        .map_err(|_| RtspError::Protocol(format!("invalid status code {code_s}")))?;

    let mut session = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("Session:") {
            // The session header may carry a `;timeout=...` suffix per
            // RFC 2326 §12.37; strip everything after the first `;`.
            let s = rest.trim();
            let primary = s.split(';').next().unwrap_or("").trim().to_string();
            if !primary.is_empty() {
                session = Some(primary);
            }
        }
    }

    Ok(RtspResponseHead {
        code,
        reason,
        session,
    })
}

/// Subset of an RTSP response head the push client cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspResponseHead {
    pub code: u16,
    pub reason: String,
    pub session: Option<String>,
}

/// Build an RTP packet header (12 bytes, no CSRCs, no extensions).
/// `marker` should be set on the last fragment of an access unit per
/// RFC 3984 §5.1 so the receiver's jitter buffer can flush.
pub fn build_rtp_header(seq: u16, ts: u32, ssrc: u32, marker: bool) -> [u8; 12] {
    let mut hdr = [0u8; 12];
    // Version=2, no padding, no extension, no CSRCs.
    hdr[0] = 0x80;
    // Marker bit + payload type.
    hdr[1] = if marker {
        0x80 | RTP_PAYLOAD_TYPE_H264
    } else {
        RTP_PAYLOAD_TYPE_H264
    };
    hdr[2..4].copy_from_slice(&seq.to_be_bytes());
    hdr[4..8].copy_from_slice(&ts.to_be_bytes());
    hdr[8..12].copy_from_slice(&ssrc.to_be_bytes());
    hdr
}

/// Wrap an RTP packet in an RFC 2326 §10.12 interleaved binary frame.
/// The TCP stream sees `$ <channel:u8> <length:u16-be> <rtp-bytes>`.
pub fn frame_interleaved(channel: u8, rtp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + rtp.len());
    out.push(b'$');
    out.push(channel);
    out.extend_from_slice(&(rtp.len() as u16).to_be_bytes());
    out.extend_from_slice(rtp);
    out
}

/// Strip the leading Annex-B start code from a NAL unit so the bytes
/// can be packed into RTP per RFC 3984. Returns the slice after the
/// start code, plus the NAL header byte. Returns `None` if no start
/// code is present (the caller should drop the unit).
pub fn split_nal_unit(unit: &[u8]) -> Option<(u8, &[u8])> {
    if unit.len() >= 4 && unit[..4] == [0, 0, 0, 1] {
        return unit.get(4).map(|h| (*h, &unit[5..]));
    }
    if unit.len() >= 3 && unit[..3] == [0, 0, 1] {
        return unit.get(3).map(|h| (*h, &unit[4..]));
    }
    None
}

/// Packetize a single NAL unit into one or more RTP packets. RFC 3984
/// §5.6 single-NAL packets when small enough; §5.8 FU-A when larger.
pub fn packetize_nal(
    header: u8,
    body: &[u8],
    seq: &mut u16,
    ts: u32,
    ssrc: u32,
    mtu: usize,
    marker_on_last: bool,
) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    // 12 bytes of RTP header, plus 1 byte of NAL header for single
    // NAL packets. For FU-A we need 2 bytes of FU header + the
    // packets share the same RTP timestamp.
    let single_nal_capacity = mtu.saturating_sub(12);
    if body.len() < single_nal_capacity {
        // Single NAL packet: prepend the NAL header byte verbatim.
        let mut buf = BytesMut::with_capacity(12 + 1 + body.len());
        buf.put_slice(&build_rtp_header(*seq, ts, ssrc, marker_on_last));
        buf.put_u8(header);
        buf.put_slice(body);
        out.push(buf.to_vec());
        *seq = seq.wrapping_add(1);
        return out;
    }

    // FU-A fragmentation. RFC 3984 §5.8:
    //   FU indicator (1 byte) = (NAL header & 0xE0) | 28
    //   FU header    (1 byte) = S|E|R|nal_unit_type
    let fu_indicator: u8 = (header & 0xE0) | 28;
    let nal_type: u8 = header & 0x1F;
    let payload_per_packet = mtu.saturating_sub(12 + 2);
    let mut offset = 0usize;
    let body_len = body.len();
    while offset < body_len {
        let remaining = body_len - offset;
        let take = remaining.min(payload_per_packet);
        let is_first = offset == 0;
        let is_last = offset + take == body_len;
        let mut fu_header: u8 = nal_type;
        if is_first {
            fu_header |= 0x80;
        }
        if is_last {
            fu_header |= 0x40;
        }
        let marker = is_last && marker_on_last;
        let mut buf = BytesMut::with_capacity(12 + 2 + take);
        buf.put_slice(&build_rtp_header(*seq, ts, ssrc, marker));
        buf.put_u8(fu_indicator);
        buf.put_u8(fu_header);
        buf.put_slice(&body[offset..offset + take]);
        out.push(buf.to_vec());
        *seq = seq.wrapping_add(1);
        offset += take;
    }
    out
}

/// Run the RTSP push loop with reconnect. Reads frames from
/// `frame_rx` and pushes them to the relay. Returns when the receiver
/// drops (the encoder pipeline shut down) or the supplied stop
/// future fires.
pub async fn run_push_loop<F>(
    config: PushConfig,
    mut frame_rx: broadcast::Receiver<EncodedFrame>,
    shutdown: F,
) where
    F: std::future::Future<Output = ()> + Send,
{
    tokio::pin!(shutdown);
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let push_one = async {
            push_session(&config, &mut frame_rx).await
        };
        tokio::select! {
            res = push_one => {
                match res {
                    Ok(()) => {
                        tracing::info!("rtsp push session ended cleanly; reconnecting");
                        backoff = INITIAL_BACKOFF;
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            backoff_secs = backoff.as_secs(),
                            "rtsp push session failed; backing off",
                        );
                    }
                }
            }
            _ = &mut shutdown => {
                tracing::info!("rtsp push loop received shutdown");
                return;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = &mut shutdown => return,
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One TCP-lived RTSP push session. Returns `Ok` on a clean teardown
/// (e.g. the encoder pipeline ended), `Err` on any I/O or protocol
/// fault. Either return path makes the supervise loop reconnect.
async fn push_session(
    config: &PushConfig,
    frame_rx: &mut broadcast::Receiver<EncodedFrame>,
) -> Result<(), RtspError> {
    let parsed = ParsedUrl::parse(&config.url)?;
    let stream = TcpStream::connect((parsed.host.as_str(), parsed.port)).await?;
    stream.set_nodelay(true)?;
    let (read_half, write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let mut writer = write_half;

    let mut cseq: u32 = 1;

    // OPTIONS — sanity check that the server speaks RTSP/1.0.
    write_request(&mut writer, method::OPTIONS, &config.url, cseq, &[], None).await?;
    let _ = read_response(&mut reader).await?;
    cseq += 1;

    // ANNOUNCE — describe the stream we are about to push.
    let sdp = build_sdp(config, &parsed);
    write_request(
        &mut writer,
        method::ANNOUNCE,
        &config.url,
        cseq,
        &[],
        Some(&sdp),
    )
    .await?;
    let _ = read_response(&mut reader).await?;
    cseq += 1;

    // SETUP — TCP-interleaved transport on channels 0/1.
    let track_url = format!("{}/trackID=0", config.url.trim_end_matches('/'));
    let setup_extra = [(
        "Transport",
        "RTP/AVP/TCP;unicast;interleaved=0-1;mode=record",
    )];
    write_request(&mut writer, method::SETUP, &track_url, cseq, &setup_extra, None).await?;
    let setup_resp = read_response(&mut reader).await?;
    cseq += 1;
    let session_id = setup_resp
        .session
        .ok_or_else(|| RtspError::Protocol("SETUP response missing Session header".into()))?;

    // RECORD — switch the server into receive mode.
    let record_extra = [
        ("Session", session_id.as_str()),
        ("Range", "npt=0.000-"),
    ];
    write_request(
        &mut writer,
        method::RECORD,
        &config.url,
        cseq,
        &record_extra,
        None,
    )
    .await?;
    let _ = read_response(&mut reader).await?;
    cseq += 1;

    // RTP push loop.
    let mut seq: u16 = rand_seed_u16();
    let ssrc: u32 = rand_seed_u32();
    let started_at = std::time::Instant::now();
    loop {
        let frame = match frame_rx.recv().await {
            Ok(f) => f,
            Err(broadcast::error::RecvError::Closed) => {
                tracing::info!("rtsp push: encoder broadcast closed; tearing down session");
                break;
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "rtsp push: broadcast receiver lagged; some frames dropped"
                );
                continue;
            }
        };

        let pts_ms = if frame.pts_ms > 0 {
            frame.pts_ms
        } else {
            started_at.elapsed().as_millis() as u64
        };
        // RFC 3984 timestamps run on the 90 kHz video clock.
        let ts: u32 = ((pts_ms.wrapping_mul(RTP_CLOCK_HZ as u64) / 1000) & 0xFFFF_FFFF) as u32;

        // Each EncodedFrame from the encoder is one Annex-B framed
        // unit (or a small group). Walk the buffer, peel off NAL
        // units, and send each one as an RTP packet. The marker bit
        // is set on the very last packet of the last NAL in the
        // access unit per RFC 3984 §5.1.
        let mut units: Vec<&[u8]> = Vec::new();
        let mut start_indices: Vec<usize> = Vec::new();
        let mut i = 0usize;
        while i < frame.bytes.len() {
            if let Some(s) = crate::nal::find_start_code(&frame.bytes, i) {
                start_indices.push(s.start);
                i = s.end;
            } else {
                break;
            }
        }
        for (idx, start) in start_indices.iter().enumerate() {
            let end = if idx + 1 < start_indices.len() {
                start_indices[idx + 1]
            } else {
                frame.bytes.len()
            };
            units.push(&frame.bytes[*start..end]);
        }

        for (idx, unit) in units.iter().enumerate() {
            if let Some((header, body)) = split_nal_unit(unit) {
                let is_last_unit = idx + 1 == units.len();
                let packets = packetize_nal(
                    header,
                    body,
                    &mut seq,
                    ts,
                    ssrc,
                    config.mtu,
                    is_last_unit,
                );
                for pkt in packets {
                    let framed = frame_interleaved(0, &pkt);
                    writer.write_all(&framed).await?;
                }
            }
        }
        writer.flush().await?;
    }

    // Best-effort TEARDOWN.
    let teardown_extra = [("Session", session_id.as_str())];
    let _ = write_request(
        &mut writer,
        method::TEARDOWN,
        &config.url,
        cseq,
        &teardown_extra,
        None,
    )
    .await;

    Ok(())
}

async fn write_request<W>(
    writer: &mut W,
    method_tok: &str,
    url: &str,
    cseq: u32,
    extra: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(), RtspError>
where
    W: AsyncWriteExt + Unpin,
{
    let req = build_request(method_tok, url, cseq, extra, body);
    writer.write_all(req.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_response<R>(reader: &mut R) -> Result<RtspResponseHead, RtspError>
where
    R: AsyncReadExt + Unpin,
{
    // Read until the blank-line terminator. RTSP headers are line-
    // oriented and tiny; we read byte-by-byte to keep the parser
    // simple and bound the buffer at 16 KiB.
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        if buf.len() > 16 * 1024 {
            return Err(RtspError::Protocol(
                "RTSP response head exceeded 16 KiB".into(),
            ));
        }
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Err(RtspError::Protocol(
                "EOF before RTSP response head completed".into(),
            ));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head_str = std::str::from_utf8(&buf)
        .map_err(|_| RtspError::Protocol("RTSP response head was not UTF-8".into()))?;
    let parsed = parse_response_head(head_str)?;
    if !(200..300).contains(&parsed.code) {
        return Err(RtspError::Status {
            code: parsed.code,
            reason: parsed.reason,
        });
    }
    Ok(parsed)
}

/// Cheap pseudo-random seed for RTP sequence + SSRC. Uses the system
/// clock; we don't need cryptographic randomness, just per-session
/// uniqueness so two reconnects do not collide on a re-used SSRC.
fn rand_seed_u32() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (nanos as u32) ^ 0xA1B2_C3D4
}

fn rand_seed_u16() -> u16 {
    (rand_seed_u32() & 0xFFFF) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;
    use tokio::net::TcpListener;

    #[test]
    fn parse_url_with_default_port() {
        let p = ParsedUrl::parse("rtsp://relay.example.com/drone-42").expect("parse");
        assert_eq!(p.host, "relay.example.com");
        assert_eq!(p.port, 554);
        assert_eq!(p.path, "/drone-42");
    }

    #[test]
    fn parse_url_with_explicit_port() {
        let p = ParsedUrl::parse("rtsp://10.0.0.5:8554/foo/bar").expect("parse");
        assert_eq!(p.host, "10.0.0.5");
        assert_eq!(p.port, 8554);
        assert_eq!(p.path, "/foo/bar");
    }

    #[test]
    fn parse_url_rejects_bad_scheme() {
        let res = ParsedUrl::parse("http://relay.example.com/x");
        assert!(matches!(res, Err(RtspError::InvalidUrl(_))));
    }

    #[test]
    fn parse_url_rejects_bad_port() {
        let res = ParsedUrl::parse("rtsp://relay.example.com:abc/x");
        assert!(matches!(res, Err(RtspError::InvalidUrl(_))));
    }

    #[test]
    fn build_request_includes_cseq_and_url() {
        let req = build_request(method::OPTIONS, "rtsp://x/y", 7, &[], None);
        assert!(req.starts_with("OPTIONS rtsp://x/y RTSP/1.0\r\n"));
        assert!(req.contains("CSeq: 7\r\n"));
        assert!(req.ends_with("\r\n\r\n"));
    }

    #[test]
    fn build_request_with_body_sets_content_length() {
        let body = "v=0\r\n";
        let req = build_request(method::ANNOUNCE, "rtsp://x/y", 1, &[], Some(body));
        assert!(req.contains("Content-Type: application/sdp\r\n"));
        assert!(req.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(req.ends_with(body));
    }

    #[test]
    fn parse_response_head_extracts_status_and_session() {
        let head = "RTSP/1.0 200 OK\r\nCSeq: 1\r\nSession: 12345abc;timeout=60\r\n\r\n";
        let p = parse_response_head(head).expect("parse");
        assert_eq!(p.code, 200);
        assert_eq!(p.reason, "OK");
        assert_eq!(p.session.as_deref(), Some("12345abc"));
    }

    #[test]
    fn parse_response_head_rejects_garbage() {
        let res = parse_response_head("not a status line\r\n\r\n");
        assert!(matches!(res, Err(RtspError::Protocol(_))));
    }

    #[test]
    fn build_rtp_header_fields_are_correct() {
        let hdr = build_rtp_header(0xDEAD, 0xCAFE_BABE, 0x1234_5678, true);
        // V=2, no flags
        assert_eq!(hdr[0], 0x80);
        // Marker + payload type 96
        assert_eq!(hdr[1], 0xE0);
        assert_eq!(&hdr[2..4], &0xDEADu16.to_be_bytes());
        assert_eq!(&hdr[4..8], &0xCAFE_BABEu32.to_be_bytes());
        assert_eq!(&hdr[8..12], &0x1234_5678u32.to_be_bytes());

        let hdr2 = build_rtp_header(1, 2, 3, false);
        // No marker
        assert_eq!(hdr2[1], RTP_PAYLOAD_TYPE_H264);
    }

    #[test]
    fn frame_interleaved_carries_channel_and_length() {
        let framed = frame_interleaved(0, &[0xAA, 0xBB]);
        assert_eq!(framed[0], b'$');
        assert_eq!(framed[1], 0);
        assert_eq!(&framed[2..4], &2u16.to_be_bytes());
        assert_eq!(&framed[4..], &[0xAA, 0xBB]);
    }

    #[test]
    fn split_nal_unit_handles_both_start_codes() {
        let four = vec![0, 0, 0, 1, 0x65, 0xAA];
        assert_eq!(split_nal_unit(&four), Some((0x65, &four[5..])));
        let three = vec![0, 0, 1, 0x67, 0xBB];
        assert_eq!(split_nal_unit(&three), Some((0x67, &three[4..])));
        let none = vec![0xFF, 0xFE];
        assert_eq!(split_nal_unit(&none), None);
    }

    #[test]
    fn packetize_small_nal_yields_single_packet() {
        let mut seq = 1u16;
        let pkts = packetize_nal(0x65, &[0; 100], &mut seq, 0, 0xDEAD, 1400, true);
        assert_eq!(pkts.len(), 1);
        // 12 bytes RTP header + 1 byte NAL header + 100 bytes body
        assert_eq!(pkts[0].len(), 12 + 1 + 100);
        assert_eq!(seq, 2);
    }

    #[test]
    fn packetize_large_nal_uses_fu_a_fragmentation() {
        // 5 KiB body → multiple FU-A packets.
        let body = vec![0xAB; 5000];
        let mut seq = 100u16;
        let pkts = packetize_nal(0x65, &body, &mut seq, 0, 0xDEAD, 1400, true);
        assert!(pkts.len() > 1, "expected multi-packet FU-A");
        // First packet has S=1 in FU header.
        let fu_hdr_first = pkts[0][13];
        assert_eq!(fu_hdr_first & 0x80, 0x80);
        // First FU indicator is type 28 = 0x1C with NRI from header.
        let fu_ind_first = pkts[0][12];
        assert_eq!(fu_ind_first & 0x1F, 28);
        // FU NAL type matches the original NAL type (5 = IDR).
        assert_eq!(fu_hdr_first & 0x1F, 5);
        // Last packet has E=1.
        let last = pkts.last().unwrap();
        let fu_hdr_last = last[13];
        assert_eq!(fu_hdr_last & 0x40, 0x40);
        // Last packet has the marker bit on RTP header.
        let rtp_hdr_last = last[1];
        assert_eq!(rtp_hdr_last & 0x80, 0x80);
        // Sequence advanced by exactly the packet count.
        assert_eq!(seq, 100u16.wrapping_add(pkts.len() as u16));
    }

    #[test]
    fn build_sdp_includes_video_track() {
        let cfg = PushConfig {
            url: "rtsp://relay.example.com:8554/drone-42".into(),
            width: 1280,
            height: 720,
            fps: 30,
            mtu: DEFAULT_MTU,
        };
        let parsed = ParsedUrl::parse(&cfg.url).expect("parse");
        let sdp = build_sdp(&cfg, &parsed);
        assert!(sdp.contains("v=0"));
        assert!(sdp.contains("m=video 0 RTP/AVP 96"));
        assert!(sdp.contains("a=rtpmap:96 H264/90000"));
        assert!(sdp.contains("a=fmtp:96 packetization-mode=1"));
        assert!(sdp.contains("a=framerate:30"));
    }

    /// Spin up a TCP listener that mocks an RTSP server. Asserts the
    /// expected request sequence (OPTIONS / ANNOUNCE / SETUP /
    /// RECORD), then reads two RTP packets from the interleaved
    /// channel and exits. Drives `push_session` via a single-shot
    /// broadcast feed.
    #[tokio::test]
    async fn push_session_handshakes_then_streams() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        // Server task. Reads RTSP requests line-by-line, replies with
        // canned 200 OKs (including a Session: header on the SETUP
        // reply), then accumulates RTP frames.
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = sock.into_split();
            let mut reader = tokio::io::BufReader::new(read_half);

            // Handle four request blocks: OPTIONS, ANNOUNCE, SETUP, RECORD.
            for i in 0..4 {
                let (request, _content_len) = read_full_request(&mut reader).await;
                // Tag the first request line for the assertion.
                let first_line = request.lines().next().unwrap_or("").to_string();
                let session_hdr = if i == 2 {
                    "Session: 12345abc;timeout=60\r\n"
                } else {
                    ""
                };
                let resp = format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {}\r\n{}\r\n",
                    i + 1,
                    session_hdr
                );
                write_half
                    .write_all(resp.as_bytes())
                    .await
                    .expect("write resp");
                match i {
                    0 => assert!(first_line.starts_with("OPTIONS"), "got: {first_line}"),
                    1 => assert!(first_line.starts_with("ANNOUNCE"), "got: {first_line}"),
                    2 => assert!(first_line.starts_with("SETUP"), "got: {first_line}"),
                    3 => assert!(first_line.starts_with("RECORD"), "got: {first_line}"),
                    _ => unreachable!(),
                }
            }

            // Read a small number of bytes from the interleaved
            // channel before disconnecting. We just check the
            // signature `$ <ch=0> ...`.
            let mut sig = [0u8; 4];
            tokio::io::AsyncReadExt::read_exact(&mut reader, &mut sig)
                .await
                .expect("read interleaved");
            assert_eq!(sig[0], b'$');
            assert_eq!(sig[1], 0);
            // No need to drain further; close the socket to make the
            // client teardown.
        });

        let cfg = PushConfig {
            url: format!("rtsp://127.0.0.1:{port}/drone-test"),
            width: 640,
            height: 480,
            fps: 30,
            mtu: DEFAULT_MTU,
        };

        let (tx, mut rx) = broadcast::channel::<EncodedFrame>(8);

        // Feed one IDR access unit so the client emits RTP packets
        // before the server closes the socket.
        let frame = EncodedFrame {
            bytes: vec![0, 0, 0, 1, 0x65, 0xAB, 0xCD, 0xEF],
            is_keyframe: true,
            pts_ms: 0,
        };
        tx.send(frame).expect("broadcast send");

        let res = push_session(&cfg, &mut rx).await;
        // The server closed mid-stream so push_session may return Ok
        // (broadcast receiver closed) or Err (TCP write/read failed
        // when the server FIN'd the socket). Either is acceptable;
        // we just want the handshake assertions on the server side
        // to have fired, plus at least one RTP packet on the wire.
        let _ = res;

        server.await.expect("server task");
    }

    /// Read one full RTSP request (head + optional body) from the
    /// reader. Returns the head text plus the body length the
    /// `Content-Length:` header reported.
    async fn read_full_request<R>(reader: &mut R) -> (String, usize)
    where
        R: AsyncBufReadExt + Unpin,
    {
        let mut head = String::new();
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.expect("read line");
            if n == 0 {
                break;
            }
            head.push_str(&line);
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let mut content_len = 0usize;
        for line in head.lines() {
            if let Some(rest) = line.strip_prefix("Content-Length:") {
                content_len = rest.trim().parse().unwrap_or(0);
            }
        }
        if content_len > 0 {
            let mut body = vec![0u8; content_len];
            tokio::io::AsyncReadExt::read_exact(reader, &mut body)
                .await
                .expect("read body");
        }
        (head, content_len)
    }
}
