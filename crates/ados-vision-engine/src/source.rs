//! Frame sources.
//!
//! A [`FrameSource`] yields one raw frame at a time for a single camera. Three
//! kinds share the trait:
//!
//! - [`TapSource`] reads length-delimited raw frames from a unix socket the
//!   video pipeline writes to (`vision-tap-main.sock`). This is the cheap path
//!   for the flight camera the video pipeline owns: the encoder already has the
//!   decoded frame, so the engine taps it rather than opening the device again.
//! - [`V4l2Source`] owns a local V4L2 capture device (`cameras[].capture: true`
//!   with a device source): MMAP streaming, the driver's monotonic buffer
//!   timestamps, the sensor exposure, and trigger records joined by sequence.
//! - [`RtspSource`] owns an `rtsp://` camera (a simulated rig): it decodes the
//!   stream and reads the World Engine frame identity out of each frame's SEI.
//!
//! The engine runs one source per camera id. Each frame carries its pixel
//! format, dimensions and capture timing so the engine can size a ring and
//! stamp a descriptor.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

use crate::ring::FrameTiming;
use ados_protocol::framebus::FrameFormat;
use ados_protocol::tap::{decode_tap_header, TAP_HEADER_LEN};
use anyhow::Result;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;

mod rtsp;
mod v4l2;

pub use rtsp::{RtspSettings, RtspSource, SimClock};
pub use v4l2::{
    buffer_timing, controls, exposure_ns_from_absolute, ControlCall, ControlQueue, V4l2Settings,
    V4l2Source,
};

/// Buffers a source keeps in circulation. One is being filled, one sits in a
/// published frame or an in-flight inference, and one is spare.
pub const POOL_BUFFERS: usize = 3;

/// Hands a frame buffer back to the source that filled it, so the next frame
/// reuses its allocation. Cheap to clone; safe to move into an inference task.
#[derive(Clone)]
pub struct BufferReturn {
    tx: mpsc::Sender<Vec<u8>>,
    parked: Arc<AtomicUsize>,
}

impl BufferReturn {
    /// Return a buffer. A buffer is dropped instead when the pool already
    /// holds [`POOL_BUFFERS`] spares, or when its pool is gone.
    pub fn give(&self, buf: Vec<u8>) {
        if self.parked.fetch_add(1, Ordering::AcqRel) >= POOL_BUFFERS {
            self.parked.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        if self.tx.send(buf).is_err() {
            self.parked.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// A per-source pool of at most [`POOL_BUFFERS`] spare frame buffers. A
/// multi-MB frame is filled into a recycled buffer instead of a fresh
/// allocation, which would otherwise mmap and fault in a new region per frame.
pub struct BufferPool {
    tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
    parked: Arc<AtomicUsize>,
    allocations: u64,
}

impl Default for BufferPool {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferPool {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            parked: Arc::new(AtomicUsize::new(0)),
            allocations: 0,
        }
    }

    /// A handle that returns buffers to this pool.
    pub fn recycler(&self) -> BufferReturn {
        BufferReturn {
            tx: self.tx.clone(),
            parked: self.parked.clone(),
        }
    }

    /// A buffer of exactly `len` bytes. A returned buffer is reused (resized,
    /// which reallocates only when it is too small); a fresh one is allocated
    /// only when none is waiting.
    pub fn take(&mut self, len: usize) -> Vec<u8> {
        match self.rx.try_recv() {
            Ok(mut buf) => {
                self.parked.fetch_sub(1, Ordering::AcqRel);
                if buf.capacity() < len {
                    self.allocations += 1;
                }
                buf.resize(len, 0);
                buf
            }
            Err(_) => {
                self.allocations += 1;
                vec![0u8; len]
            }
        }
    }

    /// How many times this pool had to allocate (or grow) a buffer.
    pub fn allocations(&self) -> u64 {
        self.allocations
    }
}

/// One raw frame plus the metadata needed to size a ring and stamp a descriptor.
#[derive(Debug, Clone)]
pub struct RawFrame {
    /// Frames the source dropped as corrupt just before this one.
    pub dropped_corrupt: u32,
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
    pub data: Vec<u8>,
    /// Capture timing for the descriptor (host monotonic arrival at minimum).
    pub timing: FrameTiming,
}

/// A per-camera frame source. `next_frame` resolves with the next frame, or an
/// error when the source has ended (socket closed, capture stopped); the
/// engine then re-opens the source after a backoff.
#[allow(async_fn_in_trait)]
pub trait FrameSource: Send {
    /// Block until the next frame is available. The frame's buffer comes from
    /// the source's pool; hand it back through [`FrameSource::recycler`].
    async fn next_frame(&mut self) -> Result<RawFrame>;
    /// The camera id this source feeds.
    fn camera_id(&self) -> &str;
    /// The handle that returns a consumed frame buffer to this source's pool.
    fn recycler(&self) -> BufferReturn;
}

/// An owned, statically-dispatched source over the source kinds. The
/// `FrameSource` trait uses `async fn` and so is not object-safe; the binary
/// holds this enum instead of a `Box<dyn FrameSource>` and dispatches per call.
pub enum AnySource {
    Tap(TapSource),
    V4l2(Box<V4l2Source>),
    Rtsp(Box<RtspSource>),
}

impl FrameSource for AnySource {
    async fn next_frame(&mut self) -> Result<RawFrame> {
        match self {
            AnySource::Tap(s) => s.next_frame().await,
            AnySource::V4l2(s) => s.next_frame().await,
            AnySource::Rtsp(s) => s.next_frame().await,
        }
    }
    fn camera_id(&self) -> &str {
        match self {
            AnySource::Tap(s) => s.camera_id(),
            AnySource::V4l2(s) => s.camera_id(),
            AnySource::Rtsp(s) => s.camera_id(),
        }
    }
    fn recycler(&self) -> BufferReturn {
        match self {
            AnySource::Tap(s) => s.recycler(),
            AnySource::V4l2(s) => s.recycler(),
            AnySource::Rtsp(s) => s.recycler(),
        }
    }
}

// --- tap source -----------------------------------------------------------

// The tap wire contract (the ADVT header codec + the shared writer) lives in
// `ados_protocol::tap` (Contract F) so the video-pipeline writer and this reader
// build against one frozen definition. `TapSource` below is the reader half.

/// Reads frames from the video pipeline's tap socket.
pub struct TapSource {
    camera_id: String,
    socket_path: String,
    stream: Option<UnixStream>,
    pool: BufferPool,
}

impl TapSource {
    pub fn new(camera_id: impl Into<String>, socket_path: impl Into<String>) -> Self {
        Self {
            camera_id: camera_id.into(),
            socket_path: socket_path.into(),
            stream: None,
            pool: BufferPool::new(),
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
        let hdr = match decode_tap_header(&header) {
            Ok(v) => v,
            Err(e) => {
                self.stream = None;
                return Err(e.into());
            }
        };
        let stream = self.stream.as_mut().expect("still connected");
        let mut data = self.pool.take(hdr.byte_len);
        if hdr.byte_len > 0 {
            if let Err(e) = stream.read_exact(&mut data).await {
                self.stream = None;
                return Err(e.into());
            }
        }
        Ok(RawFrame {
            width: hdr.width,
            height: hdr.height,
            format: hdr.format,
            data,
            timing: FrameTiming::host(hdr.ts_mono_ns),
            dropped_corrupt: 0,
        })
    }

    fn camera_id(&self) -> &str {
        &self.camera_id
    }
    fn recycler(&self) -> BufferReturn {
        self.pool.recycler()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
            timing: FrameTiming::host(987_654_321),
            dropped_corrupt: 0,
        };
        let frame_clone = frame.clone();
        let writer = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            ados_protocol::tap::write_tap_frame(
                &mut s,
                frame_clone.format,
                frame_clone.width,
                frame_clone.height,
                &frame_clone.data,
                frame_clone.timing.ts_ns,
            )
            .await
            .unwrap();
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
        assert_eq!(got.timing, FrameTiming::host(987_654_321));
        assert_eq!(src.camera_id(), "uvc-0");
        writer.abort();
    }

    #[test]
    fn buffer_pool_flat_allocations() {
        const FRAME: usize = 1920 * 1080 * 3 / 2;
        let mut pool = BufferPool::new();
        let back = pool.recycler();
        // Steady state: one frame published (returned at once), one held by an
        // in-flight inference that hands it back a frame later.
        let mut in_flight: Option<Vec<u8>> = None;
        for i in 0..300 {
            let buf = pool.take(FRAME);
            assert_eq!(buf.len(), FRAME);
            if i % 2 == 0 {
                if let Some(prev) = in_flight.replace(buf) {
                    back.give(prev);
                }
            } else {
                back.give(buf);
            }
        }
        assert!(
            pool.allocations() <= POOL_BUFFERS as u64,
            "allocations stayed flat: {}",
            pool.allocations()
        );

        // A burst of returns never parks more than the pool size.
        for _ in 0..10 {
            back.give(vec![0u8; 8]);
        }
        let before = pool.allocations();
        for _ in 0..POOL_BUFFERS {
            let _ = pool.take(8);
        }
        // Spares exhausted: the next take allocates.
        let _ = pool.take(8);
        assert_eq!(pool.allocations(), before + 1);
    }
}
