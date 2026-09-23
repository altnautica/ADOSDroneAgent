//! ArduPilot flight-mode tables, keyed by the vehicle type the FC reports.
//!
//! An ArduPilot `custom_mode` number means a different mode on each firmware:
//! `6` is RTL on Copter, FBWB on Plane and FOLLOW on Rover. The mode name the
//! state snapshot reports (decode) and the `custom_mode` a `DO_SET_MODE` command
//! carries (encode) must therefore both come from the table for the vehicle that
//! is actually connected, selected by `HEARTBEAT.type` (MAV_TYPE). This module is
//! the one copy of those tables: the MAVLink router decodes with it and the
//! control surface encodes with it, so the two can never disagree.
//!
//! PX4 packs its mode into `custom_mode` differently and is keyed on
//! `HEARTBEAT.autopilot` ([`MAV_AUTOPILOT_PX4`]), not on MAV_TYPE.

/// `HEARTBEAT.autopilot` for PX4 (`MAV_AUTOPILOT_PX4`). Any other autopilot
/// value is decoded and encoded through the ArduPilot tables below.
pub const MAV_AUTOPILOT_PX4: i64 = 12;

/// ArduCopter `custom_mode` -> mode name.
const COPTER_MODES: &[(u32, &str)] = &[
    (0, "STABILIZE"),
    (1, "ACRO"),
    (2, "ALT_HOLD"),
    (3, "AUTO"),
    (4, "GUIDED"),
    (5, "LOITER"),
    (6, "RTL"),
    (7, "CIRCLE"),
    (9, "LAND"),
    (11, "DRIFT"),
    (13, "SPORT"),
    (14, "FLIP"),
    (15, "AUTOTUNE"),
    (16, "POSHOLD"),
    (17, "BRAKE"),
    (18, "THROW"),
    (19, "AVOID_ADSB"),
    (20, "GUIDED_NOGPS"),
    (21, "SMART_RTL"),
    (22, "FLOWHOLD"),
    (23, "FOLLOW"),
    (24, "ZIGZAG"),
    (25, "SYSTEMID"),
    (26, "AUTOROTATE"),
    (27, "AUTO_RTL"),
];

/// ArduPlane (including QuadPlane) `custom_mode` -> mode name.
const PLANE_MODES: &[(u32, &str)] = &[
    (0, "MANUAL"),
    (1, "CIRCLE"),
    (2, "STABILIZE"),
    (3, "TRAINING"),
    (4, "ACRO"),
    (5, "FBWA"),
    (6, "FBWB"),
    (7, "CRUISE"),
    (8, "AUTOTUNE"),
    (10, "AUTO"),
    (11, "RTL"),
    (12, "LOITER"),
    (14, "AVOID_ADSB"),
    (15, "GUIDED"),
    (17, "QSTABILIZE"),
    (18, "QHOVER"),
    (19, "QLOITER"),
    (20, "QLAND"),
    (21, "QRTL"),
    (22, "QAUTOTUNE"),
    (23, "QACRO"),
    (24, "THERMAL"),
    (25, "LOITER_ALT_QLAND"),
];

/// ArduRover (ground and boat frames) `custom_mode` -> mode name.
const ROVER_MODES: &[(u32, &str)] = &[
    (0, "MANUAL"),
    (1, "ACRO"),
    (3, "STEERING"),
    (4, "HOLD"),
    (5, "LOITER"),
    (6, "FOLLOW"),
    (7, "SIMPLE"),
    (10, "AUTO"),
    (11, "RTL"),
    (12, "SMART_RTL"),
    (15, "GUIDED"),
];

/// The ArduPilot firmware a MAV_TYPE identifies, which selects the mode table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArduPilotFirmware {
    Copter,
    Plane,
    Rover,
}

impl ArduPilotFirmware {
    /// The firmware behind a `HEARTBEAT.type` (MAV_TYPE wire value), or `None`
    /// for a type with no mode table here (generic, submarine, airship, antenna
    /// tracker, ...). A caller encoding a mode must refuse on `None` rather than
    /// guess: a number from the wrong table commands a different mode.
    pub fn from_mav_type(mav_type: i64) -> Option<Self> {
        match mav_type {
            // COAXIAL, HELICOPTER, QUADROTOR, HEXAROTOR, OCTOROTOR, TRICOPTER,
            // DODECAROTOR, DECAROTOR: the frames ArduCopter reports.
            2 | 3 | 4 | 13 | 14 | 15 | 29 | 35 => Some(Self::Copter),
            // FIXED_WING and the VTOL types (tailsitter duo/quad, tiltrotor,
            // fixed-rotor, tailsitter, tiltwing): ArduPlane and QuadPlane.
            1 | 19..=24 => Some(Self::Plane),
            // GROUND_ROVER, SURFACE_BOAT.
            10 | 11 => Some(Self::Rover),
            _ => None,
        }
    }

    /// The `custom_mode` -> name table for this firmware.
    pub fn modes(self) -> &'static [(u32, &'static str)] {
        match self {
            Self::Copter => COPTER_MODES,
            Self::Plane => PLANE_MODES,
            Self::Rover => ROVER_MODES,
        }
    }

    /// The name of `custom_mode` on this firmware, `None` when unmapped.
    pub fn mode_name(self, custom_mode: u32) -> Option<&'static str> {
        self.modes()
            .iter()
            .find(|(num, _)| *num == custom_mode)
            .map(|(_, name)| *name)
    }

    /// The `custom_mode` for an upper-case mode name on this firmware, `None`
    /// when the firmware has no such mode.
    pub fn custom_mode(self, name: &str) -> Option<u32> {
        self.modes()
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(num, _)| *num)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ArduPilotFirmware; 3] = [
        ArduPilotFirmware::Copter,
        ArduPilotFirmware::Plane,
        ArduPilotFirmware::Rover,
    ];

    #[test]
    fn mav_type_selects_the_firmware_table() {
        for t in [2, 3, 4, 13, 14, 15, 29, 35] {
            assert_eq!(
                ArduPilotFirmware::from_mav_type(t),
                Some(ArduPilotFirmware::Copter),
                "{t}"
            );
        }
        for t in [1, 19, 20, 21, 22, 23, 24] {
            assert_eq!(
                ArduPilotFirmware::from_mav_type(t),
                Some(ArduPilotFirmware::Plane),
                "{t}"
            );
        }
        for t in [10, 11] {
            assert_eq!(
                ArduPilotFirmware::from_mav_type(t),
                Some(ArduPilotFirmware::Rover),
                "{t}"
            );
        }
        // GENERIC, ANTENNA_TRACKER, AIRSHIP, SUBMARINE, VTOL_RESERVED5.
        for t in [0, 5, 7, 12, 25, -1] {
            assert_eq!(ArduPilotFirmware::from_mav_type(t), None, "{t}");
        }
    }

    #[test]
    fn rtl_differs_by_firmware() {
        assert_eq!(ArduPilotFirmware::Copter.custom_mode("RTL"), Some(6));
        assert_eq!(ArduPilotFirmware::Plane.custom_mode("RTL"), Some(11));
        assert_eq!(ArduPilotFirmware::Rover.custom_mode("RTL"), Some(11));
        // The copter RTL number is a manual mode elsewhere.
        assert_eq!(ArduPilotFirmware::Plane.mode_name(6), Some("FBWB"));
        assert_eq!(ArduPilotFirmware::Rover.mode_name(6), Some("FOLLOW"));
    }

    #[test]
    fn every_table_is_a_bijection() {
        for fw in ALL {
            for (num, name) in fw.modes() {
                assert_eq!(fw.custom_mode(name), Some(*num), "{fw:?} {name}");
                assert_eq!(fw.mode_name(*num), Some(*name), "{fw:?} {num}");
            }
        }
    }
}
