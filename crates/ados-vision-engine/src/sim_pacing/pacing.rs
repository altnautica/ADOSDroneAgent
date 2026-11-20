//! The pure pacing state behind [`super::SimPacing`]: held batches and each
//! simulated camera's lane in the lockstep barrier.

use std::collections::{BTreeMap, HashMap};

use ados_protocol::framebus::DetectionBatch;

use super::{
    Overdue, LANE_PROBE_MS, LANE_SILENCE_MS, MAX_SOURCE_DEPTH_FRAMES, SOURCE_DEPTH_FRAMES,
};

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
/// [`super::SimPacing`] shell drives it from the clock.
#[derive(Debug, Default)]
pub struct Pacing {
    held: BTreeMap<(u64, u64), DetectionBatch>,
    seq: u64,
    lanes: HashMap<String, Lane>,
    /// Each camera's learned source depth, kept while its lane restarts.
    depths: HashMap<String, u64>,
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

    /// `camera`'s source depth, frames.
    pub fn depth(&self, camera: &str) -> u64 {
        self.depths
            .get(camera)
            .copied()
            .unwrap_or(SOURCE_DEPTH_FRAMES)
    }

    /// The sim time by which `lane`'s next frame reaches the engine: the time
    /// the frame `depth` after it is rendered.
    fn expected_ns(lane: &Lane, depth: u64) -> Option<u64> {
        let period = lane.period_ns?;
        Some(lane.last_ns? + period * (1 + depth))
    }

    /// What `camera` reports done to the barrier: half a period past the time
    /// its next frame reaches the engine, never past one of its held batches.
    /// `None` while a frame of it is busy or before its period is known.
    pub fn done_target(&self, camera: &str) -> Option<u64> {
        let lane = self.lanes.get(camera)?;
        if lane.busy > 0 {
            return None;
        }
        let next = Self::expected_ns(lane, self.depth(camera))? + lane.period_ns? / 2;
        let held = self
            .held
            .iter()
            .find(|(_, b)| b.camera_id == camera)
            .map(|((at, _), _)| *at);
        Some(held.map_or(next, |at| at.min(next)))
    }

    /// At sim time `sim_now_ns` and wall time `wall_ms`: each lane past its
    /// expected time with no frame for [`LANE_PROBE_MS`] takes one frame more
    /// depth; one at [`MAX_SOURCE_DEPTH_FRAMES`] that stays so for
    /// [`LANE_SILENCE_MS`] is removed (it leaves the barrier).
    pub fn overdue(&mut self, sim_now_ns: u64, wall_ms: u64) -> Vec<Overdue> {
        let mut out = Vec::new();
        for (camera, lane) in &mut self.lanes {
            let depth = self
                .depths
                .get(camera)
                .copied()
                .unwrap_or(SOURCE_DEPTH_FRAMES);
            let late =
                lane.busy == 0 && Self::expected_ns(lane, depth).is_some_and(|e| sim_now_ns > e);
            if !late {
                lane.overdue_since_ms = None;
                continue;
            }
            let waited = wall_ms.saturating_sub(*lane.overdue_since_ms.get_or_insert(wall_ms));
            if depth < MAX_SOURCE_DEPTH_FRAMES {
                if waited >= LANE_PROBE_MS {
                    self.depths.insert(camera.clone(), depth + 1);
                    lane.overdue_since_ms = None;
                    out.push(Overdue::Deeper(camera.clone(), depth + 1));
                }
            } else if waited >= LANE_SILENCE_MS {
                out.push(Overdue::Gone(camera.clone()));
            }
        }
        for o in &out {
            if let Overdue::Gone(c) = o {
                self.lanes.remove(c);
            }
        }
        out
    }

    pub(super) fn cameras(&self) -> Vec<String> {
        self.lanes.keys().cloned().collect()
    }
}
