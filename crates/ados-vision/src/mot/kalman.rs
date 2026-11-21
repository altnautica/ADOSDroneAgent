//! Constant-velocity Kalman filters: one over a track's image box, one over
//! its optional world position.
//!
//! Each state component is an independent position + velocity filter driven
//! by white acceleration noise. That is exact for this diagonal layout and
//! keeps every predict and update a handful of multiplies with no matrix
//! library and no allocation.

/// Image-filter components: centre x, centre y, aspect (width / height),
/// height.
const CX: usize = 0;
const CY: usize = 1;
const ASPECT: usize = 2;
const HEIGHT: usize = 3;

/// Acceleration noise of the aspect ratio, per second squared.
const ASPECT_ACCEL: f32 = 0.1;
/// Acceleration noise of the height, as a fraction of the height per second
/// squared.
const HEIGHT_ACCEL_H: f32 = 0.25;
/// Measurement noise of the aspect ratio.
const ASPECT_MEAS: f32 = 0.05;

/// A box in corner form, the shape association works on.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Box4 {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl Box4 {
    pub fn from_center(cx: f32, cy: f32, w: f32, h: f32) -> Self {
        Self {
            x1: cx - w * 0.5,
            y1: cy - h * 0.5,
            x2: cx + w * 0.5,
            y2: cy + h * 0.5,
        }
    }

    pub fn center(&self) -> (f32, f32) {
        ((self.x1 + self.x2) * 0.5, (self.y1 + self.y2) * 0.5)
    }

    pub fn width(&self) -> f32 {
        self.x2 - self.x1
    }

    pub fn height(&self) -> f32 {
        self.y2 - self.y1
    }

    /// Intersection over union; 0 for disjoint or degenerate boxes.
    pub fn iou(&self, o: &Box4) -> f32 {
        let iw = (self.x2.min(o.x2) - self.x1.max(o.x1)).max(0.0);
        let ih = (self.y2.min(o.y2) - self.y1.max(o.y1)).max(0.0);
        let inter = iw * ih;
        if inter <= 0.0 {
            return 0.0;
        }
        let union = self.width() * self.height() + o.width() * o.height() - inter;
        if union > 0.0 {
            inter / union
        } else {
            0.0
        }
    }

    /// Linear blend toward `o` by `t` in `0..=1`.
    pub fn lerp(&self, o: &Box4, t: f32) -> Box4 {
        Box4 {
            x1: self.x1 + (o.x1 - self.x1) * t,
            y1: self.y1 + (o.y1 - self.y1) * t,
            x2: self.x2 + (o.x2 - self.x2) * t,
            y2: self.y2 + (o.y2 - self.y2) * t,
        }
    }
}

/// One position + velocity filter.
#[derive(Debug, Clone, Copy, Default)]
struct Axis {
    pos: f32,
    vel: f32,
    p_pp: f32,
    p_pv: f32,
    p_vv: f32,
}

impl Axis {
    fn new(pos: f32, var_pos: f32, var_vel: f32) -> Self {
        Self {
            pos,
            vel: 0.0,
            p_pp: var_pos,
            p_pv: 0.0,
            p_vv: var_vel,
        }
    }

    /// Advance by `dt` seconds under acceleration noise of spectral density
    /// `q` (units²/s³).
    fn predict(&mut self, dt: f32, q: f32) {
        self.pos += self.vel * dt;
        let (dt2, dt3) = (dt * dt, dt * dt * dt);
        self.p_pp += 2.0 * dt * self.p_pv + dt2 * self.p_vv + q * dt3 / 3.0;
        self.p_pv += dt * self.p_vv + q * dt2 / 2.0;
        self.p_vv += q * dt;
    }

    /// Fuse a position measurement of variance `r`.
    fn update(&mut self, z: f32, r: f32) {
        let s = self.p_pp + r;
        if s <= 0.0 {
            return;
        }
        let (k_p, k_v) = (self.p_pp / s, self.p_pv / s);
        let innov = z - self.pos;
        self.pos += k_p * innov;
        self.vel += k_v * innov;
        let (pp, pv) = (self.p_pp, self.p_pv);
        self.p_pp = pp - k_p * pp;
        self.p_pv = pv - k_p * pv;
        self.p_vv -= k_v * pv;
    }
}

/// The image-space filter of one track: centre, aspect and height with their
/// velocities (per second).
#[derive(Debug, Clone, Copy)]
pub struct ImageKf {
    axes: [Axis; 4],
    accel_noise_h: f32,
    meas_noise_h: f32,
}

impl ImageKf {
    pub fn new(b: &Box4, accel_noise_h: f32, meas_noise_h: f32) -> Self {
        let (cx, cy) = b.center();
        let h = b.height().max(1.0);
        let a = (b.width() / h).clamp(0.05, 20.0);
        let r = (meas_noise_h * h).powi(2);
        let v = (2.0 * h).powi(2);
        Self {
            axes: [
                Axis::new(cx, r, v),
                Axis::new(cy, r, v),
                Axis::new(a, ASPECT_MEAS * ASPECT_MEAS, 0.04),
                Axis::new(h, r, (0.5 * h).powi(2)),
            ],
            accel_noise_h,
            meas_noise_h,
        }
    }

    fn h(&self) -> f32 {
        self.axes[HEIGHT].pos.max(1.0)
    }

    pub fn predict(&mut self, dt: f32) {
        if dt <= 0.0 {
            return;
        }
        let h = self.h();
        let q_c = (self.accel_noise_h * h).powi(2);
        self.axes[CX].predict(dt, q_c);
        self.axes[CY].predict(dt, q_c);
        self.axes[ASPECT].predict(dt, ASPECT_ACCEL * ASPECT_ACCEL);
        self.axes[HEIGHT].predict(dt, (HEIGHT_ACCEL_H * h).powi(2));
        self.axes[HEIGHT].pos = self.axes[HEIGHT].pos.max(1.0);
        self.axes[ASPECT].pos = self.axes[ASPECT].pos.clamp(0.05, 20.0);
    }

    pub fn update(&mut self, b: &Box4) {
        let (cx, cy) = b.center();
        let h = b.height().max(1.0);
        let r = (self.meas_noise_h * h).powi(2);
        self.axes[CX].update(cx, r);
        self.axes[CY].update(cy, r);
        self.axes[ASPECT].update((b.width() / h).clamp(0.05, 20.0), ASPECT_MEAS * ASPECT_MEAS);
        self.axes[HEIGHT].update(h, r);
        self.axes[HEIGHT].pos = self.axes[HEIGHT].pos.max(1.0);
        self.axes[ASPECT].pos = self.axes[ASPECT].pos.clamp(0.05, 20.0);
    }

    pub fn bbox(&self) -> Box4 {
        let h = self.h();
        Box4::from_center(
            self.axes[CX].pos,
            self.axes[CY].pos,
            self.axes[ASPECT].pos * h,
            h,
        )
    }

    /// Centre velocity, pixels per second.
    pub fn velocity(&self) -> (f32, f32) {
        (self.axes[CX].vel, self.axes[CY].vel)
    }

    /// Squared Mahalanobis distance of a centre from the predicted centre.
    pub fn center_distance2(&self, x: f32, y: f32) -> f32 {
        let r = (self.meas_noise_h * self.h()).powi(2);
        let (ax, ay) = (&self.axes[CX], &self.axes[CY]);
        (x - ax.pos).powi(2) / (ax.p_pp + r) + (y - ay.pos).powi(2) / (ay.p_pp + r)
    }

    /// Move the box to `to` (the same object seen through a camera that
    /// turned), leaving its velocities and certainty alone.
    pub fn move_to(&mut self, to: &Box4) {
        let (cx, cy) = to.center();
        let h = to.height().max(1.0);
        self.axes[CX].pos = cx;
        self.axes[CY].pos = cy;
        self.axes[HEIGHT].pos = h;
        self.axes[ASPECT].pos = (to.width() / h).clamp(0.05, 20.0);
    }
}

/// World-position measurement noise, metres.
const WORLD_MEAS_M: f64 = 0.5;
/// World acceleration noise, metres per second squared.
const WORLD_ACCEL: f64 = 2.0;

/// A constant-velocity filter over a track's world position, in whatever
/// frame the detections carry (local metres).
#[derive(Debug, Clone, Copy)]
pub struct WorldKf {
    pos: [f64; 3],
    vel: [f64; 3],
    p: [[f64; 3]; 3],
}

impl WorldKf {
    pub fn new(p: [f64; 3]) -> Self {
        let r = WORLD_MEAS_M * WORLD_MEAS_M;
        Self {
            pos: p,
            vel: [0.0; 3],
            p: [[r, 0.0, 25.0]; 3],
        }
    }

    pub fn predict(&mut self, dt: f64) {
        if dt <= 0.0 {
            return;
        }
        let q = WORLD_ACCEL * WORLD_ACCEL;
        for i in 0..3 {
            self.pos[i] += self.vel[i] * dt;
            let [pp, pv, vv] = self.p[i];
            self.p[i] = [
                pp + 2.0 * dt * pv + dt * dt * vv + q * dt.powi(3) / 3.0,
                pv + dt * vv + q * dt * dt / 2.0,
                vv + q * dt,
            ];
        }
    }

    pub fn update(&mut self, z: [f64; 3]) {
        let r = WORLD_MEAS_M * WORLD_MEAS_M;
        for (i, &zi) in z.iter().enumerate() {
            let [pp, pv, vv] = self.p[i];
            let s = pp + r;
            let (kp, kv) = (pp / s, pv / s);
            let innov = zi - self.pos[i];
            self.pos[i] += kp * innov;
            self.vel[i] += kv * innov;
            self.p[i] = [pp - kp * pp, pv - kp * pv, vv - kv * pv];
        }
    }

    pub fn position(&self) -> [f64; 3] {
        self.pos
    }

    pub fn velocity(&self) -> [f64; 3] {
        self.vel
    }
}
