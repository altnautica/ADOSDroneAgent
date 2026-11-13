//! The frame-bus → named-pipe pump behind a `framebus://` encoder input.

use std::collections::VecDeque;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_protocol::framebus::{read_slot_into, FrameDescriptor, FrameFormat, RingLayout};
use ados_protocol::state::STATE_V2_MAX_FRAME;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::net::UnixStream;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::pace::{Pacer, QUEUE_DEPTH};
use super::FeedStats;

/// Pause between attempts to reach the descriptor socket or a pipe reader.
const RETRY: Duration = Duration::from_millis(200);

/// Where the rings live; the vision engine's `ADOS_SHM_DIR` override applies.
fn shm_dir() -> PathBuf {
    std::env::var_os("ADOS_SHM_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/dev/shm"))
}

/// What one feed pumps: the camera, the frame geometry and nominal rate the
/// encoder argv was built for, and the endpoints.
#[derive(Debug, Clone)]
pub struct FeedSpec {
    pub camera_id: String,
    pub format: FrameFormat,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub fifo: PathBuf,
    pub frames_socket: PathBuf,
    pub shm_dir: PathBuf,
}

impl FeedSpec {
    /// The spec for `camera_id` at the default run-dir endpoints.
    pub fn new(camera_id: &str, format: FrameFormat, width: u32, height: u32, fps: u32) -> Self {
        Self {
            camera_id: camera_id.to_string(),
            format,
            width,
            height,
            fps,
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

/// A frame waiting for the pipe, and when to write it.
struct Queued {
    d: FrameDescriptor,
    at: Instant,
}

/// The frames between the descriptor reader and the pipe writer, and the
/// feed's counters.
#[derive(Default)]
struct Shared {
    queue: Mutex<VecDeque<Queued>>,
    ready: Notify,
    /// An encoder has the pipe open; frames are queued only then.
    attached: AtomicBool,
    written: AtomicU64,
    skipped: AtomicU64,
    seqlock_misses: AtomicU64,
    overflows: AtomicU64,
    hold_us: AtomicU64,
}

impl Shared {
    fn push(&self, q: Queued) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() >= QUEUE_DEPTH {
            queue.pop_front();
            self.overflows.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(q);
        drop(queue);
        self.ready.notify_one();
    }

    fn pop(&self) -> Option<Queued> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    fn clear(&self) {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Count a written frame and the frame ids skipped since the last one. A
    /// frame id that went backwards (a restarted source) starts over.
    fn record_written(&self, last: &mut Option<u64>, frame_id: u64) {
        if let Some(prev) = *last {
            if frame_id > prev {
                self.skipped
                    .fetch_add(frame_id - prev - 1, Ordering::Relaxed);
            }
        }
        *last = Some(frame_id);
        self.written.fetch_add(1, Ordering::Relaxed);
    }
}

/// A running pump. Dropping it stops the pump.
pub struct FramebusFeed {
    spec: FeedSpec,
    tasks: [JoinHandle<()>; 2],
    renew: watch::Sender<u64>,
    shared: Arc<Shared>,
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
        let shared = Arc::new(Shared::default());
        let (renew, renewed) = watch::channel(0);
        let reader = tokio::spawn(read_descriptors(spec.clone(), shared.clone()));
        let writer = tokio::spawn(write_frames(spec.clone(), shared.clone(), renewed));
        Ok(Self {
            spec,
            tasks: [reader, writer],
            renew,
            shared,
        })
    }

    /// Give the next encoder a fresh pipe: unlink and recreate the FIFO, and
    /// move the writer onto it. Bytes still buffered in the old pipe (a frame
    /// the previous encoder never read) stay with the old, unlinked pipe, so
    /// the new reader starts on a frame boundary. Call before every encoder
    /// spawn that reads this feed.
    pub fn renew(&self) -> io::Result<()> {
        match std::fs::remove_file(&self.spec.fifo) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        ensure_fifo(&self.spec.fifo)?;
        self.renew.send_modify(|g| *g += 1);
        Ok(())
    }

    /// What this feed pumps.
    pub fn spec(&self) -> &FeedSpec {
        &self.spec
    }

    /// The feed's counters so far.
    pub fn stats(&self) -> FeedStats {
        let s = &self.shared;
        FeedStats {
            camera_id: self.spec.camera_id.clone(),
            frames_written: s.written.load(Ordering::Relaxed),
            frames_skipped: s.skipped.load(Ordering::Relaxed),
            seqlock_misses: s.seqlock_misses.load(Ordering::Relaxed),
            queue_overflows: s.overflows.load(Ordering::Relaxed),
            hold_ms: s.hold_us.load(Ordering::Relaxed) as f64 / 1000.0,
        }
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
    nix::unistd::mkfifo(path, nix::sys::stat::Mode::from_bits_truncate(0o660))
        .map_err(io::Error::from)
}

/// Queue every accepted descriptor with its write time, reconnecting to the
/// socket whenever it drops.
async fn read_descriptors(spec: FeedSpec, shared: Arc<Shared>) {
    let mut body = Vec::new();
    let mut warned = false;
    let mut pacer = Pacer::new(spec.fps);
    let epoch = Instant::now();
    loop {
        let Ok(mut stream) = UnixStream::connect(&spec.frames_socket).await else {
            tokio::time::sleep(RETRY).await;
            continue;
        };
        while let Ok(len) = stream.read_u32().await {
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
            if !shared.attached.load(Ordering::Relaxed) {
                continue;
            }
            // The source clock when the frame carries one (a sensor or the
            // simulation: evenly spaced at capture), else host time.
            let capture_ns = d.ts_sensor_ns.unwrap_or(d.ts_ns);
            let now = Instant::now();
            let now_ns = u64::try_from(now.duration_since(epoch).as_nanos()).unwrap_or(u64::MAX);
            let hold = pacer.hold_for(capture_ns, now_ns);
            shared
                .hold_us
                .store(pacer.hold_ns() / 1000, Ordering::Relaxed);
            shared.push(Queued {
                d,
                at: now + Duration::from_nanos(hold),
            });
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

/// What reading a descriptor's frame out of its ring found.
#[derive(Debug, PartialEq, Eq)]
enum Read {
    Frame,
    /// The slot holds a newer frame: the engine reused it first.
    Recycled,
    /// No ring to read (not created yet, or the descriptor does not fit it).
    Unavailable,
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

    fn read(&mut self, d: &FrameDescriptor, out: &mut Vec<u8>) -> Read {
        match read_slot_into(&self.map, &self.layout, d.slot, d.seq, out) {
            Ok(Some(_)) => {
                self.last_seq = self.last_seq.max(d.seq);
                Read::Frame
            }
            Ok(None) => Read::Recycled,
            Err(_) => Read::Unavailable,
        }
    }

    fn still_named_by(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == self.file_id)
    }
}

/// Read `d`'s pixels into `out`, mapping (or re-mapping a replaced) ring.
fn read_frame(
    ring: &mut Option<Ring>,
    shm_dir: &Path,
    d: &FrameDescriptor,
    out: &mut Vec<u8>,
) -> Read {
    let path = shm_dir.join(&d.shm_name);
    if let Some(r) = ring.as_mut() {
        if d.seq > r.last_seq {
            let read = r.read(d, out);
            if read == Read::Frame || r.still_named_by(&path) {
                return read;
            }
        } else if r.still_named_by(&path) {
            return r.read(d, out);
        }
    }
    *ring = Ring::open(&path);
    ring.as_mut().map_or(Read::Unavailable, |r| r.read(d, out))
}

/// Open the pipe for writing once an encoder has it open for reading.
async fn open_sender(fifo: &Path) -> pipe::Sender {
    loop {
        match pipe::OpenOptions::new().open_sender(fifo) {
            Ok(s) => return s,
            // ENXIO: no reader yet (the encoder has not started).
            Err(e) if e.raw_os_error() == Some(nix::libc::ENXIO) => {}
            Err(e) => {
                tracing::debug!(error = %e, fifo = %fifo.display(), "framebus_fifo_open_failed")
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Write every queued frame to the pipe at its time; reopen the pipe whenever
/// its reader goes away.
async fn write_frames(spec: FeedSpec, shared: Arc<Shared>, mut renewed: watch::Receiver<u64>) {
    let mut ring = None;
    let mut pixels = Vec::new();
    loop {
        renewed.mark_unchanged();
        let mut sender = tokio::select! {
            s = open_sender(&spec.fifo) => s,
            // The pipe was replaced before a reader came: open the new one.
            _ = renewed.changed() => continue,
        };
        tracing::info!(camera = %spec.camera_id, "framebus_feed_reader_attached");
        // Start from the next frame, not one queued before this reader came.
        shared.clear();
        shared.attached.store(true, Ordering::Relaxed);
        let mut last_written = None;
        loop {
            let Some(q) = shared.pop() else {
                tokio::select! {
                    _ = shared.ready.notified() => continue,
                    // A new encoder is about to read a fresh pipe.
                    _ = renewed.changed() => break,
                }
            };
            tokio::select! {
                _ = tokio::time::sleep_until(q.at) => {}
                _ = renewed.changed() => break,
            }
            match read_frame(&mut ring, &spec.shm_dir, &q.d, &mut pixels) {
                Read::Frame => {}
                Read::Recycled => {
                    shared.seqlock_misses.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Read::Unavailable => continue,
            }
            if sender.write_all(&pixels).await.is_err() {
                break;
            }
            shared.record_written(&mut last_written, q.d.frame_id);
        }
        shared.attached.store(false, Ordering::Relaxed);
        tracing::info!(camera = %spec.camera_id, "framebus_feed_reader_detached");
    }
}
