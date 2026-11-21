//! Ego-motion compensation: re-express every track in the new frame's pixels
//! when the camera itself moved between frames.

use super::MultiObjectTracker;

/// A 3 x 3 image homography, row-major, mapping a pixel of the previous frame
/// to the same line of sight in the next one. An affine map is a homography
/// with a last row of `[0, 0, 1]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Homography(pub [f32; 9]);

impl Homography {
    pub const IDENTITY: Homography = Homography([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);

    /// A pure image shift (a small camera pan or tilt).
    pub fn translation(dx: f32, dy: f32) -> Self {
        Homography([1.0, 0.0, dx, 0.0, 1.0, dy, 0.0, 0.0, 1.0])
    }

    /// Where `(x, y)` lands; `None` for a point mapped to infinity.
    pub fn map(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let m = &self.0;
        let w = m[6] * x + m[7] * y + m[8];
        if w.abs() < 1e-9 {
            return None;
        }
        Some((
            (m[0] * x + m[1] * y + m[2]) / w,
            (m[3] * x + m[4] * y + m[5]) / w,
        ))
    }
}

impl MultiObjectTracker {
    /// The camera moved since the last update: `map` takes a pixel of the
    /// previous frame to where the same line of sight lands in the next one
    /// (`None` when it leaves the image plane). Every track's estimate, last
    /// observation and recent path move with it, so prediction and
    /// association see only the objects' own motion. Velocities are left as
    /// they are. Call before [`Self::update`] on the new frame.
    pub fn warp(&mut self, map: impl Fn(f32, f32) -> Option<(f32, f32)>) {
        for tr in &mut self.tracks {
            tr.warp(&map);
        }
    }

    /// [`Self::warp`] by a homography.
    pub fn warp_homography(&mut self, h: &Homography) {
        self.warp(|x, y| h.map(x, y));
    }
}
