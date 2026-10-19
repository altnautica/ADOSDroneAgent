//! The frame-bus → named-pipe pump behind a `framebus://` encoder input.

use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_protocol::framebus::{read_slot_into, FrameDescriptor, FrameFormat, RingLayout};
use ados_protocol::state::STATE_V2_MAX_FRAME;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Pause between attempts to reach the descriptor socket or a pipe reader.
const RETRY: Duration = Duration::from_millis(200);

/// Where the rings live; the vision engine's `ADOS_SHM_DIR` override applies.
fn shm_dir() -> PathBuf {
    std::env::var_os("ADOS_SHM_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/dev/shm"))
}

/// What one feed pumps: the camera, the frame geometry the encoder argv was
/// built for, and the endpoints.
#[derive(Debug, Clone)]
pub struct FeedSpec {
    pub camera_id: String,
    pub format: FrameFormat,
    pub width: u32,
    pub height: u32,
    pub fifo: PathBuf,
    pub frames_socket: PathBuf,
    pub shm_dir: PathBuf,
}

impl FeedSpec {
    /// The spec for `camera_id` at the default run-dir endpoints.
    pub fn new(camera_id: &str, format: FrameFormat, width: u32, height: u32) -> Self {
        Self {
            camera_id: camera_id.to_string(),
            format,
            width,
            height,
            fifo: super::fifo_path(camera_id),
            frames_socket: super::frames_socket_path(),
            shm_dir: shm_dir(),
        }
    }

    /// Whether a descriptor is a frame of this camera in the geometry the
    /// encoder reads. Any other frame would desynchronise the raw stream.
    pub fn accepts(&self, d: &FrameDescriptor) -> bool {
        d.camera_id == self.camera_id
            && d.format == self.format
            && d.width == self.width
            && d.height == self.height
    }
}

/// A running pump. Dropping it stops the pump.
pub struct FramebusFeed {
    spec: FeedSpec,
    tasks: [JoinHandle<()>; 2],
}

impl Drop for FramebusFeed {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl FramebusFeed {
    /// Create the pipe and start pumping on the current tokio runtime.
    pub fn spawn(spec: FeedSpec) -> io::Result<Self> {
        ensure_fifo(&spec.fifo)?;
        let (tx, rx) = watch::channel(None);
        let reader = tokio::spawn(read_descriptors(spec.clone(), tx));
        let writer = tokio::spawn(write_frames(spec.clone(), rx));
        Ok(Self {
            spec,
            tasks: [reader, writer],
        })
    }

    /// What this feed pumps.
    pub fn spec(&self) -> &FeedSpec {
        &self.spec
    }
}

/// Make `path` a named pipe, replacing anything else found there.
fn ensure_fifo(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_fifo() => return Ok(()),
        Ok(_) => std::fs::remove_file(path)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    nix::unistd::mkfifo(path, nix::sys::stat::Mode::from_bits_truncate(0o660)).map_err(io::Error::from)
}

/// Keep the newest accepted descriptor in `tx`, reconnecting to the socket
/// whenever it drops.
async fn read_descriptors(spec: FeedSpec, tx: watch::Sender<Option<FrameDescriptor>>) {
    let mut body = Vec::new();
    let mut warned = false;
    loop {
        let Ok(mut stream) = UnixStream::connect(&spec.frames_socket).await else {
            tokio::time::sleep(RETRY).await;
            continue;
        };
        loop {
            let Ok(len) = stream.read_u32().await else {
                break;
            };
            if len as usize > STATE_V2_MAX_FRAME {
                break;
            }
            body.resize(len as usize, 0);
            if stream.read_exact(&mut body).await.is_err() {
                break;
            }
            let Ok(d) = FrameDescriptor::from_msgpack(&body) else {
                continue;
            };
            if d.camera_id != spec.camera_id {
                continue;
            }
            if !spec.accepts(&d) {
                if !warned {
                    tracing::warn!(
                        camera = %spec.camera_id,
                        width = d.width,
                        height = d.height,
                        format = ?d.format,
                        "framebus_frame_geometry_mismatch: frame skipped"
                    );
                    warned = true;
                }
                continue;
            }
            tx.send_replace(Some(d));
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// One mapped ring and the newest sequence read out of it.
struct Ring {
    map: memmap2::Mmap,
    layout: RingLayout,
    file_id: (u64, u64),
    last_seq: u64,
}

impl Ring {
    fn open(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let meta = file.metadata().ok()?;
        // SAFETY: a POSIX shared-memory ring the vision engine owns, mapped
        // read-only; a concurrent slot recycle is caught by the seqlock check
        // in `read_slot_into`.
        let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        let layout = RingLayout::read_header(&map)?;
        Some(Self {
            map,
            layout,
            file_id: (meta.dev(), meta.ino()),
            last_seq: 0,
        })
    }

    fn read(&mut self, d: &FrameDescriptor, out: &mut Vec<u8>) -> bool {
        let ok = matches!(
            read_slot_into(&self.map, &self.layout, d.slot, d.seq, out),
            Ok(Some(_))
        );
        if ok {
            self.last_seq = self.last_seq.max(d.seq);
        }
        ok
    }

    fn still_named_by(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == self.file_id)
    }
}

/// Read `d`'s pixels into `out`, mapping (or re-mapping a replaced) ring.
fn read_frame(ring: &mut Option<Ring>, shm_dir: &Path, d: &FrameDescriptor, out: &mut Vec<u8>) -> bool {
    let path = shm_dir.join(&d.shm_name);
    if let Some(r) = ring.as_mut() {
        if d.seq > r.last_seq && r.read(d, out) {
            return true;
        }
        if r.still_named_by(&path) {
            return d.seq <= r.last_seq && r.read(d, out);
        }
    }
    *ring = Ring::open(&path);
    ring.as_mut().is_some_and(|r| r.read(d, out))
}

/// Open the pipe for writing once an encoder has it open for reading.
async fn open_sender(fifo: &Path) -> pipe::Sender {
    loop {
        match pipe::OpenOptions::new().open_sender(fifo) {
            Ok(s) => return s,
            // ENXIO: no reader yet (the encoder has not started).
            Err(e) if e.raw_os_error() == Some(nix::libc::ENXIO) => {}
            Err(e) => tracing::debug!(error = %e, fifo = %fifo.display(), "framebus_fifo_open_failed"),
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Write every new frame to the pipe; reopen it whenever its reader goes away.
async fn write_frames(spec: FeedSpec, mut rx: watch::Receiver<Option<FrameDescriptor>>) {
    let mut ring = None;
    let mut pixels = Vec::new();
    loop {
        let mut sender = open_sender(&spec.fifo).await;
        tracing::info!(camera = %spec.camera_id, "framebus_feed_reader_attached");
        // Start from the next frame, not one queued before this reader came.
        rx.mark_unchanged();
        loop {
            if rx.changed().await.is_err() {
                return;
            }
            let Some(d) = rx.borrow_and_update().clone() else {
                continue;
            };
            if !read_frame(&mut ring, &spec.shm_dir, &d, &mut pixels) {
                continue;
            }
            if sender.write_all(&pixels).await.is_err() {
                break;
            }
        }
        tracing::info!(camera = %spec.camera_id, "framebus_feed_reader_detached");
    }
}
