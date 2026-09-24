//! Per-pack battery state: the sample window, the live anomalies with their
//! hysteresis, and the transition history. Pure: the caller supplies the clock
//! and the state snapshot.

use std::collections::{BTreeMap, VecDeque};

use serde::Serialize;
use serde_json::Value;

use super::config::BatteryConfig;
use super::predict::{predict, Point, Prediction};
use super::rules::{evaluate, RuleId, Severity};
use super::{cells_plausible, divergence_mv, Sample};

/// Readings kept per pack: 300 s at the 2 Hz the task samples, which covers
/// the longest configurable projection window.
pub const SAMPLE_CAP: usize = 600;
/// Transitions kept per pack.
pub const HISTORY_CAP: usize = 100;
/// How long a rule must stay quiet before its anomaly clears.
pub const HYSTERESIS_MS: i64 = 5000;
/// With no new sample for this long, the read model reports `stale`.
pub const STALE_AFTER_MS: i64 = 5000;
/// Packs tracked. The router caps the ids it publishes at the same number;
/// this bounds the engine against any other producer.
pub const MAX_PACKS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransitionState {
    Raised,
    Cleared,
}

impl TransitionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raised => "raised",
            Self::Cleared => "cleared",
        }
    }
}

/// A rule currently firing, or quiet for less than [`HYSTERESIS_MS`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveAnomaly {
    pub rule: RuleId,
    pub severity: Severity,
    pub value: f64,
    pub threshold: f64,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    /// When the rule last stopped firing; `null` while it fires.
    pub cleared_at_ms: Option<i64>,
}

/// One raise or final clear, as kept in a pack's history.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnomalyEvent {
    pub rule: RuleId,
    pub severity: Severity,
    pub state: TransitionState,
    pub at_ms: i64,
    /// The reading that raised it, or the last one before it cleared.
    pub value: f64,
}

/// A raise or final clear, handed to the caller to record.
#[derive(Debug, Clone, PartialEq)]
pub struct AnomalyTransition {
    pub pack_id: u8,
    pub rule: RuleId,
    pub severity: Severity,
    pub state: TransitionState,
    pub value: f64,
    pub threshold: f64,
    pub at_ms: i64,
}

#[derive(Debug, Default)]
struct PackState {
    points: VecDeque<Point>,
    latest: Option<Sample>,
    prediction: Prediction,
    live: BTreeMap<RuleId, LiveAnomaly>,
    /// Newest first.
    history: VecDeque<AnomalyEvent>,
}

impl PackState {
    fn ingest(
        &mut self,
        pack_id: u8,
        sample: Sample,
        config: &BatteryConfig,
    ) -> Vec<AnomalyTransition> {
        let now = sample.t_ms;
        if self.points.len() == SAMPLE_CAP {
            self.points.pop_front();
        }
        self.points.push_back(Point {
            t_ms: now,
            remaining_pct: sample.remaining_pct,
            current_a: sample.current_a,
        });
        self.prediction = predict(
            &self.points,
            config.predictive_window_s,
            config.reserve_percent,
        );
        let findings = evaluate(self.latest.as_ref(), &sample, config, &self.prediction);
        self.latest = Some(sample);

        let mut transitions = Vec::new();
        let mut expired = Vec::new();
        for (rule, live) in &mut self.live {
            if findings.iter().any(|f| f.rule == *rule) {
                continue;
            }
            match live.cleared_at_ms {
                None => live.cleared_at_ms = Some(now),
                Some(at) if now - at < HYSTERESIS_MS => {}
                Some(_) => expired.push(*rule),
            }
        }
        for rule in expired {
            if let Some(live) = self.live.remove(&rule) {
                self.record(AnomalyEvent {
                    rule,
                    severity: live.severity,
                    state: TransitionState::Cleared,
                    at_ms: now,
                    value: live.value,
                });
                transitions.push(AnomalyTransition {
                    pack_id,
                    rule,
                    severity: live.severity,
                    state: TransitionState::Cleared,
                    value: live.value,
                    threshold: live.threshold,
                    at_ms: now,
                });
            }
        }
        for f in findings {
            if let Some(live) = self.live.get_mut(&f.rule) {
                live.value = f.value;
                live.threshold = f.threshold;
                live.last_seen_ms = now;
                live.cleared_at_ms = None;
                continue;
            }
            self.live.insert(
                f.rule,
                LiveAnomaly {
                    rule: f.rule,
                    severity: f.severity,
                    value: f.value,
                    threshold: f.threshold,
                    first_seen_ms: now,
                    last_seen_ms: now,
                    cleared_at_ms: None,
                },
            );
            self.record(AnomalyEvent {
                rule: f.rule,
                severity: f.severity,
                state: TransitionState::Raised,
                at_ms: now,
                value: f.value,
            });
            transitions.push(AnomalyTransition {
                pack_id,
                rule: f.rule,
                severity: f.severity,
                state: TransitionState::Raised,
                value: f.value,
                threshold: f.threshold,
                at_ms: now,
            });
        }
        transitions
    }

    fn record(&mut self, event: AnomalyEvent) {
        if self.history.len() == HISTORY_CAP {
            self.history.pop_back();
        }
        self.history.push_front(event);
    }
}

/// The battery health engine: one [`PackState`] per BATTERY_STATUS id.
#[derive(Debug)]
pub struct BatteryEngine {
    config: BatteryConfig,
    packs: BTreeMap<u8, PackState>,
    /// Epoch ms of the last ingested snapshot, 0 before the first.
    updated_at_ms: i64,
}

impl BatteryEngine {
    pub fn new(config: BatteryConfig) -> Self {
        Self {
            config,
            packs: BTreeMap::new(),
            updated_at_ms: 0,
        }
    }

    pub fn config(&self) -> &BatteryConfig {
        &self.config
    }

    /// Apply a new config. Every pack's projection is recomputed at once so
    /// the read model reflects a changed window or reserve before the next
    /// sample. Disabling drops all pack state, so re-enabling starts clean
    /// rather than resurrecting anomalies from before the pause.
    pub fn set_config(&mut self, config: BatteryConfig) {
        if config == self.config {
            return;
        }
        self.config = config;
        if !self.config.enabled {
            self.packs.clear();
            return;
        }
        for pack in self.packs.values_mut() {
            pack.prediction = predict(
                &pack.points,
                self.config.predictive_window_s,
                self.config.reserve_percent,
            );
        }
    }

    /// Ingest one state snapshot taken at `now_ms` (epoch ms) and return the
    /// anomaly transitions it caused.
    ///
    /// A snapshot whose `mavlink_alive` is `false` is skipped: the router keeps
    /// the last battery values after the FC link drops, and a frozen reading
    /// fed in as new samples would look like a steady, healthy pack.
    pub fn ingest(&mut self, now_ms: i64, snapshot: &Value) -> Vec<AnomalyTransition> {
        if !self.config.enabled || snapshot.get("mavlink_alive") == Some(&Value::Bool(false)) {
            return Vec::new();
        }
        self.updated_at_ms = now_ms;
        let mut transitions = Vec::new();
        for (id, sample) in readings(now_ms, snapshot) {
            if !self.packs.contains_key(&id) && self.packs.len() >= MAX_PACKS {
                continue;
            }
            let pack = self.packs.entry(id).or_default();
            transitions.extend(pack.ingest(id, sample, &self.config));
        }
        transitions
    }

    /// The `/api/v1/battery` read model at `now_ms` (epoch ms).
    pub fn snapshot(&self, now_ms: i64) -> BatteryHealth<'_> {
        let enabled = self.config.enabled;
        let packs = if enabled {
            self.packs
                .iter()
                .filter_map(|(&id, pack)| {
                    let latest = pack.latest.as_ref()?;
                    let extremes = latest.cell_extremes();
                    Some(PackHealth {
                        id,
                        cells_plausible: latest.cells_plausible,
                        cell_voltages_v: &latest.cells,
                        weakest_cell_index: extremes.map(|(i, _, _)| i),
                        min_cell_v: extremes.map(|(_, min, _)| min),
                        max_cell_v: extremes.map(|(_, _, max)| max),
                        divergence_mv: extremes.map(|(_, min, max)| divergence_mv(min, max)),
                        voltage_v: latest.voltage_v,
                        current_a: latest.current_a,
                        remaining_pct: latest.remaining_pct,
                        temperature_c: latest.temperature_c,
                        consumed_mah: latest.consumed_mah,
                        consumed_wh: latest.consumed_wh,
                        prediction: pack.prediction,
                        anomalies: pack.live.values().collect(),
                        history: &pack.history,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        BatteryHealth {
            enabled,
            stale: self.updated_at_ms == 0 || now_ms - self.updated_at_ms > STALE_AFTER_MS,
            updated_at_ms: self.updated_at_ms,
            thresholds: &self.config,
            packs,
        }
    }
}

/// The `/api/v1/battery` body.
#[derive(Debug, Serialize)]
pub struct BatteryHealth<'a> {
    pub enabled: bool,
    pub stale: bool,
    pub updated_at_ms: i64,
    pub thresholds: &'a BatteryConfig,
    pub packs: Vec<PackHealth<'a>>,
}

#[derive(Debug, Serialize)]
pub struct PackHealth<'a> {
    pub id: u8,
    pub cells_plausible: bool,
    pub cell_voltages_v: &'a [f64],
    pub weakest_cell_index: Option<usize>,
    pub min_cell_v: Option<f64>,
    pub max_cell_v: Option<f64>,
    pub divergence_mv: Option<f64>,
    pub voltage_v: Option<f64>,
    pub current_a: Option<f64>,
    pub remaining_pct: Option<i64>,
    pub temperature_c: Option<f64>,
    pub consumed_mah: Option<i64>,
    pub consumed_wh: Option<f64>,
    pub prediction: Prediction,
    pub anomalies: Vec<&'a LiveAnomaly>,
    pub history: &'a VecDeque<AnomalyEvent>,
}

/// One sample per pack from a state snapshot. Packs come from `batteries[]`;
/// a snapshot with none but a `battery` block carrying a reading yields pack 0
/// from that block (an FC that sends only SYS_STATUS).
fn readings(t_ms: i64, snapshot: &Value) -> Vec<(u8, Sample)> {
    let sys = snapshot.get("battery");
    let sys_field = |key: &str| sys.and_then(|b| b.get(key));
    let sys_voltage = sys_field("voltage").and_then(Value::as_f64);
    let sys_remaining = sys_field("remaining").and_then(Value::as_i64);

    // Pack 0 is the one SYS_STATUS describes, so only it borrows the
    // SYS_STATUS voltage and remaining percentage when its own are absent.
    let reading = |id: u8, entry: &Value, keys: &ReadingKeys| {
        let field = |key: &str| entry.get(key);
        let cells: Vec<f64> = field(keys.cells)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        let plausible = cells_plausible(&cells);
        let voltage_v = if plausible {
            Some(round_to_mv(cells.iter().sum()))
        } else if cells.len() == 1 {
            Some(cells[0])
        } else if id == 0 {
            sys_voltage
        } else {
            None
        };
        let remaining_pct = field(keys.remaining)
            .and_then(Value::as_i64)
            .or(if id == 0 { sys_remaining } else { None });
        Sample {
            t_ms,
            cells,
            cells_plausible: plausible,
            voltage_v,
            current_a: field(keys.current).and_then(Value::as_f64),
            remaining_pct,
            temperature_c: field(keys.temperature).and_then(Value::as_f64),
            consumed_mah: field("consumed_mah").and_then(Value::as_i64),
            consumed_wh: field("consumed_wh").and_then(Value::as_f64),
        }
    };

    let packs = snapshot
        .get("batteries")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty());
    match packs {
        Some(packs) => packs
            .iter()
            .filter_map(|entry| {
                let id = u8::try_from(entry.get("id")?.as_u64()?).ok()?;
                Some((id, reading(id, entry, &PACK_KEYS)))
            })
            .collect(),
        None => {
            let Some(block) = sys else {
                return Vec::new();
            };
            let has_cells = block
                .get("cell_voltages")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty());
            if sys_voltage.is_none() && sys_remaining.is_none() && !has_cells {
                return Vec::new();
            }
            vec![(0, reading(0, block, &SYS_KEYS))]
        }
    }
}

/// The field names a reading is taken from: a `batteries[]` entry or the
/// single-pack `battery` block.
struct ReadingKeys {
    cells: &'static str,
    current: &'static str,
    remaining: &'static str,
    temperature: &'static str,
}

const PACK_KEYS: ReadingKeys = ReadingKeys {
    cells: "cell_voltages",
    current: "current_a",
    remaining: "remaining_pct",
    temperature: "temperature_c",
};

const SYS_KEYS: ReadingKeys = ReadingKeys {
    cells: "cell_voltages",
    current: "current",
    remaining: "remaining",
    temperature: "temperature",
};

/// Cell readings are whole millivolts; summing their volt values leaves a
/// float residue that rounding to the millivolt removes.
fn round_to_mv(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
