//! Vantage choice over seeded random layouts on the pilot grid: trunk forests
//! and walls with corners, the vehicle at the origin, the subject predicted
//! 5-14 m away and often hidden. One JSONL row per layout (the occupied
//! cells, the spec, the choice, how long `choose` took), then the p50/p95
//! choose time and how many chosen points sit under the margin. A dense
//! visibility oracle scores the rows offline.
//!
//! ```text
//! cargo run --release -p ados-pilot --example vantage_batch -- --n 2000 --seed 1 --out vantage.jsonl
//! ```

use std::io::{BufWriter, Write};
use std::process::ExitCode;
use std::time::Instant;

use ados_pilot::geo::{self, V2};
use ados_pilot::obstacles::{Obstacles, CELL_M, GRID_N};
use ados_pilot::vantage::{self, SNAP_MARGIN_M, SNAP_MAX_M};
use ados_protocol::pilot::VantageSpec;
use serde_json::{json, Value};

const FROM: V2 = [0.0, 0.0];
/// Home and geofence radius: far outside every layout, so it never binds.
const FENCE: (V2, f32) = ([0.0, 0.0], 500.0);
/// Obstacles keep at least the margin plus this from the vehicle.
const VEHICLE_KEEP_M: f32 = 0.5;
/// Obstacle primitives are rasterised at this spacing (under a cell).
const STEP_M: f32 = 0.1;

/// xorshift64*: a seeded stream, no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }

    /// Uniform integer in [lo, hi].
    fn int(&mut self, lo: u32, hi: u32) -> u32 {
        lo + (self.next_u64() % u64::from(hi - lo + 1)) as u32
    }

    fn chance(&mut self, p: f32) -> bool {
        self.unit() < p
    }
}

/// A rasterised obstacle and its description for the row.
struct Prim {
    points: Vec<V2>,
    desc: Value,
}

fn disc(c: V2, r: f32) -> Prim {
    let n = (r / STEP_M).ceil() as i32;
    let mut points = Vec::new();
    for i in -n..=n {
        for j in -n..=n {
            let p = [c[0] + i as f32 * STEP_M, c[1] + j as f32 * STEP_M];
            if geo::dist(p, c) <= r {
                points.push(p);
            }
        }
    }
    Prim {
        points,
        desc: json!({"kind": "disc", "c": c, "r_m": r}),
    }
}

fn wall(a: V2, b: V2, thick: f32) -> Prim {
    let len = geo::dist(a, b);
    let u = geo::scale(geo::sub(b, a), 1.0 / len.max(1e-3));
    let perp = [-u[1], u[0]];
    let (na, nt) = ((len / STEP_M).ceil() as i32, (thick / STEP_M).ceil() as i32);
    let mut points = Vec::new();
    for k in 0..=na {
        let p = geo::add(a, geo::scale(u, (k as f32 * STEP_M).min(len)));
        for t in 0..=nt {
            let off = (t as f32 * STEP_M).min(thick) - thick / 2.0;
            points.push(geo::add(p, geo::scale(perp, off)));
        }
    }
    Prim {
        points,
        desc: json!({"kind": "wall", "a": a, "b": b, "thick_m": thick}),
    }
}

/// True when the primitive keeps clear of the vehicle.
fn clear_of_vehicle(p: &Prim, margin: f32) -> bool {
    p.points
        .iter()
        .all(|q| geo::dist(*q, FROM) >= margin + VEHICLE_KEEP_M)
}

/// 4-20 trunks, one of them usually on the line of sight to `pred`.
fn forest(rng: &mut Rng, pred: V2, margin: f32) -> Vec<Prim> {
    let d = geo::norm(pred);
    let u = geo::scale(pred, 1.0 / d);
    let perp = [-u[1], u[0]];
    let mut out = Vec::new();
    if rng.chance(0.7) {
        for _ in 0..20 {
            let c = geo::add(
                geo::scale(u, d * rng.range(0.35, 0.75)),
                geo::scale(perp, rng.range(-0.3, 0.3)),
            );
            let p = disc(c, rng.range(0.3, 0.6));
            if clear_of_vehicle(&p, margin) {
                out.push(p);
                break;
            }
        }
    }
    let n = rng.int(4, 20) as usize;
    let mut tries = 0;
    while out.len() < n && tries < 400 {
        tries += 1;
        let c = geo::scale(
            geo::bearing_unit(rng.range(0.0, std::f32::consts::TAU)),
            rng.range(0.0, 14.0),
        );
        let r = rng.range(0.2, 0.6);
        if geo::dist(c, pred) < r + 0.3 {
            continue;
        }
        let p = disc(c, r);
        if clear_of_vehicle(&p, margin) {
            out.push(p);
        }
    }
    out
}

/// 1-3 wall segments: usually an L corner across the line of sight to
/// `pred` (or one straight wall across it), plus loose straight walls.
fn walls(rng: &mut Rng, pred: V2, margin: f32) -> Vec<Prim> {
    let d = geo::norm(pred);
    let u = geo::scale(pred, 1.0 / d);
    let perp = [-u[1], u[0]];
    let corner = rng.chance(0.6);
    let mut out = Vec::new();
    for _ in 0..20 {
        let thick = rng.range(0.2, 0.4);
        let cross = geo::add(
            geo::scale(u, d * rng.range(0.4, 0.7)),
            geo::scale(perp, rng.range(-2.0, 2.0)),
        );
        let a = geo::add(cross, geo::scale(perp, rng.range(0.5, 6.0)));
        let b = geo::sub(cross, geo::scale(perp, rng.range(0.5, 6.0)));
        let mut set = vec![wall(a, b, thick)];
        if corner {
            let end = if rng.chance(0.5) { a } else { b };
            let leg = geo::scale(
                u,
                rng.range(1.5, 5.0) * if rng.chance(0.7) { 1.0 } else { -1.0 },
            );
            set.push(wall(end, geo::add(end, leg), thick));
        }
        if set.iter().all(|p| clear_of_vehicle(p, margin)) {
            out = set;
            break;
        }
    }
    let total = if corner {
        out.len() as u32 + rng.int(0, 1)
    } else {
        rng.int(1, 3)
    };
    let mut tries = 0;
    while (out.len() as u32) < total && tries < 100 {
        tries += 1;
        let c = geo::scale(
            geo::bearing_unit(rng.range(0.0, std::f32::consts::TAU)),
            rng.range(3.0, 12.0),
        );
        let h = geo::scale(
            geo::bearing_unit(rng.range(0.0, std::f32::consts::TAU)),
            rng.range(1.0, 4.0),
        );
        let p = wall(geo::sub(c, h), geo::add(c, h), rng.range(0.2, 0.4));
        if clear_of_vehicle(&p, margin) {
            out.push(p);
        }
    }
    out
}

/// A random covariance `[nn, ne, ee]`: sigmas 0.3-2.5 m at a random angle.
fn covariance(rng: &mut Rng) -> [f32; 3] {
    let s1 = rng.range(0.3, 2.5);
    let s2 = rng.range(0.3, s1);
    let (s, c) = rng.range(0.0, std::f32::consts::PI).sin_cos();
    let (v1, v2) = (s1 * s1, s2 * s2);
    [
        v1 * c * c + v2 * s * s,
        (v1 - v2) * c * s,
        v1 * s * s + v2 * c * c,
    ]
}

/// Nearest-rank percentile of sorted values.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let k = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[k - 1]
}

struct Args {
    n: usize,
    seed: u64,
    out: String,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        n: 2000,
        seed: 1,
        out: String::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let val = it.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--n" => a.n = val.parse().map_err(|e| format!("--n: {e}"))?,
            "--seed" => a.seed = val.parse().map_err(|e| format!("--seed: {e}"))?,
            "--out" => a.out = val,
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    if a.out.is_empty() {
        return Err("--out PATH is required".into());
    }
    Ok(a)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vantage_batch: {e}\nusage: vantage_batch --n N --seed S --out PATH");
            return ExitCode::from(2);
        }
    };
    if let Some(dir) = std::path::Path::new(&args.out).parent() {
        if !dir.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("vantage_batch: {}: {e}", dir.display());
                return ExitCode::FAILURE;
            }
        }
    }
    let file = match std::fs::File::create(&args.out) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("vantage_batch: {}: {e}", args.out);
            return ExitCode::FAILURE;
        }
    };
    let mut w = BufWriter::new(file);
    let mut rng = Rng::new(args.seed);
    let mut times = Vec::with_capacity(args.n);
    let (mut no_point, mut under_margin) = (0usize, Vec::new());

    for layout in 0..args.n {
        let margin = rng.range(1.0, 1.5);
        let max_speed = rng.range(2.0, 6.0);
        let pred = geo::scale(
            geo::bearing_unit(rng.range(0.0, std::f32::consts::TAU)),
            rng.range(5.0, 14.0),
        );
        let is_forest = layout % 2 == 0;
        let prims = if is_forest {
            forest(&mut rng, pred, margin)
        } else {
            walls(&mut rng, pred, margin)
        };
        let mut spec = VantageSpec::new(
            pred,
            covariance(&mut rng),
            rng.range(1.0, 2.0),
            rng.range(4.0, 6.0),
            12.0,
            FROM,
        );
        if rng.chance(0.3) {
            spec.hypotheses_ne = (0..rng.int(1, 2))
                .map(|_| {
                    geo::add(
                        pred,
                        geo::scale(
                            geo::bearing_unit(rng.range(0.0, std::f32::consts::TAU)),
                            rng.range(1.0, 4.0),
                        ),
                    )
                })
                .collect();
        }

        let mut obs = Obstacles::new();
        obs.grid.reset(FROM);
        for p in &prims {
            for q in &p.points {
                obs.grid.mark(*q);
            }
        }
        obs.grid.transform();
        let occ: Vec<usize> = (0..GRID_N * GRID_N)
            .filter(|k| obs.grid.occupied(k / GRID_N, k % GRID_N))
            .collect();

        let t = Instant::now();
        let c = vantage::choose(&obs, &spec, FROM, Some(FENCE), margin, max_speed);
        let choose_ms = t.elapsed().as_secs_f64() * 1e3;
        times.push(choose_ms);

        let clearance = c.point.and_then(|p| obs.grid.clearance(p));
        match c.point {
            None => no_point += 1,
            Some(_) if clearance.is_none_or(|cl| cl < margin) => under_margin.push(layout),
            Some(_) => {}
        }
        let path_len: f32 = c.path.windows(2).map(|s| geo::dist(s[0], s[1])).sum();
        let row = json!({
            "layout": layout,
            "kind": if is_forest { "forest" } else { "walls" },
            "grid": {"origin": obs.grid.origin, "cell_m": CELL_M, "n": GRID_N},
            "occ": occ,
            "prims": prims.iter().map(|p| &p.desc).collect::<Vec<_>>(),
            "from": FROM,
            "fence": {"home": FENCE.0, "radius_m": FENCE.1},
            "spec": spec,
            "margin": margin,
            "max_speed_mps": max_speed,
            "pred_snapped": vantage::snap_free(&obs.grid, spec.predicted_ne, SNAP_MARGIN_M, SNAP_MAX_M),
            "hyps_snapped": spec
                .hypotheses_ne
                .iter()
                .map(|h| vantage::snap_free(&obs.grid, *h, SNAP_MARGIN_M, SNAP_MAX_M))
                .collect::<Vec<_>>(),
            "choice": {
                "point": c.point,
                "coverage": c.coverage,
                "occluder": c.occluder.map(|o| json!({"ne": o.ne, "width_m": o.width_m})),
                "path_len_m": path_len,
            },
            // Null when off the grid or when the grid holds no obstacle.
            "clearance_m": clearance.filter(|v| v.is_finite()),
            "choose_ms": choose_ms,
        });
        if let Err(e) = writeln!(w, "{row}") {
            eprintln!("vantage_batch: {}: {e}", args.out);
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = w.flush() {
        eprintln!("vantage_batch: {}: {e}", args.out);
        return ExitCode::FAILURE;
    }

    times.sort_by(f64::total_cmp);
    println!(
        "vantage_batch: {} layouts (seed {}) -> {}",
        args.n, args.seed, args.out
    );
    println!(
        "choose ms: p50 {:.2}  p95 {:.2}  max {:.2}",
        percentile(&times, 0.50),
        percentile(&times, 0.95),
        times.last().copied().unwrap_or(f64::NAN)
    );
    println!("no vantage point: {no_point}");
    println!(
        "chosen points under the margin: {}{}",
        under_margin.len(),
        if under_margin.is_empty() {
            String::new()
        } else {
            format!(
                " (layouts {:?})",
                &under_margin[..under_margin.len().min(10)]
            )
        }
    );
    ExitCode::SUCCESS
}
