//! Off-thread framebuffer writer.
//!
//! Ports the decoupled SPI writer from `renderers/framebuffer.py` (the
//! `present` / `_writer_loop` / `cleanup` block). This is the load-bearing
//! fidelity surface: the synchronous mmap write must never run on a runtime
//! worker, because a blocking SPI write on the render loop's critical path was
//! the root cascade behind the LCD-freeze -> video-stall regression. The model
//! preserved here, exactly:
//!
//! * Single-slot latest-wins holder (`Mutex<Option<Frame>>`): a new frame
//!   stashed while the previous one is still pending OVERWRITES it and bumps the
//!   drop counter.
//! * A DEDICATED OS thread (`std::thread`, NOT a tokio task) owns the blocking
//!   pack + write off any runtime worker.
//! * Duplicate-skip on a hash of the RAW input bytes (not the packed buffer):
//!   an identical frame skips the pack + write entirely. The last-written hash
//!   is updated ONLY after a successful write.
//! * stats(): writes / drops / skipped_duplicates / last_write_ms.
//! * cleanup ordering: signal stop, wake the writer, JOIN it (1 s) BEFORE the
//!   sink is dropped, and drain a frame stashed right before stop so it still
//!   lands.
//!
//! The real `/dev/fbN` mmap is the [`MmapSink`] (Linux-gated). The threading
//! model is generic over a [`FrameSink`], so the latest-wins / dup-skip / stats
//! / cleanup behavior is unit-tested against a Vec-backed fake sink.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

/// Join timeout when tearing down the writer thread (matches the Python 1 s).
pub const WRITER_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// One frame's worth of already-packed bytes plus the hash of its raw input.
///
/// The caller packs the input (RGB565/RGB888/xRGB) before stashing — the hash
/// is over the RAW pre-pack bytes so a duplicate skips even the pack. (In the
/// Python version the pack happens on the writer thread; here packing is the
/// caller's job via [`crate::pack`], but the dup-skip still keys on raw input.)
#[derive(Debug, Clone)]
pub struct Frame {
    /// Bytes to write to the sink (already packed to the panel's bit depth).
    pub packed: Vec<u8>,
    /// Hash of the raw, pre-pack input bytes — the dup-skip key.
    pub input_hash: u64,
}

impl Frame {
    /// Build a frame, hashing `raw_input` for the dup-skip key.
    pub fn new(packed: Vec<u8>, raw_input: &[u8]) -> Self {
        Self {
            packed,
            input_hash: hash_bytes(raw_input),
        }
    }
}

/// Hash raw input bytes for the duplicate-skip comparison.
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Inclusive `(first, last)` byte offsets that differ between two equal-length
/// frames, or `None` when they are byte-identical.
///
/// The framebuffer sink writes only this span into the mmap and flushes only
/// this span, so the fbtft deferred-IO marks only the touched pages dirty and
/// pushes only the changed scanline band over SPI. A localized UI change (a
/// ticking number, a tab highlight) becomes a few-row push instead of the whole
/// 480x320 panel; a full repaint (page switch) widens the span back to the whole
/// frame, the same cost as before — never a regression.
pub fn changed_span(prev: &[u8], next: &[u8]) -> Option<(usize, usize)> {
    debug_assert_eq!(prev.len(), next.len(), "changed_span needs equal lengths");
    let len = prev.len().min(next.len());
    let mut first = 0;
    while first < len && prev[first] == next[first] {
        first += 1;
    }
    if first == len {
        return None;
    }
    let mut last = len - 1;
    while last > first && prev[last] == next[last] {
        last -= 1;
    }
    Some((first, last))
}

/// Where packed frames go. The real implementation mmaps `/dev/fbN`; the test
/// fake captures the bytes in a Vec.
pub trait FrameSink: Send {
    /// Write `buf` to the device. `buf.len()` is the full frame size; an error
    /// stops the writer (a disconnected SPI bus or a closed mapping).
    fn write_frame(&mut self, buf: &[u8]) -> std::io::Result<()>;
}

/// Observability snapshot, mirroring `FrameBufferRenderer.stats()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WriterStats {
    pub writes: u64,
    pub drops: u64,
    pub skipped_duplicates: u64,
    pub last_write_ms: Option<f64>,
}

/// Shared state between the public handle and the writer thread.
struct Shared {
    /// Single-slot latest-wins holder + the last successfully-written hash.
    pending: Mutex<PendingState>,
    /// Signals a fresh frame (or a stop request) to the writer.
    cv: Condvar,
    writes: AtomicU64,
    drops: AtomicU64,
    skipped: AtomicU64,
    /// last_write_ms encoded as bits (f64::to_bits); u64::MAX == None.
    last_write_ms_bits: AtomicU64,
    stop: std::sync::atomic::AtomicBool,
}

struct PendingState {
    frame: Option<Frame>,
    last_written_hash: Option<u64>,
}

const NO_LAST_WRITE: u64 = u64::MAX;

/// The off-thread framebuffer writer handle. `present` stashes a frame (returns
/// immediately); the dedicated thread packs-already-done + writes. Drop or
/// [`FbWriter::cleanup`] tears the thread down before the sink is released.
pub struct FbWriter {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl FbWriter {
    /// Spawn the dedicated writer thread around `sink`. The thread owns the sink
    /// for its whole life so the blocking write never touches a runtime worker.
    pub fn spawn<S: FrameSink + 'static>(sink: S) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(PendingState {
                frame: None,
                last_written_hash: None,
            }),
            cv: Condvar::new(),
            writes: AtomicU64::new(0),
            drops: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            last_write_ms_bits: AtomicU64::new(NO_LAST_WRITE),
            stop: std::sync::atomic::AtomicBool::new(false),
        });
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("ados-fb-writer".to_string())
                .spawn(move || writer_loop(shared, sink))
                .expect("spawn fb writer thread")
        };
        Self {
            shared,
            thread: Some(thread),
        }
    }

    /// Stash a frame for the writer and return immediately. If a frame is still
    /// pending (the writer has not picked it up yet) it is OVERWRITTEN and the
    /// drop counter is bumped — latest-wins.
    pub fn present(&self, frame: Frame) {
        {
            let mut p = self.shared.pending.lock().unwrap();
            if p.frame.is_some() {
                self.shared.drops.fetch_add(1, Ordering::Relaxed);
            }
            p.frame = Some(frame);
        }
        self.shared.cv.notify_one();
    }

    /// Current observability snapshot.
    pub fn stats(&self) -> WriterStats {
        let bits = self.shared.last_write_ms_bits.load(Ordering::Relaxed);
        WriterStats {
            writes: self.shared.writes.load(Ordering::Relaxed),
            drops: self.shared.drops.load(Ordering::Relaxed),
            skipped_duplicates: self.shared.skipped.load(Ordering::Relaxed),
            last_write_ms: if bits == NO_LAST_WRITE {
                None
            } else {
                Some(round2(f64::from_bits(bits)))
            },
        }
    }

    /// Stop the writer and join it (up to [`WRITER_JOIN_TIMEOUT`]) BEFORE the
    /// sink is dropped, so it can never write into a released mapping. A frame
    /// stashed just before stop is drained so a present() right before cleanup
    /// still lands. Idempotent.
    pub fn cleanup(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.cv.notify_all();
        if let Some(thread) = self.thread.take() {
            // Join with a soft timeout: poll is_finished so a wedged write
            // cannot block teardown forever, matching the Python join(1.0).
            let deadline = Instant::now() + WRITER_JOIN_TIMEOUT;
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
            // If still running past the deadline we drop the handle without
            // blocking; the thread holds the sink and exits on its own.
        }
    }
}

impl Drop for FbWriter {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// The dedicated-thread loop. Waits for a frame (waking periodically so cleanup
/// breaks it out), drains latest-wins, skips duplicates by raw-input hash, and
/// writes — updating the last-written hash only after a successful write.
fn writer_loop<S: FrameSink>(shared: Arc<Shared>, mut sink: S) {
    loop {
        let stop = shared.stop.load(Ordering::SeqCst);
        // Pull the pending frame (latest-wins), waiting if none and not stopping.
        let (frame, last_hash) = {
            let mut p = shared.pending.lock().unwrap();
            if p.frame.is_none() && !stop {
                // Wait for a frame or a stop, with a periodic wake.
                let (guard, _timeout) = shared
                    .cv
                    .wait_timeout(p, std::time::Duration::from_millis(500))
                    .unwrap();
                p = guard;
            }
            let frame = p.frame.take();
            (frame, p.last_written_hash)
        };

        let Some(frame) = frame else {
            // No pending work. Exit if stopping; else loop back to wait.
            if shared.stop.load(Ordering::SeqCst) {
                return;
            }
            continue;
        };

        // Duplicate-skip: identical raw input as the last successful write skips
        // the device write entirely.
        if last_hash == Some(frame.input_hash) {
            shared.skipped.fetch_add(1, Ordering::Relaxed);
            // A drained-on-stop duplicate still counts as handled; exit if
            // stopping so a present-before-cleanup duplicate does not spin.
            if shared.stop.load(Ordering::SeqCst) {
                return;
            }
            continue;
        }

        let t0 = Instant::now();
        match sink.write_frame(&frame.packed) {
            Ok(()) => {
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                shared
                    .last_write_ms_bits
                    .store(ms.to_bits(), Ordering::Relaxed);
                shared.writes.fetch_add(1, Ordering::Relaxed);
                // Update the last-written hash ONLY after a successful write.
                shared.pending.lock().unwrap().last_written_hash = Some(frame.input_hash);
            }
            Err(e) => {
                // Disconnected SPI bus or a closed mapping. Stop trying.
                tracing::warn!(error = %e, "framebuffer write failed");
                return;
            }
        }

        if shared.stop.load(Ordering::SeqCst) {
            // Drained the present-before-cleanup frame; now exit.
            // (Loop back once more only if another frame is already pending so a
            // burst right before stop is not silently dropped.)
            let has_more = shared.pending.lock().unwrap().frame.is_some();
            if !has_more {
                return;
            }
        }
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Real `/dev/fbN` mmap sink. Each write copies only the bytes that changed
/// since the last frame into the mapping and flushes only that span, so the
/// fbtft driver pushes just the changed scanline band over SPI. Linux-only (the
/// mmap + flush use the unix mapping API). Construct via [`MmapSink::open`].
#[cfg(target_os = "linux")]
pub struct MmapSink {
    _file: std::fs::File,
    map: memmap2::MmapMut,
    frame_bytes: usize,
    /// The last frame written to the device, for the per-frame diff. Empty
    /// until the first write (which always blits the full frame).
    last: Vec<u8>,
}

#[cfg(target_os = "linux")]
impl MmapSink {
    /// Open `fb_path` read-write and map `frame_bytes` of it. `frame_bytes` is
    /// `width * height * (bpp / 8)`.
    pub fn open(fb_path: &str, frame_bytes: usize) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(fb_path)?;
        // SAFETY: the framebuffer device is a fixed-size mapping the kernel
        // backs for the life of the file handle; we map exactly frame_bytes and
        // never alias it elsewhere.
        let map = unsafe {
            memmap2::MmapOptions::new()
                .len(frame_bytes)
                .map_mut(&file)?
        };
        Ok(Self {
            _file: file,
            map,
            frame_bytes,
            last: Vec::new(),
        })
    }
}

#[cfg(target_os = "linux")]
impl FrameSink for MmapSink {
    fn write_frame(&mut self, buf: &[u8]) -> std::io::Result<()> {
        let len = buf.len().min(self.frame_bytes);
        let src = &buf[..len];
        // First frame (or a geometry change that resized the buffer): the diff
        // baseline is missing, so blit the whole frame and seed `last`.
        if self.last.len() != len {
            self.map[..len].copy_from_slice(src);
            self.map.flush()?;
            self.last.clear();
            self.last.extend_from_slice(src);
            return Ok(());
        }
        // Push only the changed span. fbtft's deferred-IO faults the touched
        // pages dirty and sends just that scanline band, so an unchanged frame
        // costs nothing and a localized change costs a few rows, not 480x320.
        match changed_span(&self.last, src) {
            None => Ok(()),
            Some((first, last)) => {
                self.map[first..=last].copy_from_slice(&src[first..=last]);
                self.map.flush_range(first, last - first + 1)?;
                self.last[first..=last].copy_from_slice(&src[first..=last]);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A Vec-backed fake sink that records every write. Optionally fails after a
    /// configured number of writes to exercise the stop-on-error path.
    #[derive(Clone)]
    struct FakeSink {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        write_count: Arc<AtomicUsize>,
        fail_after: Option<usize>,
        /// Optional per-write delay so the test can fill the pending slot while
        /// the writer is "busy".
        delay: Option<std::time::Duration>,
    }

    impl FakeSink {
        fn new() -> Self {
            Self {
                writes: Arc::new(Mutex::new(Vec::new())),
                write_count: Arc::new(AtomicUsize::new(0)),
                fail_after: None,
                delay: None,
            }
        }
    }

    impl FrameSink for FakeSink {
        fn write_frame(&mut self, buf: &[u8]) -> std::io::Result<()> {
            if let Some(d) = self.delay {
                std::thread::sleep(d);
            }
            let n = self.write_count.fetch_add(1, Ordering::SeqCst);
            if let Some(limit) = self.fail_after {
                if n >= limit {
                    return Err(std::io::Error::other("simulated SPI disconnect"));
                }
            }
            self.writes.lock().unwrap().push(buf.to_vec());
            Ok(())
        }
    }

    /// Wait until the predicate holds or a short deadline passes.
    fn wait_until<F: Fn() -> bool>(f: F) {
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while !f() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn writes_a_frame_to_the_sink() {
        let sink = FakeSink::new();
        let recorded = sink.writes.clone();
        let mut w = FbWriter::spawn(sink);
        w.present(Frame::new(vec![1, 2, 3, 4], b"input-a"));
        wait_until(|| !recorded.lock().unwrap().is_empty());
        w.cleanup();
        let got = recorded.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], vec![1, 2, 3, 4]);
        assert_eq!(w.stats().writes, 1);
        assert!(w.stats().last_write_ms.is_some());
    }

    #[test]
    fn duplicate_raw_input_skips_the_write() {
        let sink = FakeSink::new();
        let writes = sink.write_count.clone();
        let mut w = FbWriter::spawn(sink);
        // Two frames with identical RAW input but the second can carry any
        // packed bytes — dup-skip keys on the raw-input hash.
        w.present(Frame::new(vec![1, 2, 3, 4], b"same-input"));
        wait_until(|| writes.load(Ordering::SeqCst) == 1);
        w.present(Frame::new(vec![9, 9, 9, 9], b"same-input"));
        // Give the writer a moment; it must NOT issue a second device write.
        wait_until(|| w.stats().skipped_duplicates == 1);
        w.cleanup();
        assert_eq!(w.stats().writes, 1);
        assert_eq!(w.stats().skipped_duplicates, 1);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn changed_input_after_a_duplicate_writes_again() {
        let sink = FakeSink::new();
        let mut w = FbWriter::spawn(sink);
        w.present(Frame::new(vec![1], b"a"));
        wait_until(|| w.stats().writes == 1);
        w.present(Frame::new(vec![1], b"a")); // dup -> skip
        wait_until(|| w.stats().skipped_duplicates == 1);
        w.present(Frame::new(vec![2], b"b")); // changed -> write
        wait_until(|| w.stats().writes == 2);
        w.cleanup();
        assert_eq!(w.stats().writes, 2);
        assert_eq!(w.stats().skipped_duplicates, 1);
    }

    #[test]
    fn latest_wins_drops_the_superseded_pending_frame() {
        // A slow sink keeps the writer busy on the first frame; stashing two
        // more while it is busy must drop one (latest-wins) and write the last.
        let mut sink = FakeSink::new();
        sink.delay = Some(std::time::Duration::from_millis(80));
        let recorded = sink.writes.clone();
        let mut w = FbWriter::spawn(sink);
        w.present(Frame::new(vec![0xA], b"frame-1"));
        // While frame-1 is being written, queue 2 and 3 back to back.
        std::thread::sleep(std::time::Duration::from_millis(10));
        w.present(Frame::new(vec![0xB], b"frame-2"));
        w.present(Frame::new(vec![0xC], b"frame-3"));
        wait_until(|| {
            let r = recorded.lock().unwrap();
            r.iter().any(|f| f == &vec![0xC])
        });
        w.cleanup();
        // At least one drop happened (frame-2 overwritten by frame-3).
        assert!(w.stats().drops >= 1, "stats={:?}", w.stats());
        let got = recorded.lock().unwrap();
        // frame-1 and frame-3 landed; frame-2 was dropped, never written.
        assert!(got.iter().any(|f| f == &vec![0xA]));
        assert!(got.iter().any(|f| f == &vec![0xC]));
        assert!(!got.iter().any(|f| f == &vec![0xB]));
    }

    #[test]
    fn drain_pending_frame_on_stop_still_writes() {
        // A frame stashed right before cleanup must still land (Python tests
        // rely on this drain-on-stop behavior).
        let sink = FakeSink::new();
        let recorded = sink.writes.clone();
        let mut w = FbWriter::spawn(sink);
        // Stash and immediately cleanup without waiting.
        w.present(Frame::new(vec![0xEE], b"last-frame"));
        w.cleanup();
        let got = recorded.lock().unwrap();
        assert!(
            got.iter().any(|f| f == &vec![0xEE]),
            "drained frame missing"
        );
    }

    #[test]
    fn write_error_stops_the_writer_without_panicking() {
        let mut sink = FakeSink::new();
        sink.fail_after = Some(1); // first write ok, second errors
        let mut w = FbWriter::spawn(sink);
        w.present(Frame::new(vec![1], b"a"));
        wait_until(|| w.stats().writes == 1);
        w.present(Frame::new(vec![2], b"b")); // triggers the error path
                                              // The writer exits on error; cleanup joins cleanly.
        std::thread::sleep(std::time::Duration::from_millis(50));
        w.cleanup();
        // Only the first frame was recorded; the writer stopped after the error.
        assert_eq!(w.stats().writes, 1);
    }

    #[test]
    fn cleanup_is_idempotent() {
        let sink = FakeSink::new();
        let mut w = FbWriter::spawn(sink);
        w.present(Frame::new(vec![1], b"a"));
        wait_until(|| w.stats().writes == 1);
        w.cleanup();
        w.cleanup(); // second call is a no-op, must not panic
        assert_eq!(w.stats().writes, 1);
    }

    #[test]
    fn stats_round_trip_starts_empty() {
        let sink = FakeSink::new();
        let w = FbWriter::spawn(sink);
        let s = w.stats();
        assert_eq!(s.writes, 0);
        assert_eq!(s.drops, 0);
        assert_eq!(s.skipped_duplicates, 0);
        assert!(s.last_write_ms.is_none());
    }

    #[test]
    fn hash_is_stable_and_distinguishes_input() {
        assert_eq!(hash_bytes(b"abc"), hash_bytes(b"abc"));
        assert_ne!(hash_bytes(b"abc"), hash_bytes(b"abd"));
    }

    #[test]
    fn changed_span_is_none_for_identical_frames() {
        let a = vec![0u8; 64];
        let b = vec![0u8; 64];
        assert_eq!(changed_span(&a, &b), None);
    }

    #[test]
    fn changed_span_brackets_a_localized_change() {
        let prev = vec![0u8; 64];
        let mut next = prev.clone();
        // A localized change at bytes 20..=23 (a small UI update) reports a
        // tight span, not the whole buffer — only those rows reach the panel.
        next[20] = 1;
        next[21] = 2;
        next[23] = 4;
        assert_eq!(changed_span(&prev, &next), Some((20, 23)));
    }

    #[test]
    fn changed_span_covers_first_and_last_byte() {
        let prev = vec![0u8; 8];
        let mut next = prev.clone();
        next[0] = 9;
        next[7] = 9;
        // First and last bytes differ -> the span is the whole buffer.
        assert_eq!(changed_span(&prev, &next), Some((0, 7)));
    }

    #[test]
    fn changed_span_single_byte() {
        let prev = vec![5u8; 16];
        let mut next = prev.clone();
        next[9] = 6;
        assert_eq!(changed_span(&prev, &next), Some((9, 9)));
    }
}
