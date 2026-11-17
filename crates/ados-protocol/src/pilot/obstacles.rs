//! The nav-stream `obstacles` kind and the pilot's published float form.

use serde::{Deserialize, Serialize};

/// One camera's obstacle (or depth) grid: 32×18 cells, row-major, row 0 at
/// the top of the frame. `near` is base64 of one byte per cell (0 unknown,
/// 1..=255 log-coded distance, see [`decode_distance`]); `measured` is base64
/// of a bitmask, bit `i` = cell `i`, byte `i >> 3`, LSB first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraGrid {
    pub cols: u16,
    pub rows: u16,
    pub near: String,
    pub measured: String,
}

/// Grid width in cells.
pub const GRID_COLS: usize = 32;
/// Grid height in cells.
pub const GRID_ROWS: usize = 18;
/// Number of 5° body sectors.
pub const SECTORS: usize = 72;

/// The nav-stream `obstacles` kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObstaclesFrame {
    pub v: u8,
    pub ts_ms: u64,
    /// 72 sectors of 5°, body yaw 0 = nose, clockwise; `None` = no return.
    pub sectors_m: Vec<Option<f64>>,
    pub cameras: std::collections::BTreeMap<String, CameraGrid>,
}

/// A published float: millimetre precision, as f64 so JSON carries the
/// decimal value rather than its f32 expansion.
pub fn wire(v: f32) -> f64 {
    (f64::from(v) * 1000.0).round() / 1000.0
}

/// Distance in metres of a grid code (0 = unknown → `None`).
pub fn decode_distance(code: u8) -> Option<f32> {
    if code == 0 {
        return None;
    }
    Some(0.2 * ((f32::from(code) - 1.0) * 300f32.ln() / 254.0).exp())
}

/// The grid code of a distance in metres, clamped to 0.2–60 m.
pub fn encode_distance(d_m: f32) -> u8 {
    let d = d_m.clamp(0.2, 60.0);
    let v = 1.0 + 254.0 * (d / 0.2).ln() / 300f32.ln();
    v.round().clamp(1.0, 255.0) as u8
}
