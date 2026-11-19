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
//! period is known. It reports done to half a period past the frame it expects
//! next, never past a held batch's release time, and nothing more while a
//! frame of it is in inference: the simulator steps no further until the next
//! frame was decoded and inferred, and every batch is published at its own
//! sim time. The half period lets the simulator through the grant boundary the
//! next frame is rendered at (the barrier holds a boundary before the renderer
//! draws it). A camera whose source fails or falls silent leaves the barrier
//! until its frames return, so a lost stream never holds the run.

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

    /// What `camera` reports done to the barrier: half a period past the frame
    /// it expects next, never past one of its held batches. `None` while a
    /// frame of it is busy or before its period is known.
    pub fn done_target(&self, camera: &str) -> Option<u64> {
        let lane = self.lanes.get(camera)?;
        if lane.busy > 0 {
            return None;
        }
        let period = lane.period_ns?;
        let next = lane.last_ns? + period + period / 2;
        let held = self
            .held
            .iter()
            .find(|(_, b)| b.camera_id == camera)
            .map(|((at, _), _)| *at);
        Some(held.map_or(next, |at| at.min(next)))
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
        self.lock().lost(camera);
        self.parts_lock().remove(camera);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pacing> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn parts_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Participant>> {
        self.parts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Report `camera`'s lane to the barrier (lockstep), registering it on
    /// its first report.
    fn report(&self, camera: &str) {
        let Some(socket) = &self.lockstep else {
            return;
        };
        let Some(done) = self.lock().done_target(camera) else {
            return;
        };
        self.parts_lock()
            .entry(camera.to_owned())
            .or_insert_with(|| Participant::connect(socket.clone()))
            .done(done);
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

#[cfg(test)]
#[path = "sim_pacing_tests.rs"]
mod tests;
