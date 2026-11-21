//! Obstacle sensor tiers and moving walkers for the benchmark.
//!
//! The planar tier is [`crate::sim::Sim::stand_scan`], the planar
//! `body_frd` scan of 360 one-degree bins. The 3D tiers
//! ([`SensorTier::Mid360`], [`SensorTier::Depth`],
//! [`SensorTier::CameraOnly`]) cast their own rays against the trunks, the
//! walkers and the ground ([`Sim::tier_rays`]) into a voxel map
//! ([`crate::voxmap`]) the pilot reads as Navigation's seam M map
//! ([`Sim::feed_voxmap`]). For comparison, [`Sim::flat_scan`] flattens the
//! same rays the way Navigation flattens a 3D cloud for 2D consumers
//! (`lidar-slam` `nav/frames.rs`): each return in the body frame, those more
//! than [`BAND_TOP_M`] over the sensor plane left out, the nearest
//! horizontal range per bin, a bin with no return 0 (read as free to the
//! 12 m horizon, even outside a camera's view).

use ados_protocol::voxmap::VoxTier;
use serde_json::{json, Value};

use crate::sim::Sim;
use crate::voxmap::DstVoxmap;

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
/// as height up) on the cylinders or the ground within `max_m`: its range
/// and whether the ground returned it.
fn cast(o: [f32; 3], d: [f32; 3], cyls: &[Cyl], max_m: f32) -> Option<(f32, bool)> {
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
    (best < max_m).then_some((best, ground))
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

    /// The rays one frame of the 3D tier `tier` casts now from the vehicle,
    /// each to `f` with its return's sensor noise. A depth pixel nearer
    /// than the camera's near limit gives no ray; `depth_stride` keeps every
    /// n-th depth pixel each way.
    fn tier_rays(&self, tier: SensorTier, depth_stride: usize, mut f: impl FnMut(Ray)) {
        let t = self.fc.truth();
        let o = [t[0], t[1], -t[2]];
        let att = self.fc.att;
        let frame = self.now / 100;
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
                    // sequence that moves on every frame. In f64: past a
                    // few hundred thousand rays an f32 product keeps a
                    // handful of fractions, and the rosette collapses onto
                    // a few fixed directions.
                    let k = frame * MID360_RAYS as u64 + i as u64;
                    let az = (k as f64 * 0.618_034).fract() as f32 * std::f32::consts::TAU;
                    let el = (lo + (hi - lo) * (k as f64 * 0.754_878).fract() as f32).to_radians();
                    let body = [el.cos() * az.cos(), el.cos() * az.sin(), -el.sin()];
                    let w = world(body);
                    let ret = cast(o, w, &cyls, MID360_RANGE_M)
                        .map(|(r, ground)| (r + 0.02 * hash_normal(k), ground));
                    f(Ray {
                        body,
                        world: w,
                        ret,
                        max_m: MID360_RANGE_M,
                    });
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
                for v in (0..DEPTH_PX.1).step_by(depth_stride) {
                    for u in (0..DEPTH_PX.0).step_by(depth_stride) {
                        // Pixel ray in the body frame: forward x, right y, down z.
                        let px = [
                            1.0,
                            (u as f32 + 0.5 - DEPTH_PX.0 as f32 / 2.0) / fx,
                            (v as f32 + 0.5 - DEPTH_PX.1 as f32 / 2.0) / fy,
                        ];
                        let n = (px[0] * px[0] + px[1] * px[1] + px[2] * px[2]).sqrt();
                        let body = [px[0] / n, px[1] / n, px[2] / n];
                        let w = world(body);
                        let max_m = far_m / body[0];
                        let ret = match cast(o, w, &cyls, max_m) {
                            None => None,
                            Some((r, ground)) => {
                                let z = r * body[0];
                                if z < near_m {
                                    continue;
                                }
                                (z <= far_m).then(|| {
                                    let k = (frame << 16) ^ ((v * DEPTH_PX.0 + u) as u64);
                                    let z = z + noise * z * z * hash_normal(k);
                                    (z / body[0], ground)
                                })
                            }
                        };
                        f(Ray {
                            body,
                            world: w,
                            ret,
                            max_m,
                        });
                    }
                }
            }
        }
    }

    /// The scan Navigation's flattening of the 3D tier `tier` would give the
    /// pilot: its rays to 360 body FRD bins (see the module doc).
    pub fn flat_scan(&self, tier: SensorTier) -> Value {
        let mut hits: Vec<Vec<f32>> = vec![Vec::new(); 360];
        self.tier_rays(tier, 1, |ray| {
            // The ground band is cut, as the pilot cuts it at the downward
            // rangefinder's height.
            let Some((r, false)) = ray.ret else {
                return;
            };
            // `body` is FRD: up is −z.
            let b = ray.body;
            let (x, y) = (b[0] * r, b[1] * r);
            if -b[2] * r > BAND_TOP_M {
                return;
            }
            let h = x.hypot(y);
            if h <= 0.05 {
                return;
            }
            let az = ados_pilot::geo::atan2(y, x).rem_euclid(std::f32::consts::TAU);
            hits[(az.to_degrees() as usize).min(359)].push(h);
        });
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

    /// One frame of the 3D tier into the run's voxel map (rays with no
    /// return clear out to the sensor's range), published to the pilot as
    /// the seam M map, the body at its true pose.
    pub fn feed_voxmap(&mut self) {
        let t = self.fc.truth();
        let tier = self.tier;
        let mut map = self
            .vox
            .take()
            .unwrap_or_else(|| DstVoxmap::new(vox_tier(tier), t));
        map.recentre(t);
        let mut rays = Vec::new();
        self.tier_rays(tier, VOXMAP_DEPTH_STRIDE, |ray| {
            let (r, hit) = ray.ret.map_or((ray.max_m, false), |(r, _)| (r, true));
            let w = ray.world;
            rays.push(([t[0] + w[0] * r, t[1] + w[1] * r, t[2] - w[2] * r], hit));
        });
        map.insert(t, &rays);
        let mut snap = std::mem::take(&mut self.vox_buf);
        map.publish(self.now, body_pose(t, self.fc.att), &mut snap);
        self.p.on_voxmap(&mut snap, self.now);
        self.vox_buf = snap;
        self.vox = Some(map);
    }
}

/// One ray of a 3D tier's frame.
struct Ray {
    /// Body FRD unit direction.
    body: [f32; 3],
    /// World unit direction: north, east, up.
    world: [f32; 3],
    /// The return: its range with sensor noise, and whether the ground
    /// returned it; `None` within the sensor's range.
    ret: Option<(f32, bool)>,
    /// The sensor's range along the ray.
    max_m: f32,
}

/// Depth pixels kept each way for the voxel map: a depth front end thins a
/// 160 × 120 image before inserting it (a voxel at 5 m spans 4 pixels).
const VOXMAP_DEPTH_STRIDE: usize = 2;

/// The seam M tier of a sensor tier.
fn vox_tier(tier: SensorTier) -> VoxTier {
    match tier {
        SensorTier::Planar => VoxTier::Planar,
        SensorTier::Mid360 => VoxTier::Lidar3d,
        SensorTier::Depth => VoxTier::Depth,
        SensorTier::CameraOnly => VoxTier::Camera,
    }
}

/// The body pose in the map frame (truth NED): translation, then the
/// quaternion (w, x, y, z) of roll, pitch, yaw.
fn body_pose(t: [f32; 3], att: [f32; 3]) -> [f32; 7] {
    let (sr, cr) = (att[0] / 2.0).sin_cos();
    let (sp, cp) = (att[1] / 2.0).sin_cos();
    let (sy, cy) = (att[2] / 2.0).sin_cos();
    [
        t[0],
        t[1],
        t[2],
        cr * cp * cy + sr * sp * sy,
        sr * cp * cy - cr * sp * sy,
        cr * sp * cy + sr * cp * sy,
        cr * cp * sy - sr * sp * cy,
    ]
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

/// On every 3D tier the pilot flies on the voxel map: its sensing names the
/// map's tier, the trunk ahead is a wall at its face, and the range seen
/// clear toward it ends there.
#[test]
fn dst_bench_3d_tiers_fly_on_the_voxel_map() {
    use ados_protocol::pilot::SensingTier;
    for (tier, want) in [
        (SensorTier::Mid360, SensingTier::Lidar3d),
        (SensorTier::Depth, SensingTier::Depth),
        (SensorTier::CameraOnly, SensingTier::Camera),
    ] {
        let mut s = Sim::new("sensors_voxmap", 1, |_| {});
        s.tier = tier;
        s.stand.push(([5.0, 0.0], 0.2));
        s.boot();
        crate::scenarios::launch_to_hold(&mut s, 4.0);
        for _ in 0..50 {
            s.step();
        }
        let now = s.now;
        let sensing = s.state.sensing.expect("sensing published");
        assert_eq!(sensing.tier, want, "{}", tier.name());
        assert!(
            s.p.world.live_3d(now),
            "{}: the map is the obstacle source",
            tier.name()
        );
        let t = s.fc.truth();
        let face = 5.0 - 0.2 - t[0];
        let near = s.p.obstacles.min_raw_m(now).expect("the trunk");
        // A camera's depth noise (twice a depth camera's, 0.02·z²) puts
        // returns up to a metre short of the face at 5 m.
        let tol = if tier == SensorTier::CameraOnly {
            1.0
        } else {
            0.4
        };
        assert!(
            (near - face).abs() < tol,
            "{}: wall at {near}, face {face}",
            tier.name()
        );
        // Facing the trunk, the seen range ends in its shadow.
        assert!(
            sensing.clear_range_m <= face + 0.6,
            "{}: {sensing:?}",
            tier.name()
        );
    }
}

/// A crown over the way to the subject, in the voxel map: the pilot reports
/// the height to fly under it while the map is the obstacle source, as it
/// does from a planar lidar's overhead returns.
#[test]
fn dst_bench_voxel_map_reports_the_crown_over_the_way() {
    use ados_protocol::pilot::{Goal, GoalKind, Request};
    let mut s = Sim::new("sensors_crown", 1, |_| {});
    s.tier = SensorTier::Mid360;
    s.boot();
    crate::scenarios::launch_to_hold(&mut s, 4.0);
    let id = crate::forest::lease(&mut s);
    let t = s.fc.truth();
    // The crown's underside 2 m over the vehicle, 3-7 m north of it.
    let crown: Vec<([f32; 3], bool)> = (0..40)
        .flat_map(|i| {
            (-4..=4).map(move |j| {
                (
                    [
                        t[0] + 3.0 + i as f32 * 0.1,
                        t[1] + j as f32 * 0.1,
                        t[2] - 2.05,
                    ],
                    true,
                )
            })
        })
        .collect();
    for _ in 0..30 {
        let mut g = Goal::new(GoalKind::Follow, [t[0], t[1], t[2]], 1.0);
        g.subject_ne = Some([t[0] + 10.0, t[1]]);
        s.request(Request::Goal {
            lease_id: id,
            goal: g,
        });
        if let Some(v) = s.vox.as_mut() {
            v.insert(t, &crown);
        }
        for _ in 0..5 {
            s.step();
        }
    }
    assert!(s.p.world.live_3d(s.now), "the map is the obstacle source");
    let o = s
        .state
        .overhead_ahead
        .clone()
        .expect("a lease goal names a subject");
    let under = o.fly_under_agl_m.expect("the crown over the way");
    // The vehicle's height (4 m) plus the crown's 2 m, less the band's top,
    // within a voxel.
    assert!((under - (6.0 - 0.33)).abs() < 0.3, "{o:?}");
    let ahead = o.ahead_m.expect("how far along");
    assert!((2.5..=7.5).contains(&ahead), "{o:?}");
}
