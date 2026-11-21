//! What one benchmark run measured, and the table and JSON of a matrix.

use serde_json::{json, Value};

/// A run reached its goal when within this of it, m.
pub const REACH_M: f32 = 1.5;
/// A surface nearer the vehicle's centre than this is a collision, m (the
/// vehicle's radius).
pub const COLLISION_M: f32 = 0.25;
/// The pilot's obstacle margin in an open place, m: crossing into it is a
/// margin entry.
pub const MARGIN_M: f32 = 1.5;

/// One run's result.
#[derive(Debug, Clone, Default)]
pub struct RunMetrics {
    pub seed: u64,
    /// Reached within [`REACH_M`] before the timeout (3 × distance / speed
    /// + 10 s).
    pub success: bool,
    /// An obstacle surface came within [`COLLISION_M`].
    pub collision: bool,
    /// Times the nearest surface crossed into [`MARGIN_M`].
    pub margin_entries: u32,
    /// From the goal's start to reaching it, s (`None`: not reached).
    pub time_to_goal_s: Option<f32>,
    /// Horizontal path flown over the straight distance.
    pub path_ratio: f32,
    pub min_clearance_m: f32,
}

/// Folds a run's ticks into [`RunMetrics`].
pub struct Meter {
    pub m: RunMetrics,
    inside: bool,
    last: [f32; 2],
    path_m: f32,
    straight_m: f32,
}

impl Meter {
    pub fn new(seed: u64, start: [f32; 2], straight_m: f32) -> Self {
        Self {
            m: RunMetrics {
                seed,
                min_clearance_m: f32::INFINITY,
                ..RunMetrics::default()
            },
            inside: false,
            last: start,
            path_m: 0.0,
            straight_m,
        }
    }

    /// One tick: the vehicle at `pos`, the nearest surface `clearance_m` off.
    pub fn tick(&mut self, pos: [f32; 2], clearance_m: f32) {
        self.path_m += (pos[0] - self.last[0]).hypot(pos[1] - self.last[1]);
        self.last = pos;
        self.m.min_clearance_m = self.m.min_clearance_m.min(clearance_m);
        if clearance_m < COLLISION_M {
            self.m.collision = true;
        }
        let inside = clearance_m < MARGIN_M;
        if inside && !self.inside {
            self.m.margin_entries += 1;
        }
        self.inside = inside;
    }

    pub fn reached(&mut self, t_s: f32) {
        self.m.success = true;
        self.m.time_to_goal_s = Some(t_s);
    }

    pub fn finish(mut self) -> RunMetrics {
        self.m.path_ratio = self.path_m / self.straight_m.max(1e-3);
        self.m
    }
}

impl RunMetrics {
    pub fn json(&self) -> Value {
        json!({
            "seed": self.seed,
            "success": self.success,
            "collision": self.collision,
            "margin_entries": self.margin_entries,
            "time_to_goal_s": self.time_to_goal_s,
            "path_ratio": self.path_ratio,
            "min_clearance_m": if self.min_clearance_m.is_finite() { Some(self.min_clearance_m) } else { None },
        })
    }
}

/// One matrix cell: a scenario, tier and speed over its seeds.
pub struct Cell {
    pub scenario: String,
    pub tier: &'static str,
    pub speed_mps: f32,
    pub runs: Vec<RunMetrics>,
}

impl Cell {
    fn rate(&self, f: impl Fn(&RunMetrics) -> bool) -> f32 {
        self.runs.iter().filter(|r| f(r)).count() as f32 / self.runs.len().max(1) as f32
    }

    fn time_to_goal_mean(&self) -> Option<f32> {
        let t: Vec<f32> = self.runs.iter().filter_map(|r| r.time_to_goal_s).collect();
        (!t.is_empty()).then(|| t.iter().sum::<f32>() / t.len() as f32)
    }

    fn min_clearance(&self) -> f32 {
        self.runs
            .iter()
            .map(|r| r.min_clearance_m)
            .fold(f32::INFINITY, f32::min)
    }

    pub fn json(&self) -> Value {
        json!({
            "scenario": self.scenario,
            "tier": self.tier,
            "speed_mps": self.speed_mps,
            "success": self.rate(|r| r.success),
            "collision": self.rate(|r| r.collision),
            "margin_entries": self.runs.iter().map(|r| r.margin_entries).sum::<u32>(),
            "time_to_goal_s": self.time_to_goal_mean(),
            "min_clearance_m": Some(self.min_clearance()).filter(|c| c.is_finite()),
            "runs": self.runs.iter().map(RunMetrics::json).collect::<Vec<_>>(),
        })
    }

    pub fn row(&self) -> String {
        format!(
            "| {} | {} | {:.0} | {}/{} | {}/{} | {} | {} | {:.2} | {} |",
            self.scenario,
            self.tier,
            self.speed_mps,
            self.runs.iter().filter(|r| r.success).count(),
            self.runs.len(),
            self.runs.iter().filter(|r| r.collision).count(),
            self.runs.len(),
            self.runs.iter().map(|r| r.margin_entries).sum::<u32>(),
            self.time_to_goal_mean()
                .map_or("-".into(), |t| format!("{t:.1}")),
            self.runs.iter().map(|r| r.path_ratio).sum::<f32>() / self.runs.len().max(1) as f32,
            Some(self.min_clearance())
                .filter(|c| c.is_finite())
                .map_or("-".into(), |c| format!("{c:.2}")),
        )
    }
}

pub const TABLE_HEAD: &str = "| scenario | tier | speed m/s | success | collision | margin entries | time to goal s | path ratio | min clearance m |\n|---|---|---|---|---|---|---|---|---|";

/// Where the matrix JSON goes: `target/dst-bench/` of the workspace (or
/// `CARGO_TARGET_DIR`).
pub fn out_dir() -> std::path::PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target"));
    target.join("dst-bench")
}
