//! The voxel map the 3D tiers fly on: the robot-centred log-odds map
//! Navigation builds for seam M, built here from the tiers' rays
//! ([`crate::sensors`]) and handed to the pilot as the snapshot it reads
//! from the seam M file ([`ados_pilot::core::Pilot::on_voxmap`]).
//!
//! The update rules are Navigation's: hit +0.85, miss −0.40, clamped to
//! −2.0..+3.5, occupied at +0.85, free at −0.40; one update per voxel per
//! scan, hits first; a voxel touching a ray's return takes only a near miss,
//! which never lowers positive evidence; three scans passing through a
//! voxel with positive evidence clear it (a walker that moved on); a ray
//! with no return clears out to its sensor's range. Simplified for speed: the
//! window moves by copying when the vehicle is 3 m off its centre (1 m in
//! height), occupied voxels do not decay (the decay's 10 s grace is longer
//! than a voxel stays unseen in these runs), and the distance field is
//! recomputed whole at 1 Hz.

use std::collections::VecDeque;

use ados_protocol::voxmap::{
    VoxSnapshot, VoxTier, VoxmapMeta, ESDF_CAP_DM, VOX_FREE, VOX_OCCUPIED, VOX_UNKNOWN,
};

/// Voxel edge, m, and the window: 30 × 30 × 6 m.
pub const RES_M: f32 = 0.15;
pub const DIMS: [usize; 3] = [200, 200, 40];
/// The window moves once the vehicle is this many voxels off its centre:
/// 3 m across, 1.05 m in height.
const RECENTRE_VOX: [i32; 3] = [20, 20, 7];
/// Log-odds in 0.05 units.
const LO_HIT: i8 = 17;
const LO_MISS: i8 = -8;
const LO_MIN: i8 = -40;
const LO_MAX: i8 = 70;
const LO_OCCUPIED: i8 = 17;
const LO_FREE: i8 = -8;
const DYNAMIC_PASSES: u8 = 3;
/// The distance field's period, ms.
const ESDF_PERIOD_MS: u64 = 1_000;

fn class(lo: i8) -> u8 {
    if lo >= LO_OCCUPIED {
        VOX_OCCUPIED
    } else if lo <= LO_FREE {
        VOX_FREE
    } else {
        VOX_UNKNOWN
    }
}

fn cell_of(p: [f32; 3]) -> [i32; 3] {
    p.map(|v| (v / RES_M).floor() as i32)
}

pub struct DstVoxmap {
    tier: VoxTier,
    /// Absolute cell of window voxel (0, 0, 0).
    origin: [i32; 3],
    lo: Vec<i8>,
    state: Vec<u8>,
    esdf: Vec<u8>,
    /// The scan that last updated each voxel, and the scan counter.
    epoch: Vec<u16>,
    scan: u16,
    /// Consecutive passing scans per voxel with positive evidence.
    streak: Vec<u8>,
    seq: u64,
    esdf_ms: Option<u64>,
    /// The distance field's scratch: each voxel's nearest obstacle, and the
    /// front.
    site: Vec<u32>,
    queue: VecDeque<u32>,
}

impl DstVoxmap {
    /// An empty map of `tier` centred on map point `centre` (NED).
    pub fn new(tier: VoxTier, centre: [f32; 3]) -> Self {
        let n = DIMS.iter().product();
        Self {
            tier,
            origin: Self::origin_for(centre),
            lo: vec![0; n],
            state: vec![VOX_UNKNOWN; n],
            esdf: vec![ESDF_CAP_DM; n],
            epoch: vec![0; n],
            scan: 0,
            streak: vec![0; n],
            seq: 0,
            esdf_ms: None,
            site: Vec::new(),
            queue: VecDeque::new(),
        }
    }

    fn origin_for(centre: [f32; 3]) -> [i32; 3] {
        let c = cell_of(centre);
        [0, 1, 2].map(|a| c[a] - DIMS[a] as i32 / 2)
    }

    fn idx(&self, c: [i32; 3]) -> Option<usize> {
        let w = [0, 1, 2].map(|a| c[a] - self.origin[a]);
        if (0..3).any(|a| w[a] < 0 || w[a] >= DIMS[a] as i32) {
            return None;
        }
        Some(w[0] as usize + DIMS[0] * (w[1] as usize + DIMS[1] * w[2] as usize))
    }

    /// The occupancy class of the voxel holding map point `p`.
    pub fn class_at(&self, p: [f32; 3]) -> u8 {
        self.idx(cell_of(p)).map_or(VOX_UNKNOWN, |i| self.state[i])
    }

    /// Keep the window round the vehicle at map point `p`: move it by
    /// copying what overlaps once the vehicle is off its centre.
    pub fn recentre(&mut self, p: [f32; 3]) {
        let want = Self::origin_for(p);
        if (0..3).all(|a| (want[a] - self.origin[a]).abs() < RECENTRE_VOX[a]) {
            return;
        }
        let n = self.lo.len();
        let mut lo = vec![0; n];
        let mut state = vec![VOX_UNKNOWN; n];
        let mut esdf = vec![ESDF_CAP_DM; n];
        let mut streak = vec![0; n];
        let old = self.origin;
        self.origin = want;
        for z in 0..DIMS[2] {
            for y in 0..DIMS[1] {
                for x in 0..DIMS[0] {
                    let abs = [want[0] + x as i32, want[1] + y as i32, want[2] + z as i32];
                    let w = [0, 1, 2].map(|a| abs[a] - old[a]);
                    if (0..3).any(|a| w[a] < 0 || w[a] >= DIMS[a] as i32) {
                        continue;
                    }
                    let from = w[0] as usize + DIMS[0] * (w[1] as usize + DIMS[1] * w[2] as usize);
                    let to = x + DIMS[0] * (y + DIMS[1] * z);
                    lo[to] = self.lo[from];
                    state[to] = self.state[from];
                    esdf[to] = self.esdf[from];
                    streak[to] = self.streak[from];
                }
            }
        }
        self.lo = lo;
        self.state = state;
        self.esdf = esdf;
        self.streak = streak;
        self.epoch.fill(0);
        self.esdf_ms = None;
    }

    fn store(&mut self, i: usize, lo: i8) {
        self.lo[i] = lo;
        self.state[i] = class(lo);
    }

    fn hit(&mut self, i: usize) {
        if self.epoch[i] == self.scan {
            return;
        }
        self.epoch[i] = self.scan;
        self.streak[i] = 0;
        self.store(i, (self.lo[i].saturating_add(LO_HIT)).min(LO_MAX));
    }

    fn miss(&mut self, i: usize, near: bool) {
        if self.epoch[i] == self.scan {
            return;
        }
        self.epoch[i] = self.scan;
        let old = self.lo[i];
        if near && old > 0 {
            return;
        }
        let mut lo = old.saturating_add(LO_MISS).max(LO_MIN);
        if old > 0 {
            self.streak[i] += 1;
            if self.streak[i] >= DYNAMIC_PASSES {
                lo = lo.min(LO_FREE);
                self.streak[i] = 0;
            }
        }
        self.store(i, lo);
    }

    /// Insert one scan from sensor position `o` (map NED): each ray ends at
    /// its point, a return (`true`) or the sensor's range with none.
    pub fn insert(&mut self, o: [f32; 3], rays: &[([f32; 3], bool)]) {
        self.scan = self.scan.wrapping_add(1);
        if self.scan == 0 {
            self.epoch.fill(0);
            self.scan = 1;
        }
        for &(end, hit) in rays {
            if hit {
                if let Some(i) = self.idx(cell_of(end)) {
                    self.hit(i);
                }
            }
        }
        for &(end, hit) in rays {
            self.cast(o, end, hit);
        }
    }

    /// Walk the voxels from `o` to `end` (3D DDA), missing each; with a
    /// return the end voxel is the hit's and its neighbours take near
    /// misses. Stops where the ray leaves the window.
    fn cast(&mut self, o: [f32; 3], end: [f32; 3], hit: bool) {
        let g0 = o.map(|v| v / RES_M);
        let g1 = end.map(|v| v / RES_M);
        let d = [g1[0] - g0[0], g1[1] - g0[1], g1[2] - g0[2]];
        let mut cell = cell_of(o);
        let last = cell_of(end);
        let mut step = [0i32; 3];
        let mut t_max = [f32::INFINITY; 3];
        let mut t_delta = [f32::INFINITY; 3];
        for a in 0..3 {
            if d[a].abs() > 1e-9 {
                step[a] = if d[a] > 0.0 { 1 } else { -1 };
                let edge = cell[a] as f32 + if d[a] > 0.0 { 1.0 } else { 0.0 };
                t_max[a] = (edge - g0[a]) / d[a];
                t_delta[a] = 1.0 / d[a].abs();
            }
        }
        loop {
            if hit && cell == last {
                return;
            }
            let Some(i) = self.idx(cell) else { return };
            let near = hit && (0..3).all(|a| (cell[a] - last[a]).abs() <= 1);
            self.miss(i, near);
            if cell == last {
                return;
            }
            let a = (0..3)
                .min_by(|&x, &y| t_max[x].total_cmp(&t_max[y]))
                .unwrap_or(0);
            if t_max[a] > 1.0 {
                return;
            }
            cell[a] += step[a];
            t_max[a] += t_delta[a];
        }
    }

    /// The distance field, whole: a front from every occupied voxel carrying
    /// its nearest obstacle, out to the 4 m cap.
    fn esdf(&mut self) {
        let n = self.lo.len();
        self.esdf.fill(ESDF_CAP_DM);
        self.site.clear();
        self.site.resize(n, u32::MAX);
        self.queue.clear();
        for i in 0..n {
            if self.lo[i] >= LO_OCCUPIED {
                self.esdf[i] = 0;
                self.site[i] = i as u32;
                self.queue.push_back(i as u32);
            }
        }
        let coords = |i: usize| {
            let x = i % DIMS[0];
            let y = (i / DIMS[0]) % DIMS[1];
            let z = i / (DIMS[0] * DIMS[1]);
            [x as i32, y as i32, z as i32]
        };
        while let Some(i) = self.queue.pop_front() {
            let i = i as usize;
            let s = coords(self.site[i] as usize);
            let c = coords(i);
            for (a, dv) in [(0, -1), (0, 1), (1, -1), (1, 1), (2, -1), (2, 1)] {
                let mut nc = c;
                nc[a] += dv;
                if nc[a] < 0 || nc[a] >= DIMS[a] as i32 {
                    continue;
                }
                let j = nc[0] as usize + DIMS[0] * (nc[1] as usize + DIMS[1] * nc[2] as usize);
                let d2: i32 = (0..3).map(|k| (nc[k] - s[k]).pow(2)).sum();
                let dm = ((d2 as f32).sqrt() * RES_M * 10.0).round();
                if dm >= f32::from(ESDF_CAP_DM) || dm as u8 >= self.esdf[j] {
                    continue;
                }
                self.esdf[j] = dm as u8;
                self.site[j] = self.site[i];
                self.queue.push_back(j as u32);
            }
        }
    }

    /// Publish the map at `now_ms` into `snap`, the body at map pose
    /// `body` (t north, east, down; q w, x, y, z).
    pub fn publish(&mut self, now_ms: u64, body: [f32; 7], snap: &mut VoxSnapshot) {
        if self
            .esdf_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= ESDF_PERIOD_MS)
        {
            self.esdf_ms = Some(now_ms);
            self.esdf();
        }
        self.seq += 2;
        snap.valid = true;
        snap.seq = self.seq;
        snap.meta = VoxmapMeta {
            stamp_ns: now_ms * 1_000_000,
            res_m: RES_M,
            origin_cell: self.origin,
            dims: DIMS.map(|d| d as u16),
            map_from_body: body,
            tier: self.tier,
        };
        snap.occupancy.clone_from(&self.state);
        snap.esdf_dm.clone_from(&self.esdf);
    }
}

/// A trunk ahead fills its voxels and the space before it reads free; the
/// space behind it stays unknown; a walker's voxels clear once it moves on.
#[test]
fn dst_voxmap_marks_hits_clears_rays_and_forgets_movers() {
    let o = [0.05, 0.05, -4.0];
    let mut m = DstVoxmap::new(VoxTier::Lidar3d, o);
    let rays: Vec<([f32; 3], bool)> = (0..20)
        .map(|k| ([5.0, 0.05 + 0.01 * k as f32, -4.0], true))
        .collect();
    for _ in 0..3 {
        m.insert(o, &rays);
    }
    assert_eq!(m.class_at([5.0, 0.1, -4.0]), VOX_OCCUPIED);
    assert_eq!(m.class_at([2.5, 0.1, -4.0]), VOX_FREE);
    assert_eq!(m.class_at([7.0, 0.1, -4.0]), VOX_UNKNOWN);
    // The walker moved on: rays pass to 10 m now.
    let through: Vec<_> = rays
        .iter()
        .map(|&(p, _)| ([10.0, p[1], p[2]], true))
        .collect();
    for _ in 0..DYNAMIC_PASSES {
        m.insert(o, &through);
    }
    assert_eq!(m.class_at([5.0, 0.1, -4.0]), VOX_FREE);
    let mut snap = VoxSnapshot::default();
    m.publish(0, [o[0], o[1], o[2], 1.0, 0.0, 0.0, 0.0], &mut snap);
    assert!(snap.valid && snap.seq == 2);
    let i = snap.index_of_point([10.0, 0.1, -4.0]).unwrap();
    assert_eq!((snap.occupancy[i], snap.esdf_dm[i]), (VOX_OCCUPIED, 0));
    let j = snap.index_of_point([9.0, 0.1, -4.0]).unwrap();
    assert!((9..=11).contains(&snap.esdf_dm[j]), "{}", snap.esdf_dm[j]);
    // Moving the window keeps what overlaps.
    m.recentre([6.0, 0.0, -4.0]);
    assert_eq!(m.class_at([10.0, 0.1, -4.0]), VOX_OCCUPIED);
    assert_eq!(m.class_at([2.5, 0.1, -4.0]), VOX_FREE);
}
