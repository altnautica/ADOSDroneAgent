//! Local V4L2 capture: the single owner of a captured camera's device.
//!
//! The device is streamed with MMAP buffers on a dedicated thread (a dequeue
//! blocks), each buffer copied once into a pooled frame buffer. Timing comes
//! from the buffer, not from the moment the frame reached user space: a
//! driver that stamps `V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC` uses the host
//! monotonic clock every agent service shares, and its start-of-exposure or
//! end-of-frame flag places the stamp relative to the exposure. A triggered
//! camera then takes the exposure midpoint and sequence of the trigger record
//! the frame joined to.

use std::time::Duration;

use super::{BufferPool, BufferReturn, FrameSource, RawFrame};
use crate::ring::FrameTiming;
use crate::trigger::CameraTriggers;
use ados_protocol::framebus::{FrameFormat, TIMEBASE_HOST, TIMEBASE_SENSOR};
use anyhow::{anyhow, Result};

/// `V4L2_BUF_FLAG_TIMESTAMP_MASK` and its monotonic value.
pub const TIMESTAMP_MASK: u32 = 0x0000_e000;
pub const TIMESTAMP_MONOTONIC: u32 = 0x0000_2000;
/// `V4L2_BUF_FLAG_TSTAMP_SRC_MASK` and its start-of-exposure value (end of
/// frame is 0).
pub const TSTAMP_SRC_MASK: u32 = 0x0007_0000;
pub const TSTAMP_SRC_SOE: u32 = 0x0001_0000;
/// `V4L2_BUF_FLAG_ERROR`: the driver filled the buffer but its data is corrupt.
pub const BUF_FLAG_ERROR: u32 = 0x0000_0040;

/// `V4L2_CID_EXPOSURE_AUTO`, its manual mode, and `V4L2_CID_EXPOSURE_ABSOLUTE`
/// (units of 100 µs).
pub const CID_EXPOSURE_AUTO: u32 = 0x009a_0901;
pub const EXPOSURE_MANUAL: i64 = 1;
pub const CID_EXPOSURE_ABSOLUTE: u32 = 0x009a_0902;
/// `V4L2_CID_ANALOGUE_GAIN`, then the older `V4L2_CID_GAIN`.
pub const CID_ANALOGUE_GAIN: u32 = 0x009e_0903;
pub const CID_GAIN: u32 = 0x0098_0913;

/// Frames between exposure-control reads: the value only changes when auto
/// exposure moves it, and a control read is a syscall per frame otherwise.
const EXPOSURE_POLL_FRAMES: u64 = 30;
/// Buffers queued to the driver.
pub(crate) const DRIVER_BUFFERS: u32 = 4;
/// How long a dequeue may block before the thread re-checks its consumer.
pub(crate) const DEQUEUE_TIMEOUT: Duration = Duration::from_secs(2);
/// Frames waiting between the capture thread and the engine.
const HANDOFF: usize = 2;

/// Exposure in ns from a `V4L2_CID_EXPOSURE_ABSOLUTE` value (100 µs units).
pub fn exposure_ns_from_absolute(value: i64) -> Option<u32> {
    u32::try_from(value.checked_mul(100_000)?)
        .ok()
        .filter(|ns| *ns > 0)
}

/// The descriptor timing of a dequeued buffer.
///
/// A monotonic buffer stamp is on the host clock: the frame keeps it as
/// `ts_sensor_ns` and its `ts_ns` moves to the exposure midpoint when the
/// exposure is known (half an exposure after a start-of-exposure stamp, half
/// before an end-of-frame one). Any other stamp is on the driver's own clock,
/// so `ts_ns` falls back to the host arrival time.
pub fn buffer_timing(
    flags: u32,
    sec: i64,
    usec: i64,
    exposure_ns: Option<u32>,
    arrival_ns: u64,
) -> FrameTiming {
    let raw = u64::try_from(sec)
        .ok()
        .zip(u64::try_from(usec).ok())
        .map(|(s, us)| s.saturating_mul(1_000_000_000).saturating_add(us * 1_000));
    let half = u64::from(exposure_ns.unwrap_or(0) / 2);
    match raw {
        Some(ts) if flags & TIMESTAMP_MASK == TIMESTAMP_MONOTONIC => FrameTiming {
            ts_ns: if flags & TSTAMP_SRC_MASK == TSTAMP_SRC_SOE {
                ts.saturating_add(half)
            } else {
                ts.saturating_sub(half)
            },
            ts_sensor_ns: Some(ts),
            exposure_ns,
            trigger_seq: None,
            timebase: TIMEBASE_HOST,
        },
        Some(ts) => FrameTiming {
            ts_ns: arrival_ns,
            ts_sensor_ns: Some(ts),
            exposure_ns,
            trigger_seq: None,
            timebase: TIMEBASE_SENSOR,
        },
        None => FrameTiming {
            exposure_ns,
            ..FrameTiming::host(arrival_ns)
        },
    }
}

/// The V4L2 pixel format of a frame-bus format.
pub fn fourcc(format: FrameFormat) -> [u8; 4] {
    match format {
        FrameFormat::Gray8 => *b"GREY",
        FrameFormat::Gray16 => *b"Y16 ",
        FrameFormat::Rgb24 => *b"RGB3",
        FrameFormat::Nv12 => *b"NV12",
        FrameFormat::Yuv420p => *b"YU12",
        FrameFormat::BayerRggb8 => *b"RGGB",
        FrameFormat::BayerRggb16 => *b"RG16",
    }
}

/// Copy a frame out of a driver buffer whose rows may be padded to `stride`
/// bytes, into `out` packed at `row_bytes` per row. Planar formats (NV12,
/// YUV 4:2:0) pad every plane row the same way, so the copy walks every row of
/// every plane. `false` when the buffer is too short for the frame.
pub fn copy_rows(src: &[u8], stride: usize, row_bytes: usize, out: &mut [u8]) -> bool {
    if stride == row_bytes || stride == 0 {
        let Some(frame) = src.get(..out.len()) else {
            return false;
        };
        out.copy_from_slice(frame);
        return true;
    }
    if row_bytes == 0 || stride < row_bytes {
        return false;
    }
    for (i, row) in out.chunks_mut(row_bytes).enumerate() {
        let Some(line) = src.get(i * stride..i * stride + row.len()) else {
            return false;
        };
        row.copy_from_slice(line);
    }
    true
}

/// What a V4L2 capture is opened with.
#[derive(Debug, Clone, PartialEq)]
pub struct V4l2Settings {
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub format: FrameFormat,
    pub exposure_us: Option<u32>,
    pub gain: Option<f32>,
}

/// One dequeued buffer, already copied, handed from the capture thread.
#[derive(Debug)]
pub(crate) struct Captured {
    pub data: Vec<u8>,
    pub sequence: u32,
    pub flags: u32,
    pub sec: i64,
    pub usec: i64,
    pub exposure_ns: Option<u32>,
    pub arrival_ns: u64,
    /// Corrupt buffers dropped since the previous handed-off frame.
    pub dropped_corrupt: u32,
}

/// The V4L2 capture source for one camera.
pub struct V4l2Source {
    camera_id: String,
    settings: V4l2Settings,
    triggers: Option<CameraTriggers>,
    pool: Option<BufferPool>,
    recycle: BufferReturn,
    rx: Option<tokio::sync::mpsc::Receiver<std::io::Result<Captured>>>,
}

impl V4l2Source {
    pub fn new(
        camera_id: impl Into<String>,
        settings: V4l2Settings,
        triggers: Option<CameraTriggers>,
    ) -> Self {
        let pool = BufferPool::new();
        let recycle = pool.recycler();
        Self {
            camera_id: camera_id.into(),
            settings,
            triggers,
            pool: Some(pool),
            recycle,
            rx: None,
        }
    }

    /// Start the capture thread on first use.
    fn ensure_started(&mut self) -> Result<()> {
        if self.rx.is_some() {
            return Ok(());
        }
        let pool = self
            .pool
            .take()
            .ok_or_else(|| anyhow!("capture ended; the source is re-opened"))?;
        let (tx, rx) = tokio::sync::mpsc::channel(HANDOFF);
        let settings = self.settings.clone();
        let camera = self.camera_id.clone();
        std::thread::Builder::new()
            .name(format!("v4l2-{camera}"))
            .spawn(move || {
                if let Err(e) = capture::run(&settings, pool, &tx) {
                    let _ = tx.blocking_send(Err(e));
                }
            })?;
        self.rx = Some(rx);
        Ok(())
    }

    /// Join the frame to its trigger record, when the camera is triggered.
    fn apply_trigger(&mut self, timing: &mut FrameTiming, sequence: u32) {
        let Some(triggers) = self.triggers.as_mut() else {
            return;
        };
        if let Some(rec) = triggers.join(u64::from(sequence), timing.ts_ns) {
            timing.ts_ns = rec.t_mid_ns;
            timing.trigger_seq = Some(rec.seq);
            if rec.exposure_ns > 0 {
                timing.exposure_ns = Some(rec.exposure_ns);
            }
        }
    }
}

impl FrameSource for V4l2Source {
    async fn next_frame(&mut self) -> Result<RawFrame> {
        self.ensure_started()?;
        let rx = self.rx.as_mut().expect("started above");
        let captured = rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("capture thread ended"))??;
        let mut timing = buffer_timing(
            captured.flags,
            captured.sec,
            captured.usec,
            captured.exposure_ns,
            captured.arrival_ns,
        );
        self.apply_trigger(&mut timing, captured.sequence);
        Ok(RawFrame {
            width: self.settings.width,
            height: self.settings.height,
            format: self.settings.format,
            data: captured.data,
            timing,
            dropped_corrupt: captured.dropped_corrupt,
        })
    }

    fn camera_id(&self) -> &str {
        &self.camera_id
    }
    fn recycler(&self) -> BufferReturn {
        self.recycle.clone()
    }
}

/// Exposure in ns read back from the device, polled every
/// [`EXPOSURE_POLL_FRAMES`] frames.
pub(crate) fn poll_due(frame: u64) -> bool {
    frame.is_multiple_of(EXPOSURE_POLL_FRAMES)
}

#[cfg(target_os = "linux")]
#[path = "v4l2_capture.rs"]
mod capture;

#[cfg(not(target_os = "linux"))]
mod capture {
    use super::{BufferPool, Captured, V4l2Settings};

    pub(super) fn run(
        _settings: &V4l2Settings,
        _pool: BufferPool,
        _tx: &tokio::sync::mpsc::Sender<std::io::Result<Captured>>,
    ) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "V4L2 capture needs Linux",
        ))
    }
}
#[cfg(target_os = "linux")]
#[path = "v4l2_sys.rs"]
mod v4l2_sys;

#[cfg(test)]
#[path = "v4l2_tests.rs"]
mod tests;
