//! Battery health: per-pack cell, sag and time-to-reserve monitoring.
//!
//! The MAVLink router publishes every BATTERY_STATUS pack on the state
//! snapshot (`batteries[]`). This module turns that stream into the
//! `/api/v1/battery` read model: a few stateless threshold rules, a linear
//! time-to-reserve projection, and per-rule hysteresis so a reading that
//! flickers across a threshold raises one anomaly rather than a burst.
//!
//! - [`config`] is the `battery:` block of the agent config.
//! - [`rules`] and [`predict`] are pure functions over samples.
//! - [`engine`] holds the per-pack sample window, live anomalies and history.
//! - [`task`] is the only I/O: it feeds the engine from the state socket,
//!   hot-reloads the config, and records each anomaly transition in the
//!   logging store.

pub mod config;
pub mod engine;
pub mod predict;
pub mod rules;
mod task;

pub use config::BatteryConfig;
pub use engine::{AnomalyTransition, BatteryEngine};
pub use task::{spawn, BatteryTaskHandle};

/// A cell voltage outside this range is not a cell. ArduPilot, for one, puts
/// the whole-pack voltage in `voltages[0]` when it has no per-cell monitor.
pub const PLAUSIBLE_CELL_V: std::ops::RangeInclusive<f64> = 2.5..=4.5;

/// One battery reading at one instant, as the rules see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// Epoch milliseconds the reading arrived.
    pub t_ms: i64,
    /// Cell voltages as reported, plausible or not.
    pub cells: Vec<f64>,
    /// Whether `cells` are real per-cell readings (see [`cells_plausible`]).
    pub cells_plausible: bool,
    pub voltage_v: Option<f64>,
    pub current_a: Option<f64>,
    pub remaining_pct: Option<i64>,
    pub temperature_c: Option<f64>,
    pub consumed_mah: Option<i64>,
    pub consumed_wh: Option<f64>,
}

impl Sample {
    /// `(weakest index, min, max)` over the cells, `None` unless they are
    /// plausible per-cell readings.
    pub fn cell_extremes(&self) -> Option<(usize, f64, f64)> {
        if !self.cells_plausible {
            return None;
        }
        let mut weakest = 0;
        let mut max = f64::MIN;
        for (i, &v) in self.cells.iter().enumerate() {
            if v < self.cells[weakest] {
                weakest = i;
            }
            max = max.max(v);
        }
        Some((weakest, self.cells[weakest], max))
    }
}

/// A cell array is plausible when it is non-empty and every value is in
/// [`PLAUSIBLE_CELL_V`].
pub fn cells_plausible(cells: &[f64]) -> bool {
    !cells.is_empty() && cells.iter().all(|v| PLAUSIBLE_CELL_V.contains(v))
}

/// The spread between the highest and lowest cell in whole millivolts. The
/// inputs are integer-millivolt readings scaled to volts, so rounding recovers
/// the exact value the FC sent rather than a float residue.
pub fn divergence_mv(min_v: f64, max_v: f64) -> f64 {
    ((max_v - min_v) * 1000.0).round()
}

/// Epoch milliseconds of `at`, 0 for a clock set before the epoch.
pub fn epoch_ms(at: std::time::SystemTime) -> i64 {
    at.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
