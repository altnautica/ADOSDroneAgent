//! Grid search on the lattice: jump point search, and plain A* over the same
//! moves and costs (the reference jump point search is checked against).
//!
//! A step costs its length times the mean cost multiplier of the two voxels
//! (1 for free, the configured factor for unknown). The heuristic is the
//! octile distance at multiplier 1, so both searches return optimal paths.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::dirs::{dirs, CENTRE, DIRECT};
use crate::lattice::{Space, BLOCKED};
use crate::world::OccupancyQuery;
use crate::PlanError;

/// Which grid search to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    /// Jump point search (the planner's front end).
    Jps,
    /// Plain A* over all 26 moves.
    Astar,
}

const NO_DIR: u8 = 26;
const CLOSED: u8 = 0x80;
const ALL_DIRS: u32 = (1 << 26) - 1;
const EPS: f32 = 1e-4;
/// Longest jump, voxels (see [`Search::jump`]).
const JUMP_MAX: u32 = 12;

#[derive(Debug, Clone, Copy)]
struct Open {
    f: f32,
    g: f32,
    idx: u32,
}

impl PartialEq for Open {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Open {}
impl PartialOrd for Open {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Open {
    /// Max-heap order: lowest `f` first, then deepest `g`, then lowest index.
    fn cmp(&self, o: &Self) -> Ordering {
        o.f.total_cmp(&self.f)
            .then(self.g.total_cmp(&o.g))
            .then(o.idx.cmp(&self.idx))
    }
}

pub(crate) fn add(a: [i32; 3], b: [i32; 3]) -> [i32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

/// The lattice direction of the run `a → b`.
fn run_dir(a: [i32; 3], b: [i32; 3]) -> [i32; 3] {
    [
        (b[0] - a[0]).signum(),
        (b[1] - a[1]).signum(),
        (b[2] - a[2]).signum(),
    ]
}

/// Octile distance between voxels, in voxels.
pub(crate) fn octile(a: [i32; 3], b: [i32; 3]) -> f32 {
    let mut d = [
        (a[0] - b[0]).abs(),
        (a[1] - b[1]).abs(),
        (a[2] - b[2]).abs(),
    ];
    d.sort_unstable();
    let [lo, mid, hi] = d.map(|v| v as f32);
    lo * 3f32.sqrt() + (mid - lo) * 2f32.sqrt() + (hi - mid)
}

/// How far a search may go and how greedy it is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    /// Voxels it may scan before [`PlanError::Timeout`].
    pub max_scan: u64,
    /// Heuristic inflation (1 = optimal).
    pub h_weight: f32,
}

/// Reusable search buffers, sized to the largest lattice seen.
#[derive(Debug, Default)]
pub(crate) struct Search {
    g: Vec<f32>,
    parent: Vec<u32>,
    dir: Vec<u8>,
    stamp: Vec<u32>,
    gen: u32,
    open: BinaryHeap<Open>,
    goal: [i32; 3],
    h_scale: f32,
    pub scanned: u64,
    max_scan: u64,
    timed_out: bool,
    pub expansions: u32,
}

impl Search {
    fn begin(&mut self, n: usize) {
        if self.g.len() < n {
            self.g.resize(n, 0.0);
            self.parent.resize(n, 0);
            self.dir.resize(n, 0);
            self.stamp.resize(n, 0);
        }
        self.gen = self.gen.wrapping_add(1);
        if self.gen == 0 {
            self.stamp.fill(0);
            self.gen = 1;
        }
        self.open.clear();
        self.scanned = 0;
        self.timed_out = false;
        self.expansions = 0;
    }

    fn touch(&mut self, i: usize) {
        if self.stamp[i] != self.gen {
            self.stamp[i] = self.gen;
            self.g[i] = f32::INFINITY;
            self.dir[i] = NO_DIR;
        }
    }

    /// Search `start → goal` on the space's lattice. On success the voxels
    /// of the path (jump points for [`SearchKind::Jps`], every voxel for A*)
    /// are written to `out` and the path cost (metres, weighted) returned;
    /// it is optimal at heuristic weight 1 and within that factor of
    /// optimal above it.
    #[allow(clippy::too_many_arguments)]
    pub fn run<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        sp: &mut Space,
        kind: SearchKind,
        start: [i32; 3],
        goal: [i32; 3],
        budget: Budget,
        out: &mut Vec<[i32; 3]>,
    ) -> Result<f32, PlanError> {
        out.clear();
        let lat = sp.lat;
        self.begin(lat.len());
        self.goal = goal;
        self.max_scan = budget.max_scan;
        self.h_scale = budget.h_weight.max(1.0) * lat.res;
        if sp.class(w, start) == BLOCKED || sp.class(w, goal) == BLOCKED {
            return Err(PlanError::NoPath);
        }
        let s = lat.index(start);
        let gi = lat.index(goal);
        self.touch(s);
        self.g[s] = 0.0;
        self.parent[s] = s as u32;
        let res = lat.res;
        self.open.push(Open {
            f: octile(start, goal) * self.h_scale,
            g: 0.0,
            idx: s as u32,
        });
        let t = dirs();
        while let Some(o) = self.open.pop() {
            let i = o.idx as usize;
            if self.dir[i] & CLOSED != 0 || o.g > self.g[i] {
                continue;
            }
            self.dir[i] |= CLOSED;
            if i == gi {
                let mut j = i;
                loop {
                    let c = lat.coords(j);
                    // A node a bounded jump left on a straight run is not a
                    // turning point: drop it.
                    let n = out.len();
                    if kind == SearchKind::Jps
                        && n >= 2
                        && run_dir(c, out[n - 1]) == run_dir(out[n - 1], out[n - 2])
                    {
                        out.pop();
                    }
                    out.push(c);
                    if j == s {
                        break;
                    }
                    j = self.parent[j] as usize;
                }
                out.reverse();
                return Ok(self.g[i]);
            }
            self.expansions += 1;
            let c = lat.coords(i);
            let d = (self.dir[i] & !CLOSED) as usize;
            let succ = match kind {
                SearchKind::Astar => ALL_DIRS,
                SearchKind::Jps if d == NO_DIR as usize => ALL_DIRS,
                SearchKind::Jps if sp.uniform(w, c) => t.natural[d],
                SearchKind::Jps => t.natural[d] | self.forced(w, sp, c, d),
            };
            let gc = self.g[i];
            for e in 0..26 {
                if succ & (1 << e) == 0 || !self.move_ok(w, sp, c, e) {
                    continue;
                }
                let found = match kind {
                    SearchKind::Astar => {
                        self.scanned += 1;
                        let n = add(c, t.off[e]);
                        let (c0, c1) = (sp.class(w, c), sp.class(w, n));
                        let (m0, m1) = (sp.mult(c0), sp.mult(c1));
                        Some((n, t.len[e] * res * 0.5 * (m0 + m1)))
                    }
                    SearchKind::Jps => self.jump(w, sp, c, e),
                };
                if self.timed_out || self.scanned > self.max_scan {
                    return Err(PlanError::Timeout);
                }
                if let Some((n, step)) = found {
                    self.relax(sp, n, gc + step, i, e);
                }
            }
        }
        Err(PlanError::NoPath)
    }

    fn relax(&mut self, sp: &Space, n: [i32; 3], g: f32, parent: usize, e: usize) {
        let j = sp.lat.index(n);
        self.touch(j);
        if self.dir[j] & CLOSED != 0 || g >= self.g[j] - 1e-6 {
            return;
        }
        self.g[j] = g;
        self.parent[j] = parent as u32;
        self.dir[j] = e as u8;
        self.open.push(Open {
            f: g + octile(n, self.goal) * self.h_scale,
            g,
            idx: j as u32,
        });
    }

    /// True when the move `c → c + e` is valid: target and side voxels
    /// traversable.
    fn move_ok<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        sp: &mut Space,
        c: [i32; 3],
        e: usize,
    ) -> bool {
        if sp.uniform(w, c) {
            return true;
        }
        let t = dirs();
        let mut bits = t.natural[e];
        while bits != 0 {
            let k = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if sp.class(w, add(c, t.off[k])) == BLOCKED {
                return false;
            }
        }
        true
    }

    /// Neighbours of `c` (entered by `d`) that no short path avoiding `c`
    /// reaches as cheaply: the forced successors, as a direction mask.
    fn forced<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        sp: &mut Space,
        c: [i32; 3],
        d: usize,
    ) -> u32 {
        let t = dirs();
        let (cls, blocked) = sp.cube(w, c);
        let own = cls[CENTRE];
        let single = cls.iter().all(|&k| k == BLOCKED || k == own);
        let mp = sp.mult(cls[t.cube[t.rev[d] as usize] as usize]);
        let mn = sp.mult(own);
        let via1 = t.len[d] * 0.5 * (mp + mn);
        let mut out = 0u32;
        for e in 0..26 {
            if t.natural[d] & (1 << e) != 0 || e == t.rev[d] as usize || t.need[e] & blocked != 0 {
                continue;
            }
            if single {
                // One cost everywhere: the dominance test is precomputed.
                if t.dominating(d, e).iter().all(|m| m & blocked != 0) {
                    out |= 1 << e;
                }
                continue;
            }
            let mm = sp.mult(cls[t.cube[e] as usize]);
            let via = via1 + t.len[e] * 0.5 * (mn + mm);
            let dominated = t.alts(d, e).iter().any(|a| {
                if a.need & blocked != 0 {
                    return false;
                }
                let cost = if a.q == DIRECT {
                    a.l1 * 0.5 * (mp + mm)
                } else {
                    let mq = sp.mult(cls[a.q as usize]);
                    a.l1 * 0.5 * (mp + mq) + a.l2 * 0.5 * (mq + mm)
                };
                cost < via - EPS || (a.canonical && cost <= via + EPS)
            });
            if !dominated {
                out |= 1 << e;
            }
        }
        out
    }

    /// Walk from `from` along `d` to the next jump point: the goal, a voxel
    /// with a forced neighbour, (for a diagonal) a voxel from which a
    /// natural sub-direction reaches one, or the voxel [`JUMP_MAX`] steps
    /// on. `None` at a dead end.
    ///
    /// Stopping at the bound adds a node whose canonical successors are
    /// exactly what the walk would have scanned on, so optimality is kept;
    /// a sub-direction probe that reaches the bound counts as a hit. In open
    /// space this keeps the scan near the path instead of sweeping every
    /// line of the window.
    fn jump<W: OccupancyQuery + ?Sized>(
        &mut self,
        w: &W,
        sp: &mut Space,
        from: [i32; 3],
        d: usize,
    ) -> Option<([i32; 3], f32)> {
        let t = dirs();
        let step = t.len[d] * sp.lat.res * 0.5;
        let mut cur = from;
        let cls = sp.class(w, cur);
        let mut cur_m = sp.mult(cls);
        let mut g = 0.0;
        let mut steps = 0;
        loop {
            if !self.move_ok(w, sp, cur, d) {
                return None;
            }
            let next = add(cur, t.off[d]);
            let cls = sp.class(w, next);
            let next_m = sp.mult(cls);
            g += step * (cur_m + next_m);
            self.scanned += 1;
            if self.scanned > self.max_scan {
                self.timed_out = true;
                return None;
            }
            if next == self.goal || (!sp.uniform(w, next) && self.forced(w, sp, next, d) != 0) {
                return Some((next, g));
            }
            if t.is_diagonal(d) {
                for k in 0..t.sub_n[d] as usize {
                    let e = t.sub_list[d][k] as usize;
                    if self.jump(w, sp, next, e).is_some() {
                        return Some((next, g));
                    }
                    if self.timed_out {
                        return None;
                    }
                }
            }
            steps += 1;
            if steps >= JUMP_MAX {
                return Some((next, g));
            }
            cur = next;
            cur_m = next_m;
        }
    }
}

/// Every voxel along a path of lattice-straight runs (jump points), in order.
pub(crate) fn densify(points: &[[i32; 3]], out: &mut Vec<[i32; 3]>) {
    out.clear();
    let Some(&first) = points.first() else {
        return;
    };
    out.push(first);
    for w in points.windows(2) {
        let mut c = w[0];
        let s = [
            (w[1][0] - c[0]).signum(),
            (w[1][1] - c[1]).signum(),
            (w[1][2] - c[2]).signum(),
        ];
        while c != w[1] {
            for k in 0..3 {
                if c[k] != w[1][k] {
                    c[k] += s[k];
                }
            }
            out.push(c);
        }
    }
}
