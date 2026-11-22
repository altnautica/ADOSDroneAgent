//! Movers from a camera's multi-object tracker (`vision.detection` v4).

use serde_json::Value;

use ados_protocol::pilot::MoverKind;

use super::Movers;

/// The one-sigma error of a camera track's world position, m.
const VISION_SIGMA_M: f32 = 0.3;

/// A short stable hash of a camera id.
fn camera_key(s: &str) -> u64 {
    s.bytes().fold(0xCBF2_9CE4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3)
    })
}

fn kind_of(label: &str) -> MoverKind {
    match label.to_ascii_lowercase().as_str() {
        "person" | "pedestrian" | "people" => MoverKind::Person,
        "car" | "truck" | "bus" | "motorcycle" | "motorbike" | "bicycle" | "vehicle" | "van"
        | "train" | "boat" => MoverKind::Vehicle,
        _ => MoverKind::Unknown,
    }
}

/// A detection's world position (local ENU) as local NED north, east.
fn world_ne(d: &Value) -> Option<[f32; 2]> {
    let w = d.get("world_pos").and_then(Value::as_array)?;
    let xyz: Vec<f32> = w
        .iter()
        .filter_map(|x| x.as_f64())
        .map(|x| x as f32)
        .collect();
    (xyz.len() == 3 && xyz.iter().all(|x| x.is_finite())).then(|| [xyz[1], xyz[0]])
}

impl Movers {
    /// Take one `vision.detection` batch (its JSON form): every confirmed
    /// track with a world position (local ENU, the FC's local origin) is a
    /// mover of its class. A lost or tentative track measures nothing; a
    /// track already held coasts on its prediction.
    pub fn ingest_detections(&mut self, payload: &Value, now_ms: u64) {
        let cam = camera_key(
            payload
                .get("camera_id")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        let Some(dets) = payload.get("detections").and_then(Value::as_array) else {
            return;
        };
        for d in dets {
            let Some(tid) = d.get("track_id").and_then(Value::as_u64) else {
                continue;
            };
            let state = d.get("track_state").and_then(Value::as_str);
            if matches!(state, Some("lost" | "tentative")) {
                continue;
            }
            let Some(ne) = world_ne(d) else {
                continue;
            };
            let kind = kind_of(d.get("class_label").and_then(Value::as_str).unwrap_or(""));
            let ext = Some((cam, tid));
            let held = match self.tracks.iter().position(|t| t.ext == ext) {
                Some(i) => Some(i),
                // A new track id where a track of this camera went unmeasured
                // is that person tracked again: the mover keeps its id.
                None => self.reacquire(ne, now_ms, Some(cam)),
            };
            match held.map(|i| &mut self.tracks[i]) {
                Some(t) => {
                    t.measure(ne, now_ms);
                    t.ext = ext;
                    t.m.kind = kind;
                    t.m.sigma_m = VISION_SIGMA_M;
                }
                None => self.add(kind, ext, ne, VISION_SIGMA_M, now_ms),
            }
        }
        self.prune(now_ms);
    }
}
