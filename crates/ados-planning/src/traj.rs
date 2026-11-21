//! Piecewise-quintic trajectories.

/// One polynomial piece: `p(t) = Σ c[k] t^k` for local time `t ∈ [0, dur]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub dur: f64,
    /// `c[k][axis]`.
    pub c: [[f64; 3]; 6],
}

impl Segment {
    /// Derivative `order` (0 position … 3 jerk) at local time `t`.
    pub fn eval(&self, t: f64, order: usize) -> [f64; 3] {
        let mut out = [0.0; 3];
        let mut tp = 1.0;
        for k in order..6 {
            let mut f = tp;
            for m in 0..order {
                f *= (k - m) as f64;
            }
            for (o, c) in out.iter_mut().zip(self.c[k]) {
                *o += f * c;
            }
            tp *= t;
        }
        out
    }

    /// The same motion started `t0` seconds later: `p'(s) = p(s + t0)`.
    fn shifted(&self, t0: f64) -> Segment {
        let mut c = [[0.0; 3]; 6];
        for (k, ck) in c.iter_mut().enumerate() {
            // Σ_{i ≥ k} c_i · C(i, k) · t0^(i−k)
            let mut binom = 1.0;
            let mut tp = 1.0;
            for i in k..6 {
                for a in 0..3 {
                    ck[a] += self.c[i][a] * binom * tp;
                }
                binom = binom * (i + 1) as f64 / (i + 1 - k) as f64;
                tp *= t0;
            }
        }
        Segment {
            dur: (self.dur - t0).max(0.0),
            c,
        }
    }
}

/// A motion through time from `t = 0` to [`Trajectory::duration`]; sampling
/// clamps to that range.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Trajectory {
    segments: Vec<Segment>,
    duration: f64,
}

fn f32x3(v: [f64; 3]) -> [f32; 3] {
    [v[0] as f32, v[1] as f32, v[2] as f32]
}

impl Trajectory {
    pub fn new(segments: Vec<Segment>) -> Self {
        let duration = segments.iter().map(|s| s.dur).sum();
        Self { segments, duration }
    }

    /// Hold `p` (zero length, at rest).
    pub fn stationary(p: [f32; 3]) -> Self {
        let mut c = [[0.0; 3]; 6];
        c[0] = [p[0] as f64, p[1] as f64, p[2] as f64];
        Self::new(vec![Segment { dur: 0.0, c }])
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn duration(&self) -> f32 {
        self.duration as f32
    }

    fn locate(&self, t: f64) -> (&Segment, f64) {
        let mut t = t.clamp(0.0, self.duration);
        let last = self.segments.len() - 1;
        for (i, s) in self.segments.iter().enumerate() {
            if t <= s.dur || i == last {
                return (s, t.min(s.dur));
            }
            t -= s.dur;
        }
        unreachable!("a trajectory always has a segment")
    }

    /// Derivative `order` (0 position … 3 jerk) at time `t`, f64.
    pub fn eval(&self, t: f64, order: usize) -> [f64; 3] {
        let (s, lt) = self.locate(t);
        s.eval(lt, order)
    }

    /// Position, velocity and acceleration at time `t` seconds.
    pub fn sample(&self, t: f32) -> ([f32; 3], [f32; 3], [f32; 3]) {
        let (s, lt) = self.locate(t as f64);
        (
            f32x3(s.eval(lt, 0)),
            f32x3(s.eval(lt, 1)),
            f32x3(s.eval(lt, 2)),
        )
    }

    pub fn jerk(&self, t: f32) -> [f32; 3] {
        f32x3(self.eval(t as f64, 3))
    }

    pub fn start(&self) -> [f32; 3] {
        self.sample(0.0).0
    }

    pub fn end(&self) -> [f32; 3] {
        self.sample(self.duration()).0
    }

    /// The motion from `t0` on, re-timed to start at zero.
    pub fn tail(&self, t0: f32) -> Trajectory {
        let mut t = (t0 as f64).clamp(0.0, self.duration);
        let mut out = Vec::with_capacity(self.segments.len());
        for s in &self.segments {
            if !out.is_empty() {
                out.push(*s);
            } else if t > s.dur {
                t -= s.dur;
            } else {
                out.push(s.shifted(t));
            }
        }
        if out.is_empty() {
            return Trajectory::stationary(self.end());
        }
        Trajectory::new(out)
    }

    /// `n ≥ 2` positions evenly spaced in time from start to end (a shadow
    /// path or a preview polyline).
    pub fn positions(&self, n: usize) -> Vec<[f32; 3]> {
        let n = n.max(2);
        (0..n)
            .map(|i| self.sample(self.duration() * i as f32 / (n - 1) as f32).0)
            .collect()
    }
}
