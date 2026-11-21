//! The per-camera state behind the `vision.depth.v2` frames: each camera's
//! held fit and publish pace, and the broadcast the frames go out on.

use std::collections::HashMap;

use ados_protocol::vision_depth_frame::{DepthFrame, MIN_INTERVAL_MS};
use tokio::sync::broadcast;

use super::{frame, metric};
use crate::backend::DenseMap;
use crate::depth::feed::Ranging;
use crate::depth::fit::Normaliser;
use crate::depth::geometry::CameraGeometry;
use crate::depth::refs;

/// Frames a slow subscriber may fall behind by before it lags.
const FRAME_QUEUE: usize = 16;

/// Model values sampled for a frame's normalisation, at most.
const NORM_SAMPLES: usize = 4_096;

#[derive(Debug, Default)]
struct CameraFrames {
    hold: metric::FitHold,
    last_ms: Option<u64>,
}

/// The metric frames: their per-camera state and their broadcast.
#[derive(Debug)]
pub struct MetricFrames {
    geometries: HashMap<String, CameraGeometry>,
    cameras: HashMap<String, CameraFrames>,
    values: Vec<f32>,
    scratch: Vec<f32>,
    tx: broadcast::Sender<DepthFrame>,
}

impl Default for MetricFrames {
    fn default() -> Self {
        Self {
            geometries: HashMap::new(),
            cameras: HashMap::new(),
            values: Vec::new(),
            scratch: Vec::new(),
            tx: broadcast::channel(FRAME_QUEUE).0,
        }
    }
}

impl MetricFrames {
    /// Set the cameras' geometry (an absent camera uses the default),
    /// keeping the broadcast and its subscribers.
    pub fn configure(&mut self, geometries: HashMap<String, CameraGeometry>) {
        self.geometries = geometries;
        self.cameras.clear();
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DepthFrame> {
        self.tx.subscribe()
    }

    /// The frame for `map` from `camera_id` (captured at `stamp_ns`, frames
    /// `aspect` wide) at `now_ms`, or `None` when the camera's last frame is
    /// under [`MIN_INTERVAL_MS`] old, the map has no spread, or there is no
    /// fit (neither on this frame nor held). A frame made is also broadcast.
    pub fn next(
        &mut self,
        camera_id: &str,
        map: &DenseMap,
        aspect: f32,
        stamp_ns: u64,
        now_ms: u64,
        ranging: &Ranging,
    ) -> Option<DepthFrame> {
        let due = self
            .cameras
            .get(camera_id)
            .and_then(|c| c.last_ms)
            .is_none_or(|t| now_ms.saturating_sub(t) >= MIN_INTERVAL_MS);
        if !due {
            return None;
        }
        let geometry = self.geometries.get(camera_id).copied().unwrap_or_default();
        let step = map.data.len().div_ceil(NORM_SAMPLES).max(1);
        self.values.clear();
        self.values.extend(map.data.iter().step_by(step));
        let norm = Normaliser::of(&self.values, &mut self.scratch)?;
        let hits = refs::collect(map, &geometry.projector(aspect), ranging);
        let pairs: Vec<(f32, f32)> = hits
            .pairs
            .iter()
            .map(|(x, z)| (norm.apply(*x), *z))
            .collect();
        let state = self.cameras.entry(camera_id.to_string()).or_default();
        let (fit, held_ms) = state.hold.update(metric::fit(&pairs), now_ms)?;
        state.last_ms = Some(now_ms);
        let out = frame::build(
            camera_id, stamp_ns, map, &norm, &fit, held_ms, &geometry, aspect,
        );
        let _ = self.tx.send(out.clone());
        Some(out)
    }
}
