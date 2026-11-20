//! Vision on the simulator's clock (`simulation.clock: sim`).
//!
//! Frame clock. A simulated camera's frame carries the simulator time it was
//! rendered at (the SEI `sim_time_ns`, [`crate::sei`]). That time, not the
//! frame's arrival, is the engine's frame clock ([`frame_clock_ns`]): the
//! per-camera model schedule and the tracker run on it, so detection is paced
//! per second of sim time and the tracker steps by the time between exposures
//! whatever speed the simulator runs at.
//!
//! Results. On the sim clock an inference costs sim time only as far as the
//! agent lags the simulator (in lockstep, none at all): a batch published the
//! moment it is ready would show the drone its subject with no delay and
//! flatter every accelerated take. So a batch of a simulated frame is held
//! until ADOS time reaches the frame's sim time plus the modelled latency
//! (`vision.sim_latency_ms`, which the bench writes from the measured p50 of a
//! real-time take of the same profile; unset, the detector's own latency as
//! measured on this host), then published. One already later goes at once.
//!
//! Lockstep (`simulation.clock_mode: lockstep`). Each simulated camera is a
//! participant of the agent's barrier from its second frame, once its frame
//! period is known. Its source hands frame F on some frames late, the
//! source's depth: frame F reaches the engine only once frame F + depth is
//! rendered. The RTSP source (the only one carrying simulated frames) is two
//! deep: an Annex-B NAL ends only at the next start code, so the source
//! writes F to the decoder once F+1's first NAL arrives, and the ffmpeg
//! decoder emits a picture only once the next one has started (measured on
//! ffmpeg 8.0.1 with x264 and NVENC streams: one frame held even when fed a
//! whole access unit and an access unit delimiter, any thread count,
//! `low_delay`). A lane expects its next frame by the time the frame `depth`
//! after it is rendered, and reports done to the later of half a period past
//! that and the next frame's release time (its sim time plus the modelled
//! latency), never past a held batch's release time. While a frame of it is
//! in inference it reports that frame's release time: the simulator runs on
//! (rendering, flying) while the detector works, up to the moment its result
//! is due, and waits there if the result is not ready. So every batch is
//! published at its own release time, and inference shows as wall time only
//! where it outlasts the sim time to its release. This holds for any
//! `grant_ms`: the barrier acknowledges a boundary while every report is past
//! it, so the run holds only at the first boundary at or past the lane's
//! target, after drawing every frame before it, the releasing frame included
//! (the half period is the margin for the step that draws it); a coarser
//! grant only lets the run go up to one grant past the target.
//!
//! The depth is learned per camera: it starts at [`SOURCE_DEPTH_FRAMES`], and
//! a lane held past its expected time with no frame for [`LANE_PROBE_MS`] of
//! wall time (the frame it waits for would have been rendered and on its
//! way) takes one frame more, up to [`MAX_SOURCE_DEPTH_FRAMES`], so a deeper
//! source holds the run for a moment, never for good. A camera still without
//! its frame [`LANE_SILENCE_MS`] after that at the deepest leaves the barrier
//! (under World Engine's default 2 s participant timeout, so a lost frame
//! never fails the run); a source that fails or falls silent leaves it too.
//! It joins again once its frames return, keeping its learned depth.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ados_config::simulation::SimulationGate;
use ados_protocol::framebus::{DetectionBatch, FrameDescriptor, TIMEBASE_SIM};
use ados_time::{Clock, Participant};
use serde::Deserialize;
use tokio::sync::{broadcast, Notify};

mod pacing;
pub use pacing::Pacing;

/// Weight of the newest inference in the measured latency.
const LATENCY_EWMA_ALPHA: f64 = 0.2;

/// The depth a camera's source starts at: the RTSP source's two frames (see
/// the module docs).
pub const SOURCE_DEPTH_FRAMES: u64 = 2;

/// The deepest source a lane learns before it leaves the barrier instead.
pub const MAX_SOURCE_DEPTH_FRAMES: u64 = 4;

/// Wall time, ms, a lane is held past its expected time with no frame before
/// it takes one frame more depth.
pub const LANE_PROBE_MS: u64 = 250;

/// Wall time, ms, a lockstep lane at the deepest waits for a frame that
/// should have come before it leaves the barrier: under World Engine's
/// default participant timeout (2 s).
pub const LANE_SILENCE_MS: u64 = 1_000;

/// How often, wall time, lockstep lanes are checked for a missing frame.
const LANE_CHECK_MS: u64 = 250;

/// What [`Pacing::overdue`] did to a lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overdue {
    /// It takes one frame more depth (now this deep); it reports again.
    Deeper(String, u64),
    /// It left the barrier.
    Gone(String),
}

/// A frame's time on the engine's frame clock: a simulated frame's sim time,
/// ns; `None` for a frame on the host clock.
pub fn frame_clock_ns(desc: &FrameDescriptor) -> Option<u64> {
    desc.ts_sensor_ns.filter(|_| desc.timebase == TIMEBASE_SIM)
}

/// The simulation settings the engine reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SimSetup {
    /// `simulation.*`.
    pub gate: SimulationGate,
    /// `vision.sim_latency_ms`: the modelled inference latency a result of a
    /// simulated frame is released after.
    pub latency_ms: Option<u32>,
}

impl SimSetup {
    /// Parse out of a config body.
    pub fn from_config(text: &str) -> Self {
        #[derive(Deserialize, Default)]
        struct Raw {
            #[serde(default)]
            vision: Option<Vision>,
        }
        #[derive(Deserialize, Default)]
        struct Vision {
            #[serde(default)]
            sim_latency_ms: Option<u32>,
        }
        let raw: Raw = ados_config::yaml_or_default(text, "vision.sim_latency_ms");
        SimSetup {
            gate: ados_config::simulation::read_gate_from(text),
            latency_ms: raw.vision.and_then(|v| v.sim_latency_ms),
        }
    }

    /// `simulation.enabled`: frames carry simulator time, which the engine
    /// reports to the clock.
    pub fn enabled(&self) -> bool {
        self.gate.enabled
    }
}

/// The modelled latency: the configured one, else the measured EWMA.
struct Latency {
    fixed_ns: Option<u64>,
    /// Measured, ms; `None` before the first inference.
    measured_ms: Mutex<Option<f64>>,
}

impl Latency {
    fn ns(&self) -> u64 {
        self.fixed_ns.unwrap_or_else(|| {
            let ms = *self.measured_ms.lock().unwrap_or_else(|e| e.into_inner());
            ms.map_or(0, |ms| (ms * 1e6).round() as u64)
        })
    }

    fn observe(&self, ms: f32) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        let ms = f64::from(ms);
        let mut m = self.measured_ms.lock().unwrap_or_else(|e| e.into_inner());
        *m = Some(m.map_or(ms, |prev| {
            LATENCY_EWMA_ALPHA * ms + (1.0 - LATENCY_EWMA_ALPHA) * prev
        }));
    }
}

/// Vision paced on the simulator's clock: the release of held batches and, in
/// lockstep, each camera's participation in the barrier.
pub struct SimPacing {
    clock: Clock,
    latency: Latency,
    /// The barrier's participants socket, in lockstep.
    lockstep: Option<PathBuf>,
    tx: broadcast::Sender<DetectionBatch>,
    state: Mutex<Pacing>,
    parts: Mutex<HashMap<String, Participant>>,
    wake: Notify,
}

impl SimPacing {
    /// The engine's pacing on a node following the simulator's clock (the
    /// plugin relay under `run_dir`), publishing on `tx`; `None` on wall time.
    /// Needs a Tokio runtime.
    pub fn start(
        setup: &SimSetup,
        run_dir: &Path,
        tx: broadcast::Sender<DetectionBatch>,
    ) -> Option<Arc<SimPacing>> {
        if !setup.gate.sim_clock() {
            return None;
        }
        let clock = ados_time::relay_clock(&setup.gate, run_dir);
        let lockstep = ados_time::lockstep_socket(&setup.gate, run_dir);
        Some(Self::new(clock, setup.latency_ms, lockstep, tx))
    }

    /// Pacing on `clock`, releasing after `latency_ms` (unset: the measured
    /// latency), a barrier participant per camera when `lockstep` names the
    /// participants socket. Spawns the release task.
    pub fn new(
        clock: Clock,
        latency_ms: Option<u32>,
        lockstep: Option<PathBuf>,
        tx: broadcast::Sender<DetectionBatch>,
    ) -> Arc<SimPacing> {
        let p = Arc::new(SimPacing {
            clock,
            latency: Latency {
                fixed_ns: latency_ms.map(|ms| u64::from(ms) * 1_000_000),
                measured_ms: Mutex::new(None),
            },
            lockstep,
            tx,
            state: Mutex::new(Pacing::default()),
            parts: Mutex::new(HashMap::new()),
            wake: Notify::new(),
        });
        tokio::spawn(release_loop(p.clone()));
        if p.lockstep.is_some() {
            tokio::spawn(silence_loop(Arc::downgrade(&p)));
        }
        p
    }

    /// The latency results are released after, ns.
    pub fn latency_ns(&self) -> u64 {
        self.latency.ns()
    }

    /// One detector batch's inference time, ms (the measured latency).
    pub fn observe_infer_ms(&self, ms: f32) {
        self.latency.observe(ms);
    }

    /// Publish `batch` once its release time is reached (now, when it already
    /// is or it is of a host-clock frame). Returns the subscribers it reaches.
    pub fn publish(&self, batch: DetectionBatch) -> usize {
        let Some(at) = Pacing::release_ns(&batch, self.latency_ns()) else {
            return self.tx.send(batch).unwrap_or(0);
        };
        if self.clock.sim_ns().is_some_and(|now| now >= at) {
            return self.tx.send(batch).unwrap_or(0);
        }
        let camera = batch.camera_id.clone();
        self.lock().hold(at, batch);
        self.wake.notify_one();
        self.report(&camera);
        self.tx.receiver_count()
    }

    /// A frame was taken off `camera`'s source. A simulated one keeps the
    /// lane busy until [`Self::free`] with its frame time (one call per
    /// frame), and lets the simulator run on to its result's release time.
    /// Returns the frame time, `None` for a host-clock frame.
    pub fn frame(&self, camera: &str, desc: &FrameDescriptor) -> Option<u64> {
        let ns = frame_clock_ns(desc)?;
        self.lock().frame(camera, ns);
        self.report(camera);
        Some(ns)
    }

    /// The frame at sim time `frame_ns` [`Self::frame`] took is finished: its
    /// batches are published or held.
    pub fn free(&self, camera: &str, frame_ns: u64) {
        self.lock().free(camera, frame_ns);
        self.report(camera);
    }

    /// The camera's source failed or fell silent: it leaves the barrier.
    pub fn lost(&self, camera: &str) {
        let mut s = self.lock();
        s.lost(camera);
        self.parts_lock().remove(camera);
    }

    /// Lanes held past their expected frame take more depth (and report the
    /// later target); ones whose frame never comes leave the barrier.
    fn check_overdue(&self, wall_ms: u64) {
        let Some(now) = self.clock.sim_ns() else {
            return;
        };
        let mut deeper = Vec::new();
        {
            let mut s = self.lock();
            let out = s.overdue(now, wall_ms);
            if out.is_empty() {
                return;
            }
            let mut parts = self.parts_lock();
            for o in out {
                match o {
                    Overdue::Deeper(c, depth) => {
                        tracing::info!(
                            camera = %c,
                            depth_frames = depth,
                            "simulated frame later than its source depth: the camera expects its frames one later"
                        );
                        deeper.push(c);
                    }
                    Overdue::Gone(c) => {
                        parts.remove(&c);
                        tracing::warn!(
                            camera = %c,
                            silence_ms = LANE_SILENCE_MS,
                            "simulated frame overdue: the camera leaves the lockstep barrier until its frames return"
                        );
                    }
                }
            }
        }
        for c in deeper {
            self.report(&c);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pacing> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn parts_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Participant>> {
        self.parts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Report `camera`'s lane to the barrier (lockstep), registering it on
    /// its first report. The pacing state is held throughout (state, then
    /// participants: the order [`Self::lost`] takes them in), so a lane lost
    /// meanwhile is never registered again.
    fn report(&self, camera: &str) {
        let Some(socket) = &self.lockstep else {
            return;
        };
        let latency = self.latency_ns();
        let s = self.lock();
        let Some(done) = s.done_target(camera, latency) else {
            return;
        };
        self.parts_lock()
            .entry(camera.to_owned())
            .or_insert_with(|| Participant::connect(socket.clone()))
            .done(done);
        drop(s);
    }

    /// Publish what is due now; every lane reports after.
    fn release_due(&self) {
        let Some(now) = self.clock.sim_ns() else {
            return;
        };
        let (due, cameras) = {
            let mut s = self.lock();
            (s.due(now), s.cameras())
        };
        for b in due {
            let _ = self.tx.send(b);
        }
        for c in cameras {
            self.report(&c);
        }
    }
}

/// Wait for the earliest held release on the clock (or a new hold), publish
/// what is due, repeat.
async fn release_loop(p: Arc<SimPacing>) {
    loop {
        let next = p.lock().next_release();
        match next {
            None => p.wake.notified().await,
            Some(at) => {
                tokio::select! {
                    _ = p.clock.sleep_until_ms(at.div_ceil(1_000_000)) => {}
                    _ = p.wake.notified() => {}
                }
            }
        }
        p.release_due();
    }
}

/// In lockstep, every [`LANE_CHECK_MS`] of wall time: lanes whose expected
/// frame never came leave the barrier. Ends with the pacing.
async fn silence_loop(p: std::sync::Weak<SimPacing>) {
    let start = std::time::Instant::now();
    // Wall time: the bound is on how long the barrier waits, not on sim time.
    let mut every = tokio::time::interval(std::time::Duration::from_millis(LANE_CHECK_MS));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        every.tick().await;
        let Some(p) = p.upgrade() else {
            return;
        };
        let wall_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        p.check_overdue(wall_ms);
    }
}

#[cfg(test)]
#[path = "sim_pacing_tests.rs"]
mod tests;
