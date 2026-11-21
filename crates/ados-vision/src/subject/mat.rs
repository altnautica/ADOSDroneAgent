//! Fixed-size matrix arithmetic for the subject filter's five-element state
//! `[north, east, v_north, v_east, turn_rate]`. Small, stack-only and exact
//! enough in `f64`; no allocation.

pub(super) const N: usize = 5;
pub(super) type Vec5 = [f64; N];
pub(super) type Mat5 = [[f64; N]; N];
pub(super) type Mat2 = [[f64; 2]; 2];

pub(super) fn identity() -> Mat5 {
    let mut m = [[0.0; N]; N];
    for (i, row) in m.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    m
}

pub(super) fn mul(a: &Mat5, b: &Mat5) -> Mat5 {
    let mut m = [[0.0; N]; N];
    for i in 0..N {
        for k in 0..N {
            let aik = a[i][k];
            if aik == 0.0 {
                continue;
            }
            for j in 0..N {
                m[i][j] += aik * b[k][j];
            }
        }
    }
    m
}

/// `a * b^T`.
pub(super) fn mul_bt(a: &Mat5, b: &Mat5) -> Mat5 {
    let mut m = [[0.0; N]; N];
    for i in 0..N {
        for j in 0..N {
            m[i][j] = (0..N).map(|k| a[i][k] * b[j][k]).sum();
        }
    }
    m
}

/// `f * p * f^T + q`, symmetrised.
pub(super) fn propagate(f: &Mat5, p: &Mat5, q: &Mat5) -> Mat5 {
    let mut m = mul_bt(&mul(f, p), f);
    for i in 0..N {
        for j in 0..N {
            m[i][j] += q[i][j];
        }
    }
    symmetrise(&mut m);
    m
}

/// Average `m` with its transpose. Indexed: each step reads and writes the
/// mirrored pair, which no row iterator can borrow at once.
#[allow(clippy::needless_range_loop)]
pub(super) fn symmetrise(m: &mut Mat5) {
    for i in 0..N {
        for j in i + 1..N {
            let v = 0.5 * (m[i][j] + m[j][i]);
            m[i][j] = v;
            m[j][i] = v;
        }
    }
}

/// `m += w * d * d^T`.
pub(super) fn add_outer(m: &mut Mat5, d: &Vec5, w: f64) {
    for i in 0..N {
        for j in 0..N {
            m[i][j] += w * d[i] * d[j];
        }
    }
}

/// The inverse of a 2 x 2 matrix and its determinant; `None` when singular.
pub(super) fn inv2(m: &Mat2) -> Option<(Mat2, f64)> {
    let det = m[0][0] * m[1][1] - m[0][1] * m[1][0];
    if det.abs() < 1e-12 || !det.is_finite() {
        return None;
    }
    Some((
        [
            [m[1][1] / det, -m[0][1] / det],
            [-m[1][0] / det, m[0][0] / det],
        ],
        det,
    ))
}

/// `v^T m v` for a 2-vector.
pub(super) fn quad2(m: &Mat2, v: [f64; 2]) -> f64 {
    v[0] * (m[0][0] * v[0] + m[0][1] * v[1]) + v[1] * (m[1][0] * v[0] + m[1][1] * v[1])
}

/// The horizontal-position block of a state covariance.
pub(super) fn pos_block(p: &Mat5) -> Mat2 {
    [[p[0][0], p[0][1]], [p[1][0], p[1][1]]]
}

/// The Kalman update of a state on a direct measurement `z` of its
/// horizontal position with covariance `r`. Returns the innovation, its
/// covariance's inverse and determinant, or `None` when that covariance is
/// singular (the state is left alone).
pub(super) fn update_position(
    x: &mut Vec5,
    p: &mut Mat5,
    z: [f64; 2],
    r: &Mat2,
) -> Option<([f64; 2], Mat2, f64)> {
    let nu = [z[0] - x[0], z[1] - x[1]];
    let mut s = pos_block(p);
    for i in 0..2 {
        for j in 0..2 {
            s[i][j] += r[i][j];
        }
    }
    let (si, det) = inv2(&s)?;
    // K = P H^T S^-1, where P H^T is P's first two columns.
    let mut k = [[0.0; 2]; N];
    for (i, row) in k.iter_mut().enumerate() {
        for (j, kij) in row.iter_mut().enumerate() {
            *kij = p[i][0] * si[0][j] + p[i][1] * si[1][j];
        }
    }
    for i in 0..N {
        x[i] += k[i][0] * nu[0] + k[i][1] * nu[1];
    }
    // P = P - K H P, H P being P's first two rows.
    let hp = [p[0], p[1]];
    for i in 0..N {
        for j in 0..N {
            p[i][j] -= k[i][0] * hp[0][j] + k[i][1] * hp[1][j];
        }
    }
    symmetrise(p);
    Some((nu, si, det))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_update_pulls_halfway_for_equal_variances() {
        let mut x = [0.0, 0.0, 1.0, 0.0, 0.0];
        let mut p = identity();
        let (nu, si, det) =
            update_position(&mut x, &mut p, [2.0, -2.0], &[[1.0, 0.0], [0.0, 1.0]]).unwrap();
        assert_eq!(nu, [2.0, -2.0]);
        assert!((x[0] - 1.0).abs() < 1e-12 && (x[1] + 1.0).abs() < 1e-12);
        assert!((p[0][0] - 0.5).abs() < 1e-12);
        assert!((quad2(&si, nu) - 4.0).abs() < 1e-12);
        assert!((det - 4.0).abs() < 1e-12);
        // The uncorrelated velocity is untouched.
        assert_eq!(x[2], 1.0);
    }
}
