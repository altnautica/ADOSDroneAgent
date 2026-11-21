//! Homotopy checks between front-end paths: two paths are taken as the same
//! way round the obstacles when they deform into each other uniformly, every
//! straight line between points at equal fractions of their lengths staying
//! traversable.

use crate::lattice::{Space, BLOCKED};
use crate::world::{dist, lerp, OccupancyQuery};

/// Points compared along each path.
const SAMPLES: usize = 24;

/// `n` points evenly spaced by arc length along `path`.
pub(crate) fn resample(path: &[[f32; 3]], n: usize, out: &mut Vec<[f32; 3]>) {
    out.clear();
    let total: f32 = path.windows(2).map(|w| dist(w[0], w[1])).sum();
    if path.len() < 2 || total <= 1e-6 {
        out.extend(std::iter::repeat_n(
            path.first().copied().unwrap_or_default(),
            n,
        ));
        return;
    }
    let mut seg = 0;
    let mut before = 0.0;
    for i in 0..n {
        let s = total * i as f32 / (n - 1) as f32;
        while seg + 2 < path.len() && before + dist(path[seg], path[seg + 1]) < s {
            before += dist(path[seg], path[seg + 1]);
            seg += 1;
        }
        let l = dist(path[seg], path[seg + 1]).max(1e-9);
        out.push(lerp(
            path[seg],
            path[seg + 1],
            ((s - before) / l).clamp(0.0, 1.0),
        ));
    }
}

fn line_clear<W: OccupancyQuery + ?Sized>(w: &W, sp: &mut Space, a: [f32; 3], b: [f32; 3]) -> bool {
    let steps = ((dist(a, b) / (0.5 * sp.lat.res)).ceil() as usize).max(1);
    (0..=steps).all(
        |i| match sp.lat.cell_of(lerp(a, b, i as f32 / steps as f32)) {
            Some(c) => sp.base(w, c) != BLOCKED,
            None => false,
        },
    )
}

/// True when `a` and `b` are the same way round the obstacles.
pub(crate) fn equivalent<W: OccupancyQuery + ?Sized>(
    w: &W,
    sp: &mut Space,
    a: &[[f32; 3]],
    b: &[[f32; 3]],
    scratch: &mut (Vec<[f32; 3]>, Vec<[f32; 3]>),
) -> bool {
    resample(a, SAMPLES, &mut scratch.0);
    resample(b, SAMPLES, &mut scratch.1);
    (0..SAMPLES).all(|i| line_clear(w, sp, scratch.0[i], scratch.1[i]))
}
