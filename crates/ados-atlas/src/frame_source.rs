//! Where the capture service gets camera frames.
//!
//! Two sources behind one enum (the [`crate::pose_source`] sibling pattern, and
//! the vision engine's own `AnySource`):
//!
//! - [`VisionFrameSource`] is the real path. It subscribes to the vision
//!   engine's `vision-frames.sock` descriptor broadcast, maps the `/dev/shm`
//!   ring each descriptor names, and reads the slot — only the small descriptor
//!   crosses the socket; the pixels are copied straight out of shared memory.
//! - [`SyntheticFrameSource`] emits deterministic frames with no hardware, for
//!   the SITL harness and demo runs.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use ados_protocol::framebus::{
    read_slot, DescriptorError, FrameDescriptor, FrameFormat, RingLayout,
};
use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};
use ados_protocol::state::STATE_V2_MAX_FRAME;
use memmap2::Mmap;
use tokio::net::UnixStream;

/// One frame pulled from a source: the raw pixels plus what they are.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub camera_id: String,
    pub ts_ms: i64,
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
    pub bytes: Vec<u8>,
}

/// The frame source the daemon runs with.
pub enum AtlasFrameSource {
    Vision(VisionFrameSource),
    Synthetic(SyntheticFrameSource),
}

impl AtlasFrameSource {
    /// Pull the next frame. `None` means the source needs a moment (a dropped
    /// socket the real source will reconnect on the next call, or an exhausted
    /// synthetic sequence); the caller backs off and calls again.
    pub async fn next(&mut self) -> Option<CapturedFrame> {
        match self {
            AtlasFrameSource::Vision(v) => v.next().await,
            AtlasFrameSource::Synthetic(s) => s.next().await,
        }
    }
}

/// The real source: the vision engine's frame-descriptor broadcast plus the
/// shared-memory rings the descriptors point at.
pub struct VisionFrameSource {
    socket_path: String,
    /// Enabled camera ids; empty means accept every camera.
    enabled: HashSet<String>,
    stream: Option<UnixStream>,
    /// One mapping per ring, opened lazily and cached: `shm_name` → (mmap, dev,
    /// ino). The device+inode identity lets a vision restart (same deterministic
    /// ring name, new inode) or a ring recreate be detected so the dead mapping
    /// is dropped instead of frozen forever.
    mmaps: HashMap<String, (Mmap, u64, u64)>,
    /// Camera ids we have already warned about dropping, so a persistent
    /// vision↔atlas id mismatch is logged once per id, not on every frame.
    warned_unmatched: HashSet<String>,
    /// Whether we have already warned about a descriptor whose wire version this
    /// build does not speak, so a persistent vision↔atlas version drift is logged
    /// once (and loudly), not silently dropped on every frame.
    warned_version: bool,
}

impl VisionFrameSource {
    pub fn new(socket_path: String, enabled: HashSet<String>) -> Self {
        Self {
            socket_path,
            enabled,
            stream: None,
            mmaps: HashMap::new(),
            warned_unmatched: HashSet::new(),
            warned_version: false,
        }
    }

    async fn ensure_connected(&mut self) -> bool {
        if self.stream.is_some() {
            return true;
        }
        match connect_with_retry(&self.socket_path, 3, Duration::from_millis(200)).await {
            Ok(s) => {
                tracing::info!(path = %self.socket_path, "atlas frame source connected");
                self.stream = Some(s);
                true
            }
            Err(e) => {
                tracing::debug!(path = %self.socket_path, error = %e, "atlas frame source connect failed");
                false
            }
        }
    }

    /// Copy a frame's pixels out of the ring the descriptor names. `None` when
    /// the ring is unmapped, the header is unreadable, or the slot was recycled
    /// mid-read (the seqlock check failed) — the frame is dropped and the next
    /// descriptor is awaited.
    fn read_frame_from_ring(&mut self, desc: &FrameDescriptor) -> Option<Vec<u8>> {
        let path = format!("/dev/shm/{}", desc.shm_name);
        // Re-stat the ring file so a vision restart (same deterministic name, new
        // inode) or a ring recreate is detected: a cached mmap of an unlinked
        // inode would otherwise read a frozen old slot and starve frames forever.
        let ident = match std::fs::metadata(&path) {
            Ok(m) => (m.dev(), m.ino()),
            Err(_) => {
                self.mmaps.remove(&desc.shm_name);
                return None;
            }
        };
        if self
            .mmaps
            .get(&desc.shm_name)
            .is_some_and(|(_, dev, ino)| (*dev, *ino) != ident)
        {
            self.mmaps.remove(&desc.shm_name);
        }
        if !self.mmaps.contains_key(&desc.shm_name) {
            let file = std::fs::File::open(&path).ok()?;
            // SAFETY: the ring is a fixed-size, single-writer shared mapping; the
            // seqlock in `read_slot` detects any write that races this read, so a
            // read-only view is sound.
            let mmap = unsafe { Mmap::map(&file) }.ok()?;
            self.mmaps
                .insert(desc.shm_name.clone(), (mmap, ident.0, ident.1));
        }

        enum Outcome {
            Frame(Vec<u8>),
            Skip,
            Stale,
        }
        let outcome = {
            let (mmap, _, _) = self.mmaps.get(&desc.shm_name).expect("just inserted");
            let region: &[u8] = &mmap[..];
            match RingLayout::read_header(region) {
                Some(layout) => match read_slot(region, &layout, desc.slot, desc.seq) {
                    Ok(Some(bytes)) => Outcome::Frame(bytes),
                    Ok(None) => Outcome::Skip,
                    // A layout mismatch means the ring was recreated at a new size;
                    // drop the stale mapping so the next frame re-maps it.
                    Err(_) => Outcome::Stale,
                },
                None => Outcome::Stale,
            }
        };
        match outcome {
            Outcome::Frame(b) => Some(b),
            Outcome::Skip => None,
            Outcome::Stale => {
                self.mmaps.remove(&desc.shm_name);
                None
            }
        }
    }

    pub async fn next(&mut self) -> Option<CapturedFrame> {
        if !self.ensure_connected().await {
            return None;
        }
        // Read descriptors until one resolves to a frame for an enabled camera.
        // A read error or EOF drops the connection and returns; the caller backs
        // off and the next call reconnects.
        loop {
            let stream = self.stream.as_mut()?;
            let payload = match read_length_prefixed(stream, STATE_V2_MAX_FRAME, true).await {
                Ok(Some(p)) => p,
                Ok(None) | Err(_) => {
                    self.stream = None;
                    return None;
                }
            };
            let desc = match FrameDescriptor::from_msgpack(&payload) {
                Ok(d) => d,
                // A version this build does not speak must be visible, never
                // silent: a vision↔atlas agent-version drift is the exact cause
                // of `ingest_rate_hz: 0` with no error. Warn once so a real drift
                // is one `ados logs` away, then keep draining (a torn/partial
                // frame is transient and stays quiet).
                Err(DescriptorError::Version { got, ours }) => {
                    if !self.warned_version {
                        self.warned_version = true;
                        tracing::warn!(
                            got,
                            ours,
                            "atlas dropping frames: vision.frame descriptor version not understood (vision/atlas agent version drift)"
                        );
                    }
                    continue;
                }
                Err(DescriptorError::Decode(_)) => continue,
            };
            if !self.enabled.is_empty() && !self.enabled.contains(&desc.camera_id) {
                // A dropped frame must be visible, never silent: a mismatched
                // camera id between the vision engine and the atlas config is the
                // classic cause of `ingest_rate_hz: 0` with no error. Warn once
                // per unexpected id so a real misconfig is one `ados logs` away.
                if self.warned_unmatched.insert(desc.camera_id.clone()) {
                    let enabled: Vec<&str> = self.enabled.iter().map(String::as_str).collect();
                    tracing::warn!(
                        camera_id = %desc.camera_id,
                        enabled = ?enabled,
                        "atlas dropping frames: camera id not in the enabled set (vision/atlas camera id mismatch)"
                    );
                }
                continue;
            }
            if let Some(bytes) = self.read_frame_from_ring(&desc) {
                return Some(CapturedFrame {
                    camera_id: desc.camera_id,
                    ts_ms: desc.ts_ms,
                    width: desc.width,
                    height: desc.height,
                    format: desc.format,
                    bytes,
                });
            }
            // A torn/stale slot: keep reading for the next descriptor.
        }
    }
}

/// A deterministic source for the SITL harness and demo runs: a fixed list of
/// frames replayed in order, then exhausted (`next` returns `None`). No
/// hardware, no sockets, no shared memory.
pub struct SyntheticFrameSource {
    frames: std::collections::VecDeque<CapturedFrame>,
}

impl SyntheticFrameSource {
    /// Build from an explicit frame list (the caller controls camera id, size,
    /// format, bytes, and timestamps).
    pub fn new(frames: Vec<CapturedFrame>) -> Self {
        Self {
            frames: frames.into(),
        }
    }

    /// Build a simple one-camera RGB sequence: `count` solid-grey frames spaced
    /// `interval_ms` apart starting at `start_ts_ms`.
    pub fn solid(
        camera_id: &str,
        width: u32,
        height: u32,
        count: usize,
        start_ts_ms: i64,
        interval_ms: i64,
    ) -> Self {
        let frames = (0..count)
            .map(|i| CapturedFrame {
                camera_id: camera_id.to_string(),
                ts_ms: start_ts_ms + i as i64 * interval_ms,
                width,
                height,
                format: FrameFormat::Rgb24,
                bytes: vec![(i % 256) as u8; (width * height * 3) as usize],
            })
            .collect();
        Self::new(frames)
    }

    async fn next(&mut self) -> Option<CapturedFrame> {
        self.frames.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn synthetic_source_replays_then_exhausts() {
        let mut s =
            AtlasFrameSource::Synthetic(SyntheticFrameSource::solid("front", 8, 8, 3, 0, 100));
        let f0 = s.next().await.unwrap();
        assert_eq!(f0.camera_id, "front");
        assert_eq!(f0.ts_ms, 0);
        assert_eq!(f0.format, FrameFormat::Rgb24);
        assert_eq!(s.next().await.unwrap().ts_ms, 100);
        assert_eq!(s.next().await.unwrap().ts_ms, 200);
        assert!(s.next().await.is_none(), "exhausted");
    }

    #[tokio::test]
    async fn vision_source_returns_none_when_socket_absent() {
        // No vision engine bound: the source cannot connect and yields None
        // (the caller backs off and retries) rather than erroring.
        let mut v = VisionFrameSource::new(
            "/nonexistent/vision-frames.sock".to_string(),
            HashSet::new(),
        );
        assert!(v.next().await.is_none());
    }
}
