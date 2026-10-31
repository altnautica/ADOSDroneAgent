//! Least-squares affine fit from calibration samples.

use super::*;

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
        // Partial pivot, pick the row with max |a[k][i]| on or below the
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
