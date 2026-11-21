//! The 26 lattice moves and the tables jump point search prunes with.
//!
//! A move from cell `n` in direction `e` is valid when the target and every
//! cell of the box spanned by the move (the "side" cells `n + e'` for each
//! sub-direction `e'` of `e`) is traversable: no corner is ever cut, so the
//! cells a path passes through always form overlapping boxes.
//!
//! Pruning: a node reached by direction `d` keeps its natural successors
//! (the sub-directions of `d`: the canonical, most-diagonal-first ordering)
//! and any neighbour `m` whose every short alternative path from the parent
//! `p = n - d` avoiding `n` is blocked or costlier than `p → n → m`. The
//! alternatives (one or two moves inside the 3×3×3 cube around `n`) are
//! enumerated once here, with the cube cells each needs, so the run-time test
//! is a few mask operations per neighbour.

use std::sync::LazyLock;

/// Index of the centre of the 3×3×3 cube.
pub(crate) const CENTRE: usize = 13;
/// Marks a direct (single-move) alternative.
pub(crate) const DIRECT: u8 = u8::MAX;

/// One alternative path `p → q → m` (or `p → m` when `q == DIRECT`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Alt {
    /// Cube cells that must be traversable (side cells of both moves).
    pub need: u32,
    pub q: u8,
    /// Unit lengths of the two moves (`l2` is 0 for a direct move).
    pub l1: f32,
    pub l2: f32,
    /// The second move is a sub-direction of the first.
    pub canonical: bool,
}

pub(crate) struct Dirs {
    pub off: [[i32; 3]; 26],
    /// Euclidean length in cells: 1, √2 or √3.
    pub len: [f32; 26],
    pub nnz: [u8; 26],
    /// Natural successors of each direction, as a direction bit mask
    /// (the direction itself included).
    pub natural: [u32; 26],
    /// The natural successors other than the direction itself.
    pub sub_list: [[u8; 6]; 26],
    pub sub_n: [u8; 26],
    /// Cube cells a move from the centre needs (target and side cells).
    pub need: [u32; 26],
    /// Cube index of each direction's target.
    pub cube: [u8; 26],
    /// The opposite direction.
    pub rev: [u8; 26],
    alt_start: Vec<u32>,
    alts: Vec<Alt>,
    dom_start: Vec<u32>,
    /// Need masks of the alternatives that dominate when every voxel costs
    /// the same (the common case), per `(d, e)`.
    dom: Vec<u32>,
}

pub(crate) fn cube_index(o: [i32; 3]) -> usize {
    ((o[0] + 1) + 3 * (o[1] + 1) + 9 * (o[2] + 1)) as usize
}

fn cube_offset(c: usize) -> [i32; 3] {
    let c = c as i32;
    [c % 3 - 1, (c / 3) % 3 - 1, c / 9 - 1]
}

fn is_sub(e: [i32; 3], d: [i32; 3]) -> bool {
    e != [0, 0, 0] && (0..3).all(|k| e[k] == 0 || e[k] == d[k])
}

fn unit(o: [i32; 3]) -> bool {
    o != [0, 0, 0] && o.iter().all(|v| (-1..=1).contains(v))
}

fn add(a: [i32; 3], b: [i32; 3]) -> [i32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn nnz(o: [i32; 3]) -> u8 {
    o.iter().filter(|v| **v != 0).count() as u8
}

/// Cube cells a move from cube offset `from` by `dir` needs, or `None` when
/// one lies outside the cube.
fn move_need(from: [i32; 3], dir: [i32; 3]) -> Option<u32> {
    let mut mask = 0u32;
    for c in 0..27 {
        let e = cube_offset(c);
        if is_sub(e, dir) {
            let t = add(from, e);
            if !t.iter().all(|v| (-1..=1).contains(v)) {
                return None;
            }
            mask |= 1 << cube_index(t);
        }
    }
    Some(mask)
}

impl Dirs {
    fn build() -> Self {
        let mut off = [[0i32; 3]; 26];
        let mut i = 0;
        for c in 0..27 {
            if c != CENTRE {
                off[i] = cube_offset(c);
                i += 1;
            }
        }
        let mut t = Dirs {
            off,
            len: [0.0; 26],
            nnz: [0; 26],
            natural: [0; 26],
            sub_list: [[0; 6]; 26],
            sub_n: [0; 26],
            need: [0; 26],
            cube: [0; 26],
            rev: [0; 26],
            alt_start: Vec::with_capacity(26 * 26 + 1),
            alts: Vec::new(),
            dom_start: Vec::with_capacity(26 * 26 + 1),
            dom: Vec::new(),
        };
        let index_of = |o: [i32; 3]| off.iter().position(|x| *x == o);
        for d in 0..26 {
            let o = off[d];
            t.nnz[d] = nnz(o);
            t.len[d] = (t.nnz[d] as f32).sqrt();
            t.cube[d] = cube_index(o) as u8;
            t.rev[d] = index_of([-o[0], -o[1], -o[2]]).unwrap_or(d) as u8;
            t.need[d] = move_need([0, 0, 0], o).unwrap_or(0);
            for e in 0..26 {
                if is_sub(off[e], o) {
                    t.natural[d] |= 1 << e;
                    if e != d {
                        let n = t.sub_n[d] as usize;
                        t.sub_list[d][n] = e as u8;
                        t.sub_n[d] += 1;
                    }
                }
            }
        }
        for d in 0..26 {
            let p = [-off[d][0], -off[d][1], -off[d][2]];
            for e in 0..26 {
                t.alt_start.push(t.alts.len() as u32);
                if t.natural[d] & (1 << e) != 0 || e == t.rev[d] as usize {
                    continue;
                }
                let m = off[e];
                let direct = [m[0] - p[0], m[1] - p[1], m[2] - p[2]];
                if unit(direct) {
                    if let Some(need) = move_need(p, direct) {
                        t.alts.push(Alt {
                            need: need & !(1 << CENTRE),
                            q: DIRECT,
                            l1: (nnz(direct) as f32).sqrt(),
                            l2: 0.0,
                            canonical: true,
                        });
                    }
                }
                for qc in 0..27 {
                    let q = cube_offset(qc);
                    if qc == CENTRE || q == p || q == m {
                        continue;
                    }
                    let a = [q[0] - p[0], q[1] - p[1], q[2] - p[2]];
                    let b = [m[0] - q[0], m[1] - q[1], m[2] - q[2]];
                    if !unit(a) || !unit(b) {
                        continue;
                    }
                    let (Some(na), Some(nb)) = (move_need(p, a), move_need(q, b)) else {
                        continue;
                    };
                    t.alts.push(Alt {
                        need: (na | nb) & !(1 << CENTRE),
                        q: qc as u8,
                        l1: (nnz(a) as f32).sqrt(),
                        l2: (nnz(b) as f32).sqrt(),
                        canonical: is_sub(b, a),
                    });
                }
            }
        }
        t.alt_start.push(t.alts.len() as u32);
        for d in 0..26 {
            for e in 0..26 {
                t.dom_start.push(t.dom.len() as u32);
                let via = t.len[d] + t.len[e];
                let k = d * 26 + e;
                for i in t.alt_start[k] as usize..t.alt_start[k + 1] as usize {
                    let a = t.alts[i];
                    let cost = a.l1 + a.l2;
                    if cost < via - 1e-4 || (a.canonical && cost <= via + 1e-4) {
                        t.dom.push(a.need);
                    }
                }
            }
        }
        t.dom_start.push(t.dom.len() as u32);
        t
    }

    /// The alternatives to reaching neighbour `e` of a node entered by `d`.
    pub fn alts(&self, d: usize, e: usize) -> &[Alt] {
        let k = d * 26 + e;
        &self.alts[self.alt_start[k] as usize..self.alt_start[k + 1] as usize]
    }

    /// Need masks of the alternatives that dominate `p → n → n + e` when
    /// every voxel costs the same.
    pub fn dominating(&self, d: usize, e: usize) -> &[u32] {
        let k = d * 26 + e;
        &self.dom[self.dom_start[k] as usize..self.dom_start[k + 1] as usize]
    }

    pub fn is_diagonal(&self, d: usize) -> bool {
        self.nnz[d] > 1
    }
}

static TABLES: LazyLock<Dirs> = LazyLock::new(Dirs::build);

pub(crate) fn dirs() -> &'static Dirs {
    &TABLES
}
