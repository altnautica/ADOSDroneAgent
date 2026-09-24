//! The `battery:` block of `/etc/ados/config.yaml`: the thresholds the battery
//! health engine evaluates against.
//!
//! Every value is an integer so a settings form needs no float input: voltages
//! in millivolts, the temperature rate in tenths of a degree per second. A
//! missing block reads as the defaults; an out-of-bounds or inconsistent value
//! falls back to its default with a warning rather than disabling monitoring.

use std::ops::RangeInclusive;
use std::path::Path;

use serde::{Deserialize, Serialize};

const LOW_CELL_MV: RangeInclusive<u32> = 2500..=4200;
const CRITICAL_CELL_MV: RangeInclusive<u32> = 2500..=4000;
const CELL_DIVERGENCE_MV: RangeInclusive<u32> = 10..=500;
const VOLTAGE_DROP_MV_PER_S: RangeInclusive<u32> = 100..=5000;
const TEMP_SPIKE_DC_PER_S: RangeInclusive<u32> = 5..=200;
const PREDICTIVE_WINDOW_S: RangeInclusive<u32> = 5..=300;
const RESERVE_PERCENT: RangeInclusive<u32> = 5..=50;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct BatteryConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// A cell below this raises `cell_low`.
    #[serde(default = "default_low_cell_mv")]
    pub low_cell_mv: u32,
    /// A cell below this raises `cell_critical`. Must be below `low_cell_mv`.
    #[serde(default = "default_critical_cell_mv")]
    pub critical_cell_mv: u32,
    /// A highest-to-lowest cell spread above this raises `cell_divergence`.
    #[serde(default = "default_cell_divergence_mv")]
    pub cell_divergence_mv: u32,
    /// A pack voltage falling faster than this raises `voltage_drop`.
    #[serde(default = "default_voltage_drop_mv_per_s")]
    pub voltage_drop_mv_per_s: u32,
    /// A temperature rising faster than this (0.1 °C/s units) raises
    /// `temp_spike`.
    #[serde(default = "default_temp_spike_dc_per_s")]
    pub temp_spike_dc_per_s: u32,
    /// How far back the time-to-reserve projection looks.
    #[serde(default = "default_predictive_window_s")]
    pub predictive_window_s: u32,
    /// The remaining percentage the projection counts down to.
    #[serde(default = "default_reserve_percent")]
    pub reserve_percent: u32,
}

fn default_enabled() -> bool {
    true
}
fn default_low_cell_mv() -> u32 {
    3500
}
fn default_critical_cell_mv() -> u32 {
    3300
}
fn default_cell_divergence_mv() -> u32 {
    50
}
fn default_voltage_drop_mv_per_s() -> u32 {
    500
}
fn default_temp_spike_dc_per_s() -> u32 {
    50
}
fn default_predictive_window_s() -> u32 {
    30
}
fn default_reserve_percent() -> u32 {
    25
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            low_cell_mv: default_low_cell_mv(),
            critical_cell_mv: default_critical_cell_mv(),
            cell_divergence_mv: default_cell_divergence_mv(),
            voltage_drop_mv_per_s: default_voltage_drop_mv_per_s(),
            temp_spike_dc_per_s: default_temp_spike_dc_per_s(),
            predictive_window_s: default_predictive_window_s(),
            reserve_percent: default_reserve_percent(),
        }
    }
}

impl BatteryConfig {
    /// Load the `battery:` block from the agent config at `path`. A missing
    /// file or block reads as the defaults; a malformed file logs the parser
    /// error and reads as the defaults; each out-of-bounds value is replaced by
    /// its default.
    pub fn load_from(path: &Path) -> Self {
        #[derive(Default, Deserialize)]
        struct ConfigFile {
            #[serde(default)]
            battery: BatteryConfig,
        }
        let file: ConfigFile = ados_config::load_yaml_or_default(path, "battery");
        file.battery.validated()
    }

    /// Replace every out-of-bounds value with its default. A critical threshold
    /// that is not below the low threshold resets both, because either one
    /// alone may still leave the pair inverted.
    pub fn validated(mut self) -> Self {
        let d = Self::default();
        bounded(
            &mut self.low_cell_mv,
            d.low_cell_mv,
            LOW_CELL_MV,
            "low_cell_mv",
        );
        bounded(
            &mut self.critical_cell_mv,
            d.critical_cell_mv,
            CRITICAL_CELL_MV,
            "critical_cell_mv",
        );
        bounded(
            &mut self.cell_divergence_mv,
            d.cell_divergence_mv,
            CELL_DIVERGENCE_MV,
            "cell_divergence_mv",
        );
        bounded(
            &mut self.voltage_drop_mv_per_s,
            d.voltage_drop_mv_per_s,
            VOLTAGE_DROP_MV_PER_S,
            "voltage_drop_mv_per_s",
        );
        bounded(
            &mut self.temp_spike_dc_per_s,
            d.temp_spike_dc_per_s,
            TEMP_SPIKE_DC_PER_S,
            "temp_spike_dc_per_s",
        );
        bounded(
            &mut self.predictive_window_s,
            d.predictive_window_s,
            PREDICTIVE_WINDOW_S,
            "predictive_window_s",
        );
        bounded(
            &mut self.reserve_percent,
            d.reserve_percent,
            RESERVE_PERCENT,
            "reserve_percent",
        );
        if self.critical_cell_mv >= self.low_cell_mv {
            tracing::warn!(
                low_cell_mv = self.low_cell_mv,
                critical_cell_mv = self.critical_cell_mv,
                "battery config critical_cell_mv is not below low_cell_mv; using the defaults for both"
            );
            self.low_cell_mv = d.low_cell_mv;
            self.critical_cell_mv = d.critical_cell_mv;
        }
        self
    }
}

fn bounded(value: &mut u32, default: u32, bounds: RangeInclusive<u32>, key: &str) {
    if !bounds.contains(value) {
        tracing::warn!(
            key,
            value = *value,
            min = *bounds.start(),
            max = *bounds.end(),
            default,
            "battery config value out of bounds; using the default"
        );
        *value = default;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(yaml: &str) -> BatteryConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        BatteryConfig::load_from(&path)
    }

    #[test]
    fn an_absent_file_or_block_reads_as_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            BatteryConfig::load_from(&dir.path().join("missing.yaml")),
            BatteryConfig::default()
        );
        assert_eq!(load("agent:\n  name: bench\n"), BatteryConfig::default());
    }

    #[test]
    fn a_partial_block_keeps_the_defaults_for_the_rest() {
        let cfg = load("battery:\n  low_cell_mv: 3600\n  reserve_percent: 30\n");
        assert_eq!(cfg.low_cell_mv, 3600);
        assert_eq!(cfg.reserve_percent, 30);
        assert_eq!(cfg.critical_cell_mv, 3300);
        assert!(cfg.enabled);
    }

    #[test]
    fn an_out_of_bounds_value_falls_back_alone() {
        let cfg = load("battery:\n  cell_divergence_mv: 5\n  predictive_window_s: 60\n");
        assert_eq!(cfg.cell_divergence_mv, 50);
        assert_eq!(cfg.predictive_window_s, 60);
    }

    #[test]
    fn bounds_are_inclusive() {
        let cfg = load(
            "battery:\n  low_cell_mv: 4200\n  critical_cell_mv: 2500\n  temp_spike_dc_per_s: 200\n",
        );
        assert_eq!(cfg.low_cell_mv, 4200);
        assert_eq!(cfg.critical_cell_mv, 2500);
        assert_eq!(cfg.temp_spike_dc_per_s, 200);
    }

    #[test]
    fn an_inverted_cell_pair_resets_both_thresholds() {
        let cfg = load("battery:\n  low_cell_mv: 2600\n  critical_cell_mv: 2700\n");
        assert_eq!((cfg.low_cell_mv, cfg.critical_cell_mv), (3500, 3300));
        let equal = load("battery:\n  low_cell_mv: 3400\n  critical_cell_mv: 3400\n");
        assert_eq!((equal.low_cell_mv, equal.critical_cell_mv), (3500, 3300));
    }
}
