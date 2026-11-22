//! Movers from a camera's multi-object tracker (`vision.detection` v4).
//!
//! A track with a world position is placed there. A person track without
//! one (the vision engine fills it only when it knows the vehicle's pose) is
//! ranged here from its box: the camera's geometry the batch carries
//! (`hfov_deg`, `mount_pitch_deg`, the frame size), the vehicle's attitude
//! and its height above the ground. The box's foot edge meets the ground
//! plane and its head edge a person's head plane; a box whose two edges
//! disagree on the range, or whose head edge looks level or up, is no person
//! standing on the ground (a trunk read as a person) and places nothing.
//! Flown lower than a head, the foot edge alone places a box, with a wider
//! error.

use serde_json::Value;

use ados_protocol::pilot::MoverKind;
use ados_protocol::vision_rpc::is_owned_model_id;

use super::{at_subject, Movers};
use crate::geo::V2;

/// The one-sigma error of a camera track's world position, m.
const VISION_SIGMA_M: f32 = 0.3;
/// The one-sigma error of a box ranged here, m, plus this much per metre of
/// range: the ground under a person seldom lies level with the ground under
/// the vehicle, and a box edge moves a few pixels frame to frame.
const BOX_SIGMA_M: f32 = 0.3;
const BOX_SIGMA_PER_M: f32 = 0.15;
/// ... this much per metre for a box ranged on its foot edge alone (the
/// camera lower than a head: no second edge checks the range).
const BOX_FEET_SIGMA_PER_M: f32 = 0.25;
/// A person's height, the head plane a box's top edge meets, m.
const BOX_PERSON_H_M: f32 = 1.75;
/// A box edge this near the frame's edge is cut there, px.
const BOX_EDGE_PX: f32 = 6.0;
/// The ranges of a box's two edges agree within this ratio for a person.
const BOX_AGREE: f32 = 1.6;
/// Only a camera mounted within this of level is ranged on (a belly
/// camera's box spans no head-to-feet height), degrees.
const BOX_MOUNT_MAX_DEG: f32 = 45.0;
/// A plane less than this below the camera is not ranged on, m.
const BOX_PLANE_MIN_M: f32 = 0.2;

/// Where the vehicle looked from when a batch arrived: its local NED
/// position, its attitude (roll, pitch, yaw, rad) and its height above the
/// ground, m.
#[derive(Debug, Clone, Copy)]
pub struct CameraFrom {
    pub ned: [f32; 3],
    pub att: [f32; 3],
    pub agl: f32,
}

/// A batch's camera: focal length and frame size, px, and mount pitch, rad
/// (negative looks down). Cameras face forward (the roster has no mount
/// yaw).
#[derive(Debug, Clone, Copy)]
struct Lens {
    f: f32,
    w: f32,
    h: f32,
    pitch: f32,
}

impl Lens {
    fn of(payload: &Value) -> Option<Self> {
        let num = |k: &str| {
            payload
                .get(k)
                .and_then(Value::as_f64)
                .map(|x| x as f32)
                .filter(|x| x.is_finite())
        };
        let hfov = num("hfov_deg").filter(|h| (1.0..179.0).contains(h))?;
        let pitch = num("mount_pitch_deg").filter(|p| p.abs() <= BOX_MOUNT_MAX_DEG)?;
        let w = num("frame_width").filter(|w| *w > 0.0)?;
        let h = num("frame_height").filter(|h| *h > 0.0)?;
        Some(Self {
            f: w / 2.0 / (hfov.to_radians() / 2.0).tan(),
            w,
            h,
            pitch: pitch.to_radians(),
        })
    }

    /// The world NED direction of pixel (`u`, `v`) seen from `from`.
    fn ray(&self, u: f32, v: f32, from: &CameraFrom) -> [f32; 3] {
        let c = [
            1.0,
            (u - self.w / 2.0) / self.f,
            (v - self.h / 2.0) / self.f,
        ];
        // Camera to body: the mount pitched up by `pitch`.
        let (sp, cp) = self.pitch.sin_cos();
        let b = [cp * c[0] + sp * c[2], c[1], -sp * c[0] + cp * c[2]];
        // Body FRD to NED.
        let (sr, cr) = from.att[0].sin_cos();
        let (st, ct) = from.att[1].sin_cos();
        let (sy, cy) = from.att[2].sin_cos();
        [
            ct * cy * b[0] + (sr * st * cy - cr * sy) * b[1] + (cr * st * cy + sr * sy) * b[2],
            ct * sy * b[0] + (sr * st * sy + cr * cy) * b[1] + (cr * st * sy - sr * cy) * b[2],
            -st * b[0] + sr * ct * b[1] + cr * ct * b[2],
        ]
    }

    /// Where the ray through pixel (`u`, `v`) meets the plane `below` m under
    /// the camera, north and east, with its horizontal range.
    fn meet(&self, u: f32, v: f32, below: f32, from: &CameraFrom) -> Option<(V2, f32)> {
        if below < BOX_PLANE_MIN_M {
            return None;
        }
        let r = self.ray(u, v, from);
        if r[2] <= 1e-3 {
            return None;
        }
        let k = below / r[2];
        let off = [k * r[0], k * r[1]];
        Some((
            [from.ned[0] + off[0], from.ned[1] + off[1]],
            off[0].hypot(off[1]),
        ))
    }

    /// A person's box (`bbox` {x, y, width, height}, px) placed on the
    /// ground from `from`: north, east and the one-sigma error, m. The
    /// nearer of the two edges' ranges when both are in the frame and agree;
    /// the head edge alone when the feet are cut at the frame's bottom; no
    /// place when the head is cut, looks level or up, or the edges disagree.
    /// A camera less than [`BOX_PLANE_MIN_M`] over a head has no head edge
    /// to range on: the foot edge alone places the box, wider.
    fn place(&self, bbox: &Value, from: &CameraFrom) -> Option<(V2, f32)> {
        let num = |k: &str| bbox.get(k).and_then(Value::as_f64).map(|x| x as f32);
        let (x, y, bw, bh) = (num("x")?, num("y")?, num("width")?, num("height")?);
        if ![x, y, bw, bh].iter().all(|v| v.is_finite()) || bw <= 0.0 || bh <= 0.0 {
            return None;
        }
        let (u, top, bottom) = (x + bw / 2.0, y, y + bh);
        let feet_in = bottom < self.h - BOX_EDGE_PX;
        if from.agl - BOX_PERSON_H_M < BOX_PLANE_MIN_M {
            if !feet_in {
                return None;
            }
            let (ne, range) = self.meet(u, bottom, from.agl, from)?;
            return Some((ne, BOX_SIGMA_M + BOX_FEET_SIGMA_PER_M * range));
        }
        if top <= BOX_EDGE_PX {
            return None;
        }
        let head = self.meet(u, top, from.agl - BOX_PERSON_H_M, from)?;
        let (ne, range) = if !feet_in {
            head
        } else {
            let feet = self.meet(u, bottom, from.agl, from)?;
            let ratio = head.1 / feet.1.max(1e-3);
            if !(1.0 / BOX_AGREE..=BOX_AGREE).contains(&ratio) {
                return None;
            }
            if head.1 <= feet.1 {
                head
            } else {
                feet
            }
        };
        Some((ne, BOX_SIGMA_M + BOX_SIGMA_PER_M * range))
    }
}

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
    /// Take one `vision.detection` batch (its JSON form) that arrived while
    /// the vehicle looked from `from`: every confirmed track with a world
    /// position (local ENU, the FC's local origin) is a mover of its class,
    /// and a confirmed person track without one is placed from its box
    /// ([`Lens::place`]). A lost or tentative track measures nothing; a
    /// track already held coasts on its prediction. Only the engine's own
    /// models place anything: a batch naming no model, or a plugin's model
    /// ([`is_owned_model_id`]: every batch a plugin publishes), is the
    /// plugin's claim and measures nothing.
    pub fn ingest_detections(&mut self, payload: &Value, from: Option<CameraFrom>, now_ms: u64) {
        let model = payload.get("model_id").and_then(Value::as_str);
        if model.is_none_or(is_owned_model_id) {
            return;
        }
        let cam = camera_key(
            payload
                .get("camera_id")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        let Some(dets) = payload.get("detections").and_then(Value::as_array) else {
            return;
        };
        let lens = from.and_then(|f| Lens::of(payload).map(|l| (l, f)));
        for d in dets {
            let Some(tid) = d.get("track_id").and_then(Value::as_u64) else {
                continue;
            };
            let state = d.get("track_state").and_then(Value::as_str);
            if matches!(state, Some("lost" | "tentative")) {
                continue;
            }
            let kind = kind_of(d.get("class_label").and_then(Value::as_str).unwrap_or(""));
            let placed = match world_ne(d) {
                Some(ne) => Some((ne, VISION_SIGMA_M)),
                None => lens
                    .filter(|_| kind == MoverKind::Person)
                    .and_then(|(l, f)| l.place(d.get("bbox")?, &f)),
            };
            let Some((ne, sigma)) = placed else {
                continue;
            };
            let ext = Some((cam, tid));
            let held = match self.tracks.iter().position(|t| t.ext == ext) {
                Some(i) => Some(i),
                // A new track id where a track of this camera went unmeasured
                // is that person tracked again: the mover keeps its id.
                None => self.reacquire(ne, now_ms, Some(cam)),
            };
            let i = match held {
                Some(i) => {
                    let t = &mut self.tracks[i];
                    t.measure(ne, now_ms);
                    t.ext = ext;
                    t.m.kind = kind;
                    t.m.sigma_m = sigma;
                    i
                }
                None => {
                    self.add(kind, ext, ne, sigma, now_ms);
                    self.tracks.len() - 1
                }
            };
            // Measured at the goal's subject, it is the subject until its
            // next measurement, wherever its prediction drifts meanwhile.
            let f = self.flight;
            self.tracks[i].subject = at_subject(&f, ne, sigma);
        }
        self.prune(now_ms);
    }
}
