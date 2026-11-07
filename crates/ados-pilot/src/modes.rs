//! ArduPilot custom-mode numbers the pilot commands and classifies.

use ados_protocol::flight_modes::ArduPilotFirmware;
use ados_protocol::pilot::VehicleClass;

/// Copter modes.
pub mod copter {
    pub const ALT_HOLD: u32 = 2;
    pub const AUTO: u32 = 3;
    pub const GUIDED: u32 = 4;
    pub const LOITER: u32 = 5;
    pub const RTL: u32 = 6;
    pub const LAND: u32 = 9;
    pub const AUTOTUNE: u32 = 15;
    pub const POSHOLD: u32 = 16;
    pub const BRAKE: u32 = 17;
    pub const SMART_RTL: u32 = 21;
    pub const AUTO_RTL: u32 = 27;
}

/// Plane (and QuadPlane) modes.
pub mod plane {
    pub const FBWA: u32 = 5;
    pub const CRUISE: u32 = 7;
    pub const AUTOTUNE: u32 = 8;
    pub const AUTO: u32 = 10;
    pub const RTL: u32 = 11;
    pub const LOITER: u32 = 12;
    pub const TAKEOFF: u32 = 13;
    pub const GUIDED: u32 = 15;
    pub const QHOVER: u32 = 18;
    pub const QLOITER: u32 = 19;
    pub const QLAND: u32 = 20;
    pub const QRTL: u32 = 21;
    pub const QAUTOTUNE: u32 = 22;
}

/// The firmware family the pilot drives, from the HEARTBEAT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Copter,
    Plane,
}

impl Family {
    /// Family from HEARTBEAT `type` (fixed-wing and VTOL types fly ArduPlane).
    pub fn from_class(class: VehicleClass) -> Self {
        match class {
            VehicleClass::Multirotor => Self::Copter,
            VehicleClass::Fixedwing | VehicleClass::Vtol => Self::Plane,
        }
    }

    pub fn guided(self) -> u32 {
        match self {
            Self::Copter => copter::GUIDED,
            Self::Plane => plane::GUIDED,
        }
    }

    pub fn auto(self) -> u32 {
        match self {
            Self::Copter => copter::AUTO,
            Self::Plane => plane::AUTO,
        }
    }

    pub fn rtl(self) -> u32 {
        match self {
            Self::Copter => copter::RTL,
            Self::Plane => plane::RTL,
        }
    }

    pub fn land(self, vtol: bool) -> u32 {
        match self {
            Self::Copter => copter::LAND,
            Self::Plane if vtol => plane::QLAND,
            Self::Plane => plane::RTL,
        }
    }

    pub fn is_auto(self, mode: u32) -> bool {
        match self {
            Self::Copter => mode == copter::AUTO || mode == copter::AUTO_RTL,
            Self::Plane => mode == plane::AUTO,
        }
    }

    pub fn is_autotune(self, mode: u32) -> bool {
        match self {
            Self::Copter => mode == copter::AUTOTUNE,
            Self::Plane => mode == plane::AUTOTUNE || mode == plane::QAUTOTUNE,
        }
    }

    pub fn is_rtl(self, mode: u32) -> bool {
        match self {
            Self::Copter => matches!(mode, copter::RTL | copter::SMART_RTL),
            Self::Plane => matches!(mode, plane::RTL | plane::QRTL),
        }
    }

    pub fn is_land(self, mode: u32) -> bool {
        match self {
            Self::Copter => mode == copter::LAND,
            Self::Plane => mode == plane::QLAND,
        }
    }

    /// The hold mode `hand_to_fc` selects.
    pub fn hand_mode(self, name: &str) -> u32 {
        match (self, name) {
            (Self::Copter, "poshold") => copter::POSHOLD,
            (Self::Copter, "althold") => copter::ALT_HOLD,
            (Self::Copter, _) => copter::LOITER,
            (Self::Plane, _) => plane::LOITER,
        }
    }

    /// Display name of a custom mode.
    pub fn mode_name(self, mode: u32) -> String {
        let fw = match self {
            Self::Copter => ArduPilotFirmware::Copter,
            Self::Plane => ArduPilotFirmware::Plane,
        };
        fw.mode_name(mode)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("MODE {mode}"))
    }
}
