//! A one-dimensional random-walk Kalman filter: the subject's height and the
//! height of the ground they stand on.

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Scalar {
    pub value: f64,
    pub var: f64,
}

impl Scalar {
    pub(super) fn new(value: f64, sigma: f64) -> Self {
        Self {
            value,
            var: sigma * sigma,
        }
    }

    /// Let the value wander by `rate` (one sigma per square-root second)
    /// over `dt` seconds.
    pub(super) fn drift(&mut self, rate: f64, dt: f64) {
        self.var += rate * rate * dt.max(0.0);
    }

    /// Fuse a measurement `z` of variance `r`.
    pub(super) fn update(&mut self, z: f64, r: f64) {
        if !(z.is_finite() && r > 0.0 && r.is_finite()) {
            return;
        }
        let k = self.var / (self.var + r);
        self.value += k * (z - self.value);
        self.var *= 1.0 - k;
    }

    pub(super) fn sigma(&self) -> f64 {
        self.var.sqrt()
    }
}
