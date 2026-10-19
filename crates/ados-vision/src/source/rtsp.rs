//! An `rtsp://` capture camera: a simulated rig's camera, owned by the engine.
//!
//! One `ffmpeg` pulls the stream and copies its H.264 out as Annex-B; this
//! source splits it into NAL units, reads the World Engine frame identity out
//! of each frame's SEI ([`crate::sei`]) and forwards the bytes to a second
//! `ffmpeg` that decodes them to raw frames. Each decoded frame pairs, in
//! order, with the identity read off its access unit: the encoder emits no
//! B-frames, so decode order is presentation order. Forwarding starts at the
//! first IDR picture, because a decoder drops the pictures before it and the
//! pairing would otherwise slip, and the decoder runs with
//! `-fps_mode passthrough` so it never duplicates or drops a frame to fit a
//! constant rate. When the pairing is lost anyway (the decoder falls more than
//! [`MAX_PENDING`] pictures behind, or emits a frame no picture accounts for)
//! both processes are torn down and the stream restarts at the next IDR,
//! rather than pairing later frames with the wrong identity.
//!
//! A frame with an identity is stamped `ts_sensor_ns = sim_time_ns`,
//! `trigger_seq = seq`, timebase sim, and its arrival is reported to the clock
//! as a host↔sim time pair. When the simulated sensor hub publishes the
//! trigger of that sequence, the frame's `ts_ns` is the trigger's mid-exposure
//! host time; otherwise it is the arrival time.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use super::{BufferPool, BufferReturn, FrameSource, RawFrame};
use crate::ring::FrameTiming;
use crate::sei::{frame_id_in_sei, nal_type, starts_picture, AnnexBScanner, WeFrameId};
use crate::sei::{NAL_IDR, NAL_SPS};
use crate::trigger::CameraTriggers;
use ados_protocol::framebus::{mono_ns, FrameFormat, TIMEBASE_SIM};
use ados_sensorbus::{Kind, Publisher, SyncObs, SyncSource};
use anyhow::{anyhow, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdout, Command};

/// NAL type of a picture parameter set.
const NAL_PPS: u8 = 8;
/// Pictures awaiting decode before the pairing is declared lost.
pub const MAX_PENDING: usize = 64;
/// Read size off the demuxer.
const CHUNK: usize = 64 * 1024;

/// Reports host↔simulation time pairs to the clock on the sensor bus.
#[derive(Clone, Default)]
pub struct SimClock {
    publisher: Option<Publisher>,
}

impl SimClock {
    /// Publish pairs through `publisher` (the node runs against a simulator).
    pub fn publishing(publisher: Publisher) -> Self {
        Self {
            publisher: Some(publisher),
        }
    }

    /// One pair: at host monotonic `host_ns` the frame of `sim_time_ns` arrived.
    pub fn observe(&self, host_ns: u64, sim_time_ns: u64) {
        let Some(p) = &self.publisher else {
            return;
        };
        let Ok(source_ns) = i64::try_from(sim_time_ns) else {
            return;
        };
        p.msgpack(
            Kind::Sync,
            SyncSource::Sim.id(),
            &SyncObs {
                source: SyncSource::Sim,
                host_ns,
                source_ns,
                rtt_ns: 0,
            },
        );
    }
}

/// The pairing of forwarded pictures with decoded frames, in order. Once
/// lost it stays lost: the stream must restart to realign.
#[derive(Debug, Default)]
pub struct Pairing {
    queue: VecDeque<Pending>,
    lost: bool,
}

/// The decoded frames no longer line up with the forwarded pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingLost;

impl Pairing {
    /// A picture went to the decoder.
    pub fn push(&mut self, p: Pending) {
        if self.lost {
            return;
        }
        if self.queue.len() >= MAX_PENDING {
            self.lost = true;
            self.queue.clear();
            return;
        }
        self.queue.push_back(p);
    }

    /// The picture a decoded frame came from.
    pub fn pop(&mut self) -> Result<Pending, PairingLost> {
        if self.lost {
            return Err(PairingLost);
        }
        self.queue.pop_front().ok_or_else(|| {
            self.lost = true;
            PairingLost
        })
    }
}

/// Why an RTSP camera cannot be opened in `format`: the decoder converts to
/// RGB, YUV or grey; a Bayer mosaic is only ever a decoder input.
pub fn check_format(format: FrameFormat) -> Result<()> {
    if format.is_bayer() {
        return Err(anyhow!(
            "an rtsp:// camera cannot deliver {format:?}: the stream decodes to \
             rgb24, nv12, yuv420p, gray8 or gray16 only"
        ));
    }
    Ok(())
}

/// One access unit forwarded to the decoder, awaiting its decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    pub id: Option<WeFrameId>,
    pub arrival_ns: u64,
}

/// The demux-side state: which NALs reach the decoder and which pictures
/// await a decoded frame.
#[derive(Debug, Default)]
pub struct AccessUnits {
    started: bool,
    next_id: Option<WeFrameId>,
    scratch: Vec<u8>,
}

impl AccessUnits {
    /// Account for one NAL; returns whether it goes to the decoder and the
    /// picture it starts, if any.
    pub fn on_nal(&mut self, nal: &[u8], arrival_ns: u64) -> (bool, Option<Pending>) {
        let kind = nal_type(nal);
        if let Some(id) = frame_id_in_sei(nal, &mut self.scratch) {
            self.next_id = Some(id);
        }
        if kind == NAL_IDR {
            self.started = true;
        }
        let forward = self.started || kind == NAL_SPS || kind == NAL_PPS;
        let picture = (self.started && starts_picture(nal)).then(|| Pending {
            id: self.next_id.take(),
            arrival_ns,
        });
        (forward, picture)
    }
}

/// The timing of a decoded frame from its access unit.
pub fn frame_timing(
    pending: Option<Pending>,
    now_ns: u64,
    triggers: Option<&mut CameraTriggers>,
) -> FrameTiming {
    let Some(p) = pending else {
        return FrameTiming::host(now_ns);
    };
    let Some(id) = p.id else {
        return FrameTiming::host(p.arrival_ns);
    };
    let mut t = FrameTiming {
        ts_ns: p.arrival_ns,
        ts_sensor_ns: Some(id.sim_time_ns),
        exposure_ns: None,
        trigger_seq: Some(id.seq),
        timebase: TIMEBASE_SIM,
    };
    if let Some(rec) = triggers.and_then(|tr| tr.join(id.seq, p.arrival_ns)) {
        t.ts_ns = rec.t_mid_ns;
        if rec.exposure_ns > 0 {
            t.exposure_ns = Some(rec.exposure_ns);
        }
    }
    t
}

/// What an RTSP capture is opened with.
#[derive(Debug, Clone, PartialEq)]
pub struct RtspSettings {
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
}

struct Running {
    _demux: Child,
    _decode: Child,
    frames: ChildStdout,
    pending: Arc<Mutex<Pairing>>,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

/// The RTSP capture source for one camera.
pub struct RtspSource {
    camera_id: String,
    settings: RtspSettings,
    triggers: Option<CameraTriggers>,
    sim: SimClock,
    pool: BufferPool,
    running: Option<Running>,
}

impl RtspSource {
    pub fn new(
        camera_id: impl Into<String>,
        settings: RtspSettings,
        triggers: Option<CameraTriggers>,
        sim: SimClock,
    ) -> Self {
        Self {
            camera_id: camera_id.into(),
            settings,
            triggers,
            sim,
            pool: BufferPool::new(),
            running: None,
        }
    }

    fn spawn(&self) -> Result<Running> {
        let s = &self.settings;
        check_format(s.format)?;
        let mut demux = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-rtsp_transport",
                "tcp",
            ])
            .args(["-fflags", "nobuffer", "-flags", "low_delay", "-i", &s.url])
            .args([
                "-map",
                "0:v:0",
                "-c:v",
                "copy",
                "-bsf:v",
                "h264_mp4toannexb",
            ])
            .args(["-f", "h264", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let scale = format!("scale={}:{}", s.width, s.height);
        let mut decode = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-fflags", "nobuffer"])
            .args([
                "-flags",
                "low_delay",
                "-f",
                "h264",
                "-i",
                "-",
                "-an",
                "-fps_mode",
                "passthrough",
                "-vf",
                &scale,
            ])
            .args(["-pix_fmt", s.format.ffmpeg_pix_fmt(), "-f", "rawvideo", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut annexb = demux
            .stdout
            .take()
            .ok_or_else(|| anyhow!("demuxer has no stdout"))?;
        let mut sink = decode
            .stdin
            .take()
            .ok_or_else(|| anyhow!("decoder has no stdin"))?;
        let frames = decode
            .stdout
            .take()
            .ok_or_else(|| anyhow!("decoder has no stdout"))?;
        let pending = Arc::new(Mutex::new(Pairing::default()));
        let queue = pending.clone();
        let pump = tokio::spawn(async move {
            let mut scanner = AnnexBScanner::default();
            let mut units = AccessUnits::default();
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = match annexb.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                scanner.push(&buf[..n]);
                let now = mono_ns();
                while let Some(nal) = scanner.next_nal() {
                    let (forward, picture) = units.on_nal(&nal, now);
                    if let Some(p) = picture {
                        queue.lock().expect("pending lock").push(p);
                    }
                    if forward
                        && (sink.write_all(&[0, 0, 0, 1]).await.is_err()
                            || sink.write_all(&nal).await.is_err())
                    {
                        return;
                    }
                }
            }
        });
        Ok(Running {
            _demux: demux,
            _decode: decode,
            frames,
            pending,
            pump,
        })
    }
}

impl FrameSource for RtspSource {
    async fn next_frame(&mut self) -> Result<RawFrame> {
        if self.running.is_none() {
            self.running = Some(self.spawn()?);
        }
        let running = self.running.as_mut().expect("spawned above");
        let bytes = self
            .settings
            .format
            .frame_bytes(self.settings.width, self.settings.height);
        let mut data = self.pool.take(bytes);
        if let Err(e) = running.frames.read_exact(&mut data).await {
            self.running = None;
            return Err(e.into());
        }
        let now = mono_ns();
        let popped = running.pending.lock().expect("pending lock").pop();
        let Ok(pending) = popped else {
            // Restart both processes; the next frames realign at an IDR.
            self.running = None;
            self.pool.recycler().give(data);
            tracing::warn!(camera = %self.camera_id, "rtsp_frame_pairing_lost; restarting the stream");
            return Err(anyhow!("frame pairing lost; restarting the stream"));
        };
        let timing = frame_timing(Some(pending), now, self.triggers.as_mut());
        if let Some(sim) = timing.ts_sensor_ns {
            self.sim.observe(pending.arrival_ns, sim);
        }
        Ok(RawFrame {
            width: self.settings.width,
            height: self.settings.height,
            format: self.settings.format,
            data,
            timing,
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
#[path = "rtsp_tests.rs"]
mod tests;
