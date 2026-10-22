//! Several RC transmitter modules on one ground node (`radio.crsf.modules`).
//!
//! Each configured module is its own serial transport with its own
//! link-statistics state. Exactly one module carries the `pilot` role: it is
//! the RC lane the channel-source authority merge drives (HID / inject /
//! hybrid, unchanged). Every `autonomy` module runs its own transmitter under
//! an inject-only merge, fed by the programmatic channel path addressed by the
//! module id. When `modules` is empty the legacy single `device` pin resolves
//! to one pilot module `m0`, so an existing config keeps its exact behaviour.
//!
//! The per-module status object (`modules[]` on the `crsf-stats` sidecar):
//!
//! ```json
//! {"id":"m0","port":"/dev/ttyACM1","role":"pilot",
//!  "link":{"rssi_dbm":[-60,-62],"lq":100,"snr_db":8,"tx_power_mw":250,"rf_mode":"500Hz"},
//!  "connected":true,"domain":null}
//! ```
//!
//! `connected` is true while the module's serial transport is open. `link` is
//! the last link-statistics frame when it is fresh, else `null`. `domain` is
//! the regulatory domain name the module firmware itself reports (for
//! ExpressLRS: `ISM2G4`, `CE_LBT`, `FCC915`, `AU915`, `EU868`, `IN866`,
//! `AU433`, `EU433`, `US433`, `US433W`), verbatim. Link statistics do not
//! carry it and the lane reads no device-information frame, so it is `null`.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::sidecar::rssi_dbm_from_wire;
use crate::transport::TelemetryState;
use ados_crsf_codec::link::STATS_FRESH_WINDOW;
use ados_crsf_codec::telemetry::LinkStatistics;

/// The id the legacy single-port config resolves to.
pub const LEGACY_MODULE_ID: &str = "m0";

/// What a module is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleRole {
    /// The operator RC lane, under the configured channel-source authority.
    Pilot,
    /// A programmatic RC lane, driven only by channel injection addressed to
    /// this module's id.
    Autonomy,
}

impl ModuleRole {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "pilot" => Some(Self::Pilot),
            "autonomy" => Some(Self::Autonomy),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pilot => "pilot",
            Self::Autonomy => "autonomy",
        }
    }
}

/// One `radio.crsf.modules[]` entry as it sits in the config.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ModuleSection {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub port: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub link: Option<String>,
}

/// One validated module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleConfig {
    /// Stable id, unique across the list (`m0`, `m1`, ...).
    pub id: String,
    /// The serial device the module is attached to.
    pub port: String,
    pub role: ModuleRole,
    /// Optional operator label naming the vehicle link this module serves.
    /// Carried through for operators; the lane does not interpret it.
    pub link: Option<String>,
}

/// Validate the configured list. An entry with no id, no port, an unknown
/// role, a repeated id or port, or a second pilot is dropped with a warning:
/// a typo degrades loudly to the remaining modules, never a crash-loop and
/// never two transmitters on one port. An absent role reads `pilot`.
pub fn validate(sections: Vec<ModuleSection>) -> Vec<ModuleConfig> {
    let mut out: Vec<ModuleConfig> = Vec::with_capacity(sections.len());
    for (index, section) in sections.into_iter().enumerate() {
        let id = section.id.as_deref().unwrap_or("").trim().to_string();
        let port = section.port.as_deref().unwrap_or("").trim().to_string();
        let role = match section.role.as_deref() {
            None => Some(ModuleRole::Pilot),
            Some(s) => ModuleRole::parse(s),
        };
        let reject = if id.is_empty() {
            Some("missing id")
        } else if port.is_empty() {
            Some("missing port")
        } else if role.is_none() {
            Some("unknown role")
        } else if out.iter().any(|m| m.id == id) {
            Some("duplicate id")
        } else if out.iter().any(|m| m.port == port) {
            Some("duplicate port")
        } else if role == Some(ModuleRole::Pilot) && out.iter().any(|m| m.role == ModuleRole::Pilot)
        {
            Some("second pilot module")
        } else {
            None
        };
        if let Some(reason) = reject {
            tracing::warn!(index, id = %id, port = %port, reason, "crsf module entry dropped");
            continue;
        }
        out.push(ModuleConfig {
            id,
            port,
            role: role.unwrap_or(ModuleRole::Pilot),
            link: section
                .link
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty()),
        });
    }
    out
}

/// The module set the lane runs: the configured list when it has entries,
/// else the legacy `device` pin as one pilot module `m0` (none when no device
/// is pinned).
pub fn resolve(modules: &[ModuleConfig], legacy_device: &str) -> Vec<ModuleConfig> {
    if !modules.is_empty() {
        return modules.to_vec();
    }
    if legacy_device.is_empty() {
        return Vec::new();
    }
    vec![ModuleConfig {
        id: LEGACY_MODULE_ID.to_string(),
        port: legacy_device.to_string(),
        role: ModuleRole::Pilot,
        link: None,
    }]
}

/// The ExpressLRS packet-rate label for a link-statistics `rf_mode` index
/// (the RFMD index the module reports). An index outside the known table
/// reads as its number, never a guessed rate.
pub fn rf_mode_label(index: u8) -> String {
    let label = match index {
        0 => "4Hz",
        1 => "25Hz",
        2 => "50Hz",
        3 => "100Hz",
        4 => "100Hz Full",
        5 => "150Hz",
        6 => "200Hz",
        7 => "250Hz",
        8 => "333Hz Full",
        9 => "500Hz",
        10 => "D250",
        11 => "D500",
        12 => "F500",
        13 => "F1000",
        14 => "D50",
        other => return other.to_string(),
    };
    label.to_string()
}

/// A module's link readings off its last fresh link-statistics frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModuleLink {
    /// Uplink RSSI, `[antenna 1, antenna 2]`, dBm.
    pub rssi_dbm: [i64; 2],
    /// Uplink link quality, 0..=100.
    pub lq: u8,
    /// Uplink SNR, dB.
    pub snr_db: i8,
    /// Uplink TX power in mW; `null` for an index outside the power table.
    pub tx_power_mw: Option<u16>,
    /// The packet-rate label for the reported RF mode.
    pub rf_mode: String,
}

impl ModuleLink {
    pub fn from_stats(stats: &LinkStatistics) -> Self {
        Self {
            rssi_dbm: [
                rssi_dbm_from_wire(stats.uplink_rssi_ant1),
                rssi_dbm_from_wire(stats.uplink_rssi_ant2),
            ],
            lq: stats.uplink_lq,
            snr_db: stats.uplink_snr,
            tx_power_mw: stats.uplink_tx_power_mw(),
            rf_mode: rf_mode_label(stats.rf_mode),
        }
    }
}

/// One `modules[]` entry on the sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModuleStatus {
    pub id: String,
    pub port: String,
    pub role: ModuleRole,
    pub link: Option<ModuleLink>,
    pub connected: bool,
    /// The firmware-reported regulatory domain name, verbatim; `null` while
    /// the module does not expose it to the lane.
    pub domain: Option<String>,
}

impl ModuleStatus {
    /// `fresh_link` must already be freshness-gated by the caller: a stale
    /// reading re-surfacing as current would be a lying surface.
    pub fn new(
        module: &ModuleConfig,
        connected: bool,
        fresh_link: Option<&LinkStatistics>,
    ) -> Self {
        Self {
            id: module.id.clone(),
            port: module.port.clone(),
            role: module.role,
            link: fresh_link.map(ModuleLink::from_stats),
            connected,
            domain: None,
        }
    }
}

/// The last link-statistics frame when it is inside the freshness window.
pub fn fresh_link(state: &TelemetryState, now: Instant) -> Option<LinkStatistics> {
    match (state.stats_age(now), state.last_link_stats.as_ref()) {
        (Some(age), Some((stats, _))) if age <= STATS_FRESH_WINDOW => Some(*stats),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
