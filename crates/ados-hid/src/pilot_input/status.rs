//! The control mode as the ground station sees it, and the runtime sidecar
//! (`/run/ados/gs-input-state.json`) that `ados-input` publishes for the REST
//! front (`GET /api/v1/ground-station/input`) and the CRSF lane.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Sidecar file name under the run dir.
pub const GS_INPUT_STATE: &str = "gs-input-state.json";

/// The sidecar is stale (its writer is gone) after this long.
pub const GS_INPUT_STATE_STALE_MS: u64 = 3000;

/// `control.mode` (per drone; the GS learns it from the relayed pilot state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    #[default]
    Ados,
    FcManual,
    Elrs,
}

impl ControlMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ados" => Some(Self::Ados),
            "fc_manual" => Some(Self::FcManual),
            "elrs" => Some(Self::Elrs),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ados => "ados",
            Self::FcManual => "fc_manual",
            Self::Elrs => "elrs",
        }
    }
}

/// Where the effective control mode came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeSource {
    /// No pilot state yet: the schema default.
    Default,
    /// The drone's relayed pilot state.
    Drone,
    /// The ground-station emergency switch, latched until the drone echoes it.
    Emergency,
}

/// The attached controller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Controller {
    pub id: String,
    pub name: String,
    /// `usb` or `bluetooth`.
    pub connection: String,
    pub driver: String,
    pub battery_pct: Option<u8>,
}

/// The sidecar body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GsInputState {
    pub v: u32,
    /// Wall clock of the write, ms since the epoch.
    pub ts_ms: u64,
    /// The ground station flies the sticks and the safety buttons natively
    /// (the kiosk browser must ignore them).
    pub native_sticks: bool,
    /// The GS pilot relay socket is connected.
    pub pilot_relay: bool,
    /// Sticks are streaming to the pilot right now.
    pub streaming: bool,
    pub control_mode: ControlMode,
    pub control_mode_source: ModeSource,
    pub controller: Option<Controller>,
}

pub fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl GsInputState {
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        ados_protocol::sidecar::write_json_atomic(path, self, 0o644)
    }

    /// The sidecar when present and fresh at `now_ms` (wall clock).
    pub fn load_fresh(path: &Path, now_ms: u64) -> Option<Self> {
        let s: Self = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        (now_ms.saturating_sub(s.ts_ms) <= GS_INPUT_STATE_STALE_MS).then_some(s)
    }
}

/// The control mode the CRSF lane obeys: the sidecar's mode whatever its age
/// (the last mode the drone reported stays in force while `ados-input` restarts),
/// and the schema default `ados` when no sidecar exists.
pub fn read_control_mode(path: &Path) -> ControlMode {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<GsInputState>(&b).ok())
        .map(|s| s.control_mode)
        .unwrap_or_default()
}

/// The effective mode from the latest drone report and the emergency latch.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModeTracker {
    drone: Option<ControlMode>,
    emergency: bool,
}

impl ModeTracker {
    /// A drone report. The emergency latch clears once the drone echoes `elrs`.
    pub fn report(&mut self, mode: ControlMode) {
        self.drone = Some(mode);
        if mode == ControlMode::Elrs {
            self.emergency = false;
        }
    }

    /// The operator held the emergency combo.
    pub fn emergency(&mut self) {
        self.emergency = true;
    }

    pub fn effective(&self) -> (ControlMode, ModeSource) {
        if self.emergency {
            (ControlMode::Elrs, ModeSource::Emergency)
        } else if let Some(m) = self.drone {
            (m, ModeSource::Drone)
        } else {
            (ControlMode::Ados, ModeSource::Default)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emergency_latch_holds_until_the_drone_echoes_elrs() {
        let mut t = ModeTracker::default();
        assert_eq!(t.effective(), (ControlMode::Ados, ModeSource::Default));
        t.report(ControlMode::Ados);
        t.emergency();
        t.report(ControlMode::Ados);
        assert_eq!(t.effective(), (ControlMode::Elrs, ModeSource::Emergency));
        t.report(ControlMode::Elrs);
        assert_eq!(t.effective(), (ControlMode::Elrs, ModeSource::Drone));
        t.report(ControlMode::Ados);
        assert_eq!(t.effective(), (ControlMode::Ados, ModeSource::Drone));
    }

    #[test]
    fn sidecar_round_trip_and_mode_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(GS_INPUT_STATE);
        assert_eq!(read_control_mode(&p), ControlMode::Ados);
        let s = GsInputState {
            v: 1,
            ts_ms: 10_000,
            native_sticks: true,
            pilot_relay: true,
            streaming: false,
            control_mode: ControlMode::Elrs,
            control_mode_source: ModeSource::Emergency,
            controller: None,
        };
        s.save(&p).unwrap();
        assert_eq!(read_control_mode(&p), ControlMode::Elrs);
        assert_eq!(GsInputState::load_fresh(&p, 11_000), Some(s));
        assert_eq!(GsInputState::load_fresh(&p, 20_000), None);
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["control_mode"], "elrs");
        assert_eq!(v["control_mode_source"], "emergency");
    }
}
