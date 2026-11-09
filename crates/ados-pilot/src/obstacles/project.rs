//! Per-camera obstacle grid (C3 `obstacles.cameras`): each of the 32×18 cells
//! casts its centre ray into the planar obstacle model. Obstacles are treated
//! as vertical from the ground up to a little above the vehicle, since the
//! planar scan sees only its own plane.
//!
//! A cell's ray goes through the pinhole the renderer and the AR layer use:
//! cell `(c, r)` is the uniform image tile, its ray in the camera frame is
//! `(1, (2u-1)·tan(hfov/2), (2v-1)·tan(vfov/2))` (x forward, y right, z down),
//! turned into NED by the same chain as the AR projection: body to NED is
//! `Rz(yaw)·Ry(pitch)·Rx(roll)`, then the camera's yaw from the nose and its
//! tilt. A stabilised gimbal holds its tilt against the horizon, so vehicle
//! roll and pitch do not enter its chain.

use ados_protocol::pilot::{encode_distance, CameraGrid, CameraModel, GRID_COLS, GRID_ROWS};
use base64::Engine as _;

use super::Obstacles;
use crate::geo::V2;

/// How far above the vehicle an obstacle is assumed to extend.
const OBSTACLE_TOP_ABOVE_M: f32 = 3.0;
/// Longest ray cast.
const MAX_RANGE_M: f32 = 30.0;
/// A ray steeper than this (horizontal part per metre) meets no wall.
const MIN_HORIZONTAL: f32 = 1e-3;
/// Cell rays are cast along their horizontal bearing snapped to this step, and
/// rays on one bearing share one march: rows of a column, and neighbouring
/// columns, mostly look along the same bearing. Half a degree moves a ray at
/// most 13 cm sideways at the 30 m range, about the grid's own step, against a
/// cell 2.5° wide.
const BEARING_STEP_RAD: f32 = 0.5 * std::f32::consts::PI / 180.0;
const BEARING_BINS: usize = 720;

/// Reusable buffers for one camera's grid.
#[derive(Debug, Default)]
pub struct Scratch {
    near: Vec<u8>,
    measured: Vec<u8>,
    /// Per bearing bin: the frame it was cast in and its distance.
    rays: Vec<(u32, Option<f32>)>,
    frame: u32,
}

impl Scratch {
    /// Rays marched for the last projected frame.
    #[cfg(test)]
    pub(super) fn rays_cast(&self) -> usize {
        self.rays.iter().filter(|r| r.0 == self.frame).count()
    }
}

/// Vehicle attitude in radians (pitch + nose up, roll + right wing down, yaw
/// clockwise from north).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Attitude {
    pub roll: f32,
    pub pitch: f32,
    pub yaw: f32,
}

type Mat = [[f32; 3]; 3];

fn mul(a: &Mat, b: &Mat) -> Mat {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

fn rx(a: f32) -> Mat {
    let (s, c) = a.sin_cos();
    [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
}

fn ry(a: f32) -> Mat {
    let (s, c) = a.sin_cos();
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}

fn rz(a: f32) -> Mat {
    let (s, c) = a.sin_cos();
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}

/// The camera-to-NED rotation.
fn camera_to_ned(cam: &CameraModel, att: Attitude) -> Mat {
    let tilt = ry(-cam.tilt_down_deg.to_radians());
    let cam_yaw = cam.yaw_deg.to_radians();
    if cam.stabilised {
        mul(&rz(att.yaw + cam_yaw), &tilt)
    } else {
        let body = mul(&mul(&rz(att.yaw), &ry(att.pitch)), &rx(att.roll));
        mul(&mul(&body, &rz(cam_yaw)), &tilt)
    }
}

/// Project the obstacle model into one camera.
#[allow(clippy::too_many_arguments)]
pub fn project_camera(
    obs: &Obstacles,
    cam: &CameraModel,
    pos: V2,
    att: Attitude,
    agl: f32,
    now_ms: u64,
    scratch: &mut Scratch,
) -> CameraGrid {
    scratch.near.clear();
    scratch.near.resize(GRID_COLS * GRID_ROWS, 0);
    scratch.measured.clear();
    scratch.measured.resize(GRID_COLS * GRID_ROWS / 8, 0);
    scratch.rays.resize(BEARING_BINS, (0, None));
    // A new frame stamp invalidates every cached bearing without clearing.
    scratch.frame = scratch.frame.wrapping_add(1).max(1);
    if scratch.frame == 1 {
        scratch.rays.fill((0, None));
    }
    let r = camera_to_ned(cam, att);
    let tan_h = (cam.hfov_deg.to_radians() / 2.0).tan();
    let tan_v = (cam.vfov_deg.to_radians() / 2.0).tan();
    for row in 0..GRID_ROWS {
        let z = ((row as f32 + 0.5) / GRID_ROWS as f32 * 2.0 - 1.0) * tan_v;
        for c in 0..GRID_COLS {
            let y = ((c as f32 + 0.5) / GRID_COLS as f32 * 2.0 - 1.0) * tan_h;
            // The cell's ray in NED (not unit length).
            let d = [
                r[0][0] + r[0][1] * y + r[0][2] * z,
                r[1][0] + r[1][1] * y + r[1][2] * z,
                r[2][0] + r[2][1] * y + r[2][2] * z,
            ];
            let horizontal = d[0].hypot(d[1]);
            if horizontal < MIN_HORIZONTAL {
                continue;
            }
            let bin = ((d[1].atan2(d[0]) / BEARING_STEP_RAD).round() as i32)
                .rem_euclid(BEARING_BINS as i32) as usize;
            let slot = &mut scratch.rays[bin];
            if slot.0 != scratch.frame {
                let (s, co) = (bin as f32 * BEARING_STEP_RAD).sin_cos();
                *slot = (
                    scratch.frame,
                    obs.ray_distance(pos, [co, s], MAX_RANGE_M, now_ms),
                );
            }
            let Some(dh) = slot.1 else {
                continue;
            };
            // Height of the ray above ground at the obstacle's distance.
            let h = agl - dh * d[2] / horizontal;
            if h < 0.0 || h > agl + OBSTACLE_TOP_ABOVE_M {
                continue;
            }
            let length = (horizontal * horizontal + d[2] * d[2]).sqrt();
            let range = dh * length / horizontal;
            let k = row * GRID_COLS + c;
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
