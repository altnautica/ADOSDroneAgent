//! Subprocess client for Rockchip RKMPI hardware H.264 encoders.
//!
//! Why a subprocess. The Rockchip vendor encoder library
//! (`librkmpi.so` on RV1106 / Luckfox Pico Zero) is shipped as a closed
//! binary linked against uclibc. The lite agent's release artifact is
//! a musl-static Rust binary; loading the vendor `.so` directly would
//! require either a glibc/uclibc rootfs at the agent or a fragile
//! dlopen-and-pray dance that crashes at runtime. Pushing the vendor
//! library into a small uclibc-built C wrapper turns a libc
//! compatibility nightmare into a clean process boundary.
//!
//! Wire format. The Rust parent and the C wrapper exchange
//! length-prefixed msgpack messages over the wrapper's stdin (parent →
//! child) and stdout (child → parent). Each message is framed as:
//!
//! ```text
//! +-----------+----------------------+
//! | u32 BE    | msgpack body         |
//! | length    | (length bytes)       |
//! +-----------+----------------------+
//! ```
//!
//! This module ships the wire types, the framing helpers, and the
//! parent-side struct today. The actual subprocess spawn / lifetime /
//! respawn loop lands alongside the C wrapper itself at
//! `agents/lite-rs/boards/luckfox-pico-zero/rkmpi-wrapper/main.c`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{EncodedFrame, Encoder, EncoderConfig, EncoderError};

/// Maximum framed message size, including the 4-byte length prefix. The
/// limit caps a single encoded NAL access unit; 4 MiB comfortably holds
/// 5MP@30fps keyframes at the documented bitrate ceiling.
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Default install path for the wrapper binary on a Luckfox image. The
/// real install location is pinned by the Buildroot recipe; the
/// installer can pass `RKMPI_WRAPPER_PATH` to override it.
pub fn default_subprocess_path() -> PathBuf {
    PathBuf::from("/usr/lib/ados/rkmpi-wrapper")
}

/// Parent → child message types.
///
/// `serde(tag = "kind")` produces an externally-tagged enum encoding
/// that the C wrapper can decode with a small msgpack parser without
/// pulling in a full library — the discriminant is a single string
/// field at a known position, and the rest of the body is a struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SubprocessRequest {
    /// Spin up the encoder and start producing frames.
    Start(EncoderConfig),
    /// Tear the encoder down. The wrapper exits after sending one final
    /// flush; the parent reaps the child via `wait()`.
    Stop,
}

/// Child → parent message types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SubprocessResponse {
    /// Sent once after a successful `Start`. The parent waits for this
    /// before treating the encoder as live.
    Ready,
    /// One encoded access unit. Maps directly to [`EncodedFrame`].
    Frame {
        is_keyframe: bool,
        pts_ms: u64,
        bytes: Vec<u8>,
    },
    /// Vendor diagnostic. The parent typically logs and respawns on
    /// receipt; detailed recovery semantics live with the supervise
    /// loop.
    Error { message: String },
}

/// Encode a request into a framed msgpack message.
pub fn frame_request(req: &SubprocessRequest) -> Result<Vec<u8>, EncoderError> {
    let body = rmp_serde::to_vec_named(req)
        .map_err(|e| EncoderError::Protocol(format!("encode request: {e}")))?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(EncoderError::Protocol(format!(
            "encoded request {} bytes exceeds {} byte cap",
            body.len(),
            MAX_FRAME_BYTES
        )));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode a framed msgpack response from a byte slice. Used by tests
/// and by the real read loop once the subprocess is wired up.
pub fn parse_response(framed: &[u8]) -> Result<(SubprocessResponse, usize), EncoderError> {
    if framed.len() < 4 {
        return Err(EncoderError::Protocol(
            "response shorter than 4-byte length prefix".into(),
        ));
    }
    let len = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(EncoderError::Protocol(format!(
            "response declares {} byte body, exceeds {} byte cap",
            len, MAX_FRAME_BYTES
        )));
    }
    let total = 4 + len;
    if framed.len() < total {
        return Err(EncoderError::Protocol(format!(
            "response declared {} byte body, only {} bytes available",
            len,
            framed.len() - 4
        )));
    }
    let resp: SubprocessResponse = rmp_serde::from_slice(&framed[4..total])
        .map_err(|e| EncoderError::Protocol(format!("decode response: {e}")))?;
    Ok((resp, total))
}

/// Async helper: write a framed request to any [`AsyncWrite`] sink. The
/// real client uses this against the child's stdin pipe.
pub async fn write_request<W>(writer: &mut W, req: &SubprocessRequest) -> Result<(), EncoderError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = frame_request(req)?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Async helper: read one framed response from any [`AsyncRead`] source.
/// Returns `EncoderError::Io` on EOF before a full frame arrives so the
/// caller can distinguish "child exited" from "child sent garbage".
pub async fn read_response<R>(reader: &mut R) -> Result<SubprocessResponse, EncoderError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(EncoderError::Protocol(format!(
            "response declares {} byte body, exceeds {} byte cap",
            len, MAX_FRAME_BYTES
        )));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    let resp: SubprocessResponse = rmp_serde::from_slice(&body)
        .map_err(|e| EncoderError::Protocol(format!("decode response: {e}")))?;
    Ok(resp)
}

/// Subprocess-backed encoder. Holds the wrapper binary path until the
/// real spawn / supervise loop lands.
#[derive(Debug)]
pub struct RkmpiEncoderSubprocess {
    wrapper_path: PathBuf,
    started: bool,
}

impl RkmpiEncoderSubprocess {
    /// Build a fresh facade. The wrapper binary is not spawned until
    /// `start()` runs.
    pub fn new<P: AsRef<Path>>(wrapper_path: P) -> Self {
        Self {
            wrapper_path: wrapper_path.as_ref().to_path_buf(),
            started: false,
        }
    }

    /// Path to the wrapper binary, exposed for diagnostics.
    pub fn wrapper_path(&self) -> &Path {
        &self.wrapper_path
    }
}

#[async_trait::async_trait]
impl Encoder for RkmpiEncoderSubprocess {
    async fn start(&mut self, _config: EncoderConfig) -> Result<(), EncoderError> {
        if self.started {
            return Err(EncoderError::AlreadyStarted);
        }
        // TODO(hardware bringup): spawn the C wrapper via
        // `tokio::process::Command::new(&self.wrapper_path)`, pipe
        // stdin + stdout, send a framed
        // `SubprocessRequest::Start(_config)`, wait for a
        // `SubprocessResponse::Ready` (with a startup timeout), then
        // split stdout into a background task that forwards
        // `SubprocessResponse::Frame` onto an internal mpsc channel
        // drained by `next_frame`. The supervise loop respawns on
        // child exit with exponential backoff capped at 60s.
        tracing::debug!(
            wrapper = ?self.wrapper_path,
            "rkmpi subprocess encoder start called (stub)"
        );
        Err(EncoderError::NotImplemented)
    }

    async fn next_frame(&mut self) -> Option<EncodedFrame> {
        // TODO(hardware bringup): drain the internal mpsc receiver.
        // None when the child has exited and the channel is closed.
        None
    }

    async fn stop(&mut self) {
        // TODO(hardware bringup): send `SubprocessRequest::Stop`, wait
        // for the child to flush + exit (bounded), `kill()` if it
        // exceeds the deadline, reap. Idempotent.
        self.started = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subprocess_request_roundtrips_through_msgpack() {
        let req = SubprocessRequest::Start(EncoderConfig {
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_kbps: 6000,
            keyframe_interval_secs: 2,
        });
        let framed = frame_request(&req).expect("encode start");
        // The framed buffer is 4 bytes of length prefix + msgpack body.
        assert!(framed.len() > 4);
        let declared = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
        assert_eq!(declared, framed.len() - 4);
        let body: SubprocessRequest = rmp_serde::from_slice(&framed[4..]).expect("decode start");
        assert_eq!(body, req);

        let stop = SubprocessRequest::Stop;
        let framed_stop = frame_request(&stop).expect("encode stop");
        let body_stop: SubprocessRequest =
            rmp_serde::from_slice(&framed_stop[4..]).expect("decode stop");
        assert_eq!(body_stop, stop);
    }

    #[test]
    fn frame_response_roundtrips() {
        // We can't reuse `frame_request` for responses because the
        // direction is reversed; build the framed bytes by hand using
        // the same length-prefixed convention the C wrapper emits.
        let resp = SubprocessResponse::Frame {
            is_keyframe: true,
            pts_ms: 33,
            bytes: vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1e],
        };
        let body = rmp_serde::to_vec_named(&resp).expect("encode frame");
        let mut framed = Vec::with_capacity(4 + body.len());
        framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
        framed.extend_from_slice(&body);

        let (parsed, consumed) = parse_response(&framed).expect("parse frame");
        assert_eq!(parsed, resp);
        assert_eq!(consumed, framed.len());

        // Ready and Error variants also roundtrip.
        for r in [
            SubprocessResponse::Ready,
            SubprocessResponse::Error {
                message: "VENC_GetStream timed out".into(),
            },
        ] {
            let b = rmp_serde::to_vec_named(&r).expect("encode");
            let mut f = Vec::with_capacity(4 + b.len());
            f.extend_from_slice(&(b.len() as u32).to_be_bytes());
            f.extend_from_slice(&b);
            let (got, _) = parse_response(&f).expect("parse");
            assert_eq!(got, r);
        }
    }

    #[test]
    fn length_prefix_framing_is_correct() {
        let req = SubprocessRequest::Start(EncoderConfig::default());
        let framed = frame_request(&req).expect("encode");
        let declared = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
        assert_eq!(declared + 4, framed.len());

        // A truncated frame is detected, not silently misparsed.
        let truncated = &framed[..framed.len() - 1];
        match parse_response(truncated) {
            Err(EncoderError::Protocol(_)) | Err(EncoderError::Io(_)) => {}
            other => panic!("expected protocol error on truncated frame, got {other:?}"),
        }

        // A frame whose declared length is below the cap but whose body
        // is invalid msgpack also surfaces a Protocol error.
        let mut bogus = Vec::new();
        bogus.extend_from_slice(&5u32.to_be_bytes());
        bogus.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff]);
        match parse_response(&bogus) {
            Err(EncoderError::Protocol(_)) => {}
            other => panic!("expected protocol error on invalid msgpack, got {other:?}"),
        }
    }

    #[test]
    fn frame_request_rejects_oversized_body() {
        // A pathological config that would somehow encode larger than
        // 4 MiB cannot be constructed today (every field is fixed
        // width), but the cap has to live somewhere; assert the helper
        // rejects an oversized synthetic frame on the read path. We
        // build a fake length prefix above the cap and confirm
        // parse_response surfaces the typed error.
        let mut framed = Vec::new();
        framed.extend_from_slice(&(MAX_FRAME_BYTES as u32 + 1).to_be_bytes());
        match parse_response(&framed) {
            Err(EncoderError::Protocol(msg)) => {
                assert!(msg.contains("exceeds"), "unexpected message: {msg}");
            }
            other => panic!("expected protocol error on oversized declared length, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_and_read_roundtrip_through_pipe() {
        // End-to-end check that `write_request` and `read_response` are
        // wire-compatible with the framing helpers. We use an in-memory
        // duplex stream so the test does not touch the filesystem.
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);

        // Parent writes a Start; in real use the child would parse this
        // and respond. Here we fake the child by writing a Ready frame
        // back through the other end.
        let req = SubprocessRequest::Start(EncoderConfig::default());
        write_request(&mut a, &req).await.expect("write request");

        // Now write a Ready response from the "child" side.
        let ready_body = rmp_serde::to_vec_named(&SubprocessResponse::Ready).expect("encode ready");
        let mut framed = Vec::new();
        framed.extend_from_slice(&(ready_body.len() as u32).to_be_bytes());
        framed.extend_from_slice(&ready_body);
        a.write_all(&framed).await.expect("write ready");

        // Drain the request from the other side first (simulating the
        // child reading parent → child traffic).
        let mut len_buf = [0u8; 4];
        b.read_exact(&mut len_buf).await.expect("read len");
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        b.read_exact(&mut body).await.expect("read body");
        let parsed_req: SubprocessRequest =
            rmp_serde::from_slice(&body).expect("decode parent request");
        assert_eq!(parsed_req, req);

        // Then read the Ready response.
        let parsed = read_response(&mut b).await.expect("read response");
        assert_eq!(parsed, SubprocessResponse::Ready);
    }
}
