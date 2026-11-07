//! Per-camera obstacle grid (C3 `obstacles.cameras`): each of the 32×18 cells
//! casts its centre ray into the planar obstacle model. Obstacles are treated
//! as vertical from the ground up to a little above the vehicle, since the
//! planar scan sees only its own plane.

use ados_protocol::pilot::{encode_distance, CameraGrid, CameraModel, GRID_COLS, GRID_ROWS};
use base64::Engine as _;

use super::Obstacles;
use crate::geo::{self, V2};

/// How far above the vehicle an obstacle is assumed to extend.
const OBSTACLE_TOP_ABOVE_M: f32 = 3.0;
/// Longest ray cast.
const MAX_RANGE_M: f32 = 30.0;

/// Reusable buffers for one camera's grid.
#[derive(Debug, Default)]
pub struct Scratch {
    near: Vec<u8>,
    measured: Vec<u8>,
}

/// Project the obstacle model into one camera.
#[allow(clippy::too_many_arguments)]
pub fn project_camera(
    obs: &Obstacles,
    cam: &CameraModel,
    pos: V2,
    yaw: f32,
    agl: f32,
    now_ms: u64,
    scratch: &mut Scratch,
) -> CameraGrid {
    scratch.near.clear();
    scratch.near.resize(GRID_COLS * GRID_ROWS, 0);
    scratch.measured.clear();
    scratch.measured.resize(GRID_COLS * GRID_ROWS / 8, 0);
    let hfov = cam.hfov_deg.to_radians();
    let vfov = cam.vfov_deg.to_radians();
    let tilt = cam.tilt_down_deg.to_radians();
    for c in 0..GRID_COLS {
        let az = ((c as f32 + 0.5) / GRID_COLS as f32 - 0.5) * hfov;
        let bearing = yaw + cam.yaw_deg.to_radians() + az;
        let dir = geo::bearing_unit(bearing);
        // One horizontal march per column; rows only differ in elevation.
        let hit_h = obs.ray_distance(pos, dir, MAX_RANGE_M, now_ms);
        let Some(dh) = hit_h else { continue };
        for r in 0..GRID_ROWS {
            let elev = (0.5 - (r as f32 + 0.5) / GRID_ROWS as f32) * vfov - tilt;
            // Height of the ray above ground at the obstacle's distance.
            let h = agl + dh * elev.tan();
            if h < 0.0 || h > agl + OBSTACLE_TOP_ABOVE_M {
                continue;
            }
            let range = dh / elev.cos().max(0.05);
            let k = r * GRID_COLS + c;
            scratch.near[k] = encode_distance(range);
            scratch.measured[k >> 3] |= 1 << (k & 7);
        }
    }
    let b64 = &base64::engine::general_purpose::STANDARD;
    CameraGrid {
        cols: GRID_COLS as u16,
        rows: GRID_ROWS as u16,
        near: b64.encode(&scratch.near),
        measured: b64.encode(&scratch.measured),
    }
}
