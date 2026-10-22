//! Constant-velocity box Kalman filter.

use ados_protocol::framebus::BoundingBox;

use super::scoring::center;
use super::TrackerConfig;

/// A constant-velocity Kalman filter over a bounding box.
///
/// State is `[cx, cy, w, h, vx, vy, vw, vh]`, the box center, size, and their
/// per-frame velocities. Each of the four box components is an independent
/// 1-D constant-velocity sub-filter (position + velocity), which keeps the math
/// small, allocation-free, and exact without a matrix library while giving the
/// same predict/update behaviour as the full 8-state form for this layout.
#[derive(Debug, Clone)]
pub(super) struct BoxKalman {
    /// Position estimate per component: cx, cy, w, h.
    pub(super) pos: [f32; 4],
    /// Velocity estimate per component.
    pub(super) vel: [f32; 4],
    /// Estimate covariance per component: [[p_pos, p_pv], [p_pv, p_vel]].
    pub(super) p_pos: [f32; 4],
    pub(super) p_vel: [f32; 4],
    pub(super) p_pv: [f32; 4],
    pub(super) measurement_noise: f32,
    pub(super) process_noise: f32,
    pub(super) max_velocity: f32,
}

impl BoxKalman {
    pub(super) fn new(b: &BoundingBox, cfg: &TrackerConfig) -> Self {
        let (cx, cy) = center(b);
        Self {
            pos: [cx, cy, b.width, b.height],
            vel: [0.0; 4],
            // Start moderately uncertain in position, very uncertain in velocity
            // (we have not observed motion yet).
            p_pos: [cfg.measurement_noise; 4],
            p_vel: [1_000.0; 4],
            p_pv: [0.0; 4],
            measurement_noise: cfg.measurement_noise,
            process_noise: cfg.process_noise,
            max_velocity: cfg.max_velocity,
        }
    }

    /// Advance the state by one frame (dt is folded into the tuned process noise
    /// for a fixed-rate stream; we treat one advance as one unit step).
    pub(super) fn predict(&mut self) {
        let q = self.process_noise;
        for i in 0..4 {
            // x = F x : pos += vel, vel unchanged.
            self.pos[i] += self.vel[i];
            // P = F P F^T + Q for the 2x2 constant-velocity block.
            let pp = self.p_pos[i];
            let pv = self.p_vel[i];
            let ppv = self.p_pv[i];
            self.p_pos[i] = pp + 2.0 * ppv + pv + q;
            self.p_pv[i] = ppv + pv;
            self.p_vel[i] = pv + q;
        }
    }

    /// Fuse a measured box into the state (the standard scalar Kalman update on
    /// each component's position observation).
    ///
    /// The velocity correction from a single update is clamped to
    /// `max_velocity`: a badly jittered box can otherwise inject an enormous
    /// velocity (the initial velocity covariance is large, so the gain on
    /// velocity is near 1), and that poisoned velocity overshoots the next
    /// prediction and shakes the lock loose. Bounding the learned velocity per
    /// step is what lets the filter hold one id through half-box-width jitter.
    pub(super) fn update(&mut self, b: &BoundingBox) {
        let (cx, cy) = center(b);
        let meas = [cx, cy, b.width, b.height];
        let r = self.measurement_noise;
        let vmax = self.max_velocity;
        for (i, &m) in meas.iter().enumerate() {
            let s = self.p_pos[i] + r; // innovation covariance
            let k_pos = self.p_pos[i] / s; // Kalman gain, position
            let k_vel = self.p_pv[i] / s; // Kalman gain, velocity
            let innov = m - self.pos[i];
            self.pos[i] += k_pos * innov;
            // Clamp the velocity the filter is allowed to learn from one update.
            let new_vel = (self.vel[i] + k_vel * innov).clamp(-vmax, vmax);
            self.vel[i] = new_vel;
            // P = (I - K H) P
            let pp = self.p_pos[i];
            let ppv = self.p_pv[i];
            self.p_pos[i] = pp - k_pos * pp;
            self.p_pv[i] = ppv - k_pos * ppv;
            self.p_vel[i] -= k_vel * ppv;
        }
    }

    /// Position standard deviation of the center (mean of cx, cy components).
    /// Used to widen the association gate as the estimate gets less certain
    /// (during a coast the covariance grows, so the gate should too).
    pub(super) fn center_pos_std(&self) -> f32 {
        let var = (self.p_pos[0] + self.p_pos[1]) * 0.5;
        var.max(0.0).sqrt()
    }

    /// The current box estimate (center+size back to top-left x/y).
    pub(super) fn bbox(&self) -> BoundingBox {
        let w = self.pos[2].max(1.0);
        let h = self.pos[3].max(1.0);
        BoundingBox {
            x: self.pos[0] - w / 2.0,
            y: self.pos[1] - h / 2.0,
            width: w,
            height: h,
        }
    }
}
