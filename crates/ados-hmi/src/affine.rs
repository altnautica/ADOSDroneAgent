//! Touchscreen affine transform: raw ADC -> LCD pixel coordinates.
//!
//! The resistive-touch driver reports raw 12-bit ADC counts in 0..4095. The
//! LCD panel may be rotated 0/90/180/270 degrees, and the resistive overlay is
//! rarely perfectly aligned with the visible pixels. Both problems collapse
//! into a single 2x3 affine matrix that maps raw `(x_raw, y_raw)` to LCD
//! `(x_px, y_px)`.
//!
//! Covers the matrix and its `apply` rounding, the four rotation matrices, the
//! normal-equations least-squares fit (the 3x3 assembled once and solved
//! twice), the 3x3 Gaussian-elimination solver with partial pivot and singular
//! guard, the RMS residual, and atomic JSON persistence of the 9-key blob.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Raw ADC range exposed by the resistive-touch driver. Used by the identity
/// transform. The driver emits values clipped to this range, so the math does
/// not have to defend against out-of-range inputs.
pub const RAW_MIN: i32 = 0;
/// Largest raw ADC value.
pub const RAW_MAX: i32 = 4095;

/// Persistence schema version. Bumped only when the on-disk shape changes ,
/// readers fall back to the identity transform on a version mismatch rather
/// than blindly trusting an unknown layout.
pub const CALIB_FILE_VERSION: i64 = 1;

/// Singular-system guard: a pivot magnitude under this is treated as a
/// rank-deficient normal-equations matrix (samples colinear).
const SINGULAR_EPS: f64 = 1e-12;

/// Errors from fitting a calibration matrix to sample/target pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FitError {
    /// Fewer than five sample/target pairs supplied.
    NotEnoughPairs,
    /// Sample and target vectors are different lengths.
    LengthMismatch,
    /// The normal-equations matrix is singular (samples lie on a line).
    Singular,
}

impl std::fmt::Display for FitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FitError::NotEnoughPairs => f.write_str("need at least 5 sample/target pairs"),
            FitError::LengthMismatch => f.write_str("samples and targets must be the same length"),
            FitError::Singular => f.write_str("affine fit is singular (samples colinear)"),
        }
    }
}

impl std::error::Error for FitError {}

/// A 2x3 affine matrix mapping raw ADC -> LCD pixels.
///
/// The transform is:
///
/// ```text
/// x_lcd = a * x_raw + b * y_raw + c
/// y_lcd = d * x_raw + e * y_raw + f
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine {
    /// x coefficient of the x row.
    pub a: f64,
    /// y coefficient of the x row.
    pub b: f64,
    /// Offset of the x row.
    pub c: f64,
    /// x coefficient of the y row.
    pub d: f64,
    /// y coefficient of the y row.
    pub e: f64,
    /// Offset of the y row.
    pub f: f64,
}

impl Affine {
    /// Map a raw sample to LCD pixel coordinates, rounding to the nearest
    /// integer (round-half-to-even).
    pub fn apply(&self, x_raw: i32, y_raw: i32) -> (i32, i32) {
        let xr = x_raw as f64;
        let yr = y_raw as f64;
        let x = self.a * xr + self.b * yr + self.c;
        let y = self.d * xr + self.e * yr + self.f;
        (round_half_even(x) as i32, round_half_even(y) as i32)
    }

    /// Flat 6-float list for JSON persistence (a, b, c, d, e, f).
    pub fn to_list(&self) -> [f64; 6] {
        [self.a, self.b, self.c, self.d, self.e, self.f]
    }

    /// Reconstruct from a flat 6-element slice. Returns `None` on wrong length.
    pub fn from_list(values: &[f64]) -> Option<Affine> {
        if values.len() != 6 {
            return None;
        }
        Some(Affine {
            a: values[0],
            b: values[1],
            c: values[2],
            d: values[3],
            e: values[4],
            f: values[5],
        })
    }
}

/// Banker's rounding (round-half-to-even). Rust's `f64::round` is
/// round-half-away-from-zero; half-to-even keeps pixel coordinates identical
/// to implementations that use the IEEE default rounding mode.
fn round_half_even(x: f64) -> f64 {
    let floor = x.floor();
    let diff = x - floor;
    if diff < 0.5 {
        floor
    } else if diff > 0.5 {
        floor + 1.0
    } else {
        // Exactly halfway: round to the even neighbour.
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    }
}

/// Return a baseline affine that maps raw ADC -> LCD with the given rotation.
///
/// With rotation=0 a raw 0..4095 sweep along x corresponds to LCD 0..lcd_w
/// along x; the raw 0..4095 sweep along y corresponds to LCD 0..lcd_h along y.
/// Rotations apply in 90-degree steps. The matrix is the best-effort fallback
/// when no calibration file exists; it is not perfectly accurate but is correct
/// enough to dispatch tab-bar taps before the operator runs the wizard.
///
/// `lcd_size` is `(lcd_w, lcd_h)`.
pub fn identity_for(rotation: i32, lcd_size: (i32, i32)) -> Affine {
    let mut rotation = rotation.rem_euclid(360);
    if !matches!(rotation, 0 | 90 | 180 | 270) {
        rotation = 0;
    }
    let (lcd_w, lcd_h) = lcd_size;
    let lcd_w = lcd_w as f64;
    let lcd_h = lcd_h as f64;
    let mut span = (RAW_MAX - RAW_MIN) as f64;
    if span <= 0.0 {
        span = 1.0;
    }
    let sx = lcd_w / span;
    let sy = lcd_h / span;
    match rotation {
        0 => Affine {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: sy,
            f: 0.0,
        },
        90 => Affine {
            a: 0.0,
            b: lcd_w / span,
            c: 0.0,
            d: -lcd_h / span,
            e: 0.0,
            f: lcd_h,
        },
        180 => Affine {
            a: -sx,
            b: 0.0,
            c: lcd_w,
            d: 0.0,
            e: -sy,
            f: lcd_h,
        },
        // 270
        _ => Affine {
            a: 0.0,
            b: -lcd_w / span,
            c: lcd_w,
            d: lcd_h / span,
            e: 0.0,
            f: 0.0,
        },
    }
}

/// The on-disk calibration blob. Deserialized leniently when reading; the
/// writer always emits the full set, so every field is always present.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CalibBlob {
    version: i64,
    calibrated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_range: Option<[i32; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lcd_size: Option<[i32; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rotation_applied_at_save: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    matrix: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rms_residual_px: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<bool>,
    saved_at: i64,
}

/// Parameters captured alongside the matrix when persisting a calibration.
#[derive(Debug, Clone)]
pub struct SaveParams {
    /// Panel rotation in degrees.
    pub rotation: i32,
    /// RMS residual of the fit in pixels.
    pub rms: f64,
    /// Name of the touch device the calibration belongs to.
    pub device: String,
    /// Raw ADC range `(min, max)` of the device.
    pub raw_range: (i32, i32),
    /// Panel size `(width, height)` in pixels.
    pub lcd_size: (i32, i32),
}

impl Default for SaveParams {
    fn default() -> Self {
        SaveParams {
            rotation: 0,
            rms: 0.0,
            device: "ads7846".to_string(),
            raw_range: (RAW_MIN, RAW_MAX),
            lcd_size: (480, 320),
        }
    }
}

mod fit;
mod persist;

pub use fit::compute_from_samples;
pub use persist::{load, load_for, save, save_skip_marker};

#[cfg(test)]
mod tests;
