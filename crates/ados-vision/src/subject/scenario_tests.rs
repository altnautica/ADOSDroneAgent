//! Synthetic walkers seen from a hovering camera: range accuracy with and
//! without depth, the motion model the filter picks, prediction accuracy,
//! an occlusion, and the per-update cost.

use std::time::Instant;

use super::*;

/// A small deterministic generator (xorshift64*), normal by Box-Muller.
pub(super) struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        ((self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    pub fn normal(&mut self) -> f64 {
        let (a, b) = (self.unit(), self.unit());
        (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
    }
}

/// How the camera sees the walker.
#[derive(Clone, Copy)]
pub(super) struct Eye {
    pub camera: [f64; 3],
    /// Box edge jitter, radians.
    pub edge_sigma: f64,
    /// Relative sigma of metric depth, when the frame has it.
    pub depth_rel: Option<f64>,
    /// Depth arrives every this many frames.
    pub depth_every: usize,
    pub ground: Option<f64>,
}

impl Eye {
    pub fn box_only(camera: [f64; 3]) -> Self {
        Self {
            camera,
            edge_sigma: 0.003,
            depth_rel: None,
            depth_every: 2,
            ground: None,
        }
    }

    pub fn with_depth(camera: [f64; 3]) -> Self {
        Self {
            depth_rel: Some(0.04),
            ..Self::box_only(camera)
        }
    }

    /// A line of sight to `p`, turned by `d_el` down and `d_az` right.
    fn ray(&self, p: [f64; 3], d_el: f64, d_az: f64) -> [f32; 3] {
        let v = [
            p[0] - self.camera[0],
            p[1] - self.camera[1],
            p[2] - self.camera[2],
        ];
        let h = v[0].hypot(v[1]);
        let el = v[2].atan2(h) + d_el;
        let az = v[1].atan2(v[0]) + d_az;
        [
            (el.cos() * az.cos()) as f32,
            (el.cos() * az.sin()) as f32,
            el.sin() as f32,
        ]
    }

    /// The sighting of a walker of height `h` with feet at `feet`, frame `k`.
    pub fn sight(&self, k: usize, t: f64, feet: [f64; 3], h: f64, rng: &mut Rng) -> Sighting {
        let az = self.edge_sigma * rng.normal();
        let top = [feet[0], feet[1], feet[2] - h];
        let depth = self
            .depth_rel
            .filter(|_| k.is_multiple_of(self.depth_every))
            .map(|rel| {
                let d = (feet[0] - self.camera[0]).hypot(feet[1] - self.camera[1]);
                DepthCue {
                    horizontal_m: (d * (1.0 + rel * rng.normal())) as f32,
                    sigma_m: (d * rel) as f32,
                }
            });
        Sighting {
            stamp_ns: (t * 1e9).round() as u64,
            camera_ned: self.camera.map(|v| v as f32),
            top_ray: self.ray(top, self.edge_sigma * rng.normal(), az),
            foot_ray: self.ray(feet, self.edge_sigma * rng.normal(), az),
            full_height: true,
            edge_sigma_rad: self.edge_sigma as f32,
            depth,
            ground: self.ground.map(|g| GroundCue {
                down_m: g as f32,
                sigma_m: 0.1,
            }),
        }
    }
}

/// One frame of a run: the truth and the published state.
pub(super) struct Step {
    pub t: f64,
    pub truth: [f64; 3],
    pub state: Option<SubjectState>,
    pub outcome: Option<Outcome>,
}

/// Run a walker along `path` (feet position at a time) for `duration`
/// seconds at 10 Hz; frames inside `hidden` are coasted.
pub(super) fn run(
    eye: &Eye,
    path: impl Fn(f64) -> [f64; 3],
    height: f64,
    duration: f64,
    hidden: (f64, f64),
    seed: u64,
) -> Vec<Step> {
    let mut rng = Rng::new(seed);
    let mut est = SubjectEstimator::new(1, SubjectConfig::default());
    let frames = (duration * 10.0).round() as usize;
    (0..=frames)
        .map(|k| {
            let t = k as f64 * 0.1;
            let truth = path(t);
            let outcome = if t >= hidden.0 && t < hidden.1 {
                est.coast((t * 1e9).round() as u64);
                None
            } else {
                Some(est.observe(&eye.sight(k, t, truth, height, &mut rng)))
            };
            Step {
                t,
                truth,
                state: est.state("main", &OpenSpace),
                outcome,
            }
        })
        .collect()
}

pub(super) fn p95(mut v: Vec<f64>) -> f64 {
    assert!(!v.is_empty());
    v.sort_by(f64::total_cmp);
    v[((v.len() as f64 * 0.95).ceil() as usize).min(v.len()) - 1]
}

/// The horizontal-range error of every state after `from` seconds.
fn range_errors(eye: &Eye, steps: &[Step], from: f64) -> Vec<f64> {
    let c = eye.camera;
    steps
        .iter()
        .filter(|s| s.t >= from)
        .map(|s| {
            let st = s.state.as_ref().expect("tracking");
            let est = (f64::from(st.pos_ned[0]) - c[0]).hypot(f64::from(st.pos_ned[1]) - c[1]);
            let truth = (s.truth[0] - c[0]).hypot(s.truth[1] - c[1]);
            (est - truth).abs()
        })
        .collect()
}

fn straight(t: f64) -> [f64; 3] {
    [7.0, -6.0 + 1.4 * t, 0.0]
}

const CAMERA: [f64; 3] = [0.0, 0.0, -5.0];
const NEVER: (f64, f64) = (f64::INFINITY, f64::INFINITY);

#[test]
fn a_straight_walker_ranges_within_0_6_m_p95_with_depth() {
    let eye = Eye::with_depth(CAMERA);
    let errors: Vec<f64> = (1..=5)
        .flat_map(|seed| range_errors(&eye, &run(&eye, straight, 1.80, 8.5, NEVER, seed), 1.0))
        .collect();
    let p = p95(errors);
    println!("range error p95 with depth, 7-9 m: {p:.3} m");
    assert!(p <= 0.6, "p95 {p:.3} m");
}

#[test]
fn a_walker_at_8_m_ranges_within_1_2_m_p95_on_the_box_alone() {
    let eye = Eye::box_only(CAMERA);
    let at8 = |t: f64| [8.0, -4.0 + 1.0 * t, 0.0];
    let errors: Vec<f64> = (1..=5)
        .flat_map(|seed| range_errors(&eye, &run(&eye, at8, 1.80, 8.0, NEVER, seed), 1.0))
        .collect();
    let p = p95(errors);
    println!("range error p95 box only, 8 m: {p:.3} m");
    assert!(p <= 1.2, "p95 {p:.3} m");
}

#[test]
fn the_one_second_prediction_of_a_straight_walker_is_within_0_8_m() {
    let eye = Eye::with_depth(CAMERA);
    let mut errors = Vec::new();
    for seed in 1..=5 {
        let steps = run(&eye, straight, 1.80, 8.5, NEVER, seed);
        for s in steps.iter().filter(|s| s.t >= 2.0 && s.t + 1.0 <= 8.5) {
            let st = s.state.as_ref().unwrap();
            let pred = st.predictions.iter().find(|p| p.dt_s == 1.0).unwrap();
            let truth = straight(s.t + 1.0);
            errors.push(
                (f64::from(pred.pos_ned[0]) - truth[0])
                    .hypot(f64::from(pred.pos_ned[1]) - truth[1]),
            );
        }
    }
    let p = p95(errors);
    println!("+1 s prediction error p95: {p:.3} m");
    assert!(p <= 0.8, "p95 {p:.3} m");
}

/// Walk north for 3 s, then round a right-hand circle of radius 5 m.
fn bend(t: f64) -> [f64; 3] {
    let v = 2.0;
    if t < 3.0 {
        return [-6.0 + v * t, -5.0, 0.0];
    }
    let a = v * (t - 3.0) / 5.0;
    [5.0 * a.sin(), -5.0 * a.cos(), 0.0]
}

fn argmax(p: &[f32; 3]) -> usize {
    (0..3).max_by(|&a, &b| p[a].total_cmp(&p[b])).unwrap()
}

#[test]
fn the_turn_model_leads_while_walking_round_a_bend() {
    let eye = Eye::with_depth([0.0, 0.0, -5.0]);
    let (mut turn, mut turn_n, mut straight_cv, mut straight_n) = (0, 0, 0, 0);
    for seed in 1..=5 {
        for s in run(&eye, bend, 1.75, 11.0, NEVER, seed) {
            let mode = argmax(&s.state.as_ref().unwrap().mode_probs);
            if s.t >= 5.0 {
                turn_n += 1;
                turn += usize::from(mode == 1);
            } else if (1.5..3.0).contains(&s.t) {
                straight_n += 1;
                straight_cv += usize::from(mode == 0);
            }
        }
    }
    let turning = turn as f64 / turn_n as f64;
    let straight_share = straight_cv as f64 / straight_n as f64;
    println!("turn picked {turning:.2} of the bend, cv {straight_share:.2} of the straight");
    assert!(turning >= 0.8, "turn model led {turning:.2} of the bend");
    assert!(
        straight_share >= 0.6,
        "cv led {straight_share:.2} of the straight"
    );
}

#[test]
fn the_stop_model_leads_once_the_walker_stands() {
    let eye = Eye::with_depth(CAMERA);
    let halt = |t: f64| straight(t.min(4.0));
    let (mut stop, mut n) = (0, 0);
    for seed in 1..=5 {
        for s in run(&eye, halt, 1.80, 8.0, NEVER, seed) {
            let st = s.state.as_ref().unwrap();
            if s.t >= 5.5 {
                n += 1;
                stop += usize::from(argmax(&st.mode_probs) == 2);
                let speed = f64::from(st.vel_ned[0]).hypot(f64::from(st.vel_ned[1]));
                assert!(speed < 0.4, "speed {speed:.2} at {:.1} s", s.t);
            }
        }
    }
    let share = stop as f64 / n as f64;
    println!("stop picked {share:.2} of the halt");
    assert!(share >= 0.9, "stop model led {share:.2} of the halt");
}

#[test]
fn a_one_second_occlusion_is_coasted_through_and_rejoined() {
    let eye = Eye::with_depth(CAMERA);
    for seed in 1..=5 {
        let steps = run(&eye, straight, 1.80, 8.5, (4.0, 5.0), seed);
        let hidden_end = steps.iter().rfind(|s| s.t < 5.0).unwrap();
        let st = hidden_end.state.as_ref().unwrap();
        assert!(st.fix_age_ms >= 900 && !st.sources.any(), "{st:?}");
        let err = (f64::from(st.pos_ned[0]) - hidden_end.truth[0])
            .hypot(f64::from(st.pos_ned[1]) - hidden_end.truth[1]);
        assert!(err <= 1.0, "seed {seed}: {err:.2} m off after 1 s hidden");
        for s in steps.iter().filter(|s| s.t >= 5.0) {
            assert_eq!(
                s.outcome,
                Some(Outcome::Fused),
                "seed {seed} at {:.1} s",
                s.t
            );
        }
        let p = p95(range_errors(&eye, &steps, 5.5));
        assert!(p <= 0.6, "seed {seed}: range p95 {p:.2} m after rejoining");
    }
}

#[test]
fn one_update_costs_under_50_us() {
    let eye = Eye::with_depth(CAMERA);
    let mut rng = Rng::new(9);
    let sightings: Vec<_> = (0..2_000)
        .map(|k| {
            let t = k as f64 * 0.1;
            eye.sight(k, t, [7.0, -6.0 + 1.4 * (t % 8.0), 0.0], 1.8, &mut rng)
        })
        .collect();
    let mut est = SubjectEstimator::new(1, SubjectConfig::default());
    let mut times = Vec::with_capacity(sightings.len());
    for s in &sightings {
        let start = Instant::now();
        est.observe(s);
        let state = est.state("main", &OpenSpace);
        times.push(start.elapsed().as_secs_f64() * 1e6);
        assert!(state.is_some());
    }
    let warm = &times[50..];
    let mean = warm.iter().sum::<f64>() / warm.len() as f64;
    println!(
        "observe + state: mean {mean:.1} us, p95 {:.1} us",
        p95(warm.to_vec())
    );
    if !cfg!(debug_assertions) {
        assert!(mean <= 50.0, "mean {mean:.1} us");
    }
}
