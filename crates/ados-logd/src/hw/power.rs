//! hwmon power-rail reader: voltage, current, and power channels.
//!
//! Power-monitor chips (INA2xx and friends) expose their channels under
//! `/sys/class/hwmon/hwmon*` as:
//!
//! - `in*_input` — voltage in millivolts,
//! - `curr*_input` — current in milliamps,
//! - `power*_input` — power in microwatts,
//!
//! each optionally paired with a `*_label`. This reader collects all three
//! kinds per chip, keyed by chip name + label, so a rail sag during a brownout
//! is recorded over time. A board with no power-monitor hwmon yields an empty
//! list; a channel with no readable input is skipped.

use std::path::Path;

use super::reader::{list_dir, read_i64, read_trimmed, under};
use super::thermal::channel_id;

/// The electrical quantity a hwmon channel measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailKind {
    /// Voltage, in millivolts.
    Voltage,
    /// Current, in milliamps.
    Current,
    /// Power, in microwatts.
    Power,
}

impl RailKind {
    /// The sysfs file prefix for this kind (`in`, `curr`, `power`).
    fn prefix(self) -> &'static str {
        match self {
            RailKind::Voltage => "in",
            RailKind::Current => "curr",
            RailKind::Power => "power",
        }
    }
}

/// One hwmon electrical-rail reading.
#[derive(Debug, Clone, PartialEq)]
pub struct Rail {
    /// The hwmon chip `name`.
    pub chip: String,
    /// The channel `*_label`, or the channel id when no label exists.
    pub label: String,
    /// Which quantity was measured.
    pub kind: RailKind,
    /// The raw reading in the sysfs unit (mV / mA / µW per [`RailKind`]).
    pub value: f32,
}

/// Read every voltage / current / power channel under each
/// `/sys/class/hwmon/hwmon*` chip. The raw sysfs units are preserved (mV / mA /
/// µW); conversion is left to the read edge so no precision is lost in storage.
pub fn read_power_rails(root: &Path) -> Vec<Rail> {
    let base = under(root, "/sys/class/hwmon");
    let mut out = Vec::new();
    for entry in list_dir(&base) {
        let cdir = base.join(&entry);
        let chip = read_trimmed(&cdir.join("name")).unwrap_or_else(|| entry.clone());
        let files = list_dir(&cdir);
        for kind in [RailKind::Voltage, RailKind::Current, RailKind::Power] {
            let prefix = kind.prefix();
            for file in &files {
                let Some(id) = channel_id(file, prefix, "_input") else {
                    continue;
                };
                let Some(raw) = read_i64(&cdir.join(file)) else {
                    continue;
                };
                let label = read_trimmed(&cdir.join(format!("{prefix}{id}_label")))
                    .unwrap_or_else(|| format!("{prefix}{id}"));
                out.push(Rail {
                    chip: chip.clone(),
                    label,
                    kind,
                    value: raw as f32,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
