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
//! period is known. Its source hands frame F on one frame late: an Annex-B
//! NAL ends only at the next start code, so the RTSP source releases F once
//! F+1's first NAL arrives (the only source that carries simulated frames).
//! A lane therefore expects its next frame by the time the frame after it is
//! rendered (two periods past its last frame), and reports done to half a
//! period past that, never past a held batch's release time, and nothing more
//! while a frame of it is in inference: the simulator steps no further until
//! the next frame was decoded and inferred, and every batch is published at
//! its own sim time. This holds for any `grant_ms`: the barrier acknowledges
//! a boundary while every report is past it, so the run holds only at the
//! first boundary at or past the lane's target, after drawing every frame
//! before it, the releasing frame included (the half period is the margin
//! for the step that draws it); a coarser grant only lets the run go up to
//! one grant past the target. A camera whose expected frame has not come
//! [`LANE_SILENCE_MS`] of wall time after the simulator passed the time it
//! should have leaves the barrier (under World Engine's default 2 s
//! participant timeout, so one lost frame never fails the run); a source that
//! fails or falls silent leaves it too. It joins again once its frames return.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ados_config::simulation::SimulationGate;
use ados_protocol::framebus::{DetectionBatch, FrameDescriptor, TIMEBASE_SIM};
use ados_time::{Clock, Participant};
use serde::Deserialize;
use tokio::sync::{broadcast, Notify};

/// Weight of the newest inference in the measured latency.
const LATENCY_EWMA_ALPHA: f64 = 0.2;

/// Frames a simulated camera's source holds back (see the module docs).
const SOURCE_DELAY_FRAMES: u64 = 1;

/// Wall time, ms, a lockstep lane waits for a frame that should have come
/// before it leaves the barrier: under World Engine's default participant
/// timeout (2 s).
pub const LANE_SILENCE_MS: u64 = 1_000;

/// How often, wall time, lockstep lanes are checked for a missing frame.
const LANE_CHECK_MS: u64 = 250;

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

/// One simulated camera's lane.
#[derive(Debug, Default, Clone, Copy)]
struct Lane {
    last_ns: Option<u64>,
    period_ns: Option<u64>,
    /// Frames taken and not yet finished (in inference, or about to be).
    busy: u32,
    /// Wall time, ms, since which the simulator is past the time this lane's
    /// next frame should have arrived by.
    overdue_since_ms: Option<u64>,
}

/// The pacing state: held batches and each camera's lane. Pure; the
/// [`SimPacing`] shell drives it from the clock.
#[derive(Debug, Default)]
pub struct Pacing {
    held: BTreeMap<(u64, u64), DetectionBatch>,
    seq: u64,
    lanes: HashMap<String, Lane>,
}

impl Pacing {
    /// When `batch` may be published, ns of sim time: its frame's sim time
    /// plus `latency_ns`. `None` for a batch of a host-clock frame.
    pub fn release_ns(batch: &DetectionBatch, latency_ns: u64) -> Option<u64> {
        let ms = u64::try_from(batch.ts_sensor_ms?).ok()?;
        Some(ms.saturating_mul(1_000_000).saturating_add(latency_ns))
    }

    /// Hold `batch` until sim time `at_ns`.
    pub fn hold(&mut self, at_ns: u64, batch: DetectionBatch) {
        self.seq += 1;
        self.held.insert((at_ns, self.seq), batch);
    }

    /// The held batches due at sim time `now_ns`, in release order.
    pub fn due(&mut self, now_ns: u64) -> Vec<DetectionBatch> {
        let later = self.held.split_off(&(now_ns.saturating_add(1), 0));
        std::mem::replace(&mut self.held, later)
            .into_values()
            .collect()
    }

    /// The earliest held release, ns.
    pub fn next_release(&self) -> Option<u64> {
        self.held.keys().next().map(|(at, _)| *at)
    }

    /// A frame of `camera` at sim time `sim_ns` was taken: the lane is busy
    /// with it until [`Self::free`].
    pub fn frame(&mut self, camera: &str, sim_ns: u64) {
        let lane = self.lanes.entry(camera.to_owned()).or_default();
        if let Some(last) = lane.last_ns.filter(|l| sim_ns > *l) {
            lane.period_ns = Some(sim_ns - last);
        }
        lane.last_ns = Some(lane.last_ns.map_or(sim_ns, |l| l.max(sim_ns)));
        lane.overdue_since_ms = None;
        lane.busy += 1;
    }

    /// One frame [`Self::frame`] took is finished.
    pub fn free(&mut self, camera: &str) {
        if let Some(lane) = self.lanes.get_mut(camera) {
            lane.busy = lane.busy.saturating_sub(1);
        }
    }

    /// The camera's source failed or fell silent: its lane starts again.
    pub fn lost(&mut self, camera: &str) {
        self.lanes.remove(camera);
    }

    /// The sim time by which `lane`'s next frame reaches the engine: the time
    /// the frame after it is rendered (the source's one-frame delay).
    fn expected_ns(lane: &Lane) -> Option<u64> {
        let period = lane.period_ns?;
        Some(lane.last_ns? + period * (1 + SOURCE_DELAY_FRAMES))
    }

    /// What `camera` reports done to the barrier: half a period past the time
    /// its next frame reaches the engine, never past one of its held batches.
    /// `None` while a frame of it is busy or before its period is known.
    pub fn done_target(&self, camera: &str) -> Option<u64> {
        let lane = self.lanes.get(camera)?;
        if lane.busy > 0 {
            return None;
        }
        let next = Self::expected_ns(lane)? + lane.period_ns? / 2;
        let held = self
            .held
            .iter()
            .find(|(_, b)| b.camera_id == camera)
            .map(|((at, _), _)| *at);
        Some(held.map_or(next, |at| at.min(next)))
    }

    /// The lanes whose expected frame has not come [`LANE_SILENCE_MS`] after
    /// the simulator passed its expected time, at sim time `sim_now_ns` and
    /// wall time `wall_ms`; they are removed (they leave the barrier).
    pub fn overdue(&mut self, sim_now_ns: u64, wall_ms: u64) -> Vec<String> {
        let mut gone = Vec::new();
        for (camera, lane) in &mut self.lanes {
            let late = lane.busy == 0 && Self::expected_ns(lane).is_some_and(|e| sim_now_ns > e);
            if !late {
                lane.overdue_since_ms = None;
                continue;
            }
            let since = *lane.overdue_since_ms.get_or_insert(wall_ms);
            if wall_ms.saturating_sub(since) >= LANE_SILENCE_MS {
                gone.push(camera.clone());
            }
        }
        for c in &gone {
            self.lanes.remove(c);
        }
        gone
    }

    fn cameras(&self) -> Vec<String> {
        self.lanes.keys().cloned().collect()
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
    /// lane busy until [`Self::free`] (one call per frame).
    pub fn frame(&self, camera: &str, desc: &FrameDescriptor) -> bool {
        let Some(ns) = frame_clock_ns(desc) else {
            return false;
        };
        self.lock().frame(camera, ns);
        true
    }

    /// The frame [`Self::frame`] took is finished: its batches are published
    /// or held.
    pub fn free(&self, camera: &str) {
        self.lock().free(camera);
        self.report(camera);
    }

    /// The camera's source failed or fell silent: it leaves the barrier.
    pub fn lost(&self, camera: &str) {
        let mut s = self.lock();
        s.lost(camera);
        self.parts_lock().remove(camera);
    }

    /// Lanes whose expected frame never came leave the barrier.
    fn drop_overdue(&self, wall_ms: u64) {
        let Some(now) = self.clock.sim_ns() else {
            return;
        };
        let mut s = self.lock();
        let gone = s.overdue(now, wall_ms);
        if gone.is_empty() {
            return;
        }
        let mut parts = self.parts_lock();
        for c in gone {
            parts.remove(&c);
            tracing::warn!(
                camera = %c,
                silence_ms = LANE_SILENCE_MS,
                "simulated frame overdue: the camera leaves the lockstep barrier until its frames return"
            );
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
        let s = self.lock();
        let Some(done) = s.done_target(camera) else {
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
        p.drop_overdue(wall_ms);
    }
}

#[cfg(test)]
#[path = "sim_pacing_tests.rs"]
mod tests;
