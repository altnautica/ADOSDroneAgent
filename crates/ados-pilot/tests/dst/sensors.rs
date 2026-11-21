//! Obstacle sensor tiers and moving walkers for the benchmark.
//!
//! The pilot today takes one obstacle input, the planar `body_frd` scan of
//! 360 one-degree bins. The planar tier is [`crate::sim::Sim::stand_scan`].
//! The 3D tiers ([`SensorTier::Mid360`], [`SensorTier::Depth`],
//! [`SensorTier::CameraOnly`]) cast their own rays against the trunks, the
//! walkers and the ground, and are then flattened the way Navigation
//! flattens a 3D cloud for the pilot (`lidar-slam` `nav/frames.rs`): each
//! return in the body frame, those more than [`BAND_TOP_M`] over the sensor
//! plane left out, the nearest horizontal range per bin. A bin with no return
//! is 0, which the pilot reads as free to its 12 m horizon. Outside a
//! camera's field of view that is space nobody saw: these tiers are the
//! planar equivalent of their sensor until the pilot has a 3D world model,
//! and score as such.

use serde_json::{json, Value};

use crate::sim::Sim;

/// The band over the sensor plane a flattened return is kept in, m: the
/// airframe's top plus 0.3 m (Navigation's `ados_livemap::BAND_TOP_M`).
pub const BAND_TOP_M: f32 = 0.33;
/// A trunk's height, m: over any flight height the benchmark flies.
const TRUNK_TOP_M: f32 = 30.0;
/// A walker's radius and height, m.
pub const WALKER_R_M: f32 = 0.3;
pub const WALKER_H_M: f32 = 1.8;

/// Which obstacle sensor a run flies with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorTier {
    /// A planar lidar fixed to the frame (RPLIDAR C1 class, 12 m).
    Planar,
    /// A Livox Mid-360: rosette 360° × −7..52°, 40 m, flattened.
    Mid360,
    /// A forward stereo/ToF depth camera, 87° × 58°, 0.3-10 m, flattened.
    Depth,
    /// A camera with learned metric depth: the depth camera's field of view
    /// at 0.6 of its reach and twice its noise, flattened.
    CameraOnly,
}

impl SensorTier {
    pub const ALL: [SensorTier; 4] = [Self::Planar, Self::Mid360, Self::Depth, Self::CameraOnly];

    pub fn name(self) -> &'static str {
        match self {
            Self::Planar => "planar",
            Self::Mid360 => "mid360",
            Self::Depth => "depth",
            Self::CameraOnly => "camera",
        }
    }
}

/// A pedestrian: an upright cylinder ([`WALKER_R_M`], [`WALKER_H_M`]) walking
/// `path` back and forth at `speed` from `start_ms`, standing at the
/// path's start before.
#[derive(Debug, Clone)]
pub struct Walker {
    pub path: Vec<[f32; 2]>,
    pub speed: f32,
    pub start_ms: u64,
}

impl Walker {
    /// Where the walker stands at `now_ms`.
    pub fn at(&self, now_ms: u64) -> [f32; 2] {
        let len: f32 = self
            .path
            .windows(2)
            .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
            .sum();
        if len <= 0.0 {
            return self.path[0];
        }
        let t = now_ms.saturating_sub(self.start_ms) as f32 / 1000.0;
        let mut s = (t * self.speed) % (2.0 * len);
        if s > len {
            s = 2.0 * len - s;
        }
        for w in self.path.windows(2) {
            let d = (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]);
            if s <= d {
                let k = s / d.max(1e-6);
                return [
                    w[0][0] + (w[1][0] - w[0][0]) * k,
                    w[0][1] + (w[1][1] - w[0][1]) * k,
                ];
            }
            s -= d;
        }
        *self.path.last().unwrap()
    }
}

/// An upright cylinder from the ground to `top`.
#[derive(Clone, Copy)]
struct Cyl {
    c: [f32; 2],
    r: f32,
    top: f32,
}

/// The first hit of the ray `o + t·d` (`d` unit, NED with `o[2]`, `d[2]`
/// as height up) on the cylinders within `max_m`. A ray that meets the
/// ground first returns nothing: the flattening drops the ground band, as
/// the pilot cuts it at the downward rangefinder's height.
fn cast(o: [f32; 3], d: [f32; 3], cyls: &[Cyl], max_m: f32) -> Option<f32> {
    let mut best = max_m;
    let mut ground = false;
    if d[2] < -1e-6 && o[2] / -d[2] < best {
        best = o[2] / -d[2];
        ground = true;
    }
    let hz2 = d[0] * d[0] + d[1] * d[1];
    if hz2 > 1e-9 {
        for c in cyls {
            let rel = [o[0] - c.c[0], o[1] - c.c[1]];
            let b = rel[0] * d[0] + rel[1] * d[1];
            let cc = rel[0] * rel[0] + rel[1] * rel[1] - c.r * c.r;
            let disc = b * b - hz2 * cc;
            if disc < 0.0 {
                continue;
            }
            let t = if cc <= 0.0 {
                0.0
            } else {
                (-b - disc.sqrt()) / hz2
            };
            if t < 0.0 || t >= best {
                continue;
            }
            let z = o[2] + d[2] * t;
            if (0.0..=c.top).contains(&z) {
                best = t;
                ground = false;
            }
        }
    }
    (best < max_m && !ground).then_some(best)
}

/// A uniform draw in [0, 1) from a hash of `k`: the noise of the 3D tiers,
/// replayed exactly by a seed without touching the run's own streams.
fn hash_unit(k: u64) -> f32 {
    let mut x = k.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 29;
    (x >> 40) as f32 / (1u64 << 24) as f32
}

/// A standard normal draw from two hashed uniforms.
fn hash_normal(k: u64) -> f32 {
    let u1 = hash_unit(k).max(1e-7);
    let u2 = hash_unit(k ^ 0xA5A5_A5A5);
    (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
}

/// Rotate a body FRD vector to world NED by roll, pitch, yaw.
fn body_to_world(v: [f32; 3], att: [f32; 3]) -> [f32; 3] {
    let (sr, cr) = att[0].sin_cos();
    let (sp, cp) = att[1].sin_cos();
    let (sy, cy) = att[2].sin_cos();
    let (x, y, z) = (v[0], v[1], v[2]);
    // R = Rz(yaw) · Ry(pitch) · Rx(roll).
    let (y1, z1) = (y * cr - z * sr, y * sr + z * cr);
    let (x2, z2) = (x * cp + z1 * sp, -x * sp + z1 * cp);
    [x2 * cy - y1 * sy, x2 * sy + y1 * cy, z2]
}

/// The rays one 0.1 s Mid-360 frame casts (20 k a frame on the sensor,
/// down-sampled for speed).
const MID360_RAYS: usize = 4_000;
const MID360_RANGE_M: f32 = 40.0;
const MID360_EL_DEG: (f32, f32) = (-7.0, 52.0);
const DEPTH_FOV_DEG: (f32, f32) = (87.0, 58.0);
const DEPTH_PX: (usize, usize) = (160, 120);
const DEPTH_RANGE_M: (f32, f32) = (0.3, 10.0);
/// The camera tier's reach and depth noise against the depth camera's.
const CAMERA_REACH: f32 = 0.6;
const CAMERA_NOISE: f32 = 2.0;
/// A camera bin's range is this quantile of its pixels' ranges: a depth
/// front end's outlier rejection. The minimum of a hundred noisy pixels
/// sits metres short of the surface. A lidar bin keeps its nearest return,
/// as Navigation's flattening does.
const CAMERA_BIN_QUANTILE: f32 = 0.1;

impl Sim {
    /// The cylinders the 3D tiers and the planar scan see now: trunks and
    /// walkers.
    fn cylinders(&self, reach_m: f32) -> Vec<Cyl> {
        let t = self.fc.truth();
        let near = |c: [f32; 2], r: f32| (c[0] - t[0]).hypot(c[1] - t[1]) - r <= reach_m;
        let mut v: Vec<Cyl> = self
            .stand
            .iter()
            .filter(|(c, r)| near(*c, *r))
            .map(|&(c, r)| Cyl {
                c,
                r,
                top: TRUNK_TOP_M,
            })
            .collect();
        v.extend(self.walkers.iter().map(|w| Cyl {
            c: w.at(self.now),
            r: WALKER_R_M,
            top: WALKER_H_M,
        }));
        v
    }

    /// The nearest walker surface to the vehicle while it is low enough to
    /// meet one, m (infinite otherwise or with no walkers).
    pub fn walker_clearance_m(&self) -> f32 {
        let t = self.fc.truth();
        if -t[2] > WALKER_H_M + 0.25 {
            return f32::INFINITY;
        }
        self.walkers
            .iter()
            .map(|w| {
                let c = w.at(self.now);
                (c[0] - t[0]).hypot(c[1] - t[1]) - WALKER_R_M
            })
            .fold(f32::INFINITY, f32::min)
    }

    /// The scan the pilot gets on the 3D tier `tier`: its rays cast and
    /// flattened to 360 body FRD bins (see the module doc).
    pub fn flat_scan(&self, tier: SensorTier) -> Value {
        let t = self.fc.truth();
        let o = [t[0], t[1], -t[2]];
        let att = self.fc.att;
        let mut hits: Vec<Vec<f32>> = vec![Vec::new(); 360];
        let frame = self.now / 100;
        let mut put = |body: [f32; 3], r: f32| {
            // `body` is FRD: up is −z.
            let (x, y) = (body[0] * r, body[1] * r);
            if -body[2] * r > BAND_TOP_M {
                return;
            }
            let h = x.hypot(y);
            if h <= 0.05 {
                return;
            }
            let az = ados_pilot::geo::atan2(y, x).rem_euclid(std::f32::consts::TAU);
            hits[(az.to_degrees() as usize).min(359)].push(h);
        };
        let world = |b: [f32; 3]| {
            let w = body_to_world(b, att);
            [w[0], w[1], -w[2]]
        };
        match tier {
            SensorTier::Planar => unreachable!("the planar tier is Sim::stand_scan"),
            SensorTier::Mid360 => {
                let cyls = self.cylinders(MID360_RANGE_M);
                let (lo, hi) = MID360_EL_DEG;
                for i in 0..MID360_RAYS {
                    // A rosette's coverage, drawn as a low-discrepancy
                    // sequence that moves on every frame.
                    let k = frame * MID360_RAYS as u64 + i as u64;
                    let az = (k as f32 * 0.618_034).fract() * std::f32::consts::TAU;
                    let el = (lo + (hi - lo) * (k as f32 * 0.754_878).fract()).to_radians();
                    let b = [el.cos() * az.cos(), el.cos() * az.sin(), -el.sin()];
                    if let Some(r) = cast(o, world(b), &cyls, MID360_RANGE_M) {
                        put(b, r + 0.02 * hash_normal(k));
                    }
                }
            }
            SensorTier::Depth | SensorTier::CameraOnly => {
                let cam = tier == SensorTier::CameraOnly;
                let (near_m, far_m) = DEPTH_RANGE_M;
                let far_m = if cam { far_m * CAMERA_REACH } else { far_m };
                let noise = if cam { 0.01 * CAMERA_NOISE } else { 0.01 };
                let cyls = self.cylinders(far_m / (DEPTH_FOV_DEG.0 / 2.0).to_radians().cos());
                let fx = (DEPTH_PX.0 as f32 / 2.0) / (DEPTH_FOV_DEG.0 / 2.0).to_radians().tan();
                let fy = (DEPTH_PX.1 as f32 / 2.0) / (DEPTH_FOV_DEG.1 / 2.0).to_radians().tan();
                for v in 0..DEPTH_PX.1 {
                    for u in 0..DEPTH_PX.0 {
                        // Pixel ray in the body frame: forward x, right y, down z.
                        let px = [
                            1.0,
                            (u as f32 + 0.5 - DEPTH_PX.0 as f32 / 2.0) / fx,
                            (v as f32 + 0.5 - DEPTH_PX.1 as f32 / 2.0) / fy,
                        ];
                        let n = (px[0] * px[0] + px[1] * px[1] + px[2] * px[2]).sqrt();
                        let b = [px[0] / n, px[1] / n, px[2] / n];
                        let Some(r) = cast(o, world(b), &cyls, far_m / b[0]) else {
                            continue;
                        };
                        let z = r * b[0];
                        if z < near_m || z > far_m {
                            continue;
                        }
                        let k = (frame << 16) ^ ((v * DEPTH_PX.0 + u) as u64);
                        let z = z + noise * z * z * hash_normal(k);
                        put(b, z / b[0]);
                    }
                }
            }
        }
        let quantile = if tier == SensorTier::Mid360 {
            0.0
        } else {
            CAMERA_BIN_QUANTILE
        };
        let bins: Vec<f32> = hits
            .iter_mut()
            .map(|h| {
                if h.is_empty() {
                    return 0.0;
                }
                h.sort_by(f32::total_cmp);
                h[(h.len() as f32 * quantile) as usize]
            })
            .collect();
        json!({"frame": "body_frd", "angle_start_rad": 0.0,
            "angle_increment_rad": std::f32::consts::TAU / 360.0, "ranges_m": bins})
    }
}

#[test]
fn dst_bench_sensors_see_a_trunk_ahead_and_a_walker() {
    for tier in SensorTier::ALL {
        let mut s = Sim::new("sensors", 1, |_| {});
        s.tier = tier;
        s.fc.pos = [0.0, 0.0, -1.5];
        s.stand.push(([5.0, 0.0], 0.2));
        s.walkers.push(Walker {
            path: vec![[3.0, 0.0], [3.0, 0.0]],
            speed: 0.0,
            start_ms: 0,
        });
        let scan = if tier == SensorTier::Planar {
            s.stand_scan()
        } else {
            s.flat_scan(tier)
        };
        let r = scan["ranges_m"][0].as_f64().unwrap() as f32;
        // The walker's face is 2.7 m out.
        assert!((r - 2.7).abs() < 0.3, "{}: ahead {r}", tier.name());
        // Overhead and behind: a camera sees nothing behind.
        let back = scan["ranges_m"][180].as_f64().unwrap();
        assert_eq!(back, 0.0, "{}", tier.name());
    }
}
