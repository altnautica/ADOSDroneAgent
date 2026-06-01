//! Touchscreen affine transform: raw ADC -> LCD pixel coordinates.
//!
//! The resistive-touch driver reports raw 12-bit ADC counts in 0..4095. The
//! LCD panel may be rotated 0/90/180/270 degrees, and the resistive overlay is
//! rarely perfectly aligned with the visible pixels. Both problems collapse
//! into a single 2x3 affine matrix that maps raw `(x_raw, y_raw)` to LCD
//! `(x_px, y_px)`.
//!
//! Ports `src/ados/services/ui/touch/transform.py`: the matrix dataclass and
//! its `apply` rounding, the four rotation matrices, the normal-equations
//! least-squares fit (the 3x3 assembled once and solved twice), the 3x3
//! Gaussian-elimination solver with partial pivot + singular guard, the RMS
//! residual, and the atomic `touch.calib` persistence with the 9-key blob.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Raw ADC range exposed by the resistive-touch driver. Used by the identity
/// transform. The driver emits values clipped to this range, so the math does
/// not have to defend against out-of-range inputs.
pub const RAW_MIN: i32 = 0;
pub const RAW_MAX: i32 = 4095;

/// Persistence schema version. Bumped only when the on-disk shape changes —
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
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Affine {
    /// Map a raw sample to LCD pixel coordinates, rounding to the nearest
    /// integer (Python `int(round(x))` — round-half-to-even).
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

/// Python `int(round(x))` uses banker's rounding (round-half-to-even). Rust's
/// `f64::round` is round-half-away-from-zero, so match Python explicitly to
/// keep pixel coordinates byte-identical across the two implementations.
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

/// Fit an affine matrix to 5+ raw/target pairs by least squares.
///
/// Returns `(affine, rms_residual_px)` where `rms_residual_px` is the
/// root-mean-square distance between the LCD-projected sample and the target,
/// in LCD pixels. The x and y equations decouple, so the 3x3 normal-equations
/// matrix is assembled once and solved twice (right-hand-side x_target, then
/// y_target).
pub fn compute_from_samples(
    samples: &[(i32, i32)],
    targets: &[(i32, i32)],
) -> Result<(Affine, f64), FitError> {
    if samples.len() < 5 || targets.len() < 5 {
        return Err(FitError::NotEnoughPairs);
    }
    if samples.len() != targets.len() {
        return Err(FitError::LengthMismatch);
    }

    let mut sxx = 0.0;
    let mut sxy = 0.0;
    let mut sx = 0.0;
    let mut syy = 0.0;
    let mut sy = 0.0;
    let mut n = 0.0;
    let mut txx = 0.0; // for x targets
    let mut txy = 0.0;
    let mut tx_ = 0.0;
    let mut tyx = 0.0; // for y targets
    let mut tyy = 0.0;
    let mut ty_ = 0.0;
    for (&(x_raw, y_raw), &(x_t, y_t)) in samples.iter().zip(targets.iter()) {
        let x_raw_f = x_raw as f64;
        let y_raw_f = y_raw as f64;
        let x_t = x_t as f64;
        let y_t = y_t as f64;
        sxx += x_raw_f * x_raw_f;
        sxy += x_raw_f * y_raw_f;
        syy += y_raw_f * y_raw_f;
        sx += x_raw_f;
        sy += y_raw_f;
        n += 1.0;
        txx += x_raw_f * x_t;
        txy += y_raw_f * x_t;
        tx_ += x_t;
        tyx += x_raw_f * y_t;
        tyy += y_raw_f * y_t;
        ty_ += y_t;
    }

    // Normal-equations matrix M (3x3, symmetric):
    //   [[sxx, sxy, sx],
    //    [sxy, syy, sy],
    //    [sx,  sy,  n ]]
    let m = [[sxx, sxy, sx], [sxy, syy, sy], [sx, sy, n]];
    let abc = solve3(&m, [txx, txy, tx_])?;
    let def = solve3(&m, [tyx, tyy, ty_])?;
    let affine = Affine {
        a: abc[0],
        b: abc[1],
        c: abc[2],
        d: def[0],
        e: def[1],
        f: def[2],
    };

    // RMS residual computed by re-applying the matrix to every sample.
    let mut sqsum = 0.0;
    for (&(x_raw, y_raw), &(x_t, y_t)) in samples.iter().zip(targets.iter()) {
        let (x_p, y_p) = affine.apply(x_raw, y_raw);
        let dx = (x_p - x_t) as f64;
        let dy = (y_p - y_t) as f64;
        sqsum += dx * dx + dy * dy;
    }
    let rms = (sqsum / (samples.len().max(1) as f64)).sqrt();
    Ok((affine, rms))
}

/// Solve a 3x3 linear system via Gaussian elimination with partial pivoting.
///
/// Returns the solution vector. Returns [`FitError::Singular`] if the system is
/// rank-deficient (e.g. all samples lie on a line, which makes the
/// normal-equations matrix degenerate).
//
// Index-based elimination: each step reads the pivot row `i` while mutating a
// lower row `k`, and the inner column sweep spans the augmented column `n`, so
// the row/column indices carry meaning the iterator rewrite would obscure.
#[allow(clippy::needless_range_loop)]
fn solve3(m: &[[f64; 3]; 3], rhs: [f64; 3]) -> Result<[f64; 3], FitError> {
    // Augmented matrix: 3 rows of 4 columns.
    let mut a = [
        [m[0][0], m[0][1], m[0][2], rhs[0]],
        [m[1][0], m[1][1], m[1][2], rhs[1]],
        [m[2][0], m[2][1], m[2][2], rhs[2]],
    ];
    let n = 3usize;
    for i in 0..n {
        // Partial pivot — pick the row with max |a[k][i]| on or below the
        // diagonal.
        let mut pivot = i;
        let mut max_abs = a[i][i].abs();
        for k in (i + 1)..n {
            if a[k][i].abs() > max_abs {
                max_abs = a[k][i].abs();
                pivot = k;
            }
        }
        if max_abs < SINGULAR_EPS {
            return Err(FitError::Singular);
        }
        if pivot != i {
            a.swap(i, pivot);
        }
        // Eliminate.
        for k in (i + 1)..n {
            let factor = a[k][i] / a[i][i];
            for j in i..=n {
                a[k][j] -= factor * a[i][j];
            }
        }
    }
    // Back-substitute.
    let mut x = [0.0f64; 3];
    for i in (0..n).rev() {
        let mut s = a[i][n];
        for j in (i + 1)..n {
            s -= a[i][j] * x[j];
        }
        x[i] = s / a[i][i];
    }
    Ok(x)
}

/// The on-disk calibration blob. Deserialized leniently when reading; the
/// writer always emits the full set, matching the Python `save` dict.
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
    pub rotation: i32,
    pub rms: f64,
    pub device: String,
    pub raw_range: (i32, i32),
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

/// Read the persisted affine from `path`.
///
/// Returns `None` when the file is missing, malformed, marked
/// `calibrated=false` (a skip marker), or the version field does not match this
/// build. The caller falls back to [`identity_for`] in any of those cases.
pub fn load(path: &Path) -> Option<Affine> {
    let text = std::fs::read_to_string(path).ok()?;
    let blob: CalibBlob = serde_json::from_str(&text).ok()?;
    if blob.version != CALIB_FILE_VERSION {
        return None;
    }
    if !blob.calibrated {
        return None;
    }
    let matrix = blob.matrix?;
    if matrix.len() != 6 {
        return None;
    }
    Affine::from_list(&matrix)
}

/// Persist the affine atomically to `path`.
///
/// The blob captures version, the device hint, raw ADC range, the LCD geometry
/// the calibration was taken at, the rotation that was applied, the matrix
/// itself, the RMS residual, and a UNIX timestamp. Atomic write via tmpfile +
/// fsync + rename so a power loss mid-save can never half-write the file.
pub fn save(affine: &Affine, path: &Path, params: &SaveParams) -> std::io::Result<()> {
    let blob = CalibBlob {
        version: CALIB_FILE_VERSION,
        calibrated: true,
        device: Some(params.device.clone()),
        raw_range: Some([params.raw_range.0, params.raw_range.1]),
        lcd_size: Some([params.lcd_size.0, params.lcd_size.1]),
        rotation_applied_at_save: Some(params.rotation.rem_euclid(360)),
        matrix: Some(affine.to_list().to_vec()),
        rms_residual_px: Some(params.rms),
        skipped: None,
        saved_at: now_unix_secs(),
    };
    atomic_write_json(path, &blob)
}

/// Persist a marker that says the operator chose to skip calibration.
///
/// The reader treats this the same as "no calibration file" and uses the
/// identity transform — but the marker stops the wizard from auto-launching
/// every boot.
pub fn save_skip_marker(path: &Path) -> std::io::Result<()> {
    let blob = CalibBlob {
        version: CALIB_FILE_VERSION,
        calibrated: false,
        device: None,
        raw_range: None,
        lcd_size: None,
        rotation_applied_at_save: None,
        matrix: None,
        rms_residual_px: None,
        skipped: Some(true),
        saved_at: now_unix_secs(),
    };
    atomic_write_json(path, &blob)
}

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn atomic_write_json(path: &Path, blob: &CalibBlob) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Compact separators, mirroring Python `json.dump(..., separators=(",",":"))`.
    let body = serde_json::to_vec(blob).map_err(std::io::Error::other)?;

    // tmp sibling in the same directory so the rename is atomic on the same
    // filesystem. Name disambiguated by pid so two writers do not collide.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("touch.calib");
    let tmp = parent.join(format!("{}.{}.tmp", file_name, std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&body)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write_result;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_rounds_to_nearest_pixel() {
        // a=2 maps raw 10 -> 20; the +0.4 offset rounds down, +0.6 rounds up.
        let m = Affine {
            a: 2.0,
            b: 0.0,
            c: 0.4,
            d: 0.0,
            e: 3.0,
            f: 0.6,
        };
        let (x, y) = m.apply(10, 5);
        assert_eq!(x, 20); // 2*10 + 0.4 = 20.4 -> 20
        assert_eq!(y, 16); // 3*5 + 0.6 = 15.6 -> 16
    }

    #[test]
    fn identity_rotation_0() {
        let m = identity_for(0, (480, 320));
        let span = (RAW_MAX - RAW_MIN) as f64;
        assert_eq!(m.apply(0, 0), (0, 0));
        // Full sweep maps to the LCD bounds.
        assert_eq!(m.apply(RAW_MAX, RAW_MAX), (480, 320));
        assert!((m.a - 480.0 / span).abs() < 1e-12);
        assert!((m.e - 320.0 / span).abs() < 1e-12);
        assert_eq!(m.b, 0.0);
        assert_eq!(m.d, 0.0);
    }

    #[test]
    fn identity_rotation_90() {
        let m = identity_for(90, (480, 320));
        // y_raw drives x_lcd, x_raw drives y_lcd (inverted to lcd_h).
        assert_eq!(m.a, 0.0);
        assert_eq!(m.e, 0.0);
        assert_eq!(m.f, 320.0);
        // raw (0,0) -> x = 0, y = lcd_h.
        assert_eq!(m.apply(0, 0), (0, 320));
        // raw (max, max) -> x = lcd_w, y = 0.
        assert_eq!(m.apply(RAW_MAX, RAW_MAX), (480, 0));
    }

    #[test]
    fn identity_rotation_180() {
        let m = identity_for(180, (480, 320));
        assert_eq!(m.c, 480.0);
        assert_eq!(m.f, 320.0);
        // raw (0,0) -> (lcd_w, lcd_h), raw (max,max) -> (0,0).
        assert_eq!(m.apply(0, 0), (480, 320));
        assert_eq!(m.apply(RAW_MAX, RAW_MAX), (0, 0));
    }

    #[test]
    fn identity_rotation_270() {
        let m = identity_for(270, (480, 320));
        assert_eq!(m.c, 480.0);
        assert_eq!(m.f, 0.0);
        // raw (0,0) -> (lcd_w, 0), raw (max,max) -> (0, lcd_h).
        assert_eq!(m.apply(0, 0), (480, 0));
        assert_eq!(m.apply(RAW_MAX, RAW_MAX), (0, 320));
    }

    #[test]
    fn identity_out_of_range_rotation_falls_back_to_zero() {
        let m45 = identity_for(45, (480, 320));
        let m0 = identity_for(0, (480, 320));
        assert_eq!(m45, m0);
        // Negative rotations normalize via rem_euclid (Python `% 360`).
        let m_neg = identity_for(-90, (480, 320));
        assert_eq!(m_neg, identity_for(270, (480, 320)));
    }

    #[test]
    fn least_squares_recovers_a_known_transform_with_zero_rms() {
        // Build a known affine and synthesize exact targets from it.
        let truth = Affine {
            a: 0.1,
            b: 0.0,
            c: 5.0,
            d: 0.0,
            e: 0.07,
            f: 3.0,
        };
        let samples = [
            (100, 200),
            (3900, 250),
            (2000, 3800),
            (500, 3000),
            (3500, 1500),
            (1800, 800),
        ];
        let targets: Vec<(i32, i32)> = samples.iter().map(|&(x, y)| truth.apply(x, y)).collect();
        let (fit, rms) = compute_from_samples(&samples, &targets).unwrap();
        // Coefficients land within the integer-rounding noise of the truth
        // (the targets were rounded to whole pixels before fitting, so an exact
        // recovery is not possible — the slope coefficients pick up that
        // perturbation, larger for the small-magnitude axes).
        assert!((fit.a - truth.a).abs() < 1e-3, "a={}", fit.a);
        assert!((fit.e - truth.e).abs() < 1e-3, "e={}", fit.e);
        assert!((fit.c - truth.c).abs() < 1.0, "c={}", fit.c);
        // The real parity check: re-applying the fit reproduces every rounded
        // target exactly, and the RMS residual is sub-pixel.
        for (&(x, y), &t) in samples.iter().zip(targets.iter()) {
            assert_eq!(fit.apply(x, y), t);
        }
        assert!(rms < 1.0, "rms={rms}");
    }

    #[test]
    fn least_squares_needs_five_pairs() {
        let s = [(0, 0), (1, 1), (2, 2), (3, 3)];
        let t = [(0, 0), (1, 1), (2, 2), (3, 3)];
        assert_eq!(
            compute_from_samples(&s, &t).unwrap_err(),
            FitError::NotEnoughPairs
        );
    }

    #[test]
    fn least_squares_rejects_length_mismatch() {
        let s = [(0, 0), (1, 1), (2, 2), (3, 3), (4, 4), (5, 5)];
        let t = [(0, 0), (1, 1), (2, 2), (3, 3), (4, 4)];
        assert_eq!(
            compute_from_samples(&s, &t).unwrap_err(),
            FitError::LengthMismatch
        );
    }

    #[test]
    fn singular_guard_rejects_colinear_samples() {
        // All samples on the line y_raw == x_raw -> normal-equations matrix
        // is rank-deficient and the solver must bail.
        let s = [(0, 0), (1, 1), (2, 2), (3, 3), (4, 4), (5, 5)];
        let t = [(0, 0), (10, 10), (20, 20), (30, 30), (40, 40), (50, 50)];
        assert_eq!(
            compute_from_samples(&s, &t).unwrap_err(),
            FitError::Singular
        );
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("touch.calib");
        let m = Affine {
            a: 0.117,
            b: 0.001,
            c: -3.2,
            d: -0.002,
            e: 0.078,
            f: 4.5,
        };
        let params = SaveParams {
            rotation: 90,
            rms: 2.5,
            ..SaveParams::default()
        };
        save(&m, &path, &params).unwrap();

        // Loads back the same matrix.
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, m);

        // The blob carries the full 9-key set with the expected values.
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for k in [
            "version",
            "calibrated",
            "device",
            "raw_range",
            "lcd_size",
            "rotation_applied_at_save",
            "matrix",
            "rms_residual_px",
            "saved_at",
        ] {
            assert!(v.get(k).is_some(), "missing key {k}");
        }
        assert_eq!(v["version"], CALIB_FILE_VERSION);
        assert_eq!(v["calibrated"], true);
        assert_eq!(v["device"], "ads7846");
        assert_eq!(v["raw_range"], serde_json::json!([0, 4095]));
        assert_eq!(v["lcd_size"], serde_json::json!([480, 320]));
        assert_eq!(v["rotation_applied_at_save"], 90);
        assert_eq!(v["matrix"].as_array().unwrap().len(), 6);
        assert_eq!(v["rms_residual_px"], 2.5);

        // Compact separators (no spaces after , or :).
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(", "));
        assert!(!text.contains(": "));

        // No leftover tmp sibling.
        let stray = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray, "stray .tmp file left behind");
    }

    #[test]
    fn skip_marker_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("touch.calib");
        save_skip_marker(&path).unwrap();
        // calibrated=false -> reader falls back to identity (None).
        assert!(load(&path).is_none());
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["calibrated"], false);
        assert_eq!(v["skipped"], true);
        // No matrix key when skipping.
        assert!(v.get("matrix").is_none());
    }

    #[test]
    fn load_missing_file_is_none() {
        assert!(load(Path::new("/nonexistent/touch.calib")).is_none());
    }

    #[test]
    fn load_version_mismatch_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("touch.calib");
        let blob = serde_json::json!({
            "version": 99,
            "calibrated": true,
            "matrix": [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            "saved_at": 0,
        });
        std::fs::write(&path, serde_json::to_vec(&blob).unwrap()).unwrap();
        assert!(load(&path).is_none());
    }

    #[test]
    fn load_wrong_matrix_length_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("touch.calib");
        let blob = serde_json::json!({
            "version": CALIB_FILE_VERSION,
            "calibrated": true,
            "matrix": [1.0, 0.0, 0.0],
            "saved_at": 0,
        });
        std::fs::write(&path, serde_json::to_vec(&blob).unwrap()).unwrap();
        assert!(load(&path).is_none());
    }
}
