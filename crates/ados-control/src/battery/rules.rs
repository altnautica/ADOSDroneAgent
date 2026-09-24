//! The anomaly rules. Stateless: each takes the previous and current sample
//! (and the projection) and reports whether it fires. Hysteresis is the
//! engine's job.

use serde::Serialize;

use super::config::BatteryConfig;
use super::predict::{Prediction, PredictionState};
use super::{divergence_mv, Sample};

/// A rate rule compares two samples only when they are at most this far apart;
/// across a longer gap the rate is not a real drop or spike.
const MAX_RATE_GAP_S: f64 = 5.0;

/// `predictive_low` fires when the projected time to reserve is under this.
pub const PREDICTIVE_LOW_ETA_S: i64 = 60;

/// The rules, in evaluation order. The derived order is the order anomalies
/// are listed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleId {
    CellCritical,
    CellLow,
    CellDivergence,
    VoltageDrop,
    TempSpike,
    PredictiveLow,
}

impl RuleId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CellCritical => "cell_critical",
            Self::CellLow => "cell_low",
            Self::CellDivergence => "cell_divergence",
            Self::VoltageDrop => "voltage_drop",
            Self::TempSpike => "temp_spike",
            Self::PredictiveLow => "predictive_low",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Critical,
}

/// A rule that fired, with the reading that fired it and the limit it crossed.
/// Units per rule: cell rules in volts, divergence in millivolts, voltage drop
/// in volts per second, temperature spike in °C per second, predictive low in
/// seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Finding {
    pub rule: RuleId,
    pub severity: Severity,
    pub value: f64,
    pub threshold: f64,
}

/// Every rule that fires for `curr`, in [`RuleId`] order.
pub fn evaluate(
    prev: Option<&Sample>,
    curr: &Sample,
    config: &BatteryConfig,
    prediction: &Prediction,
) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut fire = |rule, severity, value, threshold| {
        out.push(Finding {
            rule,
            severity,
            value,
            threshold,
        })
    };

    if let Some((_, min, max)) = curr.cell_extremes() {
        let critical = mv_to_v(config.critical_cell_mv);
        let low = mv_to_v(config.low_cell_mv);
        if min < critical {
            fire(RuleId::CellCritical, Severity::Critical, min, critical);
        } else if min < low {
            fire(RuleId::CellLow, Severity::Warning, min, low);
        }
        let divergence = divergence_mv(min, max);
        let limit = f64::from(config.cell_divergence_mv);
        if divergence > limit {
            fire(RuleId::CellDivergence, Severity::Warning, divergence, limit);
        }
    }

    if let Some(prev) = prev {
        let dt = (curr.t_ms - prev.t_ms) as f64 / 1000.0;
        if dt > 0.0 && dt <= MAX_RATE_GAP_S {
            if let (Some(before), Some(now)) = (prev.voltage_v, curr.voltage_v) {
                let rate = (before - now) / dt;
                let limit = mv_to_v(config.voltage_drop_mv_per_s);
                if rate > limit {
                    fire(RuleId::VoltageDrop, Severity::Warning, rate, limit);
                }
            }
            if let (Some(before), Some(now)) = (prev.temperature_c, curr.temperature_c) {
                let rate = (now - before) / dt;
                let limit = f64::from(config.temp_spike_dc_per_s) / 10.0;
                if rate > limit {
                    fire(RuleId::TempSpike, Severity::Warning, rate, limit);
                }
            }
        }
    }

    if matches!(
        prediction.state,
        PredictionState::Normal | PredictionState::High
    ) {
        if let Some(eta) = prediction.eta_s.filter(|&eta| eta < PREDICTIVE_LOW_ETA_S) {
            fire(
                RuleId::PredictiveLow,
                Severity::Warning,
                eta as f64,
                PREDICTIVE_LOW_ETA_S as f64,
            );
        }
    }
    out
}

fn mv_to_v(mv: u32) -> f64 {
    f64::from(mv) / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::cells_plausible;

    /// A healthy 4S sample at `t_ms`, 15.6 V, 30 °C, 80 %.
    fn sample(t_ms: i64) -> Sample {
        with_cells(t_ms, &[3.9, 3.9, 3.9, 3.9])
    }

    fn with_cells(t_ms: i64, cells: &[f64]) -> Sample {
        let plausible = cells_plausible(cells);
        Sample {
            t_ms,
            cells: cells.to_vec(),
            cells_plausible: plausible,
            voltage_v: Some(if plausible {
                cells.iter().sum()
            } else {
                cells[0]
            }),
            current_a: Some(-10.0),
            remaining_pct: Some(80),
            temperature_c: Some(30.0),
            consumed_mah: None,
            consumed_wh: None,
        }
    }

    fn rules(prev: Option<&Sample>, curr: &Sample, config: &BatteryConfig) -> Vec<RuleId> {
        evaluate(prev, curr, config, &Prediction::NONE)
            .iter()
            .map(|f| f.rule)
            .collect()
    }

    fn defaults() -> BatteryConfig {
        BatteryConfig::default()
    }

    #[test]
    fn a_healthy_sample_fires_nothing() {
        assert!(rules(Some(&sample(1000)), &sample(2000), &defaults()).is_empty());
    }

    #[test]
    fn a_cell_under_the_low_threshold_fires_cell_low_only() {
        // Cells matched within the divergence limit, so only the low rule fires.
        let found = evaluate(
            None,
            &with_cells(1000, &[3.48, 3.48, 3.48, 3.45]),
            &defaults(),
            &Prediction::NONE,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rule, RuleId::CellLow);
        assert_eq!(found[0].severity, Severity::Warning);
        assert_eq!((found[0].value, found[0].threshold), (3.45, 3.5));
    }

    #[test]
    fn a_cell_under_the_critical_threshold_escalates_without_cell_low() {
        let found = evaluate(
            None,
            &with_cells(1000, &[3.9, 3.9, 3.9, 3.2]),
            &defaults(),
            &Prediction::NONE,
        );
        let ids: Vec<RuleId> = found.iter().map(|f| f.rule).collect();
        assert!(ids.contains(&RuleId::CellCritical));
        assert!(!ids.contains(&RuleId::CellLow));
        assert_eq!(found[0].severity, Severity::Critical);
    }

    #[test]
    fn a_cell_exactly_at_a_threshold_is_not_below_it() {
        let at_low = with_cells(1000, &[3.9, 3.9, 3.9, 3.5]);
        assert!(!rules(None, &at_low, &defaults()).contains(&RuleId::CellLow));
        let at_critical = with_cells(1000, &[3.35, 3.35, 3.35, 3.3]);
        assert_eq!(
            rules(None, &at_critical, &defaults()),
            vec![RuleId::CellLow]
        );
    }

    #[test]
    fn a_spread_over_the_threshold_fires_cell_divergence() {
        let found = evaluate(
            None,
            &with_cells(1000, &[3.9, 3.9, 3.9, 3.83]),
            &defaults(),
            &Prediction::NONE,
        );
        let div = found
            .iter()
            .find(|f| f.rule == RuleId::CellDivergence)
            .expect("divergence fires");
        assert_eq!((div.value, div.threshold), (70.0, 50.0));
        // Exactly the threshold does not fire.
        let at_limit = with_cells(1000, &[3.9, 3.9, 3.9, 3.85]);
        assert!(!rules(None, &at_limit, &defaults()).contains(&RuleId::CellDivergence));
    }

    #[test]
    fn a_fast_voltage_fall_fires_voltage_drop() {
        let mut prev = sample(0);
        prev.voltage_v = Some(16.0);
        let mut curr = sample(1000);
        curr.voltage_v = Some(14.8);
        let found = evaluate(Some(&prev), &curr, &defaults(), &Prediction::NONE);
        let drop = found
            .iter()
            .find(|f| f.rule == RuleId::VoltageDrop)
            .expect("drop fires");
        assert!((drop.value - 1.2).abs() < 1e-9);
        assert_eq!(drop.threshold, 0.5);
    }

    #[test]
    fn a_gap_too_long_to_be_a_real_rate_fires_no_rate_rule() {
        let mut prev = sample(0);
        prev.voltage_v = Some(16.0);
        prev.temperature_c = Some(20.0);
        let mut curr = sample(60_000);
        curr.voltage_v = Some(14.8);
        curr.temperature_c = Some(60.0);
        assert!(rules(Some(&prev), &curr, &defaults()).is_empty());
        // A non-advancing clock is not a rate either.
        let mut same = curr.clone();
        same.t_ms = 0;
        assert!(rules(Some(&prev), &same, &defaults()).is_empty());
    }

    #[test]
    fn a_sudden_temperature_rise_fires_temp_spike() {
        let prev = sample(0);
        let mut curr = sample(1000);
        curr.temperature_c = Some(38.0);
        let found = evaluate(Some(&prev), &curr, &defaults(), &Prediction::NONE);
        let spike = found
            .iter()
            .find(|f| f.rule == RuleId::TempSpike)
            .expect("spike fires");
        assert_eq!((spike.value, spike.threshold), (8.0, 5.0));
    }

    #[test]
    fn an_unknown_temperature_fires_no_temp_spike() {
        let prev = sample(0);
        let mut curr = sample(1000);
        curr.temperature_c = None;
        assert!(!rules(Some(&prev), &curr, &defaults()).contains(&RuleId::TempSpike));
    }

    #[test]
    fn a_short_projection_fires_predictive_low() {
        let prediction = Prediction {
            state: PredictionState::High,
            eta_s: Some(45),
            drop_pct_per_s: Some(1.0),
            mean_current_a: Some(25.0),
        };
        let found = evaluate(None, &sample(1000), &defaults(), &prediction);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rule, RuleId::PredictiveLow);
        assert_eq!((found[0].value, found[0].threshold), (45.0, 60.0));
        let at_limit = Prediction {
            eta_s: Some(60),
            ..prediction
        };
        assert!(evaluate(None, &sample(1000), &defaults(), &at_limit).is_empty());
    }

    #[test]
    fn an_idle_or_past_projection_fires_no_predictive_low() {
        for state in [PredictionState::Idle, PredictionState::Past] {
            let prediction = Prediction {
                state,
                eta_s: Some(0),
                drop_pct_per_s: Some(1.0),
                mean_current_a: None,
            };
            assert!(evaluate(None, &sample(1000), &defaults(), &prediction).is_empty());
        }
    }

    #[test]
    fn the_configured_thresholds_are_the_ones_applied() {
        let strict = BatteryConfig {
            low_cell_mv: 3950,
            critical_cell_mv: 3900,
            ..defaults()
        };
        let cells = with_cells(1000, &[3.92, 3.92, 3.92, 3.92]);
        assert!(rules(None, &cells, &defaults()).is_empty());
        assert_eq!(rules(None, &cells, &strict), vec![RuleId::CellLow]);
    }

    /// ArduPilot without a per-cell monitor reports the whole pack in
    /// `voltages[0]`. That is not a 16.8 V cell, and must not read as one.
    #[test]
    fn a_whole_pack_value_in_the_cell_array_fires_no_cell_rule() {
        let pack = with_cells(1000, &[16.8]);
        assert!(!pack.cells_plausible);
        let strict = BatteryConfig {
            low_cell_mv: 4200,
            critical_cell_mv: 4000,
            cell_divergence_mv: 10,
            ..defaults()
        };
        assert!(rules(None, &pack, &strict).is_empty());
        // A single implausible entry among real cells disables the cell rules
        // for that sample too.
        let mixed = with_cells(1000, &[3.2, 3.9, 0.0]);
        assert!(rules(None, &mixed, &defaults()).is_empty());
    }
}
