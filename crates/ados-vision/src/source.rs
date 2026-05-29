//! Frame sources.
//!
//! A [`FrameSource`] yields one raw frame at a time for a single camera. Two
//! kinds share the trait:
//!
//! - [`TapSource`] reads length-delimited raw frames from a unix socket the
//!   video pipeline writes to (`vision-tap-<camera>.sock`). This is the cheap
//!   path: the encoder already has the decoded frame, so the engine taps it
//!   rather than opening the device a second time.
//! - [`CaptureSource`] spawns `ffmpeg` to capture a V4L2/CSI device directly to
//!   `rawvideo` and reads frames off its stdout. This is the fallback for a
//!   camera the video pipeline does not own.
//!
//! Each frame carries its pixel format and dimensions so the engine can size a
//! ring and stamp a descriptor. The engine runs one source per camera id.
//!
//! Engine-owned cameras are enumerated by shelling `python -m ados.hal.camera
//! --json` (the same HAL discovery the video pipeline uses); the parsing lives
//! in [`discover_cameras`].

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use ados_protocol::framebus::FrameFormat;
use anyhow::{anyhow, Result};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

/// One raw frame plus the metadata needed to size a ring and stamp a descriptor.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
    pub data: Vec<u8>,
}

/// A per-camera frame source. `next_frame` resolves with the next frame, or an
/// error when the source has ended (socket closed, capture process exited); the
/// engine then re-opens the source after a backoff.
#[allow(async_fn_in_trait)]
pub trait FrameSource: Send {
    /// Block until the next frame is available.
    async fn next_frame(&mut self) -> Result<RawFrame>;
    /// The camera id this source feeds.
    fn camera_id(&self) -> &str;
}

/// An owned, statically-dispatched source over the two source kinds. The
/// `FrameSource` trait uses `async fn` and so is not object-safe; the binary
/// holds this enum instead of a `Box<dyn FrameSource>` and dispatches per call.
pub enum AnySource {
    Tap(TapSource),
    Capture(CaptureSource),
}

impl FrameSource for AnySource {
    async fn next_frame(&mut self) -> Result<RawFrame> {
        match self {
            AnySource::Tap(s) => s.next_frame().await,
            AnySource::Capture(s) => s.next_frame().await,
        }
    }
    fn camera_id(&self) -> &str {
        match self {
            AnySource::Tap(s) => s.camera_id(),
            AnySource::Capture(s) => s.camera_id(),
        }
    }
}

// --- tap source -----------------------------------------------------------

/// The little header the video pipeline writes before each tapped frame on the
/// tap socket. 16 bytes, little-endian: a magic, the pixel format tag, the
/// dimensions, and the payload length. Keeping it self-describing lets the
/// engine size a ring without a side channel.
///
/// Layout: `magic:u32 | format:u8 | _pad:u8 | width:u16 | height:u16 | _pad:u16 | byte_len:u32`.
const TAP_MAGIC: u32 = 0x4144_5654; // "ADVT"
const TAP_HEADER_LEN: usize = 16;
/// Cap a single tapped frame so a corrupt header cannot drive an unbounded
/// allocation. 4K RGB24 is ~25 MiB; 64 MiB is a generous ceiling.
const TAP_MAX_FRAME: usize = 64 * 1024 * 1024;

fn format_tag(f: FrameFormat) -> u8 {
    match f {
        FrameFormat::Rgb24 => 0,
        FrameFormat::Nv12 => 1,
        FrameFormat::Yuv420p => 2,
    }
}

fn format_from_tag(t: u8) -> Option<FrameFormat> {
    match t {
        0 => Some(FrameFormat::Rgb24),
        1 => Some(FrameFormat::Nv12),
        2 => Some(FrameFormat::Yuv420p),
        _ => None,
    }
}

/// Encode the tap frame header. Exposed so the video-pipeline tap writer can
/// share the exact byte layout the reader expects.
pub fn encode_tap_header(
    format: FrameFormat,
    width: u32,
    height: u32,
    byte_len: u32,
) -> [u8; TAP_HEADER_LEN] {
    let mut h = [0u8; TAP_HEADER_LEN];
    h[0..4].copy_from_slice(&TAP_MAGIC.to_le_bytes());
    h[4] = format_tag(format);
    h[6..8].copy_from_slice(&(width as u16).to_le_bytes());
    h[8..10].copy_from_slice(&(height as u16).to_le_bytes());
    h[12..16].copy_from_slice(&byte_len.to_le_bytes());
    h
}

/// Decode a tap frame header into `(format, width, height, byte_len)`.
pub fn decode_tap_header(h: &[u8; TAP_HEADER_LEN]) -> Result<(FrameFormat, u32, u32, usize)> {
    let magic = u32::from_le_bytes(h[0..4].try_into().unwrap());
    if magic != TAP_MAGIC {
        return Err(anyhow!("bad tap frame magic {magic:#x}"));
    }
    let format = format_from_tag(h[4]).ok_or_else(|| anyhow!("bad tap format tag {}", h[4]))?;
    let width = u16::from_le_bytes(h[6..8].try_into().unwrap()) as u32;
    let height = u16::from_le_bytes(h[8..10].try_into().unwrap()) as u32;
    let byte_len = u32::from_le_bytes(h[12..16].try_into().unwrap()) as usize;
    if byte_len > TAP_MAX_FRAME {
        return Err(anyhow!("tap frame of {byte_len} bytes exceeds cap"));
    }
    Ok((format, width, height, byte_len))
}

/// Reads frames from the video pipeline's tap socket.
pub struct TapSource {
    camera_id: String,
    socket_path: String,
    stream: Option<UnixStream>,
}

impl TapSource {
    pub fn new(camera_id: impl Into<String>, socket_path: impl Into<String>) -> Self {
        Self {
            camera_id: camera_id.into(),
            socket_path: socket_path.into(),
            stream: None,
        }
    }

    async fn ensure_connected(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let s = UnixStream::connect(&self.socket_path).await?;
        self.stream = Some(s);
        Ok(())
    }
}

impl FrameSource for TapSource {
    async fn next_frame(&mut self) -> Result<RawFrame> {
        self.ensure_connected().await?;
        let stream = self.stream.as_mut().expect("connected above");
        let mut header = [0u8; TAP_HEADER_LEN];
        if let Err(e) = stream.read_exact(&mut header).await {
            // Drop the stream so the next call reconnects.
            self.stream = None;
            return Err(e.into());
        }
        let (format, width, height, byte_len) = match decode_tap_header(&header) {
            Ok(v) => v,
            Err(e) => {
                self.stream = None;
                return Err(e);
            }
        };
        let stream = self.stream.as_mut().expect("still connected");
        let mut data = vec![0u8; byte_len];
        if byte_len > 0 {
            if let Err(e) = stream.read_exact(&mut data).await {
                self.stream = None;
                return Err(e.into());
            }
        }
        Ok(RawFrame {
            width,
            height,
            format,
            data,
        })
    }

    fn camera_id(&self) -> &str {
        &self.camera_id
    }
}

// --- capture source -------------------------------------------------------

/// Spawns `ffmpeg` to capture a V4L2/CSI device to `rawvideo` and reads fixed-
/// size frames off its stdout. The capture is configured to a known width,
/// height, and pixel format so each frame is a fixed byte length.
pub struct CaptureSource {
    camera_id: String,
    device_path: String,
    width: u32,
    height: u32,
    format: FrameFormat,
    child: Option<Child>,
    frame_bytes: usize,
}

impl CaptureSource {
    pub fn new(
        camera_id: impl Into<String>,
        device_path: impl Into<String>,
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Self {
        Self {
            camera_id: camera_id.into(),
            device_path: device_path.into(),
            width,
            height,
            format,
            child: None,
            frame_bytes: format.frame_bytes(width, height),
        }
    }

    /// The ffmpeg pixel-format token for the rawvideo output.
    fn ffmpeg_pix_fmt(&self) -> &'static str {
        match self.format {
            FrameFormat::Rgb24 => "rgb24",
            FrameFormat::Nv12 => "nv12",
            FrameFormat::Yuv420p => "yuv420p",
        }
    }

    /// Spawn the ffmpeg capture. Reads `device_path` as a V4L2 input and writes
    /// `rawvideo` of the configured size and format to stdout.
    fn spawn(&self) -> Result<Child> {
        let size = format!("{}x{}", self.width, self.height);
        let child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "v4l2",
                "-video_size",
                &size,
                "-i",
                &self.device_path,
                "-pix_fmt",
                self.ffmpeg_pix_fmt(),
                "-f",
                "rawvideo",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Ok(child)
    }

    async fn ensure_spawned(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        self.child = Some(self.spawn()?);
        Ok(())
    }
}

impl FrameSource for CaptureSource {
    async fn next_frame(&mut self) -> Result<RawFrame> {
        self.ensure_spawned().await?;
        let child = self.child.as_mut().expect("spawned above");
        let stdout = child
            .stdout
            .as_mut()
            .ok_or_else(|| anyhow!("capture child has no stdout"))?;
        let mut data = vec![0u8; self.frame_bytes];
        if let Err(e) = stdout.read_exact(&mut data).await {
            // ffmpeg exited or the device went away; drop the child so the next
            // call respawns.
            self.child = None;
            return Err(e.into());
        }
        Ok(RawFrame {
            width: self.width,
            height: self.height,
            format: self.format,
            data,
        })
    }

    fn camera_id(&self) -> &str {
        &self.camera_id
    }
}

// --- HAL discovery --------------------------------------------------------

const DEFAULT_PYTHON: &str = "/opt/ados/venv/bin/python3";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(12);

/// A camera as reported by the Python HAL discovery JSON.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DiscoveredCamera {
    pub name: String,
    #[serde(rename = "type")]
    pub camera_type: String,
    pub device_path: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
struct DiscoveryResult {
    #[serde(default)]
    cameras: Vec<DiscoveredCamera>,
}

/// Resolve the Python interpreter for HAL discovery (`ADOS_PYTHON` override).
pub fn python_executable() -> String {
    std::env::var("ADOS_PYTHON")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PYTHON.to_string())
}

/// Enumerate cameras via `python -m ados.hal.camera --json`. A spawn failure,
/// timeout, or malformed output collapses to an empty list (logged `warn`),
/// never an error, so the engine's config-listed cameras still start.
pub async fn discover_cameras(python_exe: &str, timeout: Duration) -> Vec<DiscoveredCamera> {
    let mut cmd = Command::new(python_exe);
    cmd.args(["-m", "ados.hal.camera", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, python = python_exe, "camera_discovery_spawn_failed");
            return Vec::new();
        }
    };
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "camera_discovery_wait_failed");
            return Vec::new();
        }
        Err(_) => {
            tracing::warn!(timeout_s = timeout.as_secs(), "camera_discovery_timed_out");
            return Vec::new();
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_discovery(&stdout)
}

/// Discover with the resolved interpreter and the default timeout.
pub async fn discover_cameras_default() -> Vec<DiscoveredCamera> {
    discover_cameras(&python_executable(), DISCOVERY_TIMEOUT).await
}

/// Parse the discovery JSON out of the subprocess stdout (the last line that
/// parses as the expected object wins, matching the video pipeline's reader).
fn parse_discovery(stdout: &str) -> Vec<DiscoveredCamera> {
    for line in stdout.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(r) = serde_json::from_str::<DiscoveryResult>(trimmed) {
            return r.cameras;
        }
    }
    Vec::new()
}

/// Best-effort sanity log of the tap socket path (does not connect). Lets the
/// engine warn early when a configured tap path's parent directory is missing.
pub fn tap_socket_parent_exists(socket_path: &str) -> bool {
    Path::new(socket_path)
        .parent()
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Write one tap frame to a connected stream (header + payload). Shared by the
/// video-pipeline tap writer; lives here so the wire format has one owner.
pub async fn write_tap_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    frame: &RawFrame,
) -> std::io::Result<()> {
    let header = encode_tap_header(
        frame.format,
        frame.width,
        frame.height,
        frame.data.len() as u32,
    );
    w.write_all(&header).await?;
    if !frame.data.is_empty() {
        w.write_all(&frame.data).await?;
    }
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_header_round_trips() {
        let h = encode_tap_header(FrameFormat::Nv12, 640, 480, 460_800);
        let (f, w, ht, n) = decode_tap_header(&h).unwrap();
        assert_eq!(f, FrameFormat::Nv12);
        assert_eq!(w, 640);
        assert_eq!(ht, 480);
        assert_eq!(n, 460_800);
    }

    #[test]
    fn tap_header_rejects_bad_magic() {
        let mut h = encode_tap_header(FrameFormat::Rgb24, 8, 8, 192);
        h[0] ^= 0xFF;
        assert!(decode_tap_header(&h).is_err());
    }

    #[test]
    fn tap_header_rejects_oversized_len() {
        let h = encode_tap_header(FrameFormat::Rgb24, 8, 8, u32::MAX);
        assert!(decode_tap_header(&h).is_err());
    }

    #[test]
    fn parse_discovery_reads_cameras() {
        let json = r#"{"cameras":[{"name":"HD USB Camera","type":"usb","device_path":"/dev/video1","capabilities":["mjpeg"]}],"primary":null,"total_cameras":1}"#;
        let cams = parse_discovery(json);
        assert_eq!(cams.len(), 1);
        assert_eq!(cams[0].device_path, "/dev/video1");
        assert_eq!(cams[0].camera_type, "usb");
    }

    #[test]
    fn parse_discovery_handles_noise_and_empty() {
        assert!(parse_discovery("").is_empty());
        assert!(parse_discovery("not json\nmore noise").is_empty());
        let mixed = "log line leaked\n{\"cameras\":[],\"total_cameras\":0}\n";
        assert!(parse_discovery(mixed).is_empty());
    }

    #[tokio::test]
    async fn discover_with_missing_python_is_empty() {
        let cams = discover_cameras("/nonexistent/python-xyzzy", Duration::from_secs(2)).await;
        assert!(cams.is_empty());
    }

    #[tokio::test]
    async fn tap_source_reads_a_written_frame() {
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tap.sock");
        let path_str = path.to_string_lossy().to_string();
        let listener = UnixListener::bind(&path).unwrap();

        let frame = RawFrame {
            width: 4,
            height: 4,
            format: FrameFormat::Rgb24,
            data: (0..48u8).collect(),
        };
        let frame_clone = frame.clone();
        let writer = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            write_tap_frame(&mut s, &frame_clone).await.unwrap();
            // Keep the socket open briefly so the reader gets the frame.
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let mut src = TapSource::new("uvc-0", path_str);
        let got = tokio::time::timeout(Duration::from_secs(2), src.next_frame())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.width, 4);
        assert_eq!(got.format, FrameFormat::Rgb24);
        assert_eq!(got.data, frame.data);
        assert_eq!(src.camera_id(), "uvc-0");
        writer.abort();
    }
}
