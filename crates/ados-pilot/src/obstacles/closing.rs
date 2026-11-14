//! The scans' own closing speed toward one world sector: the range rate of its
//! nearest return, trusted only while that return behaves like one surface.

/// Scans a sector's closing speed is fitted over: 0.4 s of the 10 Hz planar
/// lidar.
pub const CLOSING_SCANS: usize = 5;
/// Fewest consecutive scans with a return in the sector that measure a
/// closing speed.
pub const CLOSING_MIN_SCANS: usize = 4;
/// Largest RMS distance, metres, of a sector's nearest ranges from the line
/// fitted through them for the line's slope to count as a closing speed. A
/// surface the vehicle moves toward (a wall, a column, a trunk) stays well
/// inside it: the lidar is good to ±30 mm 3σ, and braking at 3 m/s² bends
/// 0.4 s of ranges by under 2 cm. Leaf returns in a crown, a beam grazing an
/// edge, rain or dust land at a new depth every scan (0.5-2.5 m apart), lie on
/// no line, and measure nothing.
pub const CLOSING_FIT_RMS_M: f32 = 0.08;
/// Fastest closing speed a fit may measure, m/s: above the fastest
/// multirotor profile (14 m/s). A sector whose nearest range falls faster
/// than this has a new nearest surface (a post passing in front of a far
/// wall, a dropout recovering), not motion, and measures nothing.
pub const SCAN_CLOSING_MAX_MPS: f32 = 16.0;

/// The last [`CLOSING_SCANS`] nearest ranges of one sector, oldest first.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct RangeTrack {
    ms: [u64; CLOSING_SCANS],
    range_m: [f32; CLOSING_SCANS],
    len: usize,
}

impl RangeTrack {
    /// Add one scan's nearest range (infinite: no return, which ends the
    /// track: the next return may be another surface).
    pub(super) fn push(&mut self, ms: u64, range_m: f32) {
        if !range_m.is_finite() {
            self.len = 0;
            return;
        }
        if self.len == CLOSING_SCANS {
            self.ms.copy_within(1.., 0);
            self.range_m.copy_within(1.., 0);
            self.len -= 1;
        }
        self.ms[self.len] = ms;
        self.range_m[self.len] = range_m;
        self.len += 1;
    }

    pub(super) fn clear(&mut self) {
        self.len = 0;
    }

    /// Closing speed, m/s (≥ 0): minus the slope of the least-squares line
    /// through the tracked ranges, when they lie on it
    /// ([`CLOSING_FIT_RMS_M`]); zero otherwise.
    pub(super) fn closing(&self) -> f32 {
        let n = self.len;
        if n < CLOSING_MIN_SCANS {
            return 0.0;
        }
        let newest = self.ms[n - 1];
        let t = |i: usize| -(newest.saturating_sub(self.ms[i]) as f32) / 1000.0;
        let r = &self.range_m[..n];
        let t_mean = (0..n).map(t).sum::<f32>() / n as f32;
        let r_mean = r.iter().sum::<f32>() / n as f32;
        let (mut sxx, mut sxy) = (0.0f32, 0.0f32);
        for (i, ri) in r.iter().enumerate() {
            let dt = t(i) - t_mean;
            sxx += dt * dt;
            sxy += dt * (ri - r_mean);
        }
        if sxx < 1e-6 {
            return 0.0;
        }
        let slope = sxy / sxx;
        let ssr: f32 = r
            .iter()
            .enumerate()
            .map(|(i, ri)| (ri - r_mean - slope * (t(i) - t_mean)).powi(2))
            .sum();
        let closing = -slope;
        if ssr > n as f32 * CLOSING_FIT_RMS_M * CLOSING_FIT_RMS_M || closing > SCAN_CLOSING_MAX_MPS
        {
            return 0.0;
        }
        closing.max(0.0)
    }
}
