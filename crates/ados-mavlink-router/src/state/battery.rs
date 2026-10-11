//! Per-pack battery readings.

use super::*;

/// One battery as its own BATTERY_STATUS stream reports it, keyed by the
/// message `id`. Every reading MAVLink lets an autopilot mark "not measured" is
/// an `Option` published as `null`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatteryPack {
    pub id: u8,
    /// Volts, with the unfilled-cell sentinel (`UINT16_MAX`) dropped.
    pub cell_voltages: Vec<f64>,
    /// Amps; `None` when `current_battery` is -1.
    pub current_a: Option<f64>,
    /// Percent; `None` when `battery_remaining` is -1.
    pub remaining_pct: Option<i64>,
    /// Degrees Celsius; `None` when `temperature` is `INT16_MAX`.
    pub temperature_c: Option<f64>,
    /// mAh; `None` when `current_consumed` is -1.
    pub consumed_mah: Option<i64>,
    /// Wh (the wire's hJ / 36); `None` when `energy_consumed` is -1.
    pub consumed_wh: Option<f64>,
    /// `MAV_BATTERY_FUNCTION` as its numeric value.
    pub function: u8,
    /// When this id's last BATTERY_STATUS arrived. Published as `age_ms`, so a
    /// pack whose stream stopped is told apart from one still reporting.
    pub received_at: Option<std::time::Instant>,
}

impl BatteryPack {
    pub(super) fn from_status(m: &BATTERY_STATUS_DATA) -> Self {
        Self {
            id: m.id,
            cell_voltages: cell_voltages(&m.voltages),
            current_a: (m.current_battery != -1).then(|| m.current_battery as f64 / 100.0),
            remaining_pct: (m.battery_remaining != -1).then_some(m.battery_remaining as i64),
            temperature_c: (m.temperature != i16::MAX).then(|| m.temperature as f64 / 100.0),
            consumed_mah: (m.current_consumed != -1).then_some(m.current_consumed as i64),
            consumed_wh: (m.energy_consumed != -1).then(|| m.energy_consumed as f64 / 36.0),
            function: m.battery_function as u8,
            received_at: Some(std::time::Instant::now()),
        }
    }

    pub(super) fn to_wire(&self) -> Value {
        let age_ms = self.received_at.map(|at| {
            std::time::Instant::now()
                .saturating_duration_since(at)
                .as_millis() as u64
        });
        json!({
            "id": self.id,
            "cell_voltages": self.cell_voltages,
            "current_a": self.current_a,
            "remaining_pct": self.remaining_pct,
            "temperature_c": self.temperature_c,
            "consumed_mah": self.consumed_mah,
            "consumed_wh": self.consumed_wh,
            "function": self.function,
            "age_ms": age_ms,
        })
    }
}

/// BATTERY_STATUS cell voltages in volts, with the unfilled-cell sentinel
/// (`UINT16_MAX`) dropped.
pub(super) fn cell_voltages(raw: &[u16]) -> Vec<f64> {
    raw.iter()
        .filter(|&&v| v != 0xFFFF)
        .map(|&v| v as f64 / 1000.0)
        .collect()
}
