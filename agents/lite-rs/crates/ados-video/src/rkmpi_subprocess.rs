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
use std::time::Duration;

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
        return Err(EncoderError::Incomplete(4 - framed.len()));
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
        return Err(EncoderError::Incomplete(total - framed.len()));
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

/// Time the parent waits for the child to flush + exit cleanly after
/// a Stop request before sending SIGKILL. Tuned so a healthy child has
/// room to drain its outgoing frame queue without holding the parent
/// hostage on a wedged subprocess.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// Time the parent waits for the child's first `Ready` response before
/// declaring the start handshake failed. The C wrapper opens the
/// vendor library + initializes the encoder + allocates buffers in
/// this window; 5s is comfortable on Cortex-A7 + RKMPI cold start.
pub const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Capacity of the internal mpsc that the reader task uses to hand
/// encoded frames to `next_frame`. The encoder produces at the FC
/// frame rate (~30 Hz typical); a 64-frame buffer absorbs a 2 s
/// downstream stall before drops.
const FRAME_QUEUE_CAPACITY: usize = 64;

/// Subprocess-backed encoder. Owns the spawned wrapper child + the
/// reader task that drains stdout into an in-memory frame queue.
pub struct RkmpiEncoderSubprocess {
    wrapper_path: PathBuf,
    state: Option<Running>,
}

/// Lifetime-bound runtime state for an actively running wrapper.
/// Constructed by `start()`, consumed by `stop()`.
struct Running {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    frame_rx: tokio::sync::mpsc::Receiver<EncodedFrame>,
    reader_task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for RkmpiEncoderSubprocess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RkmpiEncoderSubprocess")
            .field("wrapper_path", &self.wrapper_path)
            .field("running", &self.state.is_some())
            .finish()
    }
}

impl RkmpiEncoderSubprocess {
    /// Build a fresh facade. The wrapper binary is not spawned until
    /// `start()` runs.
    pub fn new<P: AsRef<Path>>(wrapper_path: P) -> Self {
        Self {
            wrapper_path: wrapper_path.as_ref().to_path_buf(),
            state: None,
        }
    }

    /// Path to the wrapper binary, exposed for diagnostics.
    pub fn wrapper_path(&self) -> &Path {
        &self.wrapper_path
    }

    /// True when a wrapper child is currently running.
    pub fn is_running(&self) -> bool {
        self.state.is_some()
    }
}

impl Drop for RkmpiEncoderSubprocess {
    fn drop(&mut self) {
        // Best-effort: if the agent was dropped without an explicit
        // `stop().await`, send SIGKILL to avoid leaving an orphan
        // wrapper process holding the vendor encoder. The reader
        // task aborts when its end of the pipe closes.
        if let Some(mut state) = self.state.take() {
            let _ = state.child.start_kill();
            state.reader_task.abort();
        }
    }
}

#[async_trait::async_trait]
impl Encoder for RkmpiEncoderSubprocess {
    async fn start(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        if self.state.is_some() {
            return Err(EncoderError::AlreadyStarted);
        }

        // Spawn the wrapper with piped stdin + stdout. Stderr inherits
        // the parent's so vendor diagnostics (RKMPI's chatty lib log)
        // land in journalctl alongside the agent's own tracing output.
        let mut child = tokio::process::Command::new(&self.wrapper_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| EncoderError::Subprocess(e.to_string()))?;

        let mut stdin = child.stdin.take().ok_or_else(|| {
            EncoderError::Protocol("child stdin pipe missing after spawn".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            EncoderError::Protocol("child stdout pipe missing after spawn".into())
        })?;

        // Send the Start request. The wrapper parses, initialises the
        // vendor encoder, and replies with Ready (or Error).
        write_request(&mut stdin, &SubprocessRequest::Start(config))
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "rkmpi wrapper start request failed; aborting child");
                let _ = child.start_kill();
                e
            })?;

        // Spawn the reader task. It runs until the child closes stdout
        // (clean exit or crash) or until aborted by `stop()` / Drop.
        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel(FRAME_QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let reader_task = tokio::spawn(reader_loop(stdout, frame_tx, ready_tx));

        // Wait for the wrapper's Ready response within the startup
        // window. On timeout, abort the reader, kill the child, return
        // a typed error.
        match tokio::time::timeout(READY_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(()))) => {
                tracing::info!(
                    wrapper = ?self.wrapper_path,
                    "rkmpi wrapper ready"
                );
                self.state = Some(Running {
                    child,
                    stdin,
                    frame_rx,
                    reader_task,
                });
                Ok(())
            }
            Ok(Ok(Err(msg))) => {
                let _ = child.start_kill();
                reader_task.abort();
                Err(EncoderError::Protocol(format!(
                    "rkmpi wrapper start error: {msg}"
                )))
            }
            Ok(Err(_)) => {
                let _ = child.start_kill();
                reader_task.abort();
                Err(EncoderError::Protocol(
                    "rkmpi wrapper closed stdout before sending Ready".into(),
                ))
            }
            Err(_) => {
                let _ = child.start_kill();
                reader_task.abort();
                Err(EncoderError::Protocol(format!(
                    "rkmpi wrapper start timeout after {:?}",
                    READY_TIMEOUT
                )))
            }
        }
    }

    async fn next_frame(&mut self) -> Option<EncodedFrame> {
        // Drain the internal queue. Returns None when the reader task
        // closed the channel (child exited or crashed) — caller should
        // treat that as a signal to call `stop()` and decide whether
        // to respawn.
        match self.state.as_mut() {
            Some(state) => state.frame_rx.recv().await,
            None => None,
        }
    }

    async fn stop(&mut self) {
        let Some(mut state) = self.state.take() else {
            return; // idempotent
        };

        // Best-effort Stop request. If write fails (broken pipe because
        // the child already crashed), fall through to the kill path.
        if let Err(e) = write_request(&mut state.stdin, &SubprocessRequest::Stop).await {
            tracing::warn!(error = %e, "rkmpi wrapper stop write failed; killing");
        }

        // Closing stdin signals EOF to the wrapper's read loop, which
        // is what the C side waits on to break out of its main loop.
        drop(state.stdin);

        // Wait up to STOP_GRACE for clean exit, then SIGKILL.
        match tokio::time::timeout(STOP_GRACE, state.child.wait()).await {
            Ok(Ok(status)) => {
                tracing::info!(
                    wrapper = ?self.wrapper_path,
                    status = ?status,
                    "rkmpi wrapper exited cleanly"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "rkmpi wrapper wait failed");
            }
            Err(_) => {
                tracing::warn!(
                    wrapper = ?self.wrapper_path,
                    grace_secs = STOP_GRACE.as_secs(),
                    "rkmpi wrapper did not exit in grace window; sending SIGKILL"
                );
                let _ = state.child.start_kill();
                let _ = state.child.wait().await;
            }
        }

        // Reader task terminates naturally once the stdout pipe closes.
        // If it's still alive (e.g. blocked on something other than a
        // pipe read), abort to avoid a leak.
        state.reader_task.abort();
    }
}

/// Drain the wrapper's stdout. Forwards `Frame` responses to the
/// internal mpsc, signals the start-handshake outcome via the oneshot,
/// logs `Error` responses and exits when the pipe closes.
async fn reader_loop(
    mut stdout: tokio::process::ChildStdout,
    frame_tx: tokio::sync::mpsc::Sender<EncodedFrame>,
    ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
) {
    use tokio::io::AsyncReadExt;

    let mut ready_tx = Some(ready_tx);
    let mut leftover: Vec<u8> = Vec::with_capacity(MAX_FRAME_BYTES);
    let mut scratch = vec![0u8; 64 * 1024];

    loop {
        match stdout.read(&mut scratch).await {
            Ok(0) => {
                // EOF — child closed stdout. If we never observed a
                // Ready, signal the start handshake's failure path.
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err("child closed stdout before Ready".into()));
                }
                return;
            }
            Ok(n) => leftover.extend_from_slice(&scratch[..n]),
            Err(e) => {
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(format!("stdout read error: {e}")));
                }
                tracing::warn!(error = %e, "rkmpi wrapper stdout read failed");
                return;
            }
        }

        // Try to parse zero or more frames out of the accumulated buffer.
        loop {
            match parse_response(&leftover) {
                Ok((resp, consumed)) => {
                    leftover.drain(..consumed);
                    match resp {
                        SubprocessResponse::Ready => {
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(Ok(()));
                            }
                            // If a wrapper goes Ready twice (shouldn't,
                            // but harmless), ignore the duplicate.
                        }
                        SubprocessResponse::Frame {
                            is_keyframe,
                            pts_ms,
                            bytes,
                        } => {
                            let frame = EncodedFrame {
                                bytes,
                                is_keyframe,
                                pts_ms,
                            };
                            // try_send: if the consumer is wedged we drop
                            // rather than block the reader task; a stalled
                            // pipeline shouldn't backpressure the wrapper.
                            if let Err(e) = frame_tx.try_send(frame) {
                                tracing::warn!(error = %e, "rkmpi frame queue full; dropping frame");
                            }
                        }
                        SubprocessResponse::Error { message } => {
                            tracing::warn!(message = %message, "rkmpi wrapper reported error");
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(Err(message));
                            }
                            // Continue reading; the wrapper may recover.
                        }
                    }
                }
                Err(EncoderError::Incomplete(_)) => {
                    // Need more bytes. Break out of the inner loop and
                    // resume reading.
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "rkmpi wrapper protocol error; closing reader");
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(e.to_string()));
                    }
                    return;
                }
            }
        }
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
            Err(EncoderError::Incomplete(_)) => {}
            other => panic!("expected Incomplete on truncated frame, got {other:?}"),
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
